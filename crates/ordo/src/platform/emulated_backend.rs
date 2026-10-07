//! The shell's binding of the emulated backend: [`ordo_emulated`] owns the
//! whole workspace model (ledger, parking, persistence, enforcement); this
//! adapter supplies its [`Desktop`] port from AX/CoreGraphics and presents the
//! result through the shell's [`WorkspaceBackend`] trait. Swapping backends is
//! choosing a crate — nothing above this file knows which one is underneath.

use std::collections::HashMap;
use std::path::PathBuf;

use ordo_core::{
    MonitorId, Pid, Rect, VirtualMonitorId, VirtualMonitorsWord, WindowId, WorkspaceId,
};
use ordo_emulated::{Desktop, EmulatedWorkspaces, Move, ParkTrace, ParkTraceKind, Unhide};

use crate::app_queue::AppQueues;

use crate::backend::{
    BackendError, BackendTopology, Capabilities, MonitorWorkspace, Result, WorkspaceBackend,
};

use super::{ax, display, zorder};

/// The AX/CG implementation of the emulated crate's `Desktop` port. Writes
/// go onto the apps' queues.
struct AxDesktop {
    queues: AppQueues,
}

impl Desktop for AxDesktop {
    fn frames(&self, windows: &[WindowId]) -> Vec<(WindowId, Pid, Rect)> {
        zorder::describe(windows)
            .into_iter()
            .map(|(w, pid, f)| (w, Pid(pid), f))
            .collect()
    }

    fn move_windows(&self, moves: &[Move]) {
        self.queues.move_windows(moves);
    }

    fn in_flight(&self, window: WindowId) -> bool {
        self.queues.in_flight(window, std::time::Instant::now())
    }

    fn busy(&self) -> bool {
        !self.queues.idle()
    }

    fn hide_app(&self, pid: Pid) {
        self.queues.hide(pid);
    }

    fn app_hidden(&self, pid: Pid) -> Option<bool> {
        ax::app_hidden(pid)
    }

    fn can_hide(&self, pid: Pid) -> bool {
        ax::has_dock_icon(pid)
    }

    fn stack(&self) -> Vec<WindowId> {
        zorder::stack_front_to_back()
    }

    fn traces_stacks(&self) -> bool {
        crate::debug::enabled()
    }

    fn now(&self) -> std::time::Instant {
        std::time::Instant::now()
    }

    fn show_apps(&self, apps: &[Unhide]) {
        for u in apps {
            self.queues.show(u.pid, u.hold.clone());
        }
    }

    fn focused_window(&self) -> Option<WindowId> {
        ax::focused_window()
    }

    fn frontmost_app(&self) -> Option<Pid> {
        ax::frontmost_app()
    }

    fn main_display(&self) -> Rect {
        let displays = display::active_displays();
        displays
            .iter()
            .find(|d| d.is_main)
            .or_else(|| displays.first())
            .map(|d| d.frame)
            .unwrap_or(Rect {
                x: 0.0,
                y: 0.0,
                w: 1920.0,
                h: 1080.0,
            })
    }

    fn displays(&self) -> Vec<Rect> {
        display::active_displays().iter().map(|d| d.frame).collect()
    }

    fn existing_windows(&self, ids: &[WindowId]) -> Option<std::collections::HashSet<WindowId>> {
        let all = super::zorder::all_windows()?;
        let all: std::collections::HashSet<WindowId> = all.into_iter().map(|l| l.id).collect();
        Some(ids.iter().filter(|w| all.contains(w)).copied().collect())
    }
}

pub struct EmulatedBackend {
    model: EmulatedWorkspaces,
    desktop: AxDesktop,
}

impl EmulatedBackend {
    pub fn new(count: u8, queues: AppQueues) -> Self {
        EmulatedBackend {
            model: EmulatedWorkspaces::new(count),
            desktop: AxDesktop { queues },
        }
    }

    pub fn with_persistence(count: u8, path: PathBuf, queues: AppQueues) -> Self {
        EmulatedBackend {
            model: EmulatedWorkspaces::with_persistence(count, path),
            desktop: AxDesktop { queues },
        }
    }
}

impl WorkspaceBackend for EmulatedBackend {
    fn topology(
        &mut self,
        windows: &HashMap<WindowId, (Pid, Rect)>,
        monitors: &[(MonitorId, Rect, bool)],
    ) -> Result<BackendTopology> {
        self.model.note_scan(&self.desktop, windows);
        let mons = monitors
            .iter()
            .map(|(id, _, _)| MonitorWorkspace {
                monitor: *id,
                active: self.model.current(),
                count: self.model.count(),
            })
            .collect();
        Ok(BackendTopology {
            monitors: mons,
            window_ws: self.model.window_ws().into_iter().collect(),
            virtual_monitors: Some(VirtualMonitorsWord {
                view: self.model.monitors(),
                assignments: self.model.window_monitors(),
            }),
        })
    }

