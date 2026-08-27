//! Carrying out core effects against macOS.
//!
//! The engine calls this synchronously on its own thread. Each result is the
//! executor's view of the *attempt* — "the AX write returned success", "the
//! backend posted the gesture" — never a claim about the world, which is
//! confirmed only by the next snapshot.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use ordo_core::{Effect, OpOutcome};

use crate::ports::{Effector, RestackStats};

use super::{ax, mouse, SharedBackend};

pub struct MacEffector {
    backend: SharedBackend,
    /// Shared with the event tap: flipping this is how interception is turned
    /// off (e.g. when rescue is engaged via the CLI rather than the hotkey).
    intercepting: Arc<AtomicBool>,
    /// Parked here until the engine drains it, because the effect itself is
    /// fire-and-forget: telemetry rides beside the outcome, not in it.
    last_restack: Option<RestackStats>,
}

impl MacEffector {
    pub fn new(backend: SharedBackend, intercepting: Arc<AtomicBool>) -> Self {
        MacEffector {
            backend,
            intercepting,
            last_restack: None,
        }
    }
}

impl Effector for MacEffector {
    fn execute(&mut self, effect: &Effect) -> Option<OpOutcome> {
        match effect {
            Effect::FocusWindow { window, .. } => {
                Some(found_outcome(ax::focus(*window), "focus: window not found"))
            }
            Effect::SetWindowFrame { window, frame, .. } => Some(found_outcome(
                ax::set_frame(*window, *frame),
                "set_frame: window not found",
            )),
            Effect::SwitchWorkspace { target, .. } => Some(result_outcome(
                self.backend.borrow_mut().switch_workspace(*target),
            )),
            Effect::MoveWindowToWorkspace { window, target, .. } => Some(result_outcome(
                self.backend
                    .borrow_mut()
                    .move_window_to_workspace(*window, *target),
            )),
            Effect::WarpMouse { to } => {
                mouse::warp_to(*to);
                None
            }
            Effect::RestackWindows { order } => {
                self.last_restack = super::zorder::reassert_stack(order);
                None
            }
            Effect::SetIntercepting { enabled } => {
                self.intercepting.store(*enabled, Ordering::Relaxed);
                None
            }
            // The engine interprets this one itself (it owns the world source).
            Effect::RequestRescan { .. } => None,
        }
    }

    fn take_restack_stats(&mut self) -> Option<RestackStats> {
        self.last_restack.take()
    }
}

fn found_outcome(found: bool, not_found: &str) -> OpOutcome {
    if found {
        OpOutcome::Ok
    } else {
        OpOutcome::Failed {
            detail: not_found.to_string(),
        }
    }
}

fn result_outcome(r: crate::backend::Result<()>) -> OpOutcome {
    match r {
        Ok(()) => OpOutcome::Ok,
        Err(e) => OpOutcome::Failed {
            detail: e.to_string(),
        },
    }
}
