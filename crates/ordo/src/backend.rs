//! The workspace backend boundary — the seam that lets "native macOS Spaces"
//! and "emulate workspaces ourselves" present the same two facts to the rest of
//! the shell: what the workspace topology is right now, and please make these
//! two changes.
//!
//! The trait lives here, platform-free, so its *shape* is frozen independently
//! of either implementation (the native one is in [`crate::platform`]; the
//! emulated one arrives in a later milestone). Core vocabulary only: 1-based
//! [`WorkspaceId`] ordinals cross this boundary, never a native space id.
//!
//! The two implementations differ in who owns the truth. Native: macOS owns it,
//! `topology()` reads it, and the user can change it behind our back. Emulated:
//! Ordo owns it, `topology()` reports our ledger and flags any window that
//! escaped. The core doesn't care which — in both cases the rule is the same:
//! the snapshot is real, state is belief, effects push reality toward intent.

use std::collections::HashMap;

use ordo_core::{MonitorId, Pid, Rect, WindowId, WorkspaceId};

pub type Result<T> = std::result::Result<T, BackendError>;

#[derive(Debug)]
pub struct BackendError(pub String);

impl std::fmt::Display for BackendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for BackendError {}

/// What the enumerator folds into a [`ordo_core::WorldSnapshot`] alongside the
/// display frames (from Core Graphics) and window frames/titles (from AX).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct BackendTopology {
    /// Per monitor: its currently active workspace and how many workspaces it
    /// offers. The usable count is the minimum across monitors.
    pub monitors: Vec<MonitorWorkspace>,
    /// Every managed window's workspace assignment.
    pub window_ws: HashMap<WindowId, WorkspaceId>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct MonitorWorkspace {
    pub monitor: MonitorId,
    pub active: WorkspaceId,
    pub count: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Capabilities {
    /// Native can't create/destroy spaces (the user does that in Mission
    /// Control); emulated can mint workspaces freely. v0 only needs this one
    /// difference exposed.
    pub fixed_workspace_count: bool,
    pub max_workspaces: u8,
}

pub trait WorkspaceBackend {
    /// Ground truth. Native interrogates the OS; emulated reports its ledger
    /// cross-checked against where windows actually are. Called on every rescan.
    ///
    /// Takes the world the enumerator already gathered — the current display set
    /// (with which is main) and the live window ids — because both are needed to
    /// classify workspaces (native maps window->space; emulated consults its
    /// ledger) and neither backend should re-enumerate them independently.
    fn topology(
        &mut self,
        windows: &[WindowId],
        monitors: &[(MonitorId, bool)],
    ) -> Result<BackendTopology>;

    /// Bring every display to its `target`-th workspace. Blocking and
    /// self-verifying: the implementation re-reads and retries once before
    /// returning an honest result.
    fn switch_workspace(&mut self, target: WorkspaceId) -> Result<()>;

    /// Reassign one window's workspace without changing what's visible.
    fn move_window_to_workspace(&mut self, window: WindowId, target: WorkspaceId) -> Result<()>;

    /// Emergency single-window recovery for the kill switch: make `window`
    /// visible on the active workspace with the least machinery possible.
    fn rescue_window(&mut self, window: WindowId) -> Result<()>;

    /// Re-assert placement promises this backend has made, given the frames
    /// the enumerator already read (no backend re-enumerates on its own).
    /// Called once per rescan, and only while Ordo is actively driving —
    /// never when paused or rescued, where fighting a gather would be worse
    /// than any phantom.
    ///
    /// Band-aid until placement writes get a single owner
    /// (docs/desired-state-reconciler.md): under rapid switching a stale
    /// restore can land AFTER the park that superseded it, leaving a
    /// "phantom" — a window visibly on-screen that the ledger says is parked.
    /// Nothing delta-driven ever fires on a phantom that has already settled,
    /// so the invariant needs a standing check. Native makes no placement
    /// promises; the default is a no-op.
    fn enforce_placement(&mut self, _frames: &HashMap<WindowId, (Pid, Rect)>) {}

    fn capabilities(&self) -> Capabilities;
}
