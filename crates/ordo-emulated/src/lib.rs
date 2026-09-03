//! The emulated workspace backend: Ordo owns workspaces outright.
//!
//! Everything lives in one native Space. A window on a hidden workspace is
//! parked off-screen — slid left past the leftmost display at its own height,
//! a 1pt sliver left visible because macOS forcibly re-homes fully-off-screen
//! windows. Switching parks the outgoing workspace's windows and restores the
//! incoming one's to their saved frames. Every one of those writes is a MOVE
//! and nothing more: this model never asserts a window's size, only where it
//! sits — see [`Desktop::move_windows`] for the ratchet that taught us why.
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

use ordo_core::{Pid, Point, Rect, WindowId};

/// The slice of the desktop the emulated model needs to touch: enumerate
/// windows, move them, hide apps, and know where the main display is. Defined
/// here (the port belongs to the domain); implemented by the shell with AX.
pub trait Desktop {
    /// Every on-screen window: id, owning app, current frame.
    fn windows(&self) -> Vec<(WindowId, Pid, Rect)>;

    /// Send each window to a new origin, leaving its size alone.
    ///
    /// A [`Point`] rather than a [`Rect`] because the size is not merely
    /// unneeded here, it is actively harmful: a write carrying AXSize is capped
    /// to the usable height of the display owning the origin, and parking then
    /// recorded the shortened frame as the window's real one — a ratchet that
    /// shaved a few points off tall windows every switch, permanently. Writing
    /// the position alone was measured never to resize a window, at any origin,
    /// so a move that cannot express a size cannot ratchet. Nothing this model
    /// does is a resize; the one caller that genuinely resizes (the core's
    /// cross-display `SetWindowFrame`) does not come through this port.
    ///
    /// Batched because a switch parks the outgoing workspace and restores the
    /// incoming one in a single breath, and an implementation that has to walk
    /// each app's window list should walk it once for both.
    fn move_windows(&self, moves: &[(Pid, WindowId, Point)]);
    fn set_app_hidden(&self, pid: Pid, hidden: bool);
    fn focused_window(&self) -> Option<WindowId>;
    /// The main display's frame — where a re-homed window must land, because
    /// that is where the user is looking.
    fn main_display(&self) -> Rect;

    /// Every display's frame. Needed because hiding a window is a question
    /// about the whole arrangement, not the main screen: macOS keeps a parked
    /// window's title bar on-screen, so only the HORIZONTAL escape hides it,
    /// and it has to clear the display at the end of the arrangement —
    /// escaping past an interior edge just lands the window on the neighbour.
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
