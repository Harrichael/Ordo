//! Display plug and unplug, debounced.
//!
//! A display change is not one event. CoreGraphics fires its reconfiguration
//! callback several times per plug (begin/end, per display), and for a second
//! or so afterwards macOS is still re-homing the vanished display's windows,
//! moving the Dock and menu bar to the new main display, and settling
//! bounds. A snapshot taken inside that window sees frames that are true for
//! a few hundred milliseconds and never again; believed, they would be
//! corrected, and a correction issued against a transient is a fight with the
//! WindowServer that nobody wins.
//!
//! So the callback records nothing but the time. While fewer than `SETTLE`
//! have passed since the last one, the world source reports the world as
//! unobservable (no displays — the same path that guards display sleep), and
//! once it has, one rescan is posted and everything re-projects at once onto
//! the rig as it actually is. One second is the judgment call: long enough
//! for the re-homing and Dock dance measured on plug events, short enough
//! that monitor memory still reads as immediate.

use std::ffi::c_void;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use crossbeam_channel::Sender;
use objc2_core_graphics::{
    CGDisplayChangeSummaryFlags, CGDisplayRegisterReconfigurationCallback, CGDirectDisplayID,
};
use ordo_core::RescanTrigger;

use crate::engine::Msg;

const SETTLE: Duration = Duration::from_millis(1000);

/// How often the poster checks whether the settle window has closed. Bounds
/// the latency added on top of `SETTLE`.
const POLL: Duration = Duration::from_millis(100);

/// When the display set last changed, shared between the CG callback (which
/// stamps it), the world source (which reads it) and the poster thread.
#[derive(Clone)]
pub struct DisplaySettle {
    /// Milliseconds since process start of the last reconfiguration; 0 = never.
    last_ms: Arc<AtomicU64>,
}

fn epoch() -> Instant {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    *EPOCH.get_or_init(Instant::now)
}

fn now_ms() -> u64 {
    // +1 so a stamp taken in the first millisecond is never the "never" value.
    epoch().elapsed().as_millis() as u64 + 1
}

impl DisplaySettle {
    pub fn new() -> Self {
        epoch();
        DisplaySettle {
            last_ms: Arc::new(AtomicU64::new(0)),
        }
    }

    fn stamp(&self) {
        self.last_ms.store(now_ms(), Ordering::Relaxed);
    }

    fn last(&self) -> u64 {
        self.last_ms.load(Ordering::Relaxed)
    }

    pub fn settling(&self) -> bool {
        let last = self.last();
        last != 0 && now_ms().saturating_sub(last) < SETTLE.as_millis() as u64
    }
}

impl Default for DisplaySettle {
    fn default() -> Self {
        Self::new()
    }
}

/// Register the reconfiguration callback and start the poster. Call on the
/// main thread before it enters the AppKit run loop, which is what delivers
/// the callback. Returns the handle the world source gates on.
pub fn install(tx: Sender<Msg>) -> DisplaySettle {
    let settle = DisplaySettle::new();
    // Leaked on purpose: the callback outlives everything but the process.
    let ctx = Box::into_raw(Box::new(settle.clone()));
    let err = unsafe {
        CGDisplayRegisterReconfigurationCallback(Some(callback), ctx as *mut c_void)
    };
    if err.0 != 0 {
        eprintln!("ordo: could not register for display changes (CGError {})", err.0);
    }

    let poster = settle.clone();
    std::thread::spawn(move || {
        let mut posted_for: u64 = 0;
        loop {
            std::thread::sleep(POLL);
            let last = poster.last();
            if last == 0 || last == posted_for || poster.settling() {
                continue;
            }
            posted_for = last;
            if tx
                .send(Msg::Rescan(RescanTrigger::BackendHint {
                    kind: "display_reconfigured".into(),
                }))
                .is_err()
            {
                return; // engine gone; the daemon is shutting down
            }
        }
    });
    settle
}

unsafe extern "C-unwind" fn callback(
    _display: CGDirectDisplayID,
    _flags: CGDisplayChangeSummaryFlags,
    userinfo: *mut c_void,
) {
    // Every flavour of change stamps: the begin flag, the end flag, and each
    // display's own add/remove/move all mean "not settled yet".
    let settle = &*(userinfo as *const DisplaySettle);
    settle.stamp();
}
