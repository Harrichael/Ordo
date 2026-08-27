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

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use crossbeam_channel::Sender;
use ordo_core::WindowId;

use crate::engine::Msg;

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
pub fn spawn(tx: Sender<Msg>) -> RestackHandle {
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
        if let Some(stats) = super::zorder::reassert_stack(&order, &cancel) {
            if tx.send(Msg::RestackStats(stats)).is_err() {
                return; // engine gone; nothing left to report to
            }
        }
    });
    handle
}
