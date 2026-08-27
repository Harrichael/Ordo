//! The native workspace backend: workspaces *are* macOS Spaces.
//!
//! Reads (topology) are implemented and SIP-free. Mutations (switch a space,
//! move a window between spaces) arrive in milestone M4 — until then they return
//! an error rather than panicking, so an accidental call is logged, not fatal.
//! The ordinal <-> space-id translation lives entirely in [`super::skylight`];
//! this type is the trait-shaped front for it.

use ordo_core::{MonitorId, WindowId, WorkspaceId};

use crate::backend::{
    BackendError, BackendTopology, Capabilities, MonitorWorkspace, Result, WorkspaceBackend,
};

use super::skylight;

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

    fn switch_workspace(&mut self, _target: WorkspaceId) -> Result<()> {
        Err(BackendError("switch_workspace lands in M4".into()))
    }

    fn move_window_to_workspace(&mut self, _window: WindowId, _target: WorkspaceId) -> Result<()> {
        Err(BackendError("move_window_to_workspace lands in M4".into()))
    }

    fn rescue_window(&mut self, _window: WindowId) -> Result<()> {
        Err(BackendError("rescue_window lands in M5".into()))
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            fixed_workspace_count: true,
            max_workspaces: MAX_SPACES_PER_DISPLAY,
        }
    }
}
