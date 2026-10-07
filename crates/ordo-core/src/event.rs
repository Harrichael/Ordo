use serde::{Deserialize, Serialize};

use crate::ids::{MonitorId, OpId, Pid, Point, Rect, VirtualMonitorId, WindowId, WorkspaceId};
use crate::state::VirtualMonitors;

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
    /// The user acted on focus through a channel Ordo does not command: a
    /// click, or one of macOS's own switchers. Witnessed by the tap and passed
    /// through untouched — this is intent Ordo cannot name a target for, so
    /// it hands the key-window slot to the OS (`FocusIntent::Deferred`).
    /// Without this channel such gestures left no trace but the focus change
    /// they caused, which is what made focus look observational.
    Gesture {
        at: Ts,
        gesture: Gesture,
    },
}

/// The gestures that move focus are witnessed, and ordinary keystrokes too,
/// as a bare [`Gesture::Key`], within a limit: a key press explains a focus
/// change only inside the app it was typed into, or anywhere when it was
/// typed into no model window (a launcher, the desktop). Without that limit
/// typing would bless every app that grabs focus meanwhile: Cmd+N preceded a
/// verified app-initiated fling by 500ms (run 51 seq 12769).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum Gesture {
    /// Any mouse button went down here (global CG coordinates), on neither a
    /// menu bar nor the Dock. The point is what lets the core tell a click
    /// INTO a visible window (the OS keys the right thing; a later
    /// hidden-workspace landing is a fling) from a click elsewhere — the
    /// desktop, a notification — that can be navigation.
    MouseDown { at: Point },
    /// A mouse button went down in a display's menu bar. It moves the user to
    /// that display — macOS hands focus to its front window, or its desktop
    /// when it shows nothing — so focus is the OS's again. But nothing in a
    /// menu bar picks a window: a hidden landing after it is never followed
    /// (opening Ordo's own menu was once read as navigating to a parked
    /// window, and slid the view away).
    MenuBar { at: Point },
    /// A mouse button went down on the Dock or one of its menus: the window
    /// server routed the click to a window of the Dock's. The core cannot
    /// tell this from a point, as the Dock hides, moves between edges and
    /// displays, and lies over windows: hit-tested, its clicks were taken as
    /// clicks into the window beneath (run 55 seq 3205: Slack's, under an
    /// auto-hidden Dock). Like the app switcher, it can bring up any app's
    /// window, so a hidden landing after it is the user going there. Mission
    /// Control and Launchpad are the Dock's too, and so are their clicks,
    /// rightly: they are navigation. (Stage Manager is not the Dock's.)
    Dock { at: Point },
    /// Ordo's own menu opened or closed. Picks in it arrive as hotkeys, while
    /// it stays open; what the screen shows meanwhile is the apps reacting to
    /// losing the keyboard to it (kitty names another, parked window as
    /// focused), not the user going anywhere.
    OwnMenu { open: bool },
    /// macOS's app switcher completed (Cmd released after Cmd+Tab) or its
    /// in-app window cycle fired (Cmd+`). The target is the OS's to know; a
    /// focus landing on a hidden workspace right after is the user going there.
    SystemSwitch,
    /// The user pressed a key that was not an Ordo chord: typing, or an app's
    /// window shortcut, Spotlight, a launcher. It carries no key, so the log
    /// never holds what was typed, and the tap reports one per short interval
    /// at most. It explains the next focus change within the app it was typed
    /// into (see above), but hands the slot to no one and licenses no follow:
    /// typing is not navigation.
    Key,
}

/// Which kind of input explained a focus landing, as the log records it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Input {
    Click,
    MenuBar,
    Switcher,
    Key,
    Dock,
    /// No input of the user's: a window born without focus that its app
    /// keyed a beat later (see `Landing::awaited`), declared as a birth.
    Birth,
}

