//! The restack worker: z-order enforcement off the engine thread.
//!
//! A reassert is the one slow effect the core never needs to hear back from —
//! no op, no expectation, no belief (z-order is derived from MRU each time).
//! Running it inline made the engine deaf for its landing gates (measured up
//! to ~1.9s on a ghosted raise), which queued hotkeys and replayed them stale.
//! This worker owns all raising instead: `submit` is instant, and only the
//! LATEST desired order matters — a newer submit bumps the generation, the
//! in-flight reassert sees it via its cancel hook and yields mid-gate.
//!
//! One deliberate overlap: a raise issued for generation N can land while the
//! engine is already executing generation N+1's focus/park writes. That is
//! the same ghost the reassert's second pass has always absorbed — the
//! successor re-reads the world and re-raises what the straggler displaced.
//! Stats flow back to the engine as a message because the SQLite logger is
//! engine-thread-only by design.
//!
//! The GHOST WATCH closes the last hole in that story: a raise the reassert
//! stopped waiting for (cancelled generation, or a landing timeout) can land
//! AFTER the final read-back said converged — the lived symptom was a stale
//! window sitting wrong until the 2s rescan noticed. With the WindowServer's
//! push stream, that late landing announces itself (808/815 for a window we
//! just ordered), so the worker lingers after converging and reruns the
//! reassert on such a signal. Bounded to one rerun per generation: our own
//! rerun emits the same events it listens for, and a user's click-raise
//! inside the watch window must not start a fight (the click's focus change
//! mints a fresh generation with the user's window on top anyway).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use crossbeam_channel::Sender;
use ordo_core::WindowId;

use super::ws_events::{RaiseSignals, WaitOutcome};
use crate::app_queue::{AppQueues, Landing};
use crate::engine::Msg;

/// How long a converged reassert stays listening for a late landing. Covers
/// the observed straggler tail (Ghostty/kitty raises confirmed up to ~1.6s
/// late) without holding the watch across unrelated activity.
const GHOST_WATCH_MS: u64 = 1800;

/// The longest a restack waits for its apps to land what was queued before
/// it. Past this it plans against the screen as it stands, as it did before
/// writes were queued: an app that slow will be raised around, not waited on.
const LANDING_WAIT: Duration = Duration::from_millis(600);

struct Shared {
    /// Bumped by every submit; a running reassert compares its own generation
    /// against this to learn it has been superseded.
    generation: AtomicU64,
    /// The latest desired order and whether its top is to be made key,
    /// replacing (never queueing behind) the last.
    slot: Mutex<Option<Job>>,
    wake: Condvar,
}

struct Job {
    generation: u64,
    order: Vec<WindowId>,
    attached: Vec<(WindowId, WindowId)>,
    focus_top: bool,
    /// The switch's writes to the apps of `order`; raises only mean
    /// something once the windows are where they are going and showing.
    landing: Landing,
    /// The newest focus when this order was decided: a take-back on its
    /// behalf is dropped once a newer one has been asked for.
    focus_gen: u64,
}

#[derive(Clone)]
pub struct RestackHandle {
    shared: Arc<Shared>,
}

impl RestackHandle {
    pub fn submit(
        &self,
        order: Vec<WindowId>,
        attached: Vec<(WindowId, WindowId)>,
        focus_top: bool,
        landing: Landing,
        focus_gen: u64,
    ) {
        let generation = self.shared.generation.fetch_add(1, Ordering::SeqCst) + 1;
        let mut slot = self.shared.slot.lock().unwrap();
        *slot = Some(Job {
            generation,
            order,
            attached,
            focus_top,
            landing,
            focus_gen,
        });
        self.shared.wake.notify_one();
    }

    /// Cancel the order in flight, and its ghost watch, without a new one.
    pub fn supersede(&self) {
        self.shared.generation.fetch_add(1, Ordering::SeqCst);
        *self.shared.slot.lock().unwrap() = None;
    }
}

