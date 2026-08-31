//! The imperative shell's heart: one thread, one serial loop.
//!
//! External inputs (hotkeys from the tap thread, observations from the
//! observer thread, the rescue signal) arrive as [`Event`]s over a channel.
//! For each, the engine runs the pure core, logs the whole step, and carries
//! out the resulting effects *inline on this thread*. Running effects here —
//! rather than on yet another thread — is deliberate: AX writes must be
//! serialized anyway, and keeping them off the event-tap thread (which must
//! never block) is the only hard constraint. A stuck app can stall this loop
//! for one messaging timeout; it can never wedge the keyboard.
//!
//! An effect that asks to look again ([`Effect::RequestRescan`]) is answered by
//! this thread taking a fresh snapshot and feeding it back as the next thing to
//! process, so the reaction to one external event is a single synchronous,
//! testable cascade rather than a race across threads.

use std::collections::VecDeque;

use crossbeam_channel::Receiver;
use ordo_core::{
    coalesce_hotkeys, update, Effect, Event, HotkeyAction, OpId, RescanTrigger, State,
};

use crate::clock::Clock;
use crate::logger::Logger;
use crate::ports::{Effector, RestackStats, WorldSource};

/// What the outside world sends the engine. Producers (the tap thread, the
/// periodic timer, later the AX observer and the rescue signal) speak this;
/// the engine stamps time and turns each into a core [`Event`]. Keeping the
/// clock on this side is what lets producers stay clock-free.
pub enum Msg {
    Hotkey(HotkeyAction),
    Rescan(RescanTrigger),
    Rescue,
    /// The engage chord (or a --paused run coming alive): leave Rescued mode,
    /// bringing the workspace model up from the state file.
    Engage,
    /// The engage-fresh chord: leave Rescued mode with a BLANK workspace
    /// model; the state file is neither read nor written until SaveState (or
    /// a later Engage) turns it back on.
    EngageFresh,
    /// The save-state chord: resume persistence and write the current model
    /// as the new durable state.
    SaveState,
    /// Telemetry from the restack worker, delivered as a message because the
    /// SQLite logger is engine-thread-only. Never becomes a core event.
    RestackStats(RestackStats),
    /// Stop the loop and close the run. Needed because producer threads (the
    /// event tap) hold sender clones that outlive shutdown, so channel-close
    /// alone can't end the loop.
    Shutdown,
}

/// A single external event can only spawn so many internal follow-ups before
/// we call it a runaway and stop. The core's damping should keep real cascades
/// far below this; the cap is a backstop, and hitting it is logged.
const MAX_CASCADE: usize = 64;

pub struct Engine {
    state: State,
    logger: Logger,
    world: Box<dyn WorldSource>,
    effector: Box<dyn Effector>,
    clock: Box<dyn Clock>,
}

impl Engine {
    pub fn new(
        logger: Logger,
        world: Box<dyn WorldSource>,
        effector: Box<dyn Effector>,
        clock: Box<dyn Clock>,
    ) -> Self {
        Engine {
            state: State::new(),
            logger,
            world,
            effector,
            clock,
        }
    }

    pub fn state(&self) -> &State {
        &self.state
    }

    /// Take a fresh observation and process it. The engine's snapshots always
    /// originate here (or from a `RequestRescan` cascade) — never from an
    /// external producer, which cannot touch the thread-affine world source.
    pub fn observe(&mut self, trigger: RescanTrigger) {
        let snap = self.world.snapshot();
        // No displays means the world is unobservable, not empty — displays
        // asleep make every window "missing", and believing that once erased
        // the whole model over a weekend. Discard the blind scan; the next
        // sighted one self-heals whatever actually changed.
        if snap.monitors.is_empty() {
            return;
        }
        self.pump(Event::WorldObserved {
            at: self.clock.now(),
            trigger,
            snap,
        });
    }

