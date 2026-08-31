use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::effect::Expectation;
use crate::ids::{MonitorId, OpId, Pid, Rect, WindowId, WorkspaceId};
use crate::mru::FocusHistory;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Mode {
    Active,
    /// The kill switch fired. The core keeps absorbing observations (belief
    /// tracking costs nothing and keeps the log useful) but emits no effects:
    /// after a rescue the tool must be provably passive until restarted.
    Rescued,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MonitorRecord {
    pub id: MonitorId,
    /// Global CG coordinates (top-left origin, y-down).
    pub frame: Rect,
    pub is_main: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WindowRecord {
    pub id: WindowId,
    pub app: Pid,
    /// Log/debug metadata only — decisions key off `app` (the pid).
    pub bundle_id: Option<String>,
    pub title: String,
    pub workspace: WorkspaceId,
    /// Derived: the monitor whose frame contains the window's center. Stored
    /// (rather than recomputed per query) because MRU predicates read it on
    /// the hot path and reconcile already recomputes it per snapshot.
    pub monitor: MonitorId,
    pub frame: Rect,
    /// Placement correctives issued without the world staying put, damped per
    /// axis: workspace assignment and on-screen frame are independent fights
    /// (a new window can be wrong on both at once), so a single counter would
    /// double-count and cross-reset them. At a counter's limit we stop
    /// correcting that axis and log instead — an app that fights back becomes a
    /// loud log line, never an effect loop.
    pub ws_corrections: u8,
    pub frame_corrections: u8,
}

/// A self-initiated operation awaiting its echo in a snapshot. Deltas that
/// match `expect` are ours; unmatched expectations expire after a few rescans
/// so a lost op can't suppress genuinely external changes forever.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PendingOp {
    pub op: OpId,
    pub expect: Expectation,
    pub rescans_left: u8,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct State {
    pub mode: Mode,
    /// min over monitors of their space count — the workspaces reachable on
    /// every display, since a workspace spans all monitors.
    pub workspace_count: u8,
    /// OBSERVED active workspace per monitor. Under the native backend the OS
    /// co-owns this (the user can swipe one display's space behind our back),
    /// so a single scalar "current workspace" would be a lie. Coherence across
    /// monitors is a derived property, not a stored one.
    pub monitor_ws: BTreeMap<MonitorId, WorkspaceId>,
    pub monitors: BTreeMap<MonitorId, MonitorRecord>,
    pub windows: BTreeMap<WindowId, WindowRecord>,
    pub focused: Option<WindowId>,
    pub focus_history: FocusHistory,
    pub pending: Vec<PendingOp>,
    /// Damping for tear re-alignment, mirroring `WindowRecord::corrections`.
    pub tear_corrections: u8,
    /// When we last emitted a SwitchWorkspace (event clock — the core never
    /// reads time). Follow-the-focus consults this: a switch's focus fallout
    /// keeps arriving well after every landing signal fires, so "recently
    /// switched" is the only tell that a stray focus is our own echo.
    #[serde(default)]
    pub last_switch_mono_ns: Option<u64>,
    /// OpId counter. Lives in State — not a global — so `update` stays pure
    /// and replay mints identical ids.
    pub next_op: u64,
}

impl State {
    pub fn new() -> Self {
        State {
            mode: Mode::Active,
            workspace_count: 1,
            monitor_ws: BTreeMap::new(),
            monitors: BTreeMap::new(),
            windows: BTreeMap::new(),
            focused: None,
            focus_history: FocusHistory::new(),
            pending: Vec::new(),
            tear_corrections: 0,
            last_switch_mono_ns: None,
            next_op: 0,
        }
    }

    pub(crate) fn mint_op(&mut self) -> OpId {
        self.next_op += 1;
        OpId(self.next_op)
    }

    /// The user's focus as COMMANDS must read it: the most recent still-
    /// pending focus grant we issued, falling back to observed focus. Between
    /// issuing a FocusWindow and its echo arriving, the observation is stale
    /// by exactly the amount that once made a carry grab the wrong window
    /// (run 38 seq 20447: the previous window, not the visibly focused one).
    /// The declared target self-expires with its expectation, so a failed
    /// grant falls back to observation within a few rescans.
    pub fn declared_focus(&self) -> Option<WindowId> {
        self.pending
            .iter()
            .rev()
            .find_map(|p| match p.expect {
                Expectation::Focused(w) => Some(w),
                _ => None,
            })
            .or(self.focused)
    }

    /// The monitor the user is "at": the focused window's monitor, falling
    /// back to the main display. This anchor decides what "current workspace"
    /// means and where new windows belong.
    pub fn focused_monitor(&self) -> Option<MonitorId> {
        self.focused
            .and_then(|w| self.windows.get(&w))
            .map(|r| r.monitor)
            .or_else(|| self.monitors.values().find(|m| m.is_main).map(|m| m.id))
            .or_else(|| self.monitors.keys().next().copied())
    }

    pub fn current_workspace(&self) -> Option<WorkspaceId> {
        self.focused_monitor()
            .and_then(|m| self.monitor_ws.get(&m))
            .copied()
    }

    /// Monitors disagree about their active workspace — possible only under
    /// the native backend, where each display's space is independently
    /// switchable by the user.
    pub fn is_torn(&self) -> bool {
        let mut ws = self.monitor_ws.values();
        match ws.next() {
            Some(first) => ws.any(|w| w != first),
            None => false,
        }
    }

    /// Monitors left-to-right (then top-to-bottom): the spatial order users
    /// think in, unlike UUID order which is arbitrary.
    pub fn monitors_by_position(&self) -> Vec<MonitorId> {
        let mut ms: Vec<&MonitorRecord> = self.monitors.values().collect();
        ms.sort_by(|a, b| {
            a.frame
                .x
                .total_cmp(&b.frame.x)
                .then(a.frame.y.total_cmp(&b.frame.y))
        });
        ms.into_iter().map(|m| m.id).collect()
    }
}

impl Default for State {
    fn default() -> Self {
        Self::new()
    }
}
