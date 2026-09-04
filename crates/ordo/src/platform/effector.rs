//! Carrying out core effects against macOS.
//!
//! The engine calls this synchronously on its own thread. Each result is the
//! executor's view of the *attempt* — "the AX write returned success", "the
//! backend posted the gesture" — never a claim about the world, which is
//! confirmed only by the next snapshot.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use ordo_core::{Effect, OpOutcome};

use crate::ports::Effector;

use super::restack_worker::RestackHandle;
use super::{ax, mouse, SharedBackend};

pub struct MacEffector {
    backend: SharedBackend,
    /// Shared with the event tap: flipping this is how interception is turned
    /// off (e.g. when rescue is engaged via the CLI rather than the hotkey).
    intercepting: Arc<AtomicBool>,
    /// Z-order is enforced off-thread: submitting is instant, and a newer
    /// order preempts an in-flight one instead of queueing behind it.
    restack: RestackHandle,
}

impl MacEffector {
    pub fn new(
        backend: SharedBackend,
        intercepting: Arc<AtomicBool>,
        restack: RestackHandle,
    ) -> Self {
        MacEffector {
            backend,
            intercepting,
            restack,
        }
    }
}

impl Effector for MacEffector {
    fn bring_up_workspaces(&mut self, use_state: bool) {
        self.backend.borrow_mut().bring_up(use_state);
    }

    fn persist_workspaces(&mut self) {
        self.backend.borrow_mut().resume_persistence();
    }

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
            Effect::AssignWindowToWorkspace { window, target, .. } => Some(result_outcome(
                self.backend
                    .borrow_mut()
                    .assign_window_to_workspace(*window, *target),
            )),
            Effect::AssignWindowToMonitor { window, target, .. } => Some(result_outcome(
                self.backend
                    .borrow_mut()
                    .assign_window_to_monitor(*window, *target),
            )),
            Effect::ViewMonitor { target, .. } => {
                Some(result_outcome(self.backend.borrow_mut().view_monitor(*target)))
            }
            Effect::SetVirtualMonitors { enabled, .. } => Some(result_outcome(
                self.backend.borrow_mut().set_virtual_monitors(*enabled),
            )),
            Effect::WarpMouse { to } => {
                mouse::warp_to(*to);
                None
            }
            Effect::RestackWindows { order } => {
                self.restack.submit(order.clone());
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
