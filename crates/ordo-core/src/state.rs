use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::effect::Expectation;
use crate::ids::{MonitorId, OpId, Pid, Point, Rect, VirtualMonitorId, WindowId, WorkspaceId};
use crate::mru::FocusHistory;
use crate::project::{project, Projection};

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

/// The virtual-monitor layout declarations, as the backend's word has them:
/// how many virtual monitors exist, which one is the anchor of the view, and
/// whether virtualization is on. See [`crate::project`] for what they mean.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VirtualMonitors {
    pub count: u8,
    pub viewed: VirtualMonitorId,
    pub enabled: bool,
}

fn first_monitor() -> VirtualMonitorId {
    VirtualMonitorId(1)
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WindowRecord {
    pub id: WindowId,
    pub app: Pid,
    /// Log/debug metadata only — decisions key off `app` (the pid).
    pub bundle_id: Option<String>,
    pub title: String,
    pub workspace: WorkspaceId,
    /// DECLARED, like `workspace`: the virtual monitor this window belongs to,
    /// per the backend's word. Without a virtual layer it is the position of
    /// the display the window sits on. Every "which monitor" question a command
    /// asks — MRU scoping, where to move, where a newcomer belongs — reads
    /// this, never `monitor`. `serde(default)` for checkpoints that predate it.
    #[serde(default = "first_monitor")]
    pub vmonitor: VirtualMonitorId,
    /// OBSERVED: the display whose frame contains the window's center. Stored
    /// (rather than recomputed per query) because the projection checks read
    /// it every snapshot and reconcile already recomputes it per snapshot.
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
/// match `expect` are ours; an expectation the world never meets expires after
/// `EXPECTATION_TTL_NS` of elapsed time so a lost op can't suppress genuinely
/// external changes forever.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PendingOp {
    pub op: OpId,
    pub expect: Expectation,
    /// The `mono_ns` of the event that issued the effect. `serde(default)` so
    /// checkpoints written before expiry became time-based still decode; those
    /// ops read as issued at time zero and expire at the first snapshot, which
    /// is what a resumed run should do with them anyway.
    #[serde(default)]
    pub issued_ns: u64,
}

/// Who owns the key-window slot. A DECLARATION, written only by commands —
/// never by an observation — and read by enforcement and by every command
/// that needs "the focused window".
///
/// `Deferred` is a positive statement, not an unset `Option`: the OS owns the
/// slot (the user clicked, Cmd+Tabbed, or nobody has commanded anything since
/// start) and there is nothing to enforce. It is deliberately NOT "copy the
/// next observed focus into the declaration": that would be a declaration
/// travelling through the observation channel, and choosing WHICH of a
/// batch of focus changes to copy reintroduces the race this type exists to
/// remove. It stands until the next command overwrites it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum FocusIntent {
    /// Ordo asserts this window should be key; a contradicting observation is
    /// a violation to re-assert (damped), never to absorb.
    Window(WindowId),
    #[default]
    Deferred,
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
    /// The virtual-monitor layout, mirrored from the backend's word. `None`
    /// is "there is no virtual layer": every helper below then reads the
    /// displays one to one, so the physical model is the degenerate case of
    /// the virtual one rather than a second code path.
    #[serde(default)]
    pub virtual_monitors: Option<VirtualMonitors>,
    pub windows: BTreeMap<WindowId, WindowRecord>,
    /// OBSERVED key window. Mirrors the world every snapshot, undamped and
    /// uncorrected; its declared twin is `focus_intent`.
    pub focused: Option<WindowId>,
    /// Private with one writer (`declare_focus`) so a declaration can never be
    /// written without also resetting its damping episode and recording it in
    /// the MRU history. `serde(default)` = `Deferred`: a checkpoint from before
    /// this field, like a fresh start, has nothing to enforce.
    #[serde(default)]
    focus_intent: FocusIntent,
    /// Grants issued by ENFORCEMENT for the current declaration (the command's
    /// own grant is not counted). One slot, one counter — the focus twin of
    /// `tear_corrections`. Reset by every new declaration and whenever the
    /// world agrees.
    #[serde(default)]
    pub(crate) focus_corrections: u8,
    /// A witnessed user gesture that could be navigation — Cmd+Tab, Cmd+`, a
    /// mouse-down outside every visible window — arrived since the last
    /// observation. The next observation consumes it: a focus landing on a
    /// hidden workspace in that observation is the user going there, and is
    /// followed; without it the same landing is a violation. "Since the last
    /// observation" rather than a time window because the engine serializes
    /// events, so this is exact and replayable.
    #[serde(default)]
    pub(crate) navigation_gesture: bool,
    /// The app that kept the key window when enforcement last stood down,
    /// while it still holds the slot. Retiring to `Deferred` alone does not
    /// end a standoff against a window on a HIDDEN workspace: the
    /// visible-key-window invariant re-declares the MRU head with a fresh
    /// budget, and each grant raises the parked window again — a rate-limited
    /// loop, forever. This is the evidence that stops it: the standoff already
    /// established that this app will not yield, so the same observation is
    /// not re-litigated.
    ///
    /// The unit is the APP, not the window that happened to be key: AppKit
    /// key-window ownership is per-application, and which of its windows an
    /// app keys is its own business (Chrome's key window wandered among its
    /// windows through run 51's standoff; Cmd+H churn hops focus among an
    /// app's hidden windows routinely). Conceding to a window makes each hop
    /// look like the world moving on, and the loop returns at full rate. It is
    /// spent when key belongs to a DIFFERENT app — a vacuum (`focused ==
    /// None`) is not that: nobody else took the slot — or by any command
    /// (`declare_focus`), never by a timer.
    #[serde(default)]
    pub(crate) conceded: Option<Pid>,
    pub focus_history: FocusHistory,
    pub pending: Vec<PendingOp>,
    /// Damping for tear re-alignment, mirroring `WindowRecord::corrections`.
    pub tear_corrections: u8,
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
            virtual_monitors: None,
            windows: BTreeMap::new(),
            focused: None,
            focus_intent: FocusIntent::Deferred,
            focus_corrections: 0,
            navigation_gesture: false,
            conceded: None,
            focus_history: FocusHistory::new(),
            pending: Vec::new(),
            tear_corrections: 0,
            next_op: 0,
        }
    }

    pub(crate) fn mint_op(&mut self) -> OpId {
        self.next_op += 1;
        OpId(self.next_op)
    }

    pub fn focus_intent(&self) -> FocusIntent {
        self.focus_intent
    }

    /// The one write path for the focus declaration. Recording `Window(w)` in
    /// the MRU history here is what makes MRU declaration-driven: the order
    /// follows what Ordo decided, so an app flinging focus around cannot
    /// reorder Alt+Tab. A new declaration is a new damping episode, and it
    /// spends any gesture still waiting for its observation: a command that
    /// lands between a Dock click and the next snapshot is the latest word,
    /// and the old focus that snapshot shows parked is the command's doing,
    /// not the click's. Likewise a standing concession: the command is fresh
    /// evidence that the user wants the slot moved, so it is fought for anew.
    pub(crate) fn declare_focus(&mut self, intent: FocusIntent) {
        if let FocusIntent::Window(w) = intent {
            self.focus_history.touch(w);
        }
        self.focus_intent = intent;
        self.focus_corrections = 0;
        self.navigation_gesture = false;
        self.conceded = None;
    }

    /// The declared window while it is actually in the model. A declaration
    /// about a window that is absent (closed, or dropped by one flaky scan)
    /// is vacuous rather than wrong: nothing to enforce, nothing for commands
    /// to act on, and it resumes untouched if the window reappears.
    pub fn focus_target(&self) -> Option<WindowId> {
        match self.focus_intent {
            FocusIntent::Window(w) if self.windows.contains_key(&w) => Some(w),
            _ => None,
        }
    }

    /// The focused window as COMMANDS must read it: the declaration when Ordo
    /// holds one, else the OS's choice. Between issuing a grant and its echo
    /// arriving the observation is stale by exactly the amount that once made
    /// a carry grab the previous window (run 38 seq 20447), and a fling that
    /// contradicts a standing declaration must not redirect a command either
    /// (run 51 seq 22484: a carry dropped because focus had been flung to a
    /// parked sibling).
    pub fn declared_focus(&self) -> Option<WindowId> {
        self.focus_target().or(self.focused)
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
    /// think in, unlike UUID order which is arbitrary. This order is what the
    /// projection indexes, so it must match the backend's (same sort key).
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

    /// How many virtual monitors there are: the word's count, or one per
    /// display without a virtual layer.
    pub fn monitor_count(&self) -> u8 {
        match self.virtual_monitors {
            Some(v) => v.count.max(1),
            None => (self.monitors.len() as u8).max(1),
        }
    }

    /// The projection in force: virtual monitors onto the displays present.
    pub fn projection(&self) -> Projection {
        self.projection_with(None, None)
    }

    /// The projection that WOULD be in force with the anchor and/or the
    /// switch changed — what a command needs to see the world it is about to
    /// make, before the backend's word confirms it.
    pub fn projection_with(
        &self,
        viewed: Option<VirtualMonitorId>,
        enabled: Option<bool>,
    ) -> Projection {
        let physical = self.monitors.len();
        match self.virtual_monitors {
            Some(v) => project(
                v.count,
                viewed.unwrap_or(v.viewed),
                enabled.unwrap_or(v.enabled),
                physical,
            ),
            None => project(physical as u8, VirtualMonitorId(1), false, physical),
        }
    }

    /// The display hosting a virtual monitor under `proj`, if any.
    pub fn host_in(&self, vm: VirtualMonitorId, proj: &Projection) -> Option<MonitorId> {
        let i = proj.host(vm)?;
        self.monitors_by_position().get(i).copied()
    }

    pub fn host_of(&self, vm: VirtualMonitorId) -> Option<MonitorId> {
        self.host_in(vm, &self.projection())
    }

    /// The virtual monitor a display stands for (see
    /// [`Projection::canonical_vm`]).
    pub fn canonical_vm_of(&self, display: MonitorId) -> Option<VirtualMonitorId> {
        let i = self.monitors_by_position().iter().position(|m| *m == display)?;
        self.projection().canonical_vm(i)
    }

    /// On screen: its workspace is the current one AND its virtual monitor is
    /// hosted. The one predicate every "hidden window" question reads.
    pub fn is_visible(&self, r: &WindowRecord) -> bool {
        self.is_visible_in(r, &self.projection())
    }

    pub fn is_visible_in(&self, r: &WindowRecord, proj: &Projection) -> bool {
        Some(r.workspace) == self.current_workspace() && proj.is_hosted(r.vmonitor)
    }

    /// The virtual monitor the user is "at": the declared focus's monitor,
    /// else the one the main display stands for, else the anchor. The monitor
    /// twin of `focused_monitor`, and what new windows are corralled onto.
    pub fn focused_vmonitor(&self) -> Option<VirtualMonitorId> {
        self.declared_focus()
            .and_then(|w| self.windows.get(&w))
            .map(|r| r.vmonitor)
            .or_else(|| {
                self.monitors
                    .values()
                    .find(|m| m.is_main)
                    .and_then(|m| self.canonical_vm_of(m.id))
            })
            .or_else(|| self.virtual_monitors.map(|v| v.viewed))
    }

    /// The display holding a point, else the nearest by center — the same
    /// rule reconcile uses to attribute a window to a display.
    pub fn display_holding(&self, p: Point) -> Option<&MonitorRecord> {
        self.monitors
            .values()
            .find(|m| m.frame.contains(p))
            .or_else(|| {
                self.monitors.values().min_by(|a, b| {
                    let (ca, cb) = (a.frame.center(), b.frame.center());
                    let da = (ca.x - p.x).powi(2) + (ca.y - p.y).powi(2);
                    let db = (cb.x - p.x).powi(2) + (cb.y - p.y).powi(2);
                    da.total_cmp(&db)
                })
            })
    }

    /// Where the window belongs on screen under `proj`: its own frame when
    /// that already sits on its monitor's host, else that frame carried over
    /// proportionally from the display it is on. Pure geometry; the write is
    /// the caller's.
    pub fn projected_frame_in(&self, r: &WindowRecord, proj: &Projection) -> Rect {
        let Some(host) = self.host_in(r.vmonitor, proj).and_then(|h| self.monitors.get(&h)) else {
            return r.frame;
        };
        if host.frame.contains(r.frame.center()) {
            return r.frame;
        }
        let from = self
            .monitors
            .get(&r.monitor)
            .map(|m| m.frame)
            .unwrap_or(host.frame);
        r.frame.translate_between(&from, &host.frame)
    }

    pub fn projected_frame(&self, r: &WindowRecord) -> Rect {
        self.projected_frame_in(r, &self.projection())
    }
}

impl Default for State {
    fn default() -> Self {
        Self::new()
    }
}
