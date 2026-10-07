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

pub mod hiding;
pub mod ledger;
pub mod statefile;
pub mod trace;
pub mod workspaces;

pub use hiding::{HideWhen, Hiding, Idle};
pub use trace::{ChainStat, FocusStat, HoldStat, ParkTrace, ParkTraceKind, WriteStat};
pub use workspaces::{EmulatedWorkspaces, MonitorOutOfRange, WorkspaceOutOfRange};

use std::time::Instant;

use ordo_core::{Pid, Point, Rect, WindowId};

/// One window sent to a new origin.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Move {
    pub pid: Pid,
    pub window: WindowId,
    pub to: Point,
    /// Whether this sends the window off screen, to its park. An un-hide of
    /// its app still to come must hold it there; a move onto the screen is
    /// the opposite, and must not be held.
    pub parks: bool,
}

/// One app to un-hide, and the windows that must not come back with it.
///
/// An empty `hold` is a deliberate statement, not an omission: reveal
/// everything, which is exactly what the rescue path wants.
#[derive(Debug, Clone)]
pub struct Unhide {
    pub pid: Pid,
    /// This app's windows declared on other workspaces, each with the park
    /// origin it must still occupy when the reveal is over.
    pub hold: Vec<(WindowId, Point)>,
}

/// The slice of the desktop the emulated model needs to touch: enumerate
/// windows, move them, hide apps, and know where the main display is. Defined
/// here (the port belongs to the domain); implemented by the shell with AX.
pub trait Desktop {
    /// Where these windows are and which app owns each, per the window
    /// server: no round trip to any app, and a hidden app's windows answer
    /// too. Windows that no longer exist are left out.
    fn frames(&self, windows: &[WindowId]) -> Vec<(WindowId, Pid, Rect)>;

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
    /// incoming one in a single breath.
    ///
    /// The moves may land after this returns; see [`Desktop::in_flight`].
    fn move_windows(&self, moves: &[Move]);

    /// Whether a write to this window may not show yet in what the desktop
    /// reports: asked for and not yet seen through. What this port reports
    /// stays what the screen shows; this is what says whether to trust it.
    fn in_flight(&self, window: WindowId) -> bool;

    /// Whether writes already asked for are still being carried out. A burst
    /// keeps this true, and the deferred hides wait it out rather than hide
    /// an app that the switch still coming may show again.
    fn busy(&self) -> bool;

    /// Hide an app, the Cmd+H way. Fire-and-forget: the hide itself needs no
    /// confirming, and like a move it may land after this returns.
    fn hide_app(&self, pid: Pid);

    /// Whether the app is hidden right now, asked of the app itself; None when
    /// it doesn't answer.
    fn app_hidden(&self, pid: Pid) -> Option<bool>;

    /// Whether Ordo may hide this app. A background app (no Dock icon) whose
    /// windows Ordo manages may not: nothing would show it again, rescue
    /// included, if Ordo stopped while it was hidden. Its windows are parked
    /// all the same.
    fn can_hide(&self, pid: Pid) -> bool;

    /// On-screen windows front to back, per the window server. A hidden
    /// app's windows are not in it.
    fn stack(&self) -> Vec<WindowId>;

    /// Whether switches should trace [`Desktop::stack`] at each step: a
    /// window-list read apiece, too costly to run on every switch.
    fn traces_stacks(&self) -> bool;

    fn now(&self) -> Instant;

    /// Un-hide these apps, HOLDING each one's listed windows at the given
    /// origin until the desktop agrees they are there.
    ///
    /// The asymmetry with [`Desktop::hide_app`] is the whole point. As an
    /// un-hidden app's windows order back in, AppKit re-homes each one onto a
    /// display (`constrainFrameRect:toScreen:`), dragging the ones parked for
    /// other workspaces fully back on screen — measured on 93% of genuine
    /// un-hides of an app with a parked window, and the flash the user sees on
    /// every switch. Surviving that is a property of the un-hide itself, not a
    /// second command issued after it: a write sent in the same breath is
    /// processed BEFORE the order-in and then overwritten by it (measured: 1-4
    /// of 12 ended parked). So the positions ride WITH the un-hide, and how
    /// they are made to stick is the port's business — the model knows where a
    /// window belongs, not how to win a race with an app's main thread.
    ///
    /// Batched for the same reason [`Desktop::move_windows`] is, and like
    /// it, may land after this returns: each app's un-hide follows that app's
    /// moves already asked for, and its held windows are in flight until the
    /// hold is done.
    ///
    /// An app already showing is not un-hidden: an un-hide sent to it brings
    /// every window it owns forward, parked ones included (measured 8 of 8,
    /// and 0 of 8 without it), which reorders the windows of both the
    /// workspace left and the one arrived at. Its held windows are still
    /// checked, and held if any left its spot: an app can be revealed behind
    /// this call's back — focusing a window of a hidden app un-hides it.
    fn show_apps(&self, apps: &[Unhide]);

    fn focused_window(&self) -> Option<WindowId>;
    /// The active app, whether or not a window of it is key — Finder holding
    /// the desktop has none.
    fn frontmost_app(&self) -> Option<Pid>;
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
