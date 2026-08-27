//! The two seams between the engine and the outside world.
//!
//! Everything above these traits is deterministic and gets tested with real
//! implementations (that is the house style). These two traits are the
//! exception the style explicitly allows: a live WindowServer cannot be driven
//! reproducibly, so the OS edge is where fakes belong. The real implementations
//! live in [`crate::platform`]; the tests script a fake world.

use ordo_core::{Effect, OpOutcome, WorldSnapshot};

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

/// Observe mode: log what the core *would* do, touch nothing. Pending ops will
/// expire as `OpLost` in the log because nothing confirms them — that absence
/// is the honest record of an inert run, not a bug.
pub struct NullEffector;

impl Effector for NullEffector {
    fn execute(&mut self, _effect: &Effect) -> Option<OpOutcome> {
        None
    }
}
