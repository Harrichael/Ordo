//! Carrying out core effects against macOS.
//!
//! The engine calls this synchronously on its own thread. Each result is the
//! executor's view of the *attempt* — "the write is queued for its app", "the
//! backend posted the gesture" — never a claim about the world, which is
//! confirmed only by the next snapshot. Writes to an app go onto that app's
//! queue ([`AppQueues`]), behind whatever the same effect list queued for it
//! before: a switch's focus lands after its app's moves and un-hide.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use ordo_core::{Effect, OpOutcome, Pid};

use crate::app_queue::AppQueues;
use crate::ports::Effector;

use super::restack_worker::RestackHandle;
use super::{ax, display, mouse, zorder, SharedBackend};

pub struct MacEffector {
    backend: SharedBackend,
    /// Shared with the event tap: flipping this is how interception is turned
    /// off (e.g. when rescue is engaged via the CLI rather than the hotkey).
    intercepting: Arc<AtomicBool>,
    /// Z-order is enforced off-thread: submitting is instant, and a newer
    /// order preempts an in-flight one instead of queueing behind it.
    restack: RestackHandle,
    queues: AppQueues,
}

impl MacEffector {
    pub fn new(
        backend: SharedBackend,
        intercepting: Arc<AtomicBool>,
        restack: RestackHandle,
        queues: AppQueues,
    ) -> Self {
        MacEffector {
            backend,
            intercepting,
            restack,
            queues,
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

    fn note_app_visibility(&mut self, pid: Pid, hidden: bool) {
        self.backend.borrow_mut().note_app_visibility(pid, hidden);
    }

    fn execute(&mut self, effect: &Effect) -> Option<OpOutcome> {
        // These change what is on screen, so the order in flight is stale
        // before the new one is submitted after them: left running, its
        // ghost watch reads this effect's own hides as late landings and
        // re-raises a workspace the user is leaving.
        if matches!(
            effect,
            Effect::SwitchWorkspace { .. }
                | Effect::MoveWindowToWorkspace { .. }
                | Effect::ViewMonitor { .. }
                | Effect::SetVirtualMonitors { .. }
                | Effect::MergeMonitors { .. }
        ) {
            self.restack.supersede();
        }
        match effect {
            Effect::FocusWindow { window, .. } => {
                let owner = zorder::owner_of(*window);
                if let Some(pid) = owner {
                    self.queues.focus(Pid(pid), *window);
                }
                Some(found_outcome(owner.is_some(), "focus: window not found"))
            }
            Effect::FocusDesktop { display, .. } => {
                let frame = display::active_displays()
                    .into_iter()
                    .find(|d| d.id == *display)
                    .map(|d| d.frame);
                Some(found_outcome(
                    frame.is_some_and(ax::focus_desktop),
                    "focus_desktop: no desktop window on that display",
                ))
            }
            Effect::SetWindowFrame { window, frame, .. } => {
                let owner = zorder::owner_of(*window);
                if let Some(pid) = owner {
                    self.queues.set_frame(Pid(pid), *window, *frame);
                }
                Some(found_outcome(owner.is_some(), "set_frame: window not found"))
            }
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
            Effect::MergeMonitors { from, into, .. } => Some(result_outcome(
                self.backend.borrow_mut().merge_monitors(*from, *into),
            )),
            Effect::AddMonitor { .. } => {
                Some(result_outcome(self.backend.borrow_mut().add_monitor()))
            }
            Effect::WarpMouse { to } => {
                mouse::warp_to(*to);
                None
            }
            Effect::RestackWindows {
                order,
                attached,
                focus_top,
            } => {
                let mut apps: Vec<Pid> = zorder::describe(order)
                    .into_iter()
                    .map(|(_, pid, _)| Pid(pid))
                    .collect();
                apps.sort_by_key(|p| p.0);
                apps.dedup();
                let landing = self.queues.marker(&apps);
                self.restack
                    .submit(order.clone(), attached.clone(), *focus_top, landing);
                None
            }
            Effect::SetIntercepting { enabled } => {
                self.intercepting.store(*enabled, Ordering::Relaxed);
                // Letting go of the screen (rescue, pause): anything still
                // queued would land on top of whatever takes over.
                if !*enabled {
                    self.queues.abandon();
                }
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
