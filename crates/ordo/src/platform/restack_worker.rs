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
use crate::engine::Msg;

/// How long a converged reassert stays listening for a late landing. Covers
/// the observed straggler tail (Ghostty/kitty raises confirmed up to ~1.6s
/// late) without holding the watch across unrelated activity.
const GHOST_WATCH_MS: u64 = 1800;

struct Shared {
    /// Bumped by every submit; a running reassert compares its own generation
    /// against this to learn it has been superseded.
    generation: AtomicU64,
    /// The latest desired order, replacing (never queueing behind) the last.
    slot: Mutex<Option<(u64, Vec<WindowId>)>>,
    wake: Condvar,
}

#[derive(Clone)]
pub struct RestackHandle {
    shared: Arc<Shared>,
}

impl RestackHandle {
    pub fn submit(&self, order: Vec<WindowId>) {
        let generation = self.shared.generation.fetch_add(1, Ordering::SeqCst) + 1;
        let mut slot = self.shared.slot.lock().unwrap();
        *slot = Some((generation, order));
        self.shared.wake.notify_one();
    }
}

/// Spawn the worker; the thread lives until the process exits (same lifetime
/// contract as the tap thread). AX and CG calls are plain Mach IPC and safe
/// off the main thread — the reassert builds its own elements per pass and
/// holds nothing between passes.
pub fn spawn(tx: Sender<Msg>, signals: Arc<RaiseSignals>) -> RestackHandle {
    let shared = Arc::new(Shared {
        generation: AtomicU64::new(0),
        slot: Mutex::new(None),
        wake: Condvar::new(),
    });
    let handle = RestackHandle {
        shared: shared.clone(),
    };
    std::thread::spawn(move || loop {
        let (generation, order) = {
            let mut slot = shared.slot.lock().unwrap();
            loop {
                if let Some(job) = slot.take() {
                    break job;
                }
                slot = shared.wake.wait(slot).unwrap();
            }
        };
        let cancel = || shared.generation.load(Ordering::SeqCst) != generation;
        let stats = super::zorder::reassert_stack(&order, &cancel, Some(&signals));
        let watch = stats
            .as_ref()
            .is_some_and(|s| s.converged && !s.aborted && !s.raises.is_empty());
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
                    // zero raises when the order in fact still holds.
                    if let Some(mut stats) =
                        super::zorder::reassert_stack(&order, &cancel, Some(&signals))
                    {
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
    });
    handle
}