    /// Process messages until the channel closes (all senders dropped), then
    /// close out the run. Opens with a startup scan so the model reflects
    /// reality before the first hotkey.
    ///
    /// Messages are taken in batches — everything already queued is drained
    /// before processing — so that hotkeys which piled up while the engine was
    /// busy are seen TOGETHER and coalesced (`coalesce_hotkeys`) instead of
    /// replayed one by one: a queued backlog is one user gesture, not a
    /// script. Non-hotkey messages fence the coalescing and keep their order.
    pub fn run(mut self, rx: Receiver<Msg>) {
        self.observe(RescanTrigger::Startup);
        'recv: while let Ok(msg) = rx.recv() {
            let mut batch = vec![msg];
            while let Ok(m) = rx.try_recv() {
                batch.push(m);
            }
            let mut hotkeys: Vec<HotkeyAction> = Vec::new();
            for m in batch {
                match m {
                    Msg::Hotkey(action) => hotkeys.push(action),
                    // Deliberately NOT a coalescing fence: worker stats land
                    // mid-burst by construction (each burst switch aborts its
                    // predecessor's reassert, which reports back during the
                    // very key repeats being folded), and letting telemetry
                    // split a run would re-execute the stale intermediate
                    // switch the folding exists to skip.
                    Msg::RestackStats(stats) => {
                        let _ = self
                            .logger
                            .log_restack_stats(&stats, self.clock.now().wall_ms);
                    }
                    other => {
                        self.flush_hotkeys(&mut hotkeys);
                        match other {
                            Msg::Rescan(trigger) => self.observe(trigger),
                            Msg::Rescue => self.pump(Event::RescueEngaged {
                                at: self.clock.now(),
                            }),
                            // For both engage flavors the backend is set up
                            // FIRST, so the post-engage rescan reports the
                            // model the user chose (file-loaded or blank) and
                            // the core re-learns the world from it.
                            Msg::Engage => {
                                self.effector.bring_up_workspaces(true);
                                self.pump(Event::Engaged {
                                    at: self.clock.now(),
                                });
                                self.observe(RescanTrigger::BackendHint {
                                    kind: "engage".into(),
                                });
                            }
                            Msg::EngageFresh => {
                                self.effector.bring_up_workspaces(false);
                                self.pump(Event::Engaged {
                                    at: self.clock.now(),
                                });
                                self.observe(RescanTrigger::BackendHint {
                                    kind: "engage_fresh".into(),
                                });
                            }
                            Msg::SaveState => self.effector.persist_workspaces(),
                            Msg::Shutdown => break 'recv,
                            Msg::Hotkey(_) | Msg::RestackStats(_) => {
                                unreachable!("handled above")
                            }
                        }
                    }
                }
            }
            self.flush_hotkeys(&mut hotkeys);
        }
        let _ = self.logger.close(self.clock.now().wall_ms);
    }

    fn flush_hotkeys(&mut self, hotkeys: &mut Vec<HotkeyAction>) {
        for action in coalesce_hotkeys(&self.state, hotkeys) {
            self.pump(Event::Hotkey {
                at: self.clock.now(),
                action,
            });
        }
        hotkeys.clear();
    }

    /// Fully react to one external event, including any internal rescans and
    /// effect results it cascades into. Public for the integration tests, which
    /// drive it directly instead of through a channel.
    pub fn pump(&mut self, event: Event) {
        let mut queue = VecDeque::new();
        queue.push_back(event);

        let mut steps = 0;
        while let Some(ev) = queue.pop_front() {
            steps += 1;
            if steps > MAX_CASCADE {
                // Convergence backstop. Real cascades don't reach here; if one
                // does, the log's last checkpoint plus this gap tells the story.
                break;
            }

            let step = update(&self.state, &ev);
            let _ = self
                .logger
                .log_step(&ev, &step.effects, &step.notes, &step.state);
            if let Event::EffectResult { op, outcome, at } = &ev {
                let _ = self.logger.log_op_result(*op, outcome, at.wall_ms);
            }
            self.state = step.state;

            for effect in &step.effects {
                self.carry_out(effect, &mut queue);
            }
        }
    }

    fn carry_out(&mut self, effect: &Effect, queue: &mut VecDeque<Event>) {
        match effect {
            // Looking is always safe and never a mutation, so this is honored
            // even in observe mode. The fresh snapshot re-enters as the next
            // event, keeping the cascade single-threaded.
            Effect::RequestRescan { reason } => {
                let snap = self.world.snapshot();
                queue.push_back(Event::WorldObserved {
                    at: self.clock.now(),
                    trigger: reason.clone(),
                    snap,
                });
            }
            other => {
                let outcome = self.effector.execute(other);
                if let Some(outcome) = outcome {
                    if let Some(op) = effect_op(other) {
                        queue.push_back(Event::EffectResult {
                            at: self.clock.now(),
                            op,
                            outcome,
                        });
                    }
                }
            }
        }
    }
}

fn effect_op(e: &Effect) -> Option<OpId> {
    match e {
        Effect::SwitchWorkspace { op, .. }
        | Effect::MoveWindowToWorkspace { op, .. }
        | Effect::AssignWindowToWorkspace { op, .. }
        | Effect::SetWindowFrame { op, .. }
        | Effect::FocusWindow { op, .. } => Some(*op),
        Effect::WarpMouse { .. }
        | Effect::RestackWindows { .. }
        | Effect::RequestRescan { .. }
        | Effect::SetIntercepting { .. } => None,
    }
}
