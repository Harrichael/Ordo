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
//! The other load-bearing rule: **belief follows observation, never intent —
//! and intent never follows observation.** Issuing an effect does not update
//! belief; belief changes only when a [`WorldSnapshot`] confirms what actually
//! happened. Declarations (which workspace a window is on, which window
//! should be key — [`FocusIntent`]) are written only by commands, and an
//! observation that contradicts one is a violation to correct, never a fact
//! to absorb. See docs/intent-vs-observation.md.
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
pub mod project;
mod reconcile;
mod state;
mod update;

pub use effect::{CorrectionAxis, Effect, Expectation};
pub use event::{
    AxHintKind, Event, Gesture, HotkeyAction, MonitorSnap, MonitorWs, OpOutcome, RescanTrigger, Ts,
    VirtualMonitorsWord, WindowSnap, WorkspaceSnap, WorldSnapshot,
};
pub use ids::{
    MonitorId, OpId, Pid, Point, Rect, VirtualMonitorId, WindowId, WorkspaceId, FRAME_EPSILON,
};
pub use mru::FocusHistory;
pub use project::{project, Projection};
pub use reconcile::Delta;
pub use state::{
    FocusIntent, Mode, MonitorRecord, PendingOp, State, VirtualMonitors, WindowRecord,
};
pub use update::{coalesce_hotkeys, update, Note, Step};