impl From<Gesture> for Input {
    fn from(g: Gesture) -> Self {
        match g {
            Gesture::MouseDown { .. } => Input::Click,
            Gesture::MenuBar { .. } | Gesture::OwnMenu { .. } => Input::MenuBar,
            Gesture::Dock { .. } => Input::Dock,
            Gesture::SystemSwitch => Input::Switcher,
            Gesture::Key => Input::Key,
        }
    }
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
    /// Cmd+Shift+Left/Right: the focused window moves to the adjacent VIRTUAL
    /// monitor, clamped at the ends, and the "view" moves with it — focus
    /// stays on it and the mouse follows to its new center. When the target
    /// monitor is hidden (fewer displays than monitors) the view is switched
    /// there too, so the window is never moved out of sight.
    MoveFocusedToMonitorPrev,
    MoveFocusedToMonitorNext,
    /// Cmd+Alt+J / Cmd+Alt+K: view the adjacent virtual monitor, clamped at
    /// the ends. Global — not per workspace. Focus goes to that monitor's MRU
    /// window, so on a full rig this is a focus jump between displays and with
    /// one display it also reveals the monitor's windows.
    ViewMonitorPrev,
    ViewMonitorNext,
    /// Ctrl+Alt+Cmd+V: virtualization on/off. Off collapses every virtual monitor
    /// onto the displays present; on shows only the viewed one where displays
    /// are short. The declarations are untouched either way.
    ToggleVirtualMonitors,
    /// Ctrl+Cmd+Left/Right: carry the focused window to the adjacent workspace
    /// and switch there with it. The window keeps its frame and its focus, so
    /// the mouse has nowhere to follow — nothing on screen moves except the
    /// scenery.
    CarryFocusedToWorkspacePrev,
    CarryFocusedToWorkspaceNext,
    /// From the menu bar, never a chord: fold virtual monitor `from` into
    /// `into` on every workspace, one fewer monitor. Refused while there are
    /// no more monitors than displays — each display needs one to stand for.
    MergeMonitors {
        from: VirtualMonitorId,
        into: VirtualMonitorId,
    },
    /// From the menu bar: one more virtual monitor, empty, after the last on
    /// every workspace. Nothing on screen changes (see
    /// [`crate::anchor_after_add`]); it is room to move windows into.
    AddMonitor,
    /// From the menu bar: workspace `from` moves to position `to`, and the
    /// ones between step over (see [`crate::after_move`]). Nothing on screen
    /// changes; the workspaces are renumbered, the current one with them.
    MoveWorkspace {
        from: WorkspaceId,
        to: WorkspaceId,
    },
    /// From the menu bar: virtual monitor `from` moves to position `to` on
    /// every workspace, the anchor with it. The displays then show whichever
    /// monitors the new order puts in view.
    MoveMonitor {
        from: VirtualMonitorId,
        to: VirtualMonitorId,
    },
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

/// A full reading of the world, already translated into core vocabulary: the
/// enumerator and backend resolve SpaceIds, display ids, and AX handles before
/// this is built.
///
/// Two channels, deliberately separate. `monitors`/`windows`/`focused` are
/// OBSERVATIONS — what CG and AX actually saw; belief follows them freely.
/// `workspaces` is the backend's WORD on the workspace layer, which is not an
/// observation of the same kind: under emulation those are Ordo's own
/// declarations (only commands change them — a scan physically cannot report
/// a different assignment, which is what makes the old smuggled
/// `WindowSnap.workspace` bug class unrepresentable), while under native
/// Spaces the OS owns them and the same field genuinely is observed. The core
/// handles both identically — the backend's word is authoritative — so the
/// core path never bifurcates; only the provenance differs, behind the seam.
///
/// `unread` is neither: it says what the observation did NOT cover, so the
/// absence of those windows from `windows` is not read as their closing.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WorldSnapshot {
    pub monitors: Vec<MonitorSnap>,
    pub windows: Vec<WindowSnap>,
    pub focused: Option<WindowId>,
    /// `serde(default)` so pre-split logs still decode for inspection (their
    /// replays diverge regardless — the decision logic changed with the shape).
    #[serde(default)]
    pub workspaces: WorkspaceSnap,
    /// Windows missing from `windows` that the window server still has: their
    /// app did not answer in time, or answered without them, as an AX read
    /// now and then does. The model keeps them as last seen, placed by the
    /// backend's word like any other, so a window missing from a snapshot and
    /// not listed here is one the window server agrees is gone.
    #[serde(default)]
    pub unread: Vec<WindowId>,
    /// The key window is one Ordo does not manage (the screenshot tool's
    /// capture bar, mid-capture), so `focused` is `None` though focus is not
    /// in a vacuum: someone holds it, and nothing is to be taken back from
    /// them.
    #[serde(default)]
    pub key_unmanaged: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MonitorSnap {
    pub id: MonitorId,
    pub frame: Rect,
    pub is_main: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WindowSnap {
    pub id: WindowId,
    pub app: Pid,
    pub bundle_id: Option<String>,
    pub title: String,
    pub frame: Rect,
    /// The window's kind as the app declares it (`AXSubrole` on macOS):
    /// AXStandardWindow, AXDialog, AXFloatingWindow, AXSystemFloatingWindow.
    /// An observation like any other here — the core does not act on it yet.
    /// `serde(default)` because logs written before this field exist must
    /// still replay.
    #[serde(default)]
    pub subrole: Option<String>,
    /// The window server's layer for it (0 for normal windows; panels and
    /// notification-style windows sit higher), and the window it is a child
    /// of, if any — a child moves with its parent, as a find bar or a hover
    /// card does. Observations the core does not act on yet, logged so how
    /// popups are handled is chosen from what apps actually do. Defaulted for
    /// the same reason as `subrole`.
    #[serde(default)]
    pub layer: Option<i32>,
    #[serde(default)]
    pub parent: Option<WindowId>,
}

/// The workspace layer, as the backend tells it. Absence means UNKNOWN, never
/// a default: a monitor or window missing from these maps leaves belief
/// exactly as it was (the old contract fabricated workspace 1 for unresolved
/// windows — an unknown laundered into a fact).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceSnap {
    /// Active workspace and per-display workspace count, per monitor the
    /// backend resolved. The usable count is the minimum across the monitors
    /// covered here (extra spaces on one display are unreachable by Ordo);
    /// an uncovered monitor cannot move the count or its own active belief.
    pub monitors: std::collections::BTreeMap<MonitorId, MonitorWs>,
    /// Workspace assignment for every window the backend resolved; an
    /// unresolved window is absent, and its belief stands.
    pub assignments: std::collections::BTreeMap<WindowId, WorkspaceId>,
    /// The virtual-monitor layer, when the backend has one. `None` means
    /// there is no such layer (native Spaces, logs from before it existed):
    /// the core then takes each window's virtual monitor to be the position
    /// of the display it sits on, which is what "no virtualization" means.
    #[serde(default)]
    pub virtual_monitors: Option<VirtualMonitorsWord>,
}

/// The backend's word on the virtual-monitor layer: the layout declarations
/// (how many, which is the anchor, whether virtualization is on) and each
/// resolved window's monitor. Same provenance rules as the workspace word.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct VirtualMonitorsWord {
    pub view: VirtualMonitors,
    pub assignments: std::collections::BTreeMap<WindowId, VirtualMonitorId>,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct MonitorWs {
    pub active: WorkspaceId,
    pub count: u8,
}
