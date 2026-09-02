//! The emulated workspace backend: Ordo owns workspaces outright.
//!
//! Everything lives in one native Space. A window on a hidden workspace is
//! parked off-screen (flush to the bottom of the main display, right-aligned
//! within it, a sliver left visible because macOS forcibly re-homes
//! fully-off-screen windows). Switching parks the outgoing workspace's
//! windows and restores the incoming one's to their saved frames.
//!
//! Two kinds of data, and the split is the architecture: DECLARATIONS (a
//! window's workspace, the visible workspace) are written only by Ordo's own
//! commands — user switch/move, rescue, and a window's birth; OBSERVATIONS
//! (frames, existence, observed focus) are authoritative about the world,
//! never about intent. An observation contradicting a declaration is a violation to
//! correct on screen or surface, never to absorb into the declaration.
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
pub mod trace;
pub mod workspaces;

pub use trace::{ParkTrace, ParkTraceKind};
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
    /// The main display's frame — where a re-homed window must land, because
    /// that is where the user is looking.
    fn main_display(&self) -> Rect;

    /// Every display's frame. Needed because hiding a window is a question
    /// about the whole arrangement, not the main screen: macOS keeps a parked
    /// window's title bar on-screen, so only the HORIZONTAL escape hides it,
    /// and escaping past the main display's right edge simply lands the window
    /// on whatever display sits to the right.
    fn displays(&self) -> Vec<Rect>;
    /// Which of `ids` still exist per the WINDOW SERVER's full window list —
    /// authoritative death evidence, unlike an AX scan (one slow app drops
    /// its whole window set from a scan). Must consult all windows, not just
    /// on-screen ones: parked windows' apps are Cmd+H-hidden by dock dimming
    /// and an on-screen-only read would report exactly them as dead. `None`
    /// means the read itself failed or came back empty — not evidence; the
    /// caller keeps every belief.
    fn existing_windows(&self, ids: &[WindowId]) -> Option<std::collections::HashSet<WindowId>>;
}
