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
    /// Restore write issued onto the window's monitor's display, carried over
    /// from the display the promise was made on: the rig changed while the
    /// window was hidden (monitor memory bringing a window home).
    Rehost,
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
    /// A view change began — a monitor viewed, virtualization toggled, or the
    /// display set changed under the projection. Same grouping role.
    View,
    /// Dock dimming un-hid an app (Cmd+H cleared), revealing EVERY window it
    /// owns — including the ones parked for other workspaces. The suspected
    /// source of the switch flash, and previously unrecorded, so a flash could
    /// not be attributed to Ordo's write or the app's own reaction to it.
    AppShown,
    /// Dock dimming hid an app: all its windows live on hidden workspaces.
    AppHidden,
    /// Read right after its app was hidden, a parked window was already off
    /// its park spot. Says whether the hide itself moves parked windows, or
    /// they move later (then the next scan's `Reassert` is the first sign).
    OffAfterHide,
    /// One app's share of a batch of moves: what reaching its windows, and
    /// writing them, cost. The batch lands in the time of its slowest app.
    AppMoved,
    /// The stacking order of the ledger's on-screen windows, front to back,
    /// at a named moment of a switch. A switch's hides and un-hides reorder
    /// windows as a side effect, and nothing else records the order they
    /// leave behind for the stacking worker to repair.
    Stack,
}

/// What holding an app's parked windows through its un-hide cost.
///
/// The un-hide is the one moment the model does not control: the app orders
/// its windows back in and AppKit re-homes each one onto a display, so the
/// port has to keep re-writing the park origin until the window server agrees.
/// That loop is invisible when it works — this is the only channel that can
/// say it ran, how much it cost, and whether it still works. `converged` is
/// the health signal; `escaped` names the windows a switch left on screen.
#[derive(Debug, Clone, Serialize)]
pub struct HoldStat {
    pub pid: Pid,
    /// False when the app was already showing and was left alone.
    pub unhid: bool,
    /// Windows this un-hide had to hold — 0 for an app with nothing parked,
    /// where the hold is a single window-server read and no writes at all.
    pub windows: usize,
    pub writes: u32,
    pub elapsed_ms: u64,
    pub escaped: Vec<WindowId>,
    /// `escaped.is_empty()`, carried as its own field so the question this
    /// telemetry exists to answer is one column and not an array to measure.
    pub converged: bool,
    /// Whether the app's `AXEnhancedUserInterface` was toggled for the hold.
    pub enhanced_ui: bool,
    /// The on-screen stack, front to back, at each step of the un-hide, as
    /// the port read it.
    pub stacks: Vec<(String, Vec<WindowId>)>,
}

impl HoldStat {
    pub fn new(
        pid: Pid,
        unhid: bool,
        windows: usize,
        writes: u32,
        elapsed_ms: u64,
        escaped: Vec<WindowId>,
    ) -> Self {
        HoldStat {
            pid,
            unhid,
            windows,
            writes,
            elapsed_ms,
            converged: escaped.is_empty(),
            escaped,
            enhanced_ui: false,
            stacks: Vec::new(),
        }
    }

    pub fn with_steps(mut self, enhanced_ui: bool, stacks: Vec<(String, Vec<WindowId>)>) -> Self {
        self.enhanced_ui = enhanced_ui;
        self.stacks = stacks;
        self
    }
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
    /// The frame the model aimed at, when this record is a write. Only its
    /// ORIGIN reaches the OS — this backend moves windows and never resizes
    /// them — so a size here that differs from `observed` is the window's own,
    /// not a resize anyone asked for.
    pub requested: Option<Rect>,
    /// Whether the observed frame read as sitting at the park corner. The
    /// predicate's own answer, so a wrong one is visible after the fact.
    pub at_park: Option<bool>,
    /// Enforcement attempts charged to this window so far.
    pub attempt: Option<u8>,
    /// On an [`ParkTraceKind::AppShown`]: what it cost to keep that app's
    /// parked windows at the corner while its windows ordered back in.
    pub hold: Option<HoldStat>,
    /// On a write record: what the write cost once it was made.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub write: Option<WriteStat>,
    /// On an [`ParkTraceKind::AppMoved`] record.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub moves: Option<AppMoveStat>,
    /// On a [`ParkTraceKind::Switch`] or [`ParkTraceKind::View`] record: where
    /// the switch's own time went.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost: Option<SwitchCost>,
    pub detail: Option<String>,
}

/// One window's position write, timed in its app's thread.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct WriteStat {
    pub ax_ms: f64,
    /// When it finished, from the start of the whole batch: moves run on one
    /// thread per app, so this is what says whether a write waited.
    pub done_ms: f64,
}

/// One app's part of a batch of moves.
#[derive(Debug, Clone, Serialize)]
pub struct AppMoveStat {
    #[serde(skip)]
    pub pid: Pid,
    pub windows: usize,
    /// Reading the app's window list, before any write could start.
    pub list_ms: f64,
    /// Whether `AXEnhancedUserInterface` was toggled around the writes.
    pub enhanced_ui: bool,
    pub total_ms: f64,
    #[serde(skip)]
    pub writes: Vec<(WindowId, WriteStat)>,
}

/// Where a switch's time went, on the engine thread. The focus request that
/// precedes it and the rescan after it are timed elsewhere.
#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct SwitchCost {
    /// Reading every window's frame and the display geometry.
    pub read_ms: f64,
    pub moves_ms: f64,
    /// Writing the ledger to disk, before any window moves.
    pub persist_ms: f64,
    /// Un-hiding and holding the destination's apps.
    pub visibility_ms: f64,
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
            hold: None,
            write: None,
            moves: None,
            cost: None,
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

    pub fn hold(mut self, h: HoldStat) -> Self {
        self.hold = Some(h);
        self
    }

    pub fn moves(mut self, m: AppMoveStat) -> Self {
        self.moves = Some(m);
        self
    }

    pub fn detail(mut self, d: impl Into<String>) -> Self {
        self.detail = Some(d.into());
        self
    }
}
