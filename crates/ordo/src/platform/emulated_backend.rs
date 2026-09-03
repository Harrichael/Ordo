//! The shell's binding of the emulated backend: [`ordo_emulated`] owns the
//! whole workspace model (ledger, parking, persistence, enforcement); this
//! adapter supplies its [`Desktop`] port from AX/CoreGraphics and presents the
//! result through the shell's [`WorkspaceBackend`] trait. Swapping backends is
//! choosing a crate — nothing above this file knows which one is underneath.

use std::collections::HashMap;
use std::path::PathBuf;

use ordo_core::{MonitorId, Pid, Point, Rect, WindowId, WorkspaceId};
use ordo_emulated::{Desktop, EmulatedWorkspaces, HoldStat, Unhide};

use crate::backend::{
    BackendError, BackendTopology, Capabilities, MonitorWorkspace, Result, WorkspaceBackend,
};

use super::{ax, display};

/// The AX/CG implementation of the emulated crate's `Desktop` port.
struct AxDesktop;

impl Desktop for AxDesktop {
    fn windows(&self) -> Vec<(WindowId, Pid, Rect)> {
        ax::windows()
            .into_iter()
            .map(|w| (w.id, w.app, w.frame))
            .collect()
    }

    fn move_windows(&self, moves: &[(Pid, WindowId, Point)]) {
        ax::move_windows(moves);
    }

    fn hide_app(&self, pid: Pid) {
        ax::set_app_hidden(pid, true);
    }

    fn show_apps(&self, apps: &[Unhide]) -> Vec<HoldStat> {
        let holds: Vec<(Pid, &[(WindowId, Point)])> =
            apps.iter().map(|u| (u.pid, u.hold.as_slice())).collect();
        ax::show_apps(&holds)
            .into_iter()
            .map(|o| {
                if !o.escaped.is_empty() {
                    eprintln!(
                        "ordo: un-hiding pid {} left {} window(s) off the corner after {}ms",
                        o.pid.0,
                        o.escaped.len(),
                        o.elapsed_ms
                    );
                }
                HoldStat::new(
                    o.pid,
                    apps.iter()
                        .find(|u| u.pid == o.pid)
                        .map_or(0, |u| u.hold.len()),
                    o.writes,
                    o.elapsed_ms,
                    o.escaped,
                )
            })
            .collect()
    }

    fn focused_window(&self) -> Option<WindowId> {
        ax::focused_window()
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
        let all: std::collections::HashSet<WindowId> = all.into_iter().collect();
        Some(ids.iter().filter(|w| all.contains(w)).copied().collect())
    }
}

pub struct EmulatedBackend {
    model: EmulatedWorkspaces,
    desktop: AxDesktop,
}

impl EmulatedBackend {
    pub fn new(count: u8) -> Self {
        EmulatedBackend {
            model: EmulatedWorkspaces::new(count),
            desktop: AxDesktop,
        }
    }

    pub fn with_persistence(count: u8, path: PathBuf) -> Self {
        EmulatedBackend {
            model: EmulatedWorkspaces::with_persistence(count, path),
            desktop: AxDesktop,
        }
    }
}

impl WorkspaceBackend for EmulatedBackend {
    fn topology(
        &mut self,
        windows: &[(WindowId, Pid)],
        monitors: &[(MonitorId, bool)],
    ) -> Result<BackendTopology> {
        self.model.note_scan(&self.desktop, windows);
        let mons = monitors
            .iter()
            .map(|(id, _)| MonitorWorkspace {
                monitor: *id,
                active: self.model.current(),
                count: self.model.count(),
            })
            .collect();
        Ok(BackendTopology {
            monitors: mons,
            window_ws: self.model.window_ws().into_iter().collect(),
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

    fn enforce_placement(&mut self, frames: &HashMap<WindowId, (Pid, Rect)>) {
        self.model.enforce_placement(&self.desktop, frames);
    }

    fn believed_frames(&self, frames: &HashMap<WindowId, (Pid, Rect)>) -> HashMap<WindowId, Rect> {
        self.model.believed_frames(&self.desktop, frames)
    }

    fn take_park_trace(&mut self) -> Vec<ordo_emulated::ParkTrace> {
        self.model.take_trace()
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            // The whole point of emulation: we can mint workspaces freely.
            fixed_workspace_count: false,
            max_workspaces: self.model.count(),
        }
    }
}
