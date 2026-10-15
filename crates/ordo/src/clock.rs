//! Time enters the shell here and nowhere else, so the core stays clockless and
//! the whole event stream is something a test can fabricate.

use std::time::{Instant, SystemTime, UNIX_EPOCH};

use ordo_core::Ts;

pub trait Clock: Send {
    fn now(&self) -> Ts;
}

/// Wall time for "when did this happen" (what the user remembers); a monotonic
/// base captured at construction for ordering and latency that a wall-clock
/// step backwards can't corrupt.
pub struct SystemClock {
    mono_base: Instant,
}

impl SystemClock {
    pub fn new() -> Self {
        SystemClock {
            mono_base: Instant::now(),
        }
    }
}

impl Default for SystemClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for SystemClock {
    fn now(&self) -> Ts {
        Ts {
            wall_ms: wall_now_ms(),
            mono_ns: self.mono_base.elapsed().as_nanos() as u64,
        }
    }
}

/// Wall time for telemetry stamped where it is observed (a thread with no
/// `Clock`, such as the tap's), never for a core event.
pub fn wall_now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
