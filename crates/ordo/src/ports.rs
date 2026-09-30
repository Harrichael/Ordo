//! The two seams between the engine and the outside world.
//!
//! Everything above these traits is deterministic and gets tested with real
//! implementations (that is the house style). These two traits are the
//! exception the style explicitly allows: a live WindowServer cannot be driven
//! reproducibly, so the OS edge is where fakes belong. The real implementations
//! live in [`crate::platform`]; the tests script a fake world.

use std::time::Duration;

use ordo_core::{Effect, OpOutcome, Pid, Rect, RescanTrigger, WindowId, WorldSnapshot};
use ordo_emulated::ParkTrace;

/// Where the engine sends a look at the screen it shouldn't take yet. An
/// Accessibility read waits behind every write already sent to that app, so
/// a look taken while the apps are busy with Ordo's own writes costs as long
/// as the writes themselves.
pub trait LookGate {
    /// Whether the apps have nothing of Ordo's in hand.
    fn idle(&self) -> bool;
    /// Hand this look back, to be asked for again once they have, or at
    /// `until` whatever they're doing.
    fn defer(&self, trigger: RescanTrigger, until: std::time::Instant);
}

/// Produces a full observation of the world on demand. The engine never trusts
/// incremental hints — a hint only prompts a call to this.
///
/// Not `Send`: the real implementation holds macOS AX/CF handles that are
/// thread-affine, so the engine constructs and uses it entirely on its own
/// thread. External producers reach the engine through a channel instead.
pub trait WorldSource {
    fn snapshot(&mut self) -> WorldSnapshot;

    /// Drain diagnostic records of the workspace mechanism from the snapshot
    /// just built. Telemetry, not record: the core never sees these and the
    /// replay does not depend on them. Sources with no mechanism to hide
    /// (native Spaces, test fakes) return empty.
    ///
    /// Deliberately NOT defaulted. `SubscribingWorld` decorates this trait, and
    /// while this method had a default that wrapper inherited the empty one
    /// instead of forwarding — the engine drained the decorator, the real source
    /// kept filling a buffer nobody read, and the table stayed empty with
    /// nothing failing. Requiring the method makes that omission a compile
    /// error, which is the only reason it was found at all.
    fn take_park_trace(&mut self) -> Vec<ParkTrace>;

    /// What the snapshot just built cost, for the same telemetry side channel
    /// (see [`SnapshotStats`]). Sources that don't measure return None. Not
    /// defaulted, for the reason given on `take_park_trace`.
    fn take_snapshot_stats(&mut self) -> Option<SnapshotStats>;
}

/// Where a snapshot's time went. A hotkey that arrives mid-snapshot waits for
/// all of it, so this is what says whether reading the apps, or the placement
/// check that rides along, is what a burst press is waiting on — and which
/// app, when one app is most of it.
pub struct SnapshotStats {
    pub total: Duration,
    /// Reading every app's windows (apps in parallel).
    pub walk: Duration,
    /// The placement check, which can write.
    pub enforce: Duration,
    pub apps: usize,
    pub windows: usize,
    pub slowest: Option<(Pid, Duration)>,
}

/// Carries out a core [`Effect`] against the OS.
///
/// The returned outcome is the executor's *own* view of the attempt
/// (the gesture was posted, the AX write returned success) — never a claim
/// about the resulting world, which is confirmed only by the next snapshot.
/// `None` means "nothing to report" (e.g. a mouse warp, or observe-mode
/// dropping the effect); `Some` is logged as an `EffectResult` event.
pub trait Effector {
    fn execute(&mut self, effect: &Effect) -> Option<OpOutcome>;

    /// The engage chords' shell half, run before the core leaves Rescued
    /// mode: `use_state: true` (O) brings the workspace model up from the
    /// state file; `false` (R) brings it up blank with persistence suspended.
    /// Not an [`Effect`] — the core never decides which state to trust.
    /// Default no-op (observe mode, tests).
    fn bring_up_workspaces(&mut self, _use_state: bool) {}

    /// The save-state chord: resume persistence and write the current model
    /// as the new durable state. Default no-op.
    fn persist_workspaces(&mut self) {}

    /// An app was hidden or shown, by anyone; for the workspace backend,
    /// which alone decides what is hidden. Default no-op.
    fn note_app_visibility(&mut self, _pid: Pid, _hidden: bool) {}
}

