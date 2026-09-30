//! The imperative shell's heart: one thread, one serial loop.
//!
//! External inputs (hotkeys from the tap thread, observations from the
//! observer thread, the rescue signal) arrive as [`Event`]s over a channel.
//! For each, the engine runs the pure core, logs the whole step, and carries
//! out the resulting effects on this thread, in order. Carrying one out is
//! mostly handing it on: writes to apps go onto each app's own queue
//! ([`crate::app_queue`]) and restacks to the restack worker, so a slow app
//! never holds up the loop through a write. Reads still run here — the
//! snapshot's AX walk — and a stuck app can stall one for a messaging
//! timeout; it can never wedge the keyboard, whose tap thread never blocks.
//!
//! An effect that asks to look again ([`Effect::RequestRescan`]) is answered by
//! this thread taking a fresh snapshot and feeding it back as the next thing to
//! process, so the reaction to one external event is a single synchronous,
//! testable cascade rather than a race across threads. The exception is a
//! look asked for while the apps are still busy with Ordo's writes: every
//! read would wait behind them, so it is handed to a [`LookGate`] and comes
//! back as an ordinary rescan once they are done, or after [`LOOK_BOUND`].
//! Presses meanwhile resolve against the workspace the core declared.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use crossbeam_channel::Receiver;
use ordo_core::{
    coalesce_hotkeys, update, AxHintKind, Effect, Event, Gesture, HotkeyAction, OpId, Pid,
    RescanTrigger, State,
};

use crate::clock::Clock;
use crate::logger::{HotkeyBatch, Logger};
use crate::ports::{Effector, LookGate, RestackStats, SnapshotStats, WorldSource};

/// The longest a look is held back while the apps are busy. Bounds how stale
/// the core's picture of frames and focus can get during a burst, and keeps
/// an app that never goes idle from holding off every look.
pub const LOOK_BOUND: Duration = Duration::from_millis(250);

/// What the outside world sends the engine. Producers (the tap thread, the
/// periodic timer, later the AX observer and the rescue signal) speak this;
/// the engine stamps time and turns each into a core [`Event`]. Keeping the
/// clock on this side is what lets producers stay clock-free.
pub enum Msg {
    /// Carries the moment of the press, which only telemetry reads: the core
    /// event is stamped at dequeue like every other, so replay never depends
    /// on it. Build with [`Msg::hotkey`].
    Hotkey(HotkeyAction, Instant),
    /// A focus-moving user gesture the tap witnessed and passed through (a
    /// click, Cmd+Tab). Not a hotkey: nothing is executed for it, but it is
    /// intent, and its place in the order relative to hotkeys and snapshots
    /// is exactly what the core reads it for.
    Gesture(Gesture),
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
    /// An app was hidden (true) or shown, by anyone, for the workspace
    /// backend. Never becomes a core event; the observer follows it with a
    /// rescan hint, which is what gets it acted on.
    AppVisibility(Pid, bool),
    /// Stop the loop and close the run. Needed because producer threads (the
    /// event tap) hold sender clones that outlive shutdown, so channel-close
    /// alone can't end the loop.
    Shutdown,
}

impl Msg {
    pub fn hotkey(action: HotkeyAction) -> Msg {
        Msg::Hotkey(action, Instant::now())
    }
}

/// Between two real fences, every queued rescan is one look at the world, and
/// it goes FIRST. A rescan's place in the queue says nothing: its snapshot is
/// taken when processed, not when hinted, so it reads the present wherever it
/// sits. Left in place, the echoes of Ordo's own switches (focus
/// notifications, mostly) landed between queued presses, fenced each from the
/// next, and turned a burst the user had already moved past into a run of
/// full switches — the worst press measured waited behind three. Looking once
/// up front keeps what the rescans were for: every hotkey still acts on a
/// fresh look. Creation hints survive, one per app, because only they
/// authorize corralling a new window; every other trigger means just "go
/// look". Gestures and the mode messages stay fences: their place is intent.
fn collapse_rescans(batch: Vec<Msg>) -> Vec<Msg> {
    let mut out = Vec::with_capacity(batch.len());
    let mut rescans: Vec<RescanTrigger> = Vec::new();
    // Hotkeys, and telemetry, which fences nothing either (see `run`).
    let mut after: Vec<Msg> = Vec::new();
    for m in batch {
        match m {
            Msg::Rescan(trigger) => rescans.push(trigger),
            Msg::Hotkey(..) | Msg::RestackStats(_) => after.push(m),
            // Ahead of the look the batch's rescans become, so that look acts
            // on it.
            Msg::AppVisibility(..) => out.push(m),
            fence => {
                flush_rescans(&mut rescans, &mut out);
                out.append(&mut after);
                out.push(fence);
            }
        }
    }
    flush_rescans(&mut rescans, &mut out);
    out.append(&mut after);
    out
}

