//! The emulated workspace backend: Ordo owns workspaces outright.
//!
//! Everything lives in one native Space. A window on a hidden workspace is
//! parked off-screen (bottom-right corner, a sliver left visible because macOS
//! forcibly re-homes fully-off-screen windows). Switching parks the outgoing
//! workspace's windows and restores the incoming one's to their saved frames.
//!
//! This crate is the whole emulated model — the pure [`ledger::Ledger`]
//! bookkeeping, the [`statefile`] persistence of its promises, and the
//! [`workspaces::EmulatedWorkspaces`] orchestration — with every OS touch
//! behind the [`Desktop`] port. The shell hands in an AX-backed `Desktop`;
//! tests hand in a fake and drive the entire backend without moving a real
//! window. It deliberately mirrors the native crate's position in the
//! workspace: one crate per workspace mechanism, chosen by the shell, with
//! the core never knowing which is underneath.
//!
//! Chosen via `--backend emulated`. Its tradeoffs vs. native (Mission Control
//! clutter, Cmd-Tab showing every app, the visible sliver) are the price of
//! unlimited instant workspaces with no private Space APIs — see the research.
//! Best paired with "Displays have separate Spaces" off.

pub mod ledger;
pub mod statefile;
pub mod workspaces;

pub use workspaces::{EmulatedWorkspaces, WorkspaceOutOfRange};

use ordo_core::{Pid, Rect, WindowId};

/// The slice of the desktop the emulated model needs to touch: enumerate
/// windows, move them, hide apps, and know where the main display is. Defined
/// here (the port belongs to the domain); implemented by the shell with AX.
pub trait Desktop {
    /// Every on-screen window: id, owning app, current frame.
    fn windows(&self) -> Vec<(WindowId, Pid, Rect)>;
    fn set_frames(&self, writes: &[(Pid, WindowId, Rect)]);
    fn set_app_hidden(&self, pid: Pid, hidden: bool);
    fn focused_window(&self) -> Option<WindowId>;
    /// The main display's frame — the anchor for the park corner.
    fn main_display(&self) -> Rect;
}
