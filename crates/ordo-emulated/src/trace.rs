//! Diagnostic record of the parking mechanism.
//!
//! Parking is invisible to every other channel by design: the model
//! substitutes a parked window's remembered frame before the snapshot is
//! assembled, so the core — and the replay log built from it — sees windows at
//! the coordinates they *mean*, never at the corner they physically occupy.
//! That is the right abstraction and the reason the mechanism is undebuggable
//! from the log: when parking works it leaves no trace, and when it breaks the
//! only trace is the substitution failing to happen.
//!
//! This is the missing channel. It carries the raw observation alongside the
//! belief, and every frame write the model issues with the reason it issued
//! it. Telemetry, not record: nothing here feeds the core or the replay, and
//! dropping it loses no history those depend on.
//!
//! ```ignore
//! // Did a park write actually land, and where did the window end up?
//! for t in model.take_trace() {
//!     if t.kind == ParkTraceKind::Park {
//!         println!("{:?} asked for {:?}", t.window, t.requested);
//!     }
//! }
//! ```

use ordo_core::{Pid, Rect, WindowId, WorkspaceId};
use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum ParkTraceKind {
    /// A window's raw frame changed. The one fact no other channel records.
    Moved,
    /// Park write issued; the window's real frame was captured as its promise.
    Park,
    /// Park write issued for a window already bookkept parked.
    Reassert,
    /// Restore write issued back to the remembered frame.
    Restore,
    /// Restored with no trustworthy promise — re-homed somewhere reachable.
    Rehome,
    /// A refused promise: the remembered frame was itself a park position.
    PoisonedPromise,
    /// Enforcement saw a violation but did NOT count it, and why.
    Suppressed,
    /// Enforcement hit its write limit and stood down: the declaration is
    /// KEPT and no further writes are issued; the window stays visibly
    /// misplaced until a user command resolves it. Never a rewrite — a
    /// declaration must not follow the screen. (Rows in old logs with kind
    /// `Adopted` are from before this rule was enforced here.)
    Standoff,
    /// A workspace switch began: the boundary that groups everything after it.
    Switch,
    /// Dock dimming un-hid an app (Cmd+H cleared), revealing EVERY window it
    /// owns — including the ones parked for other workspaces. The suspected
    /// source of the switch flash, and previously unrecorded, so a flash could
    /// not be attributed to Ordo's write or the app's own reaction to it.
    AppShown,
    /// Dock dimming hid an app: all its windows live on hidden workspaces.
    AppHidden,
}

/// One diagnostic fact about a window's frame mechanics.
///
/// `observed` is always the raw frame as the OS reported it — never the
/// substituted belief. `believed` is what the core was told, present only when
/// the two differ, which is exactly the case the log could not previously see.
#[derive(Debug, Clone, Serialize)]
pub struct ParkTrace {
    /// The window this concerns. `WindowId(0)` for records about an app or a
    /// switch rather than a single window; `pid` carries the app in that case.
    pub window: WindowId,
    pub kind: ParkTraceKind,
    pub pid: Option<Pid>,
    pub declared: Option<WorkspaceId>,
    pub current: Option<WorkspaceId>,
    pub observed: Option<Rect>,
    pub believed: Option<Rect>,
    /// The frame handed to the OS, when this record is a write.
    pub requested: Option<Rect>,
    /// Whether the observed frame read as sitting at the park corner. The
    /// predicate's own answer, so a wrong one is visible after the fact.
    pub at_park: Option<bool>,
    /// Enforcement attempts charged to this window so far.
    pub attempt: Option<u8>,
    pub detail: Option<String>,
}

impl ParkTrace {
    pub fn new(window: WindowId, kind: ParkTraceKind) -> Self {
        ParkTrace {
            window,
            kind,
            pid: None,
            declared: None,
            current: None,
            observed: None,
            believed: None,
            requested: None,
            at_park: None,
            attempt: None,
            detail: None,
        }
    }

    /// For a record about an app rather than one window.
    pub fn app(pid: Pid, kind: ParkTraceKind) -> Self {
        let mut t = Self::new(WindowId(0), kind);
        t.pid = Some(pid);
        t
    }

    pub fn observed(mut self, f: Rect) -> Self {
        self.observed = Some(f);
        self
    }

    pub fn believed(mut self, f: Rect) -> Self {
        self.believed = Some(f);
        self
    }

    pub fn requested(mut self, f: Rect) -> Self {
        self.requested = Some(f);
        self
    }

    pub fn ws(mut self, declared: WorkspaceId, current: WorkspaceId) -> Self {
        self.declared = Some(declared);
        self.current = Some(current);
        self
    }

    pub fn at_park(mut self, v: bool) -> Self {
        self.at_park = Some(v);
        self
    }

    pub fn attempt(mut self, n: u8) -> Self {
        self.attempt = Some(n);
        self
    }

    pub fn detail(mut self, d: impl Into<String>) -> Self {
        self.detail = Some(d.into());
        self
    }
}