fn flush_rescans(run: &mut Vec<RescanTrigger>, out: &mut Vec<Msg>) {
    let mut births: Vec<RescanTrigger> = Vec::new();
    let mut last = None;
    for trigger in run.drain(..) {
        match trigger {
            RescanTrigger::AxHint {
                kind: AxHintKind::WindowCreated,
                ..
            } => {
                if !births.contains(&trigger) {
                    births.push(trigger);
                }
            }
            _ => last = Some(trigger),
        }
    }
    if births.is_empty() {
        out.extend(last.map(Msg::Rescan));
    } else {
        out.extend(births.into_iter().map(Msg::Rescan));
    }
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
    on_state: Option<StateWatcher>,
    /// Each queued snapshot's cost, in the order their events were queued: a
    /// snapshot taken mid-cascade has no sequence number until its event is
    /// logged, and events leave the queue in the order they entered it.
    snapshot_costs: VecDeque<Option<SnapshotStats>>,
    gate: Option<Box<dyn LookGate>>,
    /// When the oldest look still held back was first asked for.
    held_since: Option<Instant>,
    /// A gesture has been pumped since the last look. Its look is not held:
    /// the core reads a gesture on the very next look, and a press in the
    /// meantime would clear it.
    gesture_unseen: bool,
}

type StateWatcher = Box<dyn FnMut(&State)>;

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
            on_state: None,
            snapshot_costs: VecDeque::new(),
            gate: None,
            held_since: None,
            gesture_unseen: false,
        }
    }

    /// Hold looks back while the apps are busy with Ordo's writes, rather
    /// than take them into apps that answer only once those writes are done.
    pub fn with_look_gate(mut self, gate: Box<dyn LookGate>) -> Self {
        self.gate = Some(gate);
        self
    }

    /// Whether a look asked for now should be held back instead.
    fn look_must_wait(&mut self) -> bool {
        let Some(gate) = &self.gate else {
            return false;
        };
        if self.gesture_unseen || gate.idle() {
            return false;
        }
        let since = *self.held_since.get_or_insert_with(Instant::now);
        since.elapsed() < LOOK_BOUND
    }

    fn looked(&mut self) {
        self.held_since = None;
        self.gesture_unseen = false;
    }

    /// A look asked for from outside: taken now, or held back until the apps
    /// are idle (see [`LookGate`]).
    fn look(&mut self, trigger: RescanTrigger) {
        if !self.hold_look(trigger.clone()) {
            self.observe(trigger);
        }
    }

    /// Hand the look to the gate if it must wait; whether it was.
    fn hold_look(&mut self, trigger: RescanTrigger) -> bool {
        if !self.look_must_wait() {
            return false;
        }
        if let (Some(gate), Some(since)) = (&self.gate, self.held_since) {
            gate.defer(trigger, since + LOOK_BOUND);
        }
        true
    }

    /// Hand the state to `f` whenever a batch of messages has been fully
    /// processed — how displays outside the loop (the menu bar) keep up
    /// without reaching into it. Settled state only: never mid-cascade.
    pub fn on_state(mut self, f: impl FnMut(&State) + 'static) -> Self {
        self.on_state = Some(Box::new(f));
        self
    }

    fn publish(&mut self) {
        if let Some(f) = &mut self.on_state {
            f(&self.state);
        }
    }

    pub fn state(&self) -> &State {
        &self.state
    }

    /// Move the world source's diagnostic account of the workspace mechanism
    /// into the log. Kept off the event stream deliberately: it is telemetry,
    /// and a replay must not depend on it.
    fn drain_park_trace(&mut self) {
        let traces = self.world.take_park_trace();
        if traces.is_empty() {
            return;
        }
        let _ = self
            .logger
            .log_park_trace(&traces, self.clock.now().wall_ms);
    }

    /// Take a fresh observation and process it. The engine's snapshots always
    /// originate here (or from a `RequestRescan` cascade) — never from an
    /// external producer, which cannot touch the thread-affine world source.
    pub fn observe(&mut self, trigger: RescanTrigger) {
        self.looked();
        let snap = self.world.snapshot();
        self.drain_park_trace();
        let cost = self.world.take_snapshot_stats();
        // No displays means the world is unobservable, not empty — displays
        // asleep make every window "missing", and believing that once erased
        // the whole model over a weekend. Discard the blind scan; the next
        // sighted one self-heals whatever actually changed.
        if snap.monitors.is_empty() {
            return;
        }
        self.snapshot_costs.push_back(cost);
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
    /// script. Gestures and the mode messages fence the coalescing and keep
    /// their order, since "hotkey, click, hotkey" is not one burst. Rescans do
    /// not: the batch's rescans become one look ahead of its hotkeys (see
    /// [`collapse_rescans`]).
    pub fn run(mut self, rx: Receiver<Msg>) {
        self.observe(RescanTrigger::Startup);
        self.publish();
        'recv: while let Ok(msg) = rx.recv() {
            let mut batch = vec![msg];
            while let Ok(m) = rx.try_recv() {
                batch.push(m);
            }
            let batch = collapse_rescans(batch);
            let mut hotkeys: Vec<(HotkeyAction, Instant)> = Vec::new();
            for m in batch {
                match m {
                    Msg::Hotkey(action, pressed) => hotkeys.push((action, pressed)),
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
                    // Not a fence either: a switch's own un-hides report
                    // mid-burst. Recording it is instant.
                    Msg::AppVisibility(pid, hidden) => {
                        self.effector.note_app_visibility(pid, hidden);
                    }
                    other => {
                        self.flush_hotkeys(&mut hotkeys);
                        match other {
                            Msg::Rescan(trigger) => self.look(trigger),
                            Msg::Gesture(gesture) => {
                                self.gesture_unseen = true;
                                self.pump(Event::Gesture {
                                    at: self.clock.now(),
                                    gesture,
                                })
                            }
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
                            Msg::Hotkey(..) | Msg::RestackStats(_) | Msg::AppVisibility(..) => {
                                unreachable!("handled above")
                            }
                        }
                    }
                }
            }
            self.flush_hotkeys(&mut hotkeys);
            self.publish();
        }
        let _ = self.logger.close(self.clock.now().wall_ms);
    }

    fn flush_hotkeys(&mut self, hotkeys: &mut Vec<(HotkeyAction, Instant)>) {
        let (Some(oldest), Some(newest)) = (
            hotkeys.iter().map(|h| h.1).min(),
            hotkeys.iter().map(|h| h.1).max(),
        ) else {
            return;
        };
        let presses = hotkeys.len();
        let actions: Vec<HotkeyAction> = hotkeys.drain(..).map(|h| h.0).collect();
        let oldest_wait = oldest.elapsed();
        let newest_wait = newest.elapsed();
        let first_seq = self.logger.next_seq();
        let coalesced = coalesce_hotkeys(&self.state, &actions);
        let pumped = coalesced.len();
        for action in coalesced {
            self.pump(Event::Hotkey {
                at: self.clock.now(),
                action,
            });
        }
        let _ = self.logger.log_hotkey_batch(
            &HotkeyBatch {
                first_seq: (pumped > 0).then_some(first_seq),
                presses,
                pumped,
                oldest_wait,
                newest_wait,
            },
            self.clock.now().wall_ms,
        );
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
            let seq = self
                .logger
                .log_step(&ev, &step.effects, &step.notes, &step.state);
            if let Event::WorldObserved { .. } = &ev {
                if let (Ok(seq), Some(Some(cost))) = (seq, self.snapshot_costs.pop_front()) {
                    let _ = self
                        .logger
                        .log_snapshot_stats(seq, &cost, self.clock.now().wall_ms);
                }
            }
            if let Event::EffectResult { op, outcome, at } = &ev {
                let _ = self.logger.log_op_result(*op, outcome, at.wall_ms);
            }
            self.state = step.state;

            for effect in &step.effects {
                self.carry_out(effect, &mut queue);
            }
        }
        // A cascade cut off at the cap leaves queued snapshots unlogged.
        self.snapshot_costs.clear();
    }

    fn carry_out(&mut self, effect: &Effect, queue: &mut VecDeque<Event>) {
        match effect {
            // Looking is always safe and never a mutation, so this is honored
            // even in observe mode. The fresh snapshot re-enters as the next
            // event, keeping the cascade single-threaded.
            Effect::RequestRescan { reason } => {
                if self.hold_look(reason.clone()) {
                    return;
                }
                self.looked();
                let snap = self.world.snapshot();
                self.drain_park_trace();
                let cost = self.world.take_snapshot_stats();
                // The same blind-scan discard as `observe`: a post-effect
                // rescan during display sleep or a display reconfiguration
                // must not feed the core an empty world.
                if snap.monitors.is_empty() {
                    return;
                }
                self.snapshot_costs.push_back(cost);
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
        | Effect::AssignWindowToMonitor { op, .. }
        | Effect::ViewMonitor { op, .. }
        | Effect::SetVirtualMonitors { op, .. }
        | Effect::MergeMonitors { op, .. }
        | Effect::AddMonitor { op }
        | Effect::SetWindowFrame { op, .. }
        | Effect::FocusWindow { op, .. }
        | Effect::FocusDesktop { op, .. } => Some(*op),
        Effect::WarpMouse { .. }
        | Effect::RestackWindows { .. }
        | Effect::RequestRescan { .. }
        | Effect::SetIntercepting { .. } => None,
    }
}
