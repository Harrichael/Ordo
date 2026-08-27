use serde::{Deserialize, Serialize};

use crate::ids::{MonitorId, OpId, Pid, Rect, WindowId, WorkspaceId};

/// Both clocks, stamped by the shell at ingest. Wall time is what the user
/// remembers ("around 3pm"); monotonic time is what ordering and latency
/// analysis need. Events carry time so the core never reads a clock.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Ts {
    pub wall_ms: i64,
    pub mono_ns: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Event {
    Hotkey {
        at: Ts,
        action: HotkeyAction,
    },
    /// The ONLY channel through which the outside world's shape enters the
    /// core. AX notifications are unreliable hints; the shell answers a hint
    /// by performing a full rescan and delivering this. The core never sees
    /// raw hints — that is what makes the snapshot authoritative.
    WorldObserved {
        at: Ts,
        trigger: RescanTrigger,
        snap: WorldSnapshot,
    },
    /// The executor's report on an issued effect. This is "the attempt
    /// succeeded/failed", not "the world changed" — confirmation of reality
    /// still comes from the next `WorldObserved`.
    EffectResult {
        at: Ts,
        op: OpId,
        outcome: OpOutcome,
    },
    /// The kill switch already acted at the shell layer (interception is
    /// off) by the time this arrives; the core's job is only to go inert.
    RescueEngaged {
        at: Ts,
    },
    /// The inverse of `RescueEngaged`: the user asked Ordo to (re)engage. Like
    /// rescue, the tap has already flipped interception on its fast path; the
    /// core's job is to leave `Rescued` and re-assert the intent. Also how a
    /// `--paused` run comes alive for the first time.
    Engaged {
        at: Ts,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum HotkeyAction {
    /// Cmd+Left / Cmd+Right: adjacent workspace, clamped at the ends (no
    /// wrap-around — matches how macOS's own ctrl-arrow switching feels).
    WorkspacePrev,
    WorkspaceNext,
    /// Jump straight to a workspace. No key binding mints this today; it is
    /// what a queued run of Prev/Next coalesces into (see
    /// [`crate::coalesce_hotkeys`]), so a burst of presses costs one switch.
    WorkspaceSwitchTo(WorkspaceId),
    /// Alt+Tab: most-recently-used window in the current workspace.
    MruWorkspace,
    /// Alt+Shift+Tab: MRU in the current workspace AND the focused monitor.
    MruMonitor,
    /// Alt+Backtick: MRU in the current workspace AND the focused app.
    MruApp,
    /// Ctrl+Alt+Tab: MRU in the current workspace on any monitor EXCEPT the
    /// focused one — "jump to what I was last doing over there".
    MruOtherMonitor,
    /// Alt+End: banish the focused window to the back of the MRU history and
    /// the back of the visual stack, and focus the next MRU window — "done
    /// with this one, stop offering it (or showing it)".
    MruDemote,
    /// Cmd+Shift+Left/Right: the window moves and the "view" moves with it —
    /// focus stays on it and the mouse follows to its new center.
    MoveFocusedToOtherMonitor,
    /// Ctrl+Cmd+Left/Right: carry the focused window to the adjacent workspace
    /// and switch there with it. The window keeps its frame and its focus, so
    /// the mouse has nowhere to follow — nothing on screen moves except the
    /// scenery.
    CarryFocusedToWorkspacePrev,
    CarryFocusedToWorkspaceNext,
}

/// Why a rescan ran. Purely diagnostic except for `AxHint(WindowCreated)`,
/// which is the one trigger that authorizes new-window corralling — a full
/// rescan can't tell "newly created" from "previously missed", so only the
/// creation hint may treat a window as new (otherwise a startup or periodic
/// scan would drag long-standing windows onto the focused monitor).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum RescanTrigger {
    Startup,
    Periodic,
    AxHint { pid: Option<Pid>, kind: AxHintKind },
    BackendHint { kind: String },
    PostEffect { op: OpId },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum AxHintKind {
    WindowCreated,
    /// The raw notification name, for the log. The core keys no decision off
    /// these — they all mean "go look".
    Other(String),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum OpOutcome {
    Ok,
    Failed { detail: String },
    Timeout,
}

/// A full observation of the world, already translated into core vocabulary:
/// the enumerator and backend resolve SpaceIds, display ids, and AX handles
/// before this is built.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WorldSnapshot {
    pub monitors: Vec<MonitorSnap>,
    pub windows: Vec<WindowSnap>,
    pub focused: Option<WindowId>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MonitorSnap {
    pub id: MonitorId,
    pub frame: Rect,
    pub is_main: bool,
    pub active_workspace: WorkspaceId,
    /// Per-display space count; the usable workspace count is the minimum
    /// across displays (extra spaces on one display are unreachable by Ordo).
    pub workspace_count: u8,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WindowSnap {
    pub id: WindowId,
    pub app: Pid,
    pub bundle_id: Option<String>,
    pub title: String,
    pub frame: Rect,
    pub workspace: WorkspaceId,
}
