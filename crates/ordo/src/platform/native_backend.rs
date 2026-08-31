//! The native workspace backend: workspaces *are* macOS Spaces.
//!
//! Reads go through private SkyLight calls (SIP-free); the ordinal <->
//! space-id translation lives entirely in [`super::skylight`], and this type
//! is the trait-shaped front for it. Switching drives Mission Control's own
//! (rebound) keyboard shortcut per display — see [`super::mission_control`]
//! for why every private switching call failed on Tahoe, and
//! `examples/kbd_switch_probe.rs` for how this mechanism was validated.

use ordo_core::{MonitorId, Pid, WindowId, WorkspaceId};

use crate::backend::{
    BackendError, BackendTopology, Capabilities, MonitorWorkspace, Result, WorkspaceBackend,
};

use super::{display, mission_control, mouse, skylight};

/// macOS caps user spaces at 16 per display.
const MAX_SPACES_PER_DISPLAY: u8 = 16;

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
    /// Pull each display to its `target`-th space via Mission Control's lever.
    /// The shortcut acts on the display under the pointer, so the pointer is
    /// warped to each display in turn and restored afterward — a visible blip,
    /// but the only full (pixels-included) switch the OS allows from outside
    /// the Dock.
    fn drive_all_displays(&self, target: WorkspaceId) -> Result<()> {
        let idx = target.0 as usize;
        if idx == 0 {
            return Err(BackendError("workspace ordinals are 1-based".into()));
        }
        let displays = skylight::managed_display_spaces(self.cid);
        let cg = display::active_displays();
        let known: Vec<(MonitorId, bool)> = cg.iter().map(|d| (d.id, d.is_main)).collect();
        let saved = mouse::position();

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
            let Some(mon) = skylight::resolve_monitor_id(&d.identifier, &known) else {
                continue;
            };
            let Some(info) = cg.iter().find(|i| i.id == mon) else {
                continue;
            };
            mouse::warp_to(info.frame.center());
            mission_control::switch(delta.signum(), delta.unsigned_abs());
        }

        if let Some(p) = saved {
            mouse::warp_to(p);
        }
        Ok(())
    }

    /// Poll for every display to reach its `target`-th space, waiting out
    /// Mission Control's slide animation.
    fn await_target(&self, target: WorkspaceId) -> bool {
        let idx = target.0 as usize;
        for _ in 0..10 {
            let displays = skylight::managed_display_spaces(self.cid);
            let arrived = !displays.is_empty()
                && displays.iter().all(|d| {
                    // A display without that many spaces was skipped, not moved.
                    match d.spaces.get(idx.wrapping_sub(1)) {
                        Some(s) => *s == d.current_space,
                        None => true,
                    }
                });
            if arrived {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
        false
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
        windows: &[(WindowId, Pid)],
        monitors: &[(MonitorId, bool)],
    ) -> Result<BackendTopology> {
        let displays = skylight::managed_display_spaces(self.cid);
        let folded = skylight::fold_topology(&displays, monitors);
        // SkyLight keys purely on window ids; the pid half of identity is the
        // emulated ledger's concern.
        let ids: Vec<WindowId> = windows.iter().map(|(w, _)| *w).collect();
        let window_ws = skylight::window_workspaces(self.cid, &ids, &folded.space_to_ordinal);

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
        self.drive_all_displays(target)?;
        if self.await_target(target) {
            return Ok(());
        }
        // One retry: a press can land mid-animation and get dropped.
        self.drive_all_displays(target)?;
        if self.await_target(target) {
            Ok(())
        } else {
            Err(BackendError(format!(
                "workspace {} not reached after retry (are Mission Control's \"Move \
                 left/right a space\" shortcuts enabled and bound to Ctrl+Alt+Cmd+arrows?)",
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