    fn switch_workspace(&mut self, target: WorkspaceId) -> Result<()> {
        self.model.switch_workspace(&self.desktop, target);
        Ok(())
    }

    fn move_window_to_workspace(&mut self, window: WindowId, target: WorkspaceId) -> Result<()> {
        self.model
            .move_window_to_workspace(&self.desktop, window, target)
            .map_err(|e| BackendError(format!("workspace {} out of range", e.0 .0)))
    }

    fn assign_window_to_workspace(&mut self, window: WindowId, target: WorkspaceId) -> Result<()> {
        self.model
            .assign_window_to_workspace(window, target)
            .map_err(|e| BackendError(format!("workspace {} out of range", e.0 .0)))
    }

    fn view_monitor(&mut self, target: VirtualMonitorId) -> Result<()> {
        self.model
            .view_monitor(&self.desktop, target)
            .map_err(|e| BackendError(format!("virtual monitor {} out of range", e.0 .0)))
    }

    fn set_virtual_monitors(&mut self, enabled: bool) -> Result<()> {
        self.model.set_virtual_monitors(&self.desktop, enabled);
        Ok(())
    }

    fn merge_monitors(&mut self, from: VirtualMonitorId, into: VirtualMonitorId) -> Result<()> {
        self.model
            .merge_monitors(&self.desktop, from, into)
            .map_err(|_| BackendError(format!("cannot merge monitor {} into {}", from.0, into.0)))
    }

    fn add_monitor(&mut self) -> Result<()> {
        self.model
            .add_monitor(&self.desktop)
            .map_err(|_| BackendError("no room for another virtual monitor".into()))
    }

    fn move_workspace(&mut self, from: WorkspaceId, to: WorkspaceId) -> Result<()> {
        self.model
            .move_workspace(&self.desktop, from, to)
            .map_err(|_| BackendError(format!("cannot move workspace {} to {}", from.0, to.0)))
    }

    fn move_monitor(&mut self, from: VirtualMonitorId, to: VirtualMonitorId) -> Result<()> {
        self.model
            .move_monitor(&self.desktop, from, to)
            .map_err(|_| BackendError(format!("cannot move monitor {} to {}", from.0, to.0)))
    }

    fn assign_window_to_monitor(&mut self, window: WindowId, target: VirtualMonitorId) -> Result<()> {
        self.model
            .assign_window_to_monitor(window, target)
            .map_err(|e| BackendError(format!("virtual monitor {} out of range", e.0 .0)))
    }

    fn rescue_window(&mut self, window: WindowId) -> Result<()> {
        self.model.rescue_window(&self.desktop, window);
        Ok(())
    }

    fn bring_up(&mut self, use_state: bool) {
        self.model.bring_up(use_state);
    }

    fn resume_persistence(&mut self) {
        self.model.resume_persistence(&self.desktop);
    }

    fn note_app_visibility(&mut self, pid: Pid, hidden: bool) {
        self.model.note_app_visibility(pid, hidden);
    }

    fn set_hiding(&mut self, hiding: ordo_emulated::Hiding) {
        self.model.set_hiding(hiding);
    }

    fn reveal_for_focus(&mut self, pid: Pid) -> Option<Vec<(WindowId, ordo_core::Point)>> {
        self.model.reveal_for_focus(&self.desktop, pid)
    }

    fn enforce_placement(&mut self, frames: &HashMap<WindowId, (Pid, Rect)>) {
        self.model.enforce_placement(&self.desktop, frames);
    }

    fn believed_frames(&self, frames: &HashMap<WindowId, (Pid, Rect)>) -> HashMap<WindowId, Rect> {
        self.model.believed_frames(&self.desktop, frames)
    }

    fn take_park_trace(&mut self) -> Vec<ParkTrace> {
        let mut trace = self.model.take_trace();
        trace.extend(
            self.desktop
                .queues
                .take_chains()
                .into_iter()
                .map(|c| ParkTrace::app(c.pid, ParkTraceKind::AppChain).chain(c)),
        );
        trace
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            // The whole point of emulation: we can mint workspaces freely.
            fixed_workspace_count: false,
            max_workspaces: self.model.count(),
        }
    }
}
