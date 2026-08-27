//! The pure decision core of Ordo.
//!
//! Everything Ordo *decides* happens here; nothing Ordo *does* happens here.
//! The shell (the `ordo` binary) observes macOS and feeds [`Event`]s in; the
//! core returns a new [`State`] plus [`Effect`]s for the shell to execute.
//! This is the onion: all I/O lives outside this crate, which is why this
//! crate has no dependency on OS bindings, clocks, or threads.
//!
//! Determinism rule: the core never reads the clock, never generates
//! randomness, and never allocates identifiers from globals. Timestamps ride
//! on events; operation ids come from a counter inside [`State`]. The payoff
//! is that a logged event stream replays byte-for-byte — feed the same events
//! from the same starting state and you get the same effects, which is how
//! "I think I had a bug at 3pm" becomes a reproducible test case.
//!
//! The other load-bearing rule: **belief follows observation, never intent.**
//! Issuing an effect does not update state; state changes only when a
//! [`WorldSnapshot`] confirms what actually happened. macOS co-owns much of
//! this state (the user can switch spaces behind our back), so intent-ahead
//! bookkeeping would inevitably diverge from reality.
//!
//! ```
//! use ordo_core::{update, Event, HotkeyAction, State, Ts};
//!
//! let state = State::new();
//! let step = update(&state, &Event::Hotkey {
//!     at: Ts { wall_ms: 0, mono_ns: 0 },
//!     action: HotkeyAction::WorkspaceNext,
//! });
//! // An empty world has no current workspace, so the core decides to do nothing.
//! assert!(step.effects.is_empty());
//! ```

mod effect;
mod event;
mod ids;
mod mru;
mod reconcile;
mod state;
mod update;

pub use effect::{Effect, Expectation};
pub use event::{
    AxHintKind, Event, HotkeyAction, MonitorSnap, OpOutcome, RescanTrigger, Ts, WindowSnap,
    WorldSnapshot,
};
pub use ids::{MonitorId, OpId, Pid, Point, Rect, WindowId, WorkspaceId, FRAME_EPSILON};
pub use mru::FocusHistory;
pub use reconcile::Delta;
pub use state::{Mode, MonitorRecord, PendingOp, State, WindowRecord};
pub use update::{update, Note, Step};
