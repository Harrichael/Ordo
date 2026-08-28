//! The two seams between the engine and the outside world.
//!
//! Everything above these traits is deterministic and gets tested with real
//! implementations (that is the house style). These two traits are the
//! exception the style explicitly allows: a live WindowServer cannot be driven
//! reproducibly, so the OS edge is where fakes belong. The real implementations
//! live in [`crate::platform`]; the tests script a fake world.

use ordo_core::{Effect, OpOutcome, WindowId, WorldSnapshot};

/// Produces a full observation of the world on demand. The engine never trusts
/// incremental hints — a hint only prompts a call to this.
///
/// Not `Send`: the real implementation holds macOS AX/CF handles that are
/// thread-affine, so the engine constructs and uses it entirely on its own
/// thread. External producers reach the engine through a channel instead.
pub trait WorldSource {
    fn snapshot(&mut self) -> WorldSnapshot;
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
    /// Windows already in correct relative order at the bottom, not raised.
    pub skipped_suffix: u32,
    /// A ghost-absorption pass actually ran (pass one ended mis-ordered).
    pub second_pass: bool,
    /// Final read-back matched the desired order exactly.
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
    pub raises: Vec<RaiseStat>,
}

/// One issued raise and how long its landing took to confirm.
#[derive(Clone, Debug)]
pub struct RaiseStat {
    pub window: WindowId,
    pub pid: i32,
    pub kind: RaiseKind,
    pub pass: u8,
    /// Windows above this one when its pass began — `above_scope` counts
    /// only windows being ordered, `above_all` the whole layer-0 stack.
    /// Read once per pass, not per raise: positions drift as earlier raises
    /// land, so these are "how buried was it", not exact hop counts.
    pub above_scope: u32,
    pub above_all: u32,
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