/// One restack's timing breakdown. The point is the question it exists to
/// answer later: raises are serialized today (each confirmed landed before
/// the next — the only order-deterministic option known), and whether
/// OVERLAPPING them is safe 99.9% of the time is a statistics question.
/// These numbers, aggregated per app over weeks of real use, are the input
/// to that decision — collect first, design heuristics from data.
#[derive(Clone, Debug)]
pub struct RestackStats {
    pub total_ms: u64,
    /// Waiting for the destination's apps to work through the writes queued
    /// before this restack: its windows' moves, un-hides and focus.
    pub landing_wait_ms: u64,
    /// Waiting for un-hidden windows to resurface in the CG list before
    /// ordering could even start. If this dominates, overlapping raises is
    /// optimizing the wrong phase.
    pub presence_wait_ms: u64,
    /// Waiting for the in-flight focus handoff to leave the ordering set.
    pub handoff_wait_ms: u64,
    /// Length of the desired order, including the designated top.
    pub desired: u32,
    /// Desired windows that never resurfaced and were ordered around.
    pub missing: u32,
    /// A ghost-absorption pass actually ran (pass one ended mis-ordered).
    pub second_pass: bool,
    /// No overlapping pair was out of order at the final read-back.
    pub converged: bool,
    /// A newer desired order arrived mid-reassert and this one yielded to it.
    /// Aborted rows are expected under rapid switching and are NOT failures;
    /// exclude them when aggregating latency distributions.
    pub aborted: bool,
    /// This reassert was started by the worker's post-convergence ghost
    /// watch (a late 808/815 for an ordered window), not by the engine. Its
    /// frequency is the measure of how often the old "wrong until the 2s
    /// rescan" window actually fired.
    pub ghost_pass: bool,
    /// Times this reassert made its designated top key because focus was
    /// elsewhere: after the un-hides resurfaced, and at the final read-back.
    /// How often a switch's own focus request was stolen.
    pub refocused: u32,
    /// The desired windows as they stood, front to back, once the un-hides
    /// had resurfaced them: the order this reassert set out to repair.
    pub start_order: Vec<WindowId>,
    /// Overlapping pairs whose order is enforced, at the first plan.
    pub edges: u32,
    /// Independent overlap groups the first plan raised in.
    pub lanes: u32,
    /// Windows the first plan had to raise, the top included.
    pub raise_set: u32,
    /// Windows present at the first plan that it left in place.
    pub untouched: u32,
    /// Windows still below one they must be above at the final read-back.
    pub violated_end: u32,
    /// The planned windows' frames, attached ones included, so any restack
    /// can be replanned offline.
    pub frames: Vec<(WindowId, Rect)>,
    pub raises: Vec<RaiseStat>,
}

/// One issued raise and how long its landing took to confirm.
#[derive(Clone, Debug)]
pub struct RaiseStat {
    pub window: WindowId,
    pub pid: i32,
    pub kind: RaiseKind,
    pub pass: u8,
    /// Which overlap group it was raised in; raises in different lanes can be
    /// in flight at once.
    pub lane: u8,
    /// Windows above this one when its pass began — `above_scope` counts
    /// only windows being ordered, `above_all` the whole layer-0 stack.
    /// Read once per pass, not per raise: positions drift as earlier raises
    /// land, so these are "how buried was it", not exact hop counts.
    pub above_scope: u32,
    pub above_all: u32,
    /// The raise call itself, which blocks until the app acknowledges it.
    /// Lanes overlap only landing, so if this dominates, a slow app still
    /// holds up the others.
    pub ax_ms: u64,
    /// From issue to confirmed landing, `ax_ms` included.
    pub wait_ms: u64,
    pub timed_out: bool,
    /// The landing was confirmed on a wake caused by the WindowServer's push
    /// stream (808/815), not a fallback tick. The hit rate across weeks is
    /// what decides whether the polling fallback can shrink further.
    pub via_event: bool,
}

/// Raise physics class — these are expected to have different latency
/// distributions (same-app raises are FIFO in the app's AX queue; the
/// designated-top re-raise targets an already-active app).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RaiseKind {
    Background,
    Sibling,
    Top,
}

impl RaiseKind {
    pub fn as_str(self) -> &'static str {
        match self {
            RaiseKind::Background => "background",
            RaiseKind::Sibling => "sibling",
            RaiseKind::Top => "top",
        }
    }
}

/// Observe mode: log what the core *would* do, touch nothing. Pending ops will
/// expire as `OpLost` in the log because nothing confirms them — that absence
/// is the honest record of an inert run, not a bug.
pub struct NullEffector;

impl Effector for NullEffector {
    fn execute(&mut self, _effect: &Effect) -> Option<OpOutcome> {
        None
    }
}
