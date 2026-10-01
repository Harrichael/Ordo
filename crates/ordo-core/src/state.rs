use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::effect::Expectation;
use crate::event::Input;
use crate::ids::{MonitorId, OpId, Pid, Point, Rect, VirtualMonitorId, WindowId, WorkspaceId};
use crate::mru::FocusHistory;
use crate::project::{after_merge, after_move, project, Projection};

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
    /// OBSERVED: the window this one is attached to (a popup, a hover card),
    /// per the window server. See [`State::root_of`].
    #[serde(default)]
    pub parent: Option<WindowId>,
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
    /// Ordo asserts that NO window should be key: the user is on a virtual
    /// monitor with nothing on it (the anchor), and its display's desktop has
    /// focus — what macOS itself does for a click on empty desktop. Without
    /// this an empty monitor could not hold focus at all: it stayed on the
    /// window the view had just hidden, and the visible-key-window invariant
    /// handed it to some window on another display — which macOS, still
    /// counting the hidden window's app as the one in use, kept taking back.
    Desktop,
    #[default]
    Deferred,
}

/// What the user's latest input can still explain of the focus changes that
/// follow it: the only way an observed focus enters the MRU order. What the
/// OS or an app keys with no input behind it is not where the user went. (A
/// look that shows the focused window closing writes the order too, but
/// through Ordo's declaration of the window it hands focus on to.)
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct Landing {
    /// A key press or click: the first focus change after it is the user's,
    /// if it comes within `AWAY_TTL_NS` (update.rs). Spent by that change, or
    /// lapsed; a later key press or click starts it afresh.
    pub(crate) away: Option<Away>,
    /// The windows (roots) a click hit, and when: a landing on one of them is
    /// the click's within the longer `INTO_TTL_NS`, whatever landed first,
    /// since a look can come between a click and the app keying the window
    /// it hit. Spent by that landing, or lapsed.
    pub(crate) into: Option<(Vec<WindowId>, u64)>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct Away {
    /// The focus as it stood at the input: a change is a move from it.
    pub(crate) from: Option<WindowId>,
    pub(crate) since_ns: u64,
    pub(crate) by: Input,
    /// The app a key press was typed into: the change it explains must stay
    /// in that app (a window shortcut), since another app grabbing focus
    /// while the user types is that app's doing. `None` for every other
    /// input, and for a key typed into no model window (a launcher, the
    /// desktop), which can send focus anywhere.
    pub(crate) within: Option<Pid>,
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
    /// events, so this is exact and replayable. Only the follow reads it:
    /// what a gesture can write into the MRU order is `unseen_landing`, which
    /// is spent by its own rules.
    #[serde(default)]
    pub(crate) navigation_gesture: bool,
    /// The MRU half of a gesture, where `navigation_gesture` is the follow
    /// half, and a key press has only this half. Unlike that flag it outlives
    /// the next observation: each part is spent by the landing it explains or
    /// lapses (see [`Landing`]); a newer input replaces it, and any command
    /// clears it.
    #[serde(default)]
    pub(crate) unseen_landing: Landing,
    /// A menu-bar click opened a menu that no click or hotkey has closed yet.
    /// While a menu tracks, the front app can report a window on a hidden
    /// workspace as focused (kitty was caught doing it on every click), and
    /// refocusing in answer closes the menu; the click that ends it may be a
    /// menu item, which can open anything.
    #[serde(default)]
    pub(crate) menu_open: bool,
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
    /// Visible windows found standing on a display other than their monitor's,
    /// and when (`mono_ns`) each was first seen there. Adoption waits on this:
    /// macOS re-homes a vanished display's windows a beat BEFORE it reports
    /// the display gone, so a window that has just moved displays is either
    /// the user's drag or the first sign of an unplug, and one snapshot cannot
    /// tell them apart (run 56: Slack was adopted onto the laptop's monitor
    /// in the snapshot before the removal arrived). Cleared by any topology
    /// change and whenever the window is back on its host.
    #[serde(default)]
    pub(crate) misplaced_since: BTreeMap<WindowId, u64>,
    /// Visible windows moved or resized, by a hand that wasn't Ordo's, in the
    /// last observation, and settled ones whose stack check is still owed.
    /// See `restack_settled_moves`.
    #[serde(default)]
    pub(crate) moving: BTreeSet<WindowId>,
    /// Windows missing from the model since `mono_ns`, whose place in the
    /// focus history is kept for now. See `keep_vanished_places`.
    #[serde(default)]
    pub(crate) vanished: BTreeMap<WindowId, u64>,
    pub pending: Vec<PendingOp>,
    /// The workspace the newest switch Ordo issued is taking the user to, and
    /// that switch's op. Intent, not bookkeeping: it lives exactly as long as
    /// its op, and ends when the op is confirmed, fails, is lost, or a newer
    /// switch replaces it. See [`State::declared_workspace`].
    #[serde(default)]
    pub(crate) workspace_intent: Option<(OpId, WorkspaceId)>,
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
            unseen_landing: Landing::default(),
            menu_open: false,
            conceded: None,
            focus_history: FocusHistory::new(),
            misplaced_since: BTreeMap::new(),
            moving: BTreeSet::new(),
            vanished: BTreeMap::new(),
            pending: Vec::new(),
            workspace_intent: None,
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
            let root = self.root_of(w);
            self.focus_history.touch(root);
        }
        self.focus_intent = intent;
        self.focus_corrections = 0;
        self.navigation_gesture = false;
        self.unseen_landing = Landing::default();
        self.conceded = None;
    }

    /// The window `w` is attached to at the top: itself, unless it has a
    /// parent in the model. Stacking and MRU are about roots: the window
    /// server always draws an attached window just above its parent, so it
    /// has no place of its own in either, and ordering it against its parent
    /// is an order that can never land.
    pub fn root_of(&self, w: WindowId) -> WindowId {
        let mut root = w;
        // Bounded, so a cycle in what the window server reports can't hang.
        for _ in 0..8 {
            match self.windows.get(&root).and_then(|r| r.parent) {
                Some(p) if p != root && self.windows.contains_key(&p) => root = p,
                _ => break,
            }
        }
        root
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
        match self.focus_intent {
            FocusIntent::Desktop => None,
            _ => self.focus_target().or(self.focused),
        }
    }

    /// The workspace Ordo is taking the user to, while the switch it issued
    /// is under way, else the one observed. A burst of presses runs faster
    /// than the looks that confirm each switch, and "next" must step from
    /// where the last press went, not from where the last look was taken.
    pub fn declared_workspace(&self) -> Option<WorkspaceId> {
        self.workspace_intent
            .map(|(_, ws)| ws)
            .or_else(|| self.current_workspace())
    }

    /// The workspace a window is being moved to, while that move is
    /// unconfirmed, else the one it was seen on. `declared_workspace`'s
    /// counterpart for one window, read from `pending` rather than a field of
    /// its own: the assignment itself is the backend's word, which every
    /// snapshot carries back, so the pending move only bridges the gap until
    /// it does, and at most one move per window is ever pending.
    pub fn declared_workspace_of(&self, window: WindowId) -> Option<WorkspaceId> {
        self.pending
            .iter()
            .rev()
            .find_map(|p| match p.expect {
                Expectation::WindowOn { window: w, workspace } if w == window => Some(workspace),
                _ => None,
            })
            .or_else(|| self.windows.get(&window).map(|r| r.workspace))
    }

    /// The virtual monitor a window is going to, once Ordo's word on it
    /// lands: its unconfirmed assignment, else the one the backend gave it,
    /// renumbered by any merge or monitor move issued since. The monitor
    /// twin of `declared_workspace_of`, read from `pending` for its reason.
    pub fn declared_vmonitor_of(&self, window: WindowId) -> Option<VirtualMonitorId> {
        let mut m = self.windows.get(&window)?.vmonitor;
        for p in &self.pending {
            match p.expect {
                Expectation::WindowOnMonitor { window: w, monitor } if w == window => m = monitor,
                Expectation::MonitorsMerged { from, into, .. } => m = after_merge(m, from, into),
                Expectation::MonitorsMoved { from, to, .. } => {
                    m = VirtualMonitorId(after_move(m.0, from.0, to.0))
                }
                _ => {}
            }
        }
        Some(m)
    }

    /// The projection once Ordo's word on the layout lands: the views, the
    /// virtualization switch, the merges and the monitor moves still
    /// pending, applied in the order they were issued. Read with
    /// `declared_vmonitor_of`, whose numbering it shares. An add is left
    /// out: it changes nothing on screen (see `anchor_after_add`).
    pub fn declared_projection(&self) -> Projection {
        let Some(mut v) = self.virtual_monitors else {
            return self.projection();
        };
        for p in &self.pending {
            match p.expect {
                Expectation::Viewing(t) => v.viewed = t,
                Expectation::VirtualMonitorsEnabled(e) => v.enabled = e,
                Expectation::MonitorsMerged { from, into, count } => {
                    v.viewed = after_merge(v.viewed, from, into);
                    v.count = count;
                }
                Expectation::MonitorsMoved { viewed, .. } => v.viewed = viewed,
                _ => {}
            }
        }
        project(v.count, v.viewed, v.enabled, self.monitors.len())
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
        if self.focus_intent == FocusIntent::Desktop {
            if let Some(v) = self.virtual_monitors {
                return Some(v.viewed);
            }
        }
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
