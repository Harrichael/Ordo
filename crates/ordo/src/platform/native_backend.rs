//! The native workspace backend: workspaces *are* macOS Spaces.
//!
//! Reads (topology) are implemented and SIP-free. Mutations (switch a space,
//! move a window between spaces) arrive in milestone M4 — until then they return
//! an error rather than panicking, so an accidental call is logged, not fatal.
//! The ordinal <-> space-id translation lives entirely in [`super::skylight`];
//! this type is the trait-shaped front for it.

use std::time::Duration;

use ordo_core::{MonitorId, WindowId, WorkspaceId};

use crate::backend::{
    BackendError, BackendTopology, Capabilities, MonitorWorkspace, Result, WorkspaceBackend,
};

use super::{display, gesture, mouse, skylight};

/// macOS caps user spaces at 16 per display.
const MAX_SPACES_PER_DISPLAY: u8 = 16;

/// Between synthesized swipes, so Mission Control registers each as a discrete
/// space change rather than coalescing them.
const SWIPE_GAP: Duration = Duration::from_millis(120);

pub struct NativeBackend {
    cid: ordo_skylight_sys::CgsConnectionId,
}

impl NativeBackend {
    pub fn new() -> Self {
        NativeBackend {
            cid: skylight::connection(),
        }
    }
}

impl Default for NativeBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl NativeBackend {
    /// Swipe every display to its `target`-th space. Because gestures land on
    /// the display under the cursor, we warp to each display in turn, then
    /// restore the pointer.
    fn gesture_switch(&self, target: WorkspaceId) -> Result<()> {
        let displays = skylight::managed_display_spaces(self.cid);
        let cg = display::active_displays();
        let known: Vec<(MonitorId, bool)> = cg.iter().map(|d| (d.id, d.is_main)).collect();
        let saved = gesture::cursor_position();

        let idx = target.0 as usize;
        if idx == 0 {
            return Err(BackendError("workspace ordinals are 1-based".into()));
        }
        for d in &displays {
            let Some(cur_idx) = d.spaces.iter().position(|s| *s == d.current_space) else {
                continue;
            };
            if idx > d.spaces.len() {
                continue; // this display doesn't have that many spaces
            }
            let delta = idx as i64 - 1 - cur_idx as i64;
            if delta == 0 {
                continue;
            }
            // Aim the gesture at this display.
            if let Some(mon) = skylight::resolve_monitor_id(&d.identifier, &known) {
                if let Some(info) = cg.iter().find(|i| i.id == mon) {
                    mouse::warp_to(info.frame.center());
                }
            }
            for _ in 0..delta.unsigned_abs() {
                gesture::swipe(delta.signum());
                std::thread::sleep(SWIPE_GAP);
            }
        }

        if let Some(p) = saved {
            mouse::warp_to(ordo_core::Point { x: p.x, y: p.y });
        }
        Ok(())
    }

    /// True once every display's active space is its `target`-th.
    fn on_target(&self, target: WorkspaceId) -> bool {
        let idx = target.0 as usize;
        let displays = skylight::managed_display_spaces(self.cid);
        !displays.is_empty()
            && displays.iter().all(|d| {
                d.spaces
                    .get(idx.wrapping_sub(1))
                    .is_some_and(|s| *s == d.current_space)
            })
    }

    /// The space id for `target` on the display that currently holds `window`.
    fn target_space_for_window(
        &self,
        window: WindowId,
        target: WorkspaceId,
    ) -> Option<ordo_skylight_sys::CgsSpaceId> {
        let cur = skylight::raw_space_of_window(self.cid, window)?;
        let idx = target.0 as usize;
        let displays = skylight::managed_display_spaces(self.cid);
        for d in &displays {
            if d.spaces.contains(&cur) {
                return d.spaces.get(idx.wrapping_sub(1)).copied();
            }
        }
        None
    }
}

impl WorkspaceBackend for NativeBackend {
    fn topology(
        &mut self,
        windows: &[WindowId],
        monitors: &[(MonitorId, bool)],
    ) -> Result<BackendTopology> {
        let displays = skylight::managed_display_spaces(self.cid);
        let folded = skylight::fold_topology(&displays, monitors);
        let window_ws = skylight::window_workspaces(self.cid, windows, &folded.space_to_ordinal);

        let monitors = folded
            .per_monitor
            .into_iter()
            .map(|(monitor, (active, count))| MonitorWorkspace {
                monitor,
                active,
                count,
            })
            .collect();

        Ok(BackendTopology {
            monitors,
            window_ws,
        })
    }

    fn switch_workspace(&mut self, target: WorkspaceId) -> Result<()> {
        self.gesture_switch(target)?;
        if self.on_target(target) {
            return Ok(());
        }
        // One retry: gestures occasionally get dropped under load.
        self.gesture_switch(target)?;
        if self.on_target(target) {
            Ok(())
        } else {
            Err(BackendError(format!(
                "workspace {} not reached after retry (gesture may need on-device tuning)",
                target.0
            )))
        }
    }

    fn move_window_to_workspace(&mut self, window: WindowId, target: WorkspaceId) -> Result<()> {
        let Some(space) = self.target_space_for_window(window, target) else {
            return Err(BackendError(format!(
                "no space {} on window {}'s display",
                target.0, window.0
            )));
        };
        skylight::move_window_to_space(self.cid, window, space);
        // Verify against a fresh read; SLSMoveWindowsToManagedSpace can no-op
        // on recent macOS from a non-Dock process.
        match skylight::raw_space_of_window(self.cid, window) {
            Some(now) if now == space => Ok(()),
            _ => Err(BackendError(
                "window did not move (SLSMoveWindowsToManagedSpace may be restricted on this macOS; \
                 consider the emulated backend)"
                    .into(),
            )),
        }
    }

    fn rescue_window(&mut self, window: WindowId) -> Result<()> {
        // Move the window to whatever space is currently active on its display,
        // so the kill switch can surface it. Uses the same move path.
        let displays = skylight::managed_display_spaces(self.cid);
        let cur = skylight::raw_space_of_window(self.cid, window);
        for d in &displays {
            if cur.is_some_and(|c| d.spaces.contains(&c)) {
                skylight::move_window_to_space(self.cid, window, d.current_space);
                return Ok(());
            }
        }
        Err(BackendError(format!("window {} not located", window.0)))
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            fixed_workspace_count: true,
            max_workspaces: MAX_SPACES_PER_DISPLAY,
        }
    }
}