/// Spawn the worker; the thread lives until the process exits (same lifetime
/// contract as the tap thread). AX and CG calls are plain Mach IPC and safe
/// off the main thread — the reassert builds its own elements per pass and
/// holds nothing between passes.
pub fn spawn(tx: Sender<Msg>, signals: Arc<RaiseSignals>, queues: AppQueues) -> RestackHandle {
    let shared = Arc::new(Shared {
        generation: AtomicU64::new(0),
        slot: Mutex::new(None),
        wake: Condvar::new(),
    });
    let handle = RestackHandle {
        shared: shared.clone(),
    };
    std::thread::spawn(move || {
        // A cancelled generation's raises can still land after its successor
        // converged, so the watch is owed even when the successor raised nothing:
        // its plan is minimal, and often touches none of them.
        let mut owed = false;
        loop {
            let Job {
                generation,
                order,
                attached,
                focus_top,
                landing,
                focus_gen,
            } = {
                let mut slot = shared.slot.lock().unwrap();
                loop {
                    if let Some(job) = slot.take() {
                        break job;
                    }
                    slot = shared.wake.wait(slot).unwrap();
                }
            };
            let cancel = || shared.generation.load(Ordering::SeqCst) != generation;
            let focus = |w: WindowId| {
                super::zorder::owner_of(w)
                    .is_some_and(|pid| queues.focus_if_current(ordo_core::Pid(pid), w, focus_gen))
            };
            let waited = Instant::now();
            landing.wait(waited + LANDING_WAIT, &cancel);
            let landing_wait_ms = waited.elapsed().as_millis() as u64;
            let mut stats = super::zorder::reassert_stack(
                &order,
                &attached,
                focus_top,
                &cancel,
                Some(&signals),
                &focus,
            );
            if let Some(s) = &mut stats {
                s.landing_wait_ms = landing_wait_ms;
            }
            let issued = stats.as_ref().is_some_and(|s| !s.raises.is_empty());
            let watch = stats
                .as_ref()
                .is_some_and(|s| s.converged && !s.aborted && (issued || owed));
            owed = match &stats {
                None => owed,
                Some(s) if s.aborted => owed || issued,
                Some(_) => false,
            };
            if let Some(stats) = stats {
                if tx.send(Msg::RestackStats(stats)).is_err() {
                    return; // engine gone; nothing left to report to
                }
            }
            if !watch {
                continue;
            }
            // Ghost watch: an 808/815 for an ORDERED window after convergence is
            // a landing we stopped waiting for, touching down where the read-back
            // can no longer see it. The cursor starts HERE — our own raises'
            // events are behind it — and is refreshed past each rerun's own
            // events, or the watch would feed on itself. Two reruns is the cap:
            // more within one watch means something (a user, an app) is actively
            // reordering, and the next real generation owns that fight.
            let ordered = |w: u32| order.contains(&WindowId(w));
            let deadline = Instant::now() + Duration::from_millis(GHOST_WATCH_MS);
            let mut cursor = signals.cursor();
            for _ in 0..2 {
                match signals.wait(&mut cursor, &ordered, deadline, &cancel) {
                    WaitOutcome::Hint => {
                        // The rerun is the read-back: it exits converged with
                        // zero raises when the order in fact still holds. It
                        // never takes focus: a late landing doesn't move focus,
                        // and a click in the watch window would be fought.
                        if let Some(mut stats) = super::zorder::reassert_stack(
                            &order,
                            &attached,
                            false,
                            &cancel,
                            Some(&signals),
                            &focus,
                        ) {
                            stats.ghost_pass = true;
                            if tx.send(Msg::RestackStats(stats)).is_err() {
                                return;
                            }
                        }
                        cursor = signals.cursor();
                    }
                    WaitOutcome::Timeout | WaitOutcome::Cancelled => break,
                }
            }
        }
    });
    handle
}
