//! Debug mode: diagnostics too costly to leave running, switched on from the
//! menu bar's Settings. Off at every launch, so a forgotten toggle never
//! outlives the session it was for. Off, each diagnostic costs one flag read.

use std::sync::atomic::{AtomicBool, Ordering};

static ENABLED: AtomicBool = AtomicBool::new(false);

pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

pub fn set(on: bool) {
    ENABLED.store(on, Ordering::Relaxed);
}
