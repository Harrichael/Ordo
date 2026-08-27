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
        let wall_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        Ts {
            wall_ms,
            mono_ns: self.mono_base.elapsed().as_nanos() as u64,
        }
    }
}
