//! The emulated backend's orchestration: applying the [`Ledger`]'s decisions
//! to the desktop through the [`Desktop`] port, persisting its promises, and
//! policing that reality keeps matching them.
//!
//! The model's data splits in two, and the split is the architecture.
//! DECLARATIONS — which workspace and virtual monitor a window belongs to,
//! which workspace is visible, which monitor is the anchor, whether
//! virtualization is on — are written only by Ordo's own commands: a user
//! switch, view or move, rescue, and a window's birth (a brand-new window has
//! no prior intent to preserve). OBSERVATIONS — frames, existence, focus — are
//! authoritative about the world, never about intent. An observation that
//! contradicts a declaration is a violation to correct on screen or to surface
//! to the user, NEVER to absorb into the declaration: a declaration must not
//! travel through the observation channel.
//!
//! This is also the projection plane: the virtual monitors of the control
//! plane land on whatever displays are present (`ordo_core::project`), and a
//! window whose monitor has no display is parked exactly like one on a hidden
//! workspace. A display coming or going is a change to the projection and is
//! planned like a switch — which is the whole of monitor memory.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;

use ordo_core::{
    project, Pid, Point, Rect, VirtualMonitorId, VirtualMonitors, WindowId,
    WorkspaceId, FRAME_EPSILON,
};

use crate::ledger::{Claim, Ledger, SwitchPlan};
use crate::statefile::{self, PersistedState, PersistedWindow};
use crate::trace::{HoldStat, ParkTrace, ParkTraceKind};
use crate::{Desktop, Unhide};

/// How much of a parked window stays on-screen. macOS refuses to keep a fully
/// off-screen window where you put it, so we leave a 1px handle — also the
/// manual escape hatch if Ordo dies mid-park.
const SLIVER: f64 = 1.0;

/// How far the WindowServer may move one of Ordo's ON-SCREEN writes — a
/// restore, a rehome, a rescue — from where it asked, on the clamped
/// (vertical) axis: it pushes a frame down under the menu bar, and hoists the
/// origin of a request too tall for its display up to that display's top
/// inset. A park landing needs none of this any more: a park moves only x,
/// which macOS never clamps, so it is compared to its request exactly.
///
/// What is left is slack around a frame ORDO ITSELF REQUESTED, serving the
/// enforcement budget's own-write exemption and the retired park corners
/// below. It stays generous on purpose: a false match leaves one violation
/// uncounted (it is still corrected on the same pass), while a false mismatch
/// charges Ordo's own write to the window — the leak that drained budgets
/// until damping surrendered.
const CLAMP_SLACK: f64 = 160.0;

/// Foreign-attributed re-parks of an escaped window before enforcement stands
/// down: no more writes, stderr + a Standoff trace, and the declaration LEFT
/// ALONE. Never adoption — losing where the user filed a window is silent and
/// permanent, while a visibly misplaced window is obvious and self-heals on
/// the next switch or command.
const ENFORCE_LIMIT: u8 = 3;

/// The rectangles park geometry depends on. They are NOT interchangeable, and
/// conflating them is how a row of title bars ended up across the bottom of
/// the main display (2026-09-01).
///
/// macOS refuses to push a window's title bar off the bottom of the screen it
/// is on, so vertical hiding is impossible; only the horizontal escape hides
/// anything, and it has to clear a display with nothing beyond it to catch the
/// window. Escaping RIGHT meant either dropping the window onto the next
/// screen (Michael's second display wore a 1470x66 band for weeks) or aiming
/// at the rightmost display's own bottom corner — where the write asked for a
/// frame that display could not hold, and got a shorter one back.
///
/// The escape goes LEFT instead: nothing sits left of the leftmost display,
/// and x is the one axis macOS never clamps.
#[derive(Clone, PartialEq, Debug)]
struct Geometry {
    /// Where a window RETURNS to: a re-homed window must land where the user
    /// is looking.
    main: Rect,
    /// The display windows hide PAST — the leftmost, so nothing lies beyond
    /// it to catch them.
    park_host: Rect,
    /// The rightmost display: no longer a park target, only the corner
    /// windows parked by builds up to 2026-09-02 still sit at. It is here so
    /// [`in_park_corner`] can still recognize them; drop it once no live
    /// window or state file can predate this change.
    legacy_host: Rect,
    /// Every display, left to right — the order the projection indexes. Must
    /// sort exactly as the core's `State::monitors_by_position`.
    displays: Vec<Rect>,
}

impl Geometry {
    fn read(d: &dyn Desktop) -> Self {
        let main = d.main_display();
        let mut displays = d.displays();
        displays.sort_by(|a, b| a.x.total_cmp(&b.x).then(a.y.total_cmp(&b.y)));
        let park_host = displays.first().copied().unwrap_or(main);
        let legacy_host = displays
            .iter()
            .copied()
            .max_by(|a, b| {
                (a.x + a.w)
                    .total_cmp(&(b.x + b.w))
                    .then((a.y + a.h).total_cmp(&(b.y + b.h)))
            })
            .unwrap_or(main);
        Geometry {
            main,
            park_host,
            legacy_host,
            displays,
        }
    }

    fn physical(&self) -> usize {
        self.displays.len()
    }

    /// The display holding a point, if any.
    fn display_at(&self, p: Point) -> Option<Rect> {
        self.displays.iter().copied().find(|d| d.contains(p))
    }
}

/// A requested workspace outside the configured range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkspaceOutOfRange(pub WorkspaceId);

/// A requested virtual monitor outside the known range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MonitorOutOfRange(pub VirtualMonitorId);

pub struct EmulatedWorkspaces {
    ledger: Ledger,
    /// The real on-screen frame each parked window was taken from. Whole,
    /// because it is what the core is told the window's frame IS while the
    /// window is really a sliver; only its origin is written back on restore.
    saved: HashMap<WindowId, Rect>,
    /// The frame each window last held with the FULL rig present — every
    /// virtual monitor on a display of its own. Refreshed from observation in
    /// that state only, never while collapsed or viewed on a shared display,
    /// so neither a toggle nor an undocked session can overwrite the docked
    /// layout. This is what lets a replug put a window back exactly where it
    /// was: macOS re-homes a vanished display's windows onto the laptop before
    /// Ordo sees anything, so `saved` alone can only remember the laptop frame.
    home: HashMap<WindowId, Rect>,
    /// Windows currently parked off-screen. Tracked so re-parking an
    /// already-parked window (a hidden->hidden move) doesn't overwrite its real
    /// saved frame with the sliver position.
    parked: HashSet<WindowId>,
    /// FOREIGN-attributed re-parks issued per window since it last sat parked
    /// correctly (our own writes and systemic events don't count). At
    /// ENFORCE_LIMIT enforcement stands down for that window.
    enforce_attempts: HashMap<WindowId, u8>,
    /// The park-corner frame most recently REQUESTED per window — the anchor
    /// compliance is judged against, because macOS lands a park pulled back
    /// up from the corner by an app-dependent amount no constant should try
    /// to predict. Deliberately NOT persisted (the state file carries only
    /// declarations): after a restart each parked window reads as one
    /// violation and is re-parked once, re-establishing the anchor — which is
    /// also what migrates windows parked at a corner an older Ordo used.
    park_request: HashMap<WindowId, Rect>,
    /// The last frame Ordo asked the OS to give each window, whatever the
    /// reason (park, restore, rehome, rescue). A window observed at (a clamp
    /// of) this frame is our own write landing late, never an app fighting
    /// back — charging those drained the enforcement budget against us. Only
    /// the origin was ever sent, and only the origin is ever compared; the
    /// size rides along because callers hand whole rects.
    last_requested: HashMap<WindowId, Rect>,
    /// Windows with an uncounted re-park issued and not yet observed
    /// compliant. Gates the suppressed path's WRITES, not just its counting:
    /// re-issuing an identical corner write every pass while the first is
    /// still in flight was a self-sustaining write loop.
    pending_repark: HashSet<WindowId>,
    /// Display geometry as of the last enforcement pass. Any change moves the
    /// park corner itself — a systemic event no window should be blamed for.
    last_geometry: Option<Geometry>,
    /// Where the ledger's promises persist across restarts (None = ephemeral,
    /// e.g. `--fresh`). Written through on every mutation; see statefile.rs
    /// for the trust model.
    state_path: Option<PathBuf>,
    boot_time: i64,
    /// True during a fresh session (the R chord): the state file is neither
    /// read nor written, so O can still bring the pre-R organization back.
    suspended: bool,
    /// Diagnostic record of what this model did to windows' frames, drained by
    /// the shell each snapshot. See [`crate::trace`] for why it must exist:
    /// every other channel sees the substituted belief, so without this the
    /// parking mechanism is invisible when it works and misleading when it
    /// doesn't.
    trace: Vec<ParkTrace>,
}

impl EmulatedWorkspaces {
    pub fn new(count: u8) -> Self {
        EmulatedWorkspaces {
            ledger: Ledger::new(count),
            saved: HashMap::new(),
            home: HashMap::new(),
            parked: HashSet::new(),
            enforce_attempts: HashMap::new(),
            park_request: HashMap::new(),
            last_requested: HashMap::new(),
            pending_repark: HashSet::new(),
            last_geometry: None,
            state_path: None,
            boot_time: statefile::boot_time_sec(),
            suspended: false,
            trace: Vec::new(),
        }
    }

    /// Drain the diagnostic trace. The shell calls this every snapshot; an
    /// undrained trace is capped rather than grown without bound, because a
    /// paused or observe-mode daemon never drains.
    pub fn take_trace(&mut self) -> Vec<ParkTrace> {
        std::mem::take(&mut self.trace)
    }

    fn note(&mut self, t: ParkTrace) {
        const CAP: usize = 4096;
        if self.trace.len() < CAP {
            self.trace.push(t);
        }
    }

    /// Like `new`, but promises survive restarts via `path`. A valid file
    /// makes a restart placement-invisible: nothing moved while we were dead,
    /// so reloading what the slivers MEAN is the entire job.
    pub fn with_persistence(count: u8, path: PathBuf) -> Self {
        let mut b = Self::new(count);
        b.state_path = Some(path);
        b.load_state();
        b
    }

    pub fn current(&self) -> WorkspaceId {
        self.ledger.current()
    }

    pub fn count(&self) -> u8 {
        self.ledger.count()
    }

    pub fn monitors(&self) -> VirtualMonitors {
        self.ledger.monitors()
    }

    pub fn window_ws(&self) -> BTreeMap<WindowId, WorkspaceId> {
        self.ledger.window_ws()
    }

    pub fn window_monitors(&self) -> BTreeMap<WindowId, VirtualMonitorId> {
        self.ledger.window_monitors()
    }

    /// Fold a completed window scan into the model: adopt the genuinely new,
    /// forget the provably dead, learn the display set, keep the park
    /// bookkeeping in lockstep with the ledger.
    ///
    /// Absence is not death. An empty scan looks exactly like "every window
    /// closed" when displays sleep (the weekend flatten), and a PARTIAL scan
    /// looks like one app's windows closed when that app blows the AX
    /// timeout — the mechanism behind the deterministic wrong-workspace
    /// phantom (a parked Chrome window missed ONE scan, was forgotten, and
    /// re-adopted onto the visible workspace three seconds later). A missing
    /// window is forgotten only when the window server's full list confirms
    /// it no longer exists; a failed CG read is not evidence either, and
    /// everything is kept.
    pub fn note_scan(&mut self, d: &dyn Desktop, windows: &[(WindowId, Pid)]) {
        let g = Geometry::read(d);
        let mut dirty = self.note_displays(d, &g);
        if windows.is_empty() {
            if dirty {
                self.persist();
            }
            return;
        }
        let before = self.ledger.window_claims();
        let scanned: HashSet<WindowId> = windows.iter().map(|(w, _)| *w).collect();
        let absent: Vec<WindowId> = self
            .ledger
            .window_ws()
            .keys()
            .filter(|w| !scanned.contains(w))
            .copied()
            .collect();
        if !absent.is_empty() {
            if let Some(alive) = d.existing_windows(&absent) {
                let dead: Vec<WindowId> = absent
                    .iter()
                    .filter(|w| !alive.contains(w))
                    .copied()
                    .collect();
                self.ledger.forget(&dead);
                self.drop_park_bookkeeping(&dead);
            }
        }
        // A new window is adopted onto the monitor its display stands for —
        // it is visibly there — and onto the anchor where the display stands
        // for several (collapsed) or none.
        let frames = current_frames(d);
        let proj = self.ledger.projection();
        let viewed = self.ledger.monitors().viewed;
        let adopt = |id: WindowId| {
            frames
                .get(&id)
                .and_then(|(_, f)| g.displays.iter().position(|d| d.contains(f.center())))
                .and_then(|i| proj.canonical_vm(i))
                .unwrap_or(viewed)
        };
        // A recycled id's saved frame belongs to a dead stranger; the new
        // window must not inherit a teleport to it.
        let recycled = self.ledger.note_seen(windows, adopt);
        self.drop_park_bookkeeping(&recycled);
        dirty |= self.ledger.window_claims() != before;
        dirty |= self.refresh_home(&frames, &g);
        if dirty {
            self.persist();
        }
    }

    /// Learn the display set. A change is a change to the projection, so the
    /// view is decided here — BEFORE enforcement projects anything — by one
    /// named policy: the view follows the focused window. A topology change is
    /// systemic; without this the anchor stays where it was, the window the
    /// user is typing into is parked, and the core's focus invariant then
    /// yanks focus to whatever is left. Deciding it here also spares the
    /// screen a park of the wrong set followed by its unpark a snapshot later.
    /// The park/restore writes themselves belong to `enforce_placement`, which
    /// runs only while Ordo is driving.
    fn note_displays(&mut self, d: &dyn Desktop, g: &Geometry) -> bool {
        let physical = g.physical();
        // Displays asleep are not a rig with no monitors; nothing is learned.
        if physical == 0 || physical == self.ledger.physical() {
            return false;
        }
        let m = self.ledger.monitors();
        let after = project(m.count.max(physical as u8), m.viewed, m.enabled, physical);
        if let Some(c) = d.focused_window().and_then(|w| self.ledger.claim(w)) {
            if c.ws == self.ledger.current() && !after.is_hosted(c.monitor) {
                self.ledger.set_viewed(c.monitor);
            }
        }
        let plan = self.ledger.note_displays(physical);
        let current = self.ledger.current();
        self.note(
            ParkTrace::new(WindowId(0), ParkTraceKind::View)
                .ws(current, current)
                .detail(format!(
                    "{physical} display(s); anchor {}; hiding {}, revealing {}",
                    self.ledger.monitors().viewed.0,
                    plan.park.len(),
                    plan.restore.len()
                )),
        );
        true
    }

    /// Whether every virtual monitor has a display of its own right now.
    fn full_rig(&self, g: &Geometry) -> bool {
        g.physical() >= self.ledger.monitors().count as usize
    }

    /// Remember where each on-screen window sits while the full rig is
    /// present. Returns whether anything durable changed.
    fn refresh_home(&mut self, frames: &HashMap<WindowId, (Pid, Rect)>, g: &Geometry) -> bool {
        if !self.full_rig(g) {
            return false;
        }
        let proj = self.ledger.projection();
        let mut changed = false;
        for (w, (_, f)) in frames {
            if !self.ledger.visible(*w, &proj)
                || self.parked.contains(w)
                || self.reads_parked(*w, f, g)
            {
                continue;
            }
            if self
                .home
                .get(w)
                .is_none_or(|h| !h.approx_eq(f, FRAME_EPSILON))
            {
                self.home.insert(*w, *f);
                changed = true;
            }
        }
        changed
    }

    fn drop_park_bookkeeping(&mut self, ids: &[WindowId]) {
        for id in ids {
            self.saved.remove(id);
            self.home.remove(id);
            self.parked.remove(id);
            self.enforce_attempts.remove(id);
            self.park_request.remove(id);
            self.last_requested.remove(id);
            self.pending_repark.remove(id);
        }
    }

    /// Every write funnels through here so the model remembers what it asked
    /// for — the only way to later tell "our write landing" apart from "an app
    /// fighting back".
    ///
    /// Callers compute a whole target `Rect` because that is what the promise
    /// and the trace are about, but only its ORIGIN is sent: this model moves
    /// windows, it never resizes them ([`Desktop::move_windows`]).
    fn move_windows(&mut self, d: &dyn Desktop, writes: &[(Pid, WindowId, Rect)]) {
        for (_, w, f) in writes {
            self.last_requested.insert(*w, *f);
        }
        let moves: Vec<(Pid, WindowId, Point)> = writes
            .iter()
            .map(|(pid, w, f)| (*pid, *w, Point { x: f.x, y: f.y }))
            .collect();
        d.move_windows(&moves);
    }

    /// Carry out a plan: park what left the screen, restore what entered it.
    /// Frames first, then visibility — a window must already be at the corner
    /// before its app is un-hidden, or the unhide reveals it where it still
    /// stands. Whether that ordering is SUFFICIENT is the open question: it
    /// cannot stop an app from restoring its own geometry in response to
    /// being un-hidden, which is what the trace is here to show.
    ///
    /// Stacking is NOT this backend's problem: the core follows every switch
    /// or view with a RestackWindows effect derived from the MRU history,
    /// which the effector reasserts after this returns.
    fn apply_plan(&mut self, d: &dyn Desktop, plan: SwitchPlan, boundary: ParkTrace) {
        if plan.is_empty() {
            self.persist(); // the declaration may still have changed
            return;
        }
        let frames = current_frames(d);
        let g = Geometry::read(d);
        self.note(boundary.detail(format!(
            "parking {}, restoring {}",
            plan.park.len(),
            plan.restore.len()
        )));
        let mut writes = Vec::new();
        for w in plan.park {
            writes.extend(self.park(w, None, &frames, &g));
        }
        for w in plan.restore {
            writes.extend(self.restore(w, &frames, &g));
        }
        self.persist();
        self.move_windows(d, &writes);
        self.apply_app_visibility(d, &frames, &g);
    }

    pub fn switch_workspace(&mut self, d: &dyn Desktop, target: WorkspaceId) {
        // Before the ledger moves: afterwards `current` IS the target.
        let from = self.ledger.current();
        let plan = self.ledger.switch(target);
        let boundary = ParkTrace::new(WindowId(0), ParkTraceKind::Switch).ws(from, target);
        self.apply_plan(d, plan, boundary);
    }

    pub fn view_monitor(
        &mut self,
        d: &dyn Desktop,
        target: VirtualMonitorId,
    ) -> Result<(), MonitorOutOfRange> {
        let plan = self
            .ledger
            .view_monitor(target)
            .ok_or(MonitorOutOfRange(target))?;
        let current = self.ledger.current();
        let boundary = ParkTrace::new(WindowId(0), ParkTraceKind::View)
            .ws(current, current)
            .detail(format!("anchor {}", target.0));
        self.apply_plan(d, plan, boundary);
        Ok(())
    }

    pub fn set_virtual_monitors(&mut self, d: &dyn Desktop, enabled: bool) {
        let plan = self.ledger.set_enabled(enabled);
        let current = self.ledger.current();
        let boundary = ParkTrace::new(WindowId(0), ParkTraceKind::View)
            .ws(current, current)
            .detail(if enabled {
                "virtualization on"
            } else {
                "virtualization off"
            });
        self.apply_plan(d, plan, boundary);
    }

    pub fn move_window_to_workspace(
        &mut self,
        d: &dyn Desktop,
        window: WindowId,
        target: WorkspaceId,
    ) -> Result<(), WorkspaceOutOfRange> {
        let mut plan = self
            .ledger
            .assign_window(window, target)
            .ok_or(WorkspaceOutOfRange(target))?;
        // A move onto the visible workspace of a window already standing there
        // still gets its restore attempt: the plan sees no change, but a
        // window physically at the corner (a promise from disk, a model gap)
        // needs bringing back, and `restore` is a no-op for anything else.
        if plan.is_empty()
            && self.ledger.visible(window, &self.ledger.projection())
            && !plan.restore.contains(&window)
        {
            plan.restore.push(window);
        }
        let current = self.ledger.current();
        let boundary = ParkTrace::new(window, ParkTraceKind::Switch)
            .ws(target, current)
            .detail("window moved to workspace");
        self.apply_plan(d, plan, boundary);
        Ok(())
    }

    /// Rewrite the window's declaration and nothing else — no frame write, no
    /// park bookkeeping. The carry path: the window is visible and stays
    /// exactly where it is; only which workspace it belongs to changes. (The
    /// full move used to park it and the following switch immediately
    /// restored it — two frame writes racing on the app's schedule.)
    pub fn assign_window_to_workspace(
        &mut self,
        window: WindowId,
        target: WorkspaceId,
    ) -> Result<(), WorkspaceOutOfRange> {
        if self.ledger.assign_window(window, target).is_none() {
            return Err(WorkspaceOutOfRange(target));
        }
        // A promise about a window now declared onto the visible workspace is
        // void (nothing should re-park it); toward a hidden workspace, any
        // needed parking is enforcement's job and keeps its bookkeeping.
        if target == self.ledger.current() {
            self.drop_park_bookkeeping(&[window]);
        }
        self.persist();
        Ok(())
    }

    /// The monitor twin of `assign_window_to_workspace`: declaration only.
    /// The core moves the frame itself when the target monitor's display
    /// differs, and views the target first when it is hidden — so the window
    /// is never parked by this call, and any parking a stray declaration does
    /// imply is enforcement's to assert.
    pub fn assign_window_to_monitor(
        &mut self,
        window: WindowId,
        target: VirtualMonitorId,
    ) -> Result<(), MonitorOutOfRange> {
        if self.ledger.assign_monitor(window, target).is_none() {
            return Err(MonitorOutOfRange(target));
        }
        self.persist();
        Ok(())
    }

    pub fn bring_up(&mut self, use_state: bool) {
        if use_state {
            // O: reload the file. With write-through active this is the model
            // we already have; after a fresh session it's the restore.
            self.load_state();
            self.suspended = false;
        } else {
            // R: blank model on the same workspace ordinal and layout, file
            // untouched and unused. Parked slivers keep sitting where they
            // are — O can always bring their meaning back from the file.
            self.ledger = Ledger::restore(
                self.ledger.count(),
                self.ledger.current(),
                self.ledger.monitors(),
                self.ledger.physical(),
                BTreeMap::new(),
            );
            self.saved.clear();
            self.home.clear();
            self.parked.clear();
            self.enforce_attempts.clear();
            self.park_request.clear();
            self.last_requested.clear();
            self.pending_repark.clear();
            self.suspended = true;
        }
    }

    pub fn resume_persistence(&mut self, d: &dyn Desktop) {
        // S after R must not blind-overwrite: the fresh session's model holds
        // only what it saw and adopted, while the file still carries the
        // pre-R promises for everything parked on hidden workspaces.
        // Persisting the blank model verbatim would destroy declarations this
        // command has never seen. Merge instead — the model wins for windows
        // the user demonstrably placed; the file wins for windows still
        // physically sitting at the park corner (their "current workspace"
        // claim is R-mode adoption noise, not intent).
        if self.suspended {
            let file = self
                .state_path
                .as_ref()
                .and_then(|p| statefile::load(p, self.boot_time));
            if let Some(ps) = file {
                let frames = current_frames(d);
                let g = Geometry::read(d);
                let merged =
                    merge_fresh_session(&self.ledger.window_claims(), &self.saved, &ps, |id| {
                        frames
                            .get(id)
                            .is_some_and(|(_, f)| self.reads_parked(*id, f, &g))
                    });
                self.ledger = Ledger::restore(
                    self.ledger.count(),
                    self.ledger.current(),
                    self.ledger.monitors(),
                    self.ledger.physical(),
                    merged.assign,
                );
                // Cheap assertion of the parked ⊆ assigned invariant (restore
                // clamps rather than drops, so this always holds today — a
                // parked entry with no assignment would arm enforcement
                // against a workspace-less window).
                let assigned = self.ledger.window_ws();
                for (id, f) in merged.saved {
                    if assigned.contains_key(&id) {
                        self.saved.insert(id, f);
                        self.parked.insert(id);
                    }
                }
                for (id, f) in merged.home {
                    if assigned.contains_key(&id) {
                        self.home.insert(id, f);
                    }
                }
            }
        }
        self.suspended = false;
        self.persist();
    }

    /// Substitute the promise for the mechanism: a window observed at the
    /// park corner with a live restore promise is REALLY at its saved frame —
    /// the sliver is this backend's own artifact, and letting it into belief
    /// poisoned everything downstream (mouse warps aimed at the corner, MRU
    /// monitor scoping, our own park/restore writes logged as external
    /// changes). Covers restore lag too: `saved` outlives the parked flag, so
    /// a just-restored window reads as its real frame while the write is
    /// still in flight.
    pub fn believed_frames(
        &self,
        d: &dyn Desktop,
        frames: &HashMap<WindowId, (Pid, Rect)>,
    ) -> HashMap<WindowId, Rect> {
        if self.saved.is_empty() {
            return HashMap::new();
        }
        let g = Geometry::read(d);
        frames
            .iter()
            .filter_map(|(w, (_, f))| {
                let saved = self.saved.get(w)?;
                self.reads_parked(*w, f, &g).then_some((*w, *saved))
            })
            .collect()
    }

    /// Is this window's observed frame the park frame Ordo asked it to
    /// occupy? Exactly it — the park write moves only x and macOS never
    /// clamps x, so a compliant window is AT its request, not near it. The
    /// request stays the anchor rather than a computed corner: a window's
    /// park frame depends on its own width and y, and predicting where a
    /// window "should" be parked once misread deep landings as escapes.
    fn at_park(&self, w: WindowId, f: &Rect) -> bool {
        self.park_request
            .get(&w)
            .is_some_and(|req| same_position(f, req))
    }

    /// `at_park`, plus the geometric fallback for frames with no request to
    /// anchor on (a restarted daemon, an R-mode blank, promises from disk).
    fn reads_parked(&self, w: WindowId, f: &Rect, g: &Geometry) -> bool {
        self.at_park(w, f) || in_park_corner(f, g)
    }

    /// The display the window's monitor is projected onto right now.
    fn host_rect(&self, window: WindowId, g: &Geometry) -> Option<Rect> {
        let claim = self.ledger.claim(window)?;
        let i = self.ledger.projection().host(claim.monitor)?;
        g.displays.get(i).copied()
    }

    /// Assert the declarations: every window that is not on screen must sit
    /// at the park corner, and every window that IS on screen must not be
    /// bookkept parked. Iterates the LEDGER, not the parked set — a window
    /// whose park write never happened (its frame was unreadable at park
    /// time) is still declared hidden, and a parked-set walk was blind to it
    /// forever. The second half is what a display change costs: the
    /// projection moved under the ledger (learned in `note_scan`), and the
    /// windows whose monitor just regained a display are restored onto it —
    /// monitor memory, on the rescan that follows the plug event.
    ///
    /// Enforcement asserts declarations by MOVING windows; it never writes a
    /// declaration. Violations are classified by frame before they cost
    /// budget:
    /// - at the park frame we last requested: compliant. An app that re-homes
    ///   the window on un-hide is not at it, and is corrected, not tolerated.
    /// - at its own restore promise, or at any frame Ordo itself last wrote
    ///   (position match; size is the window's own): OUR write landed late
    ///   (or the app re-applied its autosaved frame — same response).
    ///   Re-park without counting — and without re-issuing while the last
    ///   re-park is still unconfirmed: our own writes are not an app
    ///   fighting back, and counting them drained the budget until damping
    ///   surrendered to them (run 41's enforcement war), while blindly
    ///   re-writing every pass was a self-sustaining write loop against a
    ///   window that never moved.
    /// - anywhere else: a foreign write; count it. At the limit the episode
    ///   ends in a STANDOFF, loudly: no further writes, and the declaration
    ///   stays. The window sits visibly misplaced until the user's next
    ///   switch or command resolves it — the misplacement is obvious and
    ///   self-heals; a declaration rewritten from the screen is a silent,
    ///   permanent loss of where the user filed the window.
    ///
    /// A display change is a systemic event: the park corner moves, so every
    /// parked window reads as in violation at once for a reason that has
    /// nothing to do with opposition. That pass re-asserts without counting
    /// and clears every budget.
    ///
    /// `frames` arrives from the caller rather than `d.windows()` because the
    /// shell's enumerator has always just scanned when this runs — no backend
    /// re-enumerates on its own.
    pub fn enforce_placement(&mut self, d: &dyn Desktop, frames: &HashMap<WindowId, (Pid, Rect)>) {
        let g = Geometry::read(d);
        // Displays asleep: the projection would host nothing and every window
        // would read as hidden. Not a world to assert anything against.
        if g.displays.is_empty() {
            return;
        }
        let current = self.ledger.current();
        let proj = self.ledger.projection();
        let claims = self.ledger.window_claims();
        let hidden: Vec<WindowId> = claims
            .keys()
            .filter(|w| !self.ledger.visible(**w, &proj))
            .copied()
            .collect();
        let stranded: Vec<WindowId> = claims
            .keys()
            .filter(|w| self.ledger.visible(**w, &proj) && self.parked.contains(w))
            .copied()
            .collect();
        // This runs on every snapshot; don't pay for a quiet desktop.
        if hidden.is_empty() && stranded.is_empty() {
            return;
        }
        let systemic = self.last_geometry.as_ref().is_some_and(|prev| *prev != g);
        self.last_geometry = Some(g.clone());
        if systemic {
            self.enforce_attempts.clear();
        }
        let mut writes = Vec::new();
        let mut newly_parked = false;
        for w in hidden {
            let Some((pid, f)) = frames.get(&w) else {
                continue;
            };
            if self.at_park(w, f) {
                self.enforce_attempts.remove(&w);
                self.pending_repark.remove(&w);
                continue;
            }
            let own_write = self.saved.get(&w).is_some_and(|s| same_position(f, s))
                || self
                    .last_requested
                    .get(&w)
                    .is_some_and(|r| near_own_request(f, r));
            if own_write || systemic {
                // Forgiveness is the decision that hid an app refusing to stay
                // parked: it looks identical to our own write still landing,
                // and it was previously silent, so the window was excused on
                // every pass forever with nothing written down.
                let withheld = !systemic && self.pending_repark.contains(&w);
                let t = ParkTrace::new(w, ParkTraceKind::Suppressed)
                    .observed(*f)
                    .ws(
                        *self.ledger.window_ws().get(&w).unwrap_or(&current),
                        current,
                    )
                    .at_park(false)
                    .attempt(self.enforce_attempts.get(&w).copied().unwrap_or(0))
                    .detail(if systemic {
                        "display set changed; whole pass uncounted"
                    } else if withheld {
                        "our own write; re-park already in flight, none issued"
                    } else {
                        "frame matches our own write; re-park uncounted"
                    });
                self.note(t);
                if withheld {
                    continue;
                }
            } else {
                // A freshly issued park looks like a phantom until the app
                // applies it, so the first "attempt" is usually just that
                // write landing; the budget exists for the window that never
                // complies.
                let n = self.enforce_attempts.entry(w).or_insert(0);
                if *n >= ENFORCE_LIMIT {
                    // Stand down, once and loudly. The bookkeeping stays
                    // armed: if our last write lands after all, the window
                    // reads compliant and the episode clears; otherwise the
                    // user's next switch restores or re-parks it cleanly.
                    if *n == ENFORCE_LIMIT {
                        *n += 1;
                        eprintln!(
                            "ordo: window {} keeps escaping its park; standing down — \
                             it stays declared on workspace {}",
                            w.0,
                            self.ledger.window_ws().get(&w).unwrap_or(&current).0
                        );
                        let t = ParkTrace::new(w, ParkTraceKind::Standoff)
                            .observed(*f)
                            .ws(
                                *self.ledger.window_ws().get(&w).unwrap_or(&current),
                                current,
                            )
                            .attempt(ENFORCE_LIMIT)
                            .detail("write limit reached; declaration kept, writes stopped");
                        self.note(t);
                    }
                    continue;
                }
                *n += 1;
                if *n > 1 {
                    eprintln!("ordo: re-parking phantom window {} (attempt {n})", w.0);
                }
            }
            // Re-assert. A bookkept parked window keeps its promise and just
            // gets the corner write again; one that was never parked (the
            // blind spot) parks properly, capturing its promise on the way.
            if self.parked.contains(&w) {
                let want = park_frame(*f, &g);
                let t = ParkTrace::new(w, ParkTraceKind::Reassert)
                    .observed(*f)
                    .requested(want)
                    .ws(
                        *self.ledger.window_ws().get(&w).unwrap_or(&current),
                        current,
                    )
                    .at_park(false)
                    .attempt(self.enforce_attempts.get(&w).copied().unwrap_or(0));
                self.note(t);
                self.park_request.insert(w, want);
                if own_write && !systemic {
                    self.pending_repark.insert(w);
                }
                writes.push((*pid, w, want));
            } else {
                let write = self.park(w, self.enforce_attempts.get(&w).copied(), frames, &g);
                newly_parked |= write.is_some();
                writes.extend(write);
            }
        }
        let mut restored = false;
        for w in stranded {
            if let Some(write) = self.restore(w, frames, &g) {
                writes.push(write);
                restored = true;
            }
        }
        // A blind-spot park or a restore changed durable promises — and what
        // is on screen, so the Dock follows, as after a switch.
        if newly_parked || restored {
            self.persist();
        }
        self.move_windows(d, &writes);
        if newly_parked || restored {
            self.apply_app_visibility(d, frames, &g);
        }
    }

    pub fn rescue_window(&mut self, d: &dyn Desktop, window: WindowId) {
        // Claim it for the visible workspace and the anchor monitor, and bring
        // it back on-screen. No visibility pass here: rescue must only ever
        // reveal, and the gather already unhides every app up front.
        let frames = current_frames(d);
        let viewed = self.ledger.monitors().viewed;
        self.ledger.assign_window(window, self.ledger.current());
        self.ledger.assign_monitor(window, viewed);
        // An empty hold: rescue reveals everything, deliberately.
        d.show_apps(&[Unhide {
            pid: frames.get(&window).map(|(p, _)| *p).unwrap_or(Pid(0)),
            hold: Vec::new(),
        }]);
        let write = self.restore(window, &frames, &Geometry::read(d));
        self.persist();
        self.move_windows(d, write.as_slice());
    }

    /// Replace the in-memory model with the state file's, when it validates.
    fn load_state(&mut self) {
        let Some(path) = &self.state_path else { return };
        let Some(ps) = statefile::load(path, self.boot_time) else {
            return;
        };
        let count = self.ledger.count();
        let claims: BTreeMap<WindowId, Claim> = ps
            .windows
            .iter()
            .map(|w| {
                (
                    w.id,
                    Claim {
                        ws: w.workspace,
                        monitor: w.monitor,
                        owner: w.owner,
                    },
                )
            })
            .collect();
        // A pre-monitor file has no count; the displays seen so far stand in.
        let known = self.ledger.monitors();
        let monitors = VirtualMonitors {
            count: if ps.virtual_monitor_count == 0 {
                known.count
            } else {
                ps.virtual_monitor_count
            },
            viewed: ps.viewed,
            enabled: ps.virtual_monitors_enabled,
        };
        self.ledger = Ledger::restore(count, ps.current, monitors, self.ledger.physical(), claims);
        self.saved.clear();
        self.home.clear();
        self.parked.clear();
        self.enforce_attempts.clear();
        self.park_request.clear();
        self.last_requested.clear();
        self.pending_repark.clear();
        for w in &ps.windows {
            if let Some(f) = w.saved {
                self.saved.insert(w.id, f);
                self.parked.insert(w.id);
            }
            if let Some(f) = w.home {
                self.home.insert(w.id, f);
            }
        }
    }

    fn persist(&self) {
        if self.suspended {
            return;
        }
        let Some(path) = &self.state_path else { return };
        let windows = self
            .ledger
            .window_claims()
            .into_iter()
            .map(|(id, claim)| PersistedWindow {
                id,
                workspace: claim.ws,
                monitor: claim.monitor,
                owner: claim.owner,
                saved: self
                    .parked
                    .contains(&id)
                    .then(|| self.saved.get(&id).copied())
                    .flatten(),
                home: self.home.get(&id).copied(),
            })
            .collect();
        let m = self.ledger.monitors();
        statefile::save(
            path,
            &PersistedState {
                version: statefile::VERSION,
                boot_time_sec: self.boot_time,
                current: self.ledger.current(),
                viewed: m.viewed,
                virtual_monitors_enabled: m.enabled,
                virtual_monitor_count: m.count,
                windows,
            },
        );
    }

    /// Bookkeep a park and return the frame write it requires, so a switch can
    /// batch every write into one parallel pass instead of moving windows one
    /// by one (which made multi-monitor switches visibly ripple).
    /// `attempt`: enforcement's charge count for this window, when the caller
    /// is enforcement. Carried only so the trace can show the countdown toward
    /// the limit on this path too — a switch's park has no such notion.
    fn park(
        &mut self,
        window: WindowId,
        attempt: Option<u8>,
        frames: &HashMap<WindowId, (Pid, Rect)>,
        g: &Geometry,
    ) -> Option<(Pid, WindowId, Rect)> {
        // Save the real frame only on the transition onto-screen -> parked; a
        // window already parked keeps its original saved frame rather than
        // recording the sliver position.
        if self.parked.contains(&window) {
            return None;
        }
        let (pid, f) = frames.get(&window)?;
        // Never canonicalize a sliver as a window's real frame. Gaps in the
        // model (a partial scan dropping the ledger entry, R-mode
        // re-adoption at birth) can hand this path a window that is already
        // physically parked; capturing that frame would turn a transient
        // wrong belief into a durable one — the window's recorded "real
        // position" becomes the corner artifact, and persist() writes it to
        // disk. A missing promise is recoverable (rescue); a lying one is
        // not.
        if self.reads_parked(window, f, g) {
            self.parked.insert(window);
            self.enforce_attempts.remove(&window);
            self.pending_repark.remove(&window);
            let mut t = ParkTrace::new(window, ParkTraceKind::Park)
                .observed(*f)
                .at_park(true)
                .detail("already at the corner; promise left untouched");
            if let Some(n) = attempt {
                t = t.attempt(n);
            }
            self.note(t);
            return None;
        }
        let (pid, f) = (*pid, *f);
        self.saved.insert(window, f);
        if self.full_rig(g) {
            self.home.insert(window, f);
        }
        self.parked.insert(window);
        self.enforce_attempts.remove(&window);
        self.pending_repark.remove(&window);
        let want = park_frame(f, g);
        self.park_request.insert(window, want);
        let mut t = ParkTrace::new(window, ParkTraceKind::Park)
            .observed(f)
            .requested(want)
            .at_park(false);
        if let Some(n) = attempt {
            t = t.attempt(n);
        }
        self.note(t);
        Some((pid, window, want))
    }

    /// Send a parked window back to its promise — its POSITION, and only that,
    /// even when the promise's size and the window's differ — on the display
    /// its monitor is projected onto NOW.
    ///
    /// The size gap is real: a window ratcheted short by an older build, or
    /// one an app resized while it sat parked, comes back at the size it has
    /// now and not the one the promise records. Re-imposing the promised size
    /// would put the capped write back on the path where it is likeliest to
    /// be wrong — a size that differs is, by construction, one the
    /// WindowServer or the app has already refused once, and asking again
    /// risks the same cap plus the y-hoist that comes with it. Worse, the
    /// model cannot tell a height Ordo stole from a height the user or the app
    /// deliberately changed, so re-asserting it would override a live intent
    /// with a stale observation. The promise stays whole for the core's
    /// benefit; the window keeps its own size, and the next park records what
    /// the window really is.
    ///
    /// The display gap is the rig changing while the window was hidden. In
    /// order of preference: the promise itself, when it already lies on the
    /// host; the window's `home` frame, when the host is the display it was
    /// docked on (the rig came back — land exactly where it was); the promise
    /// carried over proportionally from the display it was made on; and, when
    /// that display is gone too, the promise clamped into the host. A carried
    /// or clamped target replaces the promise, so what the core is told and
    /// what the write asks for stay one frame.
    fn restore(
        &mut self,
        window: WindowId,
        frames: &HashMap<WindowId, (Pid, Rect)>,
        g: &Geometry,
    ) -> Option<(Pid, WindowId, Rect)> {
        let was_parked = self.parked.remove(&window);
        self.enforce_attempts.remove(&window);
        self.pending_repark.remove(&window);
        let (pid, f) = frames.get(&window)?;
        // Restoring is only meaningful for a window that needs it: bookkept
        // parked, or physically at the corner. A window already standing
        // visible (a carried resident, a corrective toward the current
        // workspace) must NOT be written — `saved` outlives the parked flag
        // for restore-lag substitution, and honoring that stale promise here
        // teleported carried windows back to where they used to live.
        if !was_parked && !self.reads_parked(window, f, g) {
            return None;
        }
        let (pid, f) = (*pid, *f);
        let host = self.host_rect(window, g);
        let (kind, want) = match self.saved.get(&window).copied() {
            // A promise that is itself a park artifact is not a promise. It is
            // residue from before the corner was recognizable, persisted to
            // disk, so it outlives the bug that wrote it. Honoring it re-parks
            // the window the instant its workspace comes up — the window you
            // cannot switch to. A promise has no request to anchor on (it may
            // predate this process), so this is the geometric test.
            Some(s) if in_park_corner(&s, g) => (
                ParkTraceKind::PoisonedPromise,
                rehome_into(&s, host.unwrap_or(g.main)),
            ),
            Some(s) => match host {
                Some(h) if h.contains(s.center()) => (ParkTraceKind::Restore, s),
                Some(h) => {
                    let home = self
                        .home
                        .get(&window)
                        .filter(|hm| h.contains(hm.center()));
                    let want = match (home, g.display_at(s.center())) {
                        (Some(hm), _) => Rect { x: hm.x, y: hm.y, ..s },
                        (None, Some(from)) => {
                            let t = s.translate_between(&from, &h);
                            Rect { x: t.x, y: t.y, ..s }
                        }
                        (None, None) => clamp_into(&s, &h),
                    };
                    self.saved.insert(window, want);
                    (ParkTraceKind::Rehost, want)
                }
                None => (ParkTraceKind::Restore, s),
            },
            // Parked with no promise (its real frame was never trustworthily
            // seen — see park()'s sliver guard): don't leave it a 1px sliver
            // on the now-visible workspace. Re-home it somewhere reachable;
            // the next park captures its real frame and it self-heals.
            None if self.reads_parked(window, &f, g) => (
                ParkTraceKind::Rehome,
                rehome_into(&f, host.unwrap_or(g.main)),
            ),
            None => return None,
        };
        let t = ParkTrace::new(window, kind)
            .observed(f)
            .requested(want)
            .at_park(self.reads_parked(window, &f, g));
        self.note(t);
        Some((pid, window, want))
    }

    /// Dock dimming: hide (Cmd+H-style) every app whose known windows are all
    /// off screen, unhide every app with a window here. With the Dock's
    /// `showhidden` pref, "hidden" renders as a translucent icon — the
    /// closest macOS gets to a per-workspace Dock.
    ///
    /// The app owning the focused window is never hidden: hiding the active
    /// app makes macOS fling focus somewhere arbitrary. Core-side, switches
    /// hand focus to the destination before this runs, so the exemption
    /// almost never bites; when it does, the app just stays undimmed.
    ///
    /// The un-hide carries the park origins of the app's windows declared
    /// elsewhere: revealing an app drags exactly those back on screen unless
    /// they are held (see [`Desktop::show_apps`]). Which windows and where is
    /// the model's knowledge; making it stick is the port's.
    fn apply_app_visibility(
        &mut self,
        d: &dyn Desktop,
        frames: &HashMap<WindowId, (Pid, Rect)>,
        g: &Geometry,
    ) {
        let current = self.ledger.current();
        let proj = self.ledger.projection();
        let mut here_by_app: HashMap<Pid, bool> = HashMap::new();
        for (window, (pid, _)) in frames {
            if self.ledger.claim(*window).is_some() {
                *here_by_app.entry(*pid).or_insert(false) |= self.ledger.visible(*window, &proj);
            }
        }
        let focused_app = d
            .focused_window()
            .and_then(|w| frames.get(&w).map(|(p, _)| *p));
        // Each app's windows that are NOT on screen: the ones an unhide
        // reveals along with the wanted one, so also the ones it has to be
        // told to hold — and where. The park REQUEST is the anchor when there
        // is one; without it (a promise loaded from disk, a window this
        // process never parked) the corner is recomputed, which is the same
        // answer because a park depends only on the window's width and keeps
        // its y.
        let mut elsewhere: HashMap<Pid, Vec<(WindowId, Point)>> = HashMap::new();
        for (window, (pid, f)) in frames {
            if self.ledger.claim(*window).is_some() && !self.ledger.visible(*window, &proj) {
                let want = self
                    .park_request
                    .get(window)
                    .copied()
                    .unwrap_or_else(|| park_frame(*f, g));
                elsewhere.entry(*pid).or_default().push((
                    *window,
                    Point {
                        x: want.x,
                        y: want.y,
                    },
                ));
            }
        }
        for hold in elsewhere.values_mut() {
            hold.sort_by_key(|(w, _)| w.0);
        }
        let mut shows: Vec<Unhide> = Vec::new();
        let mut notes = Vec::new();
        for (pid, has_window_here) in here_by_app {
            let hold = elsewhere.get(&pid).cloned().unwrap_or_default();
            if has_window_here {
                shows.push(Unhide { pid, hold });
            } else if Some(pid) != focused_app {
                d.hide_app(pid);
                notes.push(
                    ParkTrace::app(pid, ParkTraceKind::AppHidden)
                        .ws(current, current)
                        .detail(format!("hidden; {} window(s) parked", hold.len())),
                );
            }
        }
        // One call for every un-hide of this pass: they overlap in time rather
        // than queueing, so a switch costs the slowest app's reveal.
        let held: HashMap<Pid, HoldStat> = d
            .show_apps(&shows)
            .into_iter()
            .map(|s| (s.pid, s))
            .collect();
        for u in shows {
            let mut t = ParkTrace::app(u.pid, ParkTraceKind::AppShown)
                .ws(current, current)
                .detail(format!(
                    "unhidden; also reveals {} window(s) parked for other workspaces",
                    u.hold.len()
                ));
            if let Some(s) = held.get(&u.pid) {
                t = t.hold(s.clone());
            }
            notes.push(t);
        }
        for t in notes {
            self.note(t);
        }
    }
}

fn current_frames(d: &dyn Desktop) -> HashMap<WindowId, (Pid, Rect)> {
    d.windows()
        .into_iter()
        .map(|(id, pid, frame)| (id, (pid, frame)))
        .collect()
}

/// The park spot: the window slides LEFT until only `SLIVER` points of it
/// remain on the leftmost display. Its y is left exactly as the window had it.
///
/// Keeping the window's own y is what makes this write exact, and exactness is
/// worth more than a smaller artifact. macOS never clamps x (measured: an
/// origin 1453pt off the left edge was granted verbatim), and a y the window
/// already occupies is a y it was already granted — so the position that lands
/// is the position requested, to the point, and everything downstream can
/// compare them exactly. Parking flush to a display's BOTTOM instead bought a
/// shorter visible artifact (a 1pt x ~40pt dash rather than a 1pt column the
/// height of the window) and paid for it with the WindowServer pulling the
/// landing back up by an unpredictable, app-dependent amount.
///
/// The window's WIDTH is what the escape has to clear, so a window wider than
/// the display simply starts further left; nothing lies out there to catch it.
///
/// MIGRATION: windows persisted while parked at a retired corner read as one
/// violation on the first enforcement pass after this change and are re-parked
/// once (see `park_request` for why the same happens after any restart).
fn park_frame(f: Rect, g: &Geometry) -> Rect {
    Rect {
        x: g.park_host.x - f.w + SLIVER,
        y: f.y,
        w: f.w,
        h: f.h,
    }
}

/// Are these two frames at the same position, modulo AX's rounding? Size is
/// the window's own business on every path that asks.
fn same_position(a: &Rect, b: &Rect) -> bool {
    (a.x - b.x).abs() <= 1.0 && (a.y - b.y).abs() <= 1.0
}

/// Did one of Ordo's ON-SCREEN writes (a restore, a rehome, a rescue) land
/// where it asked, modulo the WindowServer's vertical clamp? Those writes can
/// be pushed down under the menu bar or hoisted up by a size cap, in either
/// direction and by an app-dependent amount, so y is compared with
/// [`CLAMP_SLACK`]; x still has to match, since nothing clamps horizontally.
///
/// Park writes deliberately do NOT come through here — they land exactly (see
/// [`park_frame`]) and are compared with [`same_position`].
fn near_own_request(observed: &Rect, requested: &Rect) -> bool {
    (observed.x - requested.x).abs() <= 1.0 && (observed.y - requested.y).abs() <= CLAMP_SLACK
}

/// Does this frame LOOK like a park artifact, with no request to anchor on?
/// The geometric fallback for frames whose park (if any) this process never
/// issued: promises loaded from disk, windows parked by an older Ordo, the
/// first pass after a restart, windows already slivered when an R-mode blank
/// or a model gap meets them.
///
/// The consumers of this test share an asymmetry that tolerates its
/// generosity: a false "parked" skips a promise capture or re-homes a window
/// (recoverable, self-heals on the next park); a false "not parked"
/// canonicalizes a park artifact as a window's real frame — a silent, durable
/// lie persisted to disk.
///
/// It therefore accepts the corner this Ordo parks at AND all three its
/// predecessors used, so an upgrade does not turn every already-parked window
/// into a window whose "real frame" is a sliver. The retired ones can go once
/// no live window or state file can predate this change.
fn in_park_corner(f: &Rect, g: &Geometry) -> bool {
    let near = |a: f64, b: f64| (a - b).abs() <= 1.0;
    // The current corner: the window's whole body off the left of the leftmost
    // display, at whatever y it already had — so y says nothing here, and only
    // the width-derived x does. No user placement lands there by accident.
    if near(f.x, g.park_host.x - f.w + SLIVER) {
        return true;
    }
    // The retired corners, all bottom-flush: the rightmost display's bottom
    // right (through 2026-09-02), the main display's right edge, and — briefly,
    // on 2026-09-01 — right-aligned inside the main display. y confines the
    // window's TOP edge to the bottom CLAMP_SLACK points of the display, where
    // it shows less than a title bar's worth of itself.
    let retired = [
        (
            g.legacy_host.x + g.legacy_host.w - SLIVER,
            g.legacy_host.y + g.legacy_host.h - SLIVER,
        ),
        (g.main.x + g.main.w - SLIVER, g.main.y + g.main.h - SLIVER),
        (
            (g.main.x + g.main.w - f.w).max(g.main.x),
            g.main.y + g.main.h - SLIVER,
        ),
    ];
    retired
        .iter()
        .any(|(cx, cy)| near(f.x, *cx) && f.y <= cy + 1.0 && f.y >= cy - CLAMP_SLACK)
}

/// Re-home a frame to `area`'s top-left — the no-cascade twin of the rescue
/// gather's clamp, for the lone promise-less window a restore must not leave as
/// a sliver. Reaching the window is the whole job, and its title bar at the
/// display's origin is reachable whatever its size, so an oversized window is
/// left oversized: a shrink here would be a size this model asked for and never
/// got (the write carries only the origin), i.e. a request contradicted the
/// moment it lands.
fn rehome_into(f: &Rect, area: Rect) -> Rect {
    Rect {
        x: area.x,
        y: area.y,
        ..*f
    }
}

/// Slide a frame the least distance that puts its origin inside `area`, size
/// untouched — for a promise made on a display that no longer exists, where
/// there is nothing to carry it over from.
fn clamp_into(f: &Rect, area: &Rect) -> Rect {
    Rect {
        x: f.x.clamp(area.x, (area.x + area.w - f.w).max(area.x)),
        y: f.y.clamp(area.y, (area.y + area.h - f.h).max(area.y)),
        ..*f
    }
}

/// The merged model an S-after-R resume adopts.
struct FreshMerge {
    assign: BTreeMap<WindowId, Claim>,
    /// Restore promises revived from the file (window is parked again).
    saved: Vec<(WindowId, Rect)>,
    /// Docked frames revived from the file.
    home: Vec<(WindowId, Rect)>,
}

/// Merge a fresh (R-mode) session's model with the pre-R state file. The file
/// wins for windows the model never met, and for windows the model adopted but
/// that are still physically slivers — everything else is the user's live
/// arrangement and the model wins.
///
/// `own_saved` is the fresh session's own capture set: park()'s sliver guard
/// guarantees an entry there means THIS session saw the window at a real frame
/// and deliberately parked it — that sliver is the user's arrangement, not
/// adoption noise, however it reads physically.
fn merge_fresh_session(
    model: &BTreeMap<WindowId, Claim>,
    own_saved: &HashMap<WindowId, Rect>,
    file: &PersistedState,
    is_slivered: impl Fn(&WindowId) -> bool,
) -> FreshMerge {
    let mut assign = model.clone();
    let mut saved = Vec::new();
    let mut home = Vec::new();
    for w in &file.windows {
        let model_knows = model.contains_key(&w.id);
        let noise_sliver =
            w.saved.is_some() && is_slivered(&w.id) && !own_saved.contains_key(&w.id);
        if !model_knows || noise_sliver {
            assign.insert(
                w.id,
                Claim {
                    ws: w.workspace,
                    monitor: w.monitor,
                    owner: w.owner,
                },
            );
            if let Some(f) = w.saved {
                saved.push((w.id, f));
            }
            if let Some(f) = w.home {
                home.push((w.id, f));
            }
        }
    }
    FreshMerge {
        assign,
        saved,
        home,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(n: u32) -> WindowId {
        WindowId(n)
    }
    fn ws(n: u8) -> WorkspaceId {
        WorkspaceId(n)
    }
    fn rect(x: f64, y: f64) -> Rect {
        Rect {
            x,
            y,
            w: 800.0,
            h: 600.0,
        }
    }
    const MAIN: Rect = Rect {
        x: 0.0,
        y: 0.0,
        w: 1920.0,
        h: 1080.0,
    };

    /// A SECOND display, to the right and shorter — Michael's actual rig. The
    /// fake modelled one display for a long time, which is precisely why it
    /// could not see that parking escaped the main display's right edge
    /// straight onto another screen.
    const SECOND: Rect = Rect {
        x: 1920.0,
        y: 66.0,
        w: 1470.0,
        h: 956.0,
    };

    fn geo() -> Geometry {
        Geometry {
            main: MAIN,
            park_host: MAIN,
            legacy_host: SECOND,
            displays: vec![MAIN, SECOND],
        }
    }

    fn vm(n: u8) -> VirtualMonitorId {
        VirtualMonitorId(n)
    }

    /// What each display keeps for itself at the top, measured on Michael's
    /// rig: main hands a window at most 1050 of its 1080, the second 923 of
    /// its 956. This is the whole difference between a park that hides a
    /// window and a park that files 127pt off it.
    fn usable_inset(d: Rect) -> f64 {
        if d == MAIN {
            30.0
        } else {
            33.0
        }
    }

    /// AppKit's `constrainFrameRect:toScreen:`, as an un-hidden app's windows
    /// meet it on the way back in: the window is pushed until its body sits
    /// inside the display owning its origin (an origin off to the left of
    /// every display is main's, same rule as `write_whole_frame`). A parked
    /// window's whole body is off that edge, so this is what hauls it back.
    ///
    /// Only the HORIZONTAL push is modelled, because only it was measured. The
    /// vertical rule this fake already has — a landing may be pulled DOWN so a
    /// title bar stays reachable, never hoisted to make a tall window fit —
    /// comes from `land`, which every write goes through.
    fn constrain_to_screen(displays: &[Rect], f: Rect) -> Rect {
        let owner = displays
            .iter()
            .copied()
            .find(|d| f.x >= d.x && f.x < d.x + d.w && f.y >= d.y && f.y < d.y + d.h)
            .unwrap_or(MAIN);
        Rect {
            x: f.x.clamp(owner.x, (owner.x + owner.w - f.w).max(owner.x)),
            ..f
        }
    }

    /// The widest strip of `f` any display still shows. The park hides a
    /// window horizontally, so this is the axis the invariant lives on.
    fn visible_width(f: &Rect) -> f64 {
        [MAIN, SECOND]
            .iter()
            .map(|d| ((f.x + f.w).min(d.x + d.w) - f.x.max(d.x)).max(0.0))
            .fold(0.0, f64::max)
    }

    /// A desktop where moves land instantly — enough to drive the whole backend
    /// through the port without a real window. `freeze()` makes later writes
    /// vanish, simulating an app that hasn't applied them yet.
    struct FakeDesktop {
        windows: std::cell::RefCell<BTreeMap<WindowId, (Pid, Rect)>>,
        frozen: std::cell::Cell<bool>,
        cg_down: std::cell::Cell<bool>,
        /// How far this WindowServer pulls a bottom-clamped write back up.
        /// Defaults to a title bar; tests raise it to the deepest landing
        /// measured in production (124pt), which no constant-box predicate
        /// survived.
        pull: std::cell::Cell<f64>,
        /// Apps hidden the Cmd+H way. Their windows stay in `windows`, because
        /// an AX scan really does still see a hidden app's windows — that is
        /// what lets the model restore them later, and why the port's death
        /// evidence has to come from the window server instead. What hiding
        /// changes is what happens on the way BACK: see `show_apps`.
        hidden: std::cell::RefCell<HashSet<Pid>>,
        /// The rig. Starts as Michael's two displays; `set_displays` unplugs
        /// or replugs, re-homing as macOS does.
        displays: std::cell::RefCell<Vec<Rect>>,
        focused: std::cell::Cell<Option<WindowId>>,
    }

    impl FakeDesktop {
        fn new(windows: &[(WindowId, Pid, Rect)]) -> Self {
            FakeDesktop {
                windows: std::cell::RefCell::new(
                    windows.iter().map(|(w, p, f)| (*w, (*p, *f))).collect(),
                ),
                frozen: std::cell::Cell::new(false),
                cg_down: std::cell::Cell::new(false),
                pull: std::cell::Cell::new(28.0),
                hidden: std::cell::RefCell::new(HashSet::new()),
                displays: std::cell::RefCell::new(vec![MAIN, SECOND]),
                focused: std::cell::Cell::new(None),
            }
        }

        /// The display set changes. macOS moves every window whose display
        /// vanished onto the remaining one before anybody is told, keeping its
        /// offset from the display's origin — the fact that makes `saved`
        /// useless for coming home, and `home` necessary.
        fn set_displays(&self, ds: &[Rect]) {
            let old = self.displays.replace(ds.to_vec());
            let main = ds.first().copied().unwrap_or(MAIN);
            let mut ws = self.windows.borrow_mut();
            for (_, f) in ws.values_mut() {
                let c = f.center();
                if ds.iter().any(|d| d.contains(c)) {
                    continue;
                }
                let from = old.iter().copied().find(|d| d.contains(c)).unwrap_or(MAIN);
                let moved = Rect {
                    x: main.x + (f.x - from.x),
                    y: main.y + (f.y - from.y),
                    ..*f
                };
                *f = constrain_to_screen(ds, moved);
            }
        }

        /// A write that DOES carry AXSize, as `ax::set_frame` still issues for
        /// a cross-display move. Not reachable through the `Desktop` port any
        /// more — it is modelled so the tests can still show what the port's
        /// position-only write buys: the WindowServer caps the height to what
        /// the display owning the ORIGIN can hold (an origin off to the left of
        /// every display is main's), and a request that does not fit also loses
        /// its y to that display's top inset.
        fn write_whole_frame(&self, pid: Pid, w: WindowId, f: Rect) {
            let owner = self
                .displays
                .borrow()
                .iter()
                .copied()
                .find(|d| f.x >= d.x && f.x < d.x + d.w && f.y >= d.y && f.y < d.y + d.h)
                .unwrap_or(MAIN);
            let inset = usable_inset(owner);
            let mut landed = f;
            if landed.h > owner.h - inset {
                landed.h = owner.h - inset;
                landed.y = owner.y + inset;
            }
            self.land(pid, w, landed);
        }

        /// The last thing every write goes through: macOS keeps the title bar
        /// reachable on whichever display the window sits over, so the floor
        /// differs per display — the fact that made one park request land at
        /// three heights.
        fn land(&self, pid: Pid, w: WindowId, mut landed: Rect) {
            let host = self
                .displays
                .borrow()
                .iter()
                .copied()
                .filter(|d| landed.x < d.x + d.w && landed.x + landed.w > d.x)
                .min_by(|a, b| (landed.x - a.x).abs().total_cmp(&(landed.x - b.x).abs()))
                .unwrap_or(MAIN);
            landed.y = landed.y.min(host.y + host.h - self.pull.get());
            self.windows.borrow_mut().insert(w, (pid, landed));
        }

        fn frame(&self, w: WindowId) -> Rect {
            self.windows.borrow()[&w].1
        }

        fn is_hidden(&self, pid: Pid) -> bool {
            self.hidden.borrow().contains(&pid)
        }

        fn freeze(&self) {
            self.frozen.set(true);
        }

        fn thaw(&self) {
            self.frozen.set(false);
        }

        /// The window really closes: gone from the window server too.
        fn close(&self, w: WindowId) {
            self.windows.borrow_mut().remove(&w);
        }

        /// An external hand (the app, the user) moves the window.
        fn place(&self, w: WindowId, f: Rect) {
            let mut ws = self.windows.borrow_mut();
            let pid = ws[&w].0;
            ws.insert(w, (pid, f));
        }

        /// A scan of this desktop, as the shell would deliver it.
        fn scan(&self) -> Vec<(WindowId, Pid)> {
            self.windows
                .borrow()
                .iter()
                .map(|(w, (p, _))| (*w, *p))
                .collect()
        }
    }

    impl Desktop for FakeDesktop {
        fn windows(&self) -> Vec<(WindowId, Pid, Rect)> {
            self.windows
                .borrow()
                .iter()
                .map(|(w, (p, f))| (*w, *p, *f))
                .collect()
        }

        /// Moves land CLAMPED, as the real WindowServer lands them: a fake that
        /// stores positions verbatim is a fake that cannot reproduce parking,
        /// which is how an exact-point park predicate passed every test here
        /// and fought every window in production.
        ///
        /// The window's SIZE is untouched, measured to be true at every origin
        /// tried — which is the whole reason the port carries no size. What a
        /// size WOULD suffer is modelled in `write_whole_frame`; the ratchet
        /// that shrank the author's windows by 58pt lived in the gap between
        /// the two.
        fn move_windows(&self, moves: &[(Pid, WindowId, Point)]) {
            if self.frozen.get() {
                return;
            }
            for (pid, w, at) in moves {
                let size_is_the_windows_own = self.windows.borrow()[w].1;
                self.land(
                    *pid,
                    *w,
                    Rect {
                        x: at.x,
                        y: at.y,
                        ..size_is_the_windows_own
                    },
                );
            }
        }

        fn hide_app(&self, pid: Pid) {
            self.hidden.borrow_mut().insert(pid);
        }

        /// The un-hide, modelled as the measurements found it — because the
        /// defect lives entirely in what a no-op `set_app_hidden` did not say.
        ///
        /// A genuinely hidden app orders its windows back in, and AppKit runs
        /// `constrainFrameRect:toScreen:` over each one on the way, dragging a
        /// window parked off the left edge fully onto the display owning its
        /// origin. Everything written BEFORE that moment — the switch's own
        /// park writes, or a blind re-park issued in the same breath as the
        /// un-hide — is what the constrain undoes; only the positions carried
        /// through this call survive, which is why they are a parameter and
        /// not a follow-up `move_windows`.
        ///
        /// Un-hiding an app that was never hidden orders nothing in and moves
        /// nothing: no re-home here either.
        fn show_apps(&self, apps: &[Unhide]) -> Vec<HoldStat> {
            let mut out = Vec::new();
            for a in apps {
                // A frozen app applies nothing — neither our writes nor its
                // own reveal — so it stays hidden and everything holds still.
                if !self.frozen.get() {
                    if self.hidden.borrow_mut().remove(&a.pid) {
                        let ordering_in: Vec<(WindowId, Rect)> = self
                            .windows
                            .borrow()
                            .iter()
                            .filter(|(_, (p, _))| *p == a.pid)
                            .map(|(w, (_, f))| (*w, *f))
                            .collect();
                        let ds = self.displays.borrow().clone();
                        for (w, f) in ordering_in {
                            self.land(a.pid, w, constrain_to_screen(&ds, f));
                        }
                    }
                    for (w, at) in &a.hold {
                        let size_is_the_windows_own = self.windows.borrow()[w].1;
                        self.land(
                            a.pid,
                            *w,
                            Rect {
                                x: at.x,
                                y: at.y,
                                ..size_is_the_windows_own
                            },
                        );
                    }
                }
                let escaped = a
                    .hold
                    .iter()
                    .filter(|(w, at)| {
                        let f = self.windows.borrow()[w].1;
                        !same_position(
                            &f,
                            &Rect {
                                x: at.x,
                                y: at.y,
                                ..f
                            },
                        )
                    })
                    .map(|(w, _)| *w)
                    .collect();
                out.push(HoldStat::new(
                    a.pid,
                    a.hold.len(),
                    a.hold.len() as u32,
                    0,
                    escaped,
                ));
            }
            out
        }

        fn focused_window(&self) -> Option<WindowId> {
            self.focused.get()
        }

        fn main_display(&self) -> Rect {
            self.displays.borrow().first().copied().unwrap_or(MAIN)
        }

        fn displays(&self) -> Vec<Rect> {
            self.displays.borrow().clone()
        }

        fn existing_windows(&self, ids: &[WindowId]) -> Option<HashSet<WindowId>> {
            if self.cg_down.get() {
                return None;
            }
            let known = self.windows.borrow();
            Some(
                ids.iter()
                    .filter(|w| known.contains_key(w))
                    .copied()
                    .collect(),
            )
        }
    }

    #[test]
    fn a_move_and_switch_round_trip_through_the_desktop_port() {
        let d = FakeDesktop::new(&[
            (w(1), Pid(10), rect(100.0, 100.0)),
            (w(2), Pid(20), rect(300.0, 200.0)),
        ]);
        let mut b = EmulatedWorkspaces::new(3);
        b.note_scan(&d, &d.scan());

        // Moving w2 to a hidden workspace parks it at the corner…
        b.move_window_to_workspace(&d, w(2), ws(2)).unwrap();
        assert_eq!(b.window_ws()[&w(2)], ws(2));
        assert!(in_park_corner(&d.frame(w(2)), &geo()));

        // …and switching there parks w1 and puts w2 back where it was.
        b.switch_workspace(&d, ws(2));
        assert_eq!(b.current(), ws(2));
        assert!(in_park_corner(&d.frame(w(1)), &geo()));
        assert_eq!(d.frame(w(2)), rect(300.0, 200.0));

        // Round home: both windows end at their original frames.
        b.switch_workspace(&d, ws(1));
        assert_eq!(d.frame(w(1)), rect(100.0, 100.0));
        assert!(in_park_corner(&d.frame(w(2)), &geo()));
    }

    /// A switch must be readable end to end from the trace alone: which
    /// workspaces, every frame write, and — the part no channel recorded before
    /// — the app unhide that reveals a straddling app's OTHER windows. That
    /// unhide is the prime suspect for the flash on arriving at a workspace, so
    /// it has to be attributable to a moment.
    #[test]
    fn a_switch_is_legible_end_to_end_including_the_app_unhide() {
        // One app owning windows on two workspaces: the straddling case, where
        // hiding cannot express the split and parking has to carry it.
        let d = FakeDesktop::new(&[
            (w(1), Pid(10), rect(100.0, 100.0)),
            (w(2), Pid(10), rect(300.0, 200.0)),
        ]);
        let mut b = EmulatedWorkspaces::new(3);
        b.note_scan(&d, &d.scan());
        b.assign_window_to_workspace(w(2), ws(2)).unwrap();
        b.take_trace();

        b.switch_workspace(&d, ws(2)); // ws1 -> ws2: park w1, restore w2
        let trace = b.take_trace();

        let sw = trace
            .iter()
            .find(|t| t.kind == ParkTraceKind::Switch)
            .expect("the switch names itself");
        assert_eq!(
            (sw.declared, sw.current),
            (Some(ws(1)), Some(ws(2))),
            "and says where FROM, not the target twice"
        );

        let parked = trace
            .iter()
            .find(|t| t.kind == ParkTraceKind::Park && t.window == w(1))
            .expect("w1's park is on the record");
        assert!(in_park_corner(
            &parked.requested.expect("with the frame asked for"),
            &geo()
        ));

        // The app stays visible (it owns a window here), and the record says
        // how many of its windows the unhide also exposes — 1, the parked w1.
        let shown = trace
            .iter()
            .find(|t| t.kind == ParkTraceKind::AppShown)
            .expect("the unhide is attributable");
        assert_eq!(shown.pid, Some(Pid(10)));
        assert!(
            shown
                .detail
                .as_deref()
                .is_some_and(|d| d.contains("1 window")),
            "counts the windows a flash would show: {:?}",
            shown.detail
        );
    }

    /// Un-hiding an app is not the mirror of hiding it. Dock dimming hides an
    /// app whose windows all live elsewhere; arriving at a workspace un-hides
    /// it, and the app orders EVERY window it owns back in — AppKit re-homing
    /// each one onto a display on the way. The windows parked for other
    /// workspaces come back with the wanted one unless the un-hide itself
    /// holds them, which is the flash on every switch.
    ///
    /// Without the hold this test is red exactly where it should be: w2 is
    /// found at x=0 (dragged onto main) instead of the corner.
    #[test]
    fn an_unhide_leaves_the_apps_other_workspaces_parked() {
        // pid 10 straddles ws2 and ws3 and owns nothing on ws1, so it is
        // genuinely hidden while we sit on ws1 — the only state an un-hide can
        // re-home anything out of. pid 20 keeps ws1 populated.
        let d = FakeDesktop::new(&[
            (w(1), Pid(10), rect(100.0, 100.0)),
            (w(2), Pid(10), rect(300.0, 200.0)),
            (w(3), Pid(20), rect(500.0, 300.0)),
        ]);
        let mut b = EmulatedWorkspaces::new(3);
        b.note_scan(&d, &d.scan());
        b.move_window_to_workspace(&d, w(1), ws(2)).unwrap();
        b.move_window_to_workspace(&d, w(2), ws(3)).unwrap();
        assert!(
            d.is_hidden(Pid(10)),
            "dimming hid the app with nothing here"
        );
        b.take_trace();

        b.switch_workspace(&d, ws(2));

        assert_eq!(
            d.frame(w(1)),
            rect(100.0, 100.0),
            "the wanted window is back"
        );
        assert!(
            in_park_corner(&d.frame(w(2)), &geo()),
            "the un-hide dragged ws3's window back on screen: {:?}",
            d.frame(w(2))
        );
        assert!(!d.is_hidden(Pid(10)));

        // And the un-hide says what it cost, because a hold that quietly stops
        // working looks exactly like one that works.
        let trace = b.take_trace();
        let shown = trace
            .iter()
            .find(|t| t.kind == ParkTraceKind::AppShown && t.pid == Some(Pid(10)))
            .expect("the unhide is attributable");
        let hold = shown.hold.as_ref().expect("with its hold on the record");
        assert_eq!((hold.windows, hold.converged), (1, true));
        assert!(hold.escaped.is_empty());
    }

    /// The trace exists because every other channel is laundered: the core is
    /// told a parked window's promise, so the corner it physically occupies
    /// appears nowhere. A park must therefore be legible here — with the frame
    /// actually requested — even while `believed_frames` hides it.
    #[test]
    fn the_trace_records_the_park_the_snapshot_hides() {
        let d = FakeDesktop::new(&[(w(1), Pid(10), rect(100.0, 100.0))]);
        let mut b = EmulatedWorkspaces::new(3);
        b.note_scan(&d, &d.scan());
        b.take_trace(); // discard adoption noise from the scan

        b.move_window_to_workspace(&d, w(1), ws(2)).unwrap();

        let frames = frames_of(&d);
        let believed = b.believed_frames(&d, &frames);
        assert_eq!(
            believed.get(&w(1)),
            Some(&rect(100.0, 100.0)),
            "the snapshot still shows the promise, not the corner"
        );

        let trace = b.take_trace();
        let park = trace
            .iter()
            .find(|t| t.kind == ParkTraceKind::Park)
            .expect("the park is on the record");
        assert_eq!(
            park.observed,
            Some(rect(100.0, 100.0)),
            "raw frame, pre-park"
        );
        let want = park.requested.expect("the frame we asked the OS for");
        assert!(
            in_park_corner(&want, &geo()),
            "and it names the corner the snapshot never shows: {want:?}"
        );
        // Draining is a move, not a copy: a second read must not double-count.
        assert!(b.take_trace().is_empty());
    }

    #[test]
    fn believed_frames_substitute_the_promise_for_the_sliver() {
        let d = FakeDesktop::new(&[
            (w(1), Pid(10), rect(100.0, 100.0)),
            (w(2), Pid(20), rect(300.0, 200.0)),
        ]);
        let mut b = EmulatedWorkspaces::new(3);
        b.note_scan(&d, &d.scan());
        b.move_window_to_workspace(&d, w(1), ws(2)).unwrap(); // parks w1

        let frames: HashMap<WindowId, (Pid, Rect)> = d
            .windows()
            .into_iter()
            .map(|(id, p, f)| (id, (p, f)))
            .collect();
        let believed = b.believed_frames(&d, &frames);
        // The parked window reads as its promise, not the corner artifact…
        assert_eq!(believed.get(&w(1)), Some(&rect(100.0, 100.0)));
        // …and a visible window's observation stands.
        assert_eq!(believed.get(&w(2)), None);

        // Restore lag: switch to w1's workspace, but the app never applies
        // the restore write — the window is bookkept unparked while still
        // physically a sliver. The promise must keep substituting, which is
        // exactly why `saved` outlives the parked flag.
        d.freeze();
        b.switch_workspace(&d, ws(2));
        let frames: HashMap<WindowId, (Pid, Rect)> = d
            .windows()
            .into_iter()
            .map(|(id, p, f)| (id, (p, f)))
            .collect();
        let believed = b.believed_frames(&d, &frames);
        assert_eq!(believed.get(&w(1)), Some(&rect(100.0, 100.0)));
    }

    #[test]
    fn park_never_captures_a_sliver_as_the_saved_frame() {
        let mut b = EmulatedWorkspaces::new(3);
        let sliver = park_frame(rect(100.0, 100.0), &geo());
        // The deepest clamp measured in production (124pt): still an artifact,
        // and beyond any title-bar-sized guess.
        let deep = Rect {
            x: MAIN.w - 800.0,
            y: MAIN.h - 124.0,
            w: 800.0,
            h: 600.0,
        };
        // The pre-right-aligned corner, as restarts still find on disk-era
        // windows: x at the display edge, the body hanging past it.
        let legacy = Rect {
            x: MAIN.w - SLIVER,
            y: MAIN.h - 40.0,
            w: 1470.0,
            h: 900.0,
        };
        // The corner the build shipping before this one used: the rightmost
        // display's own bottom-right. Every window parked at the moment of the
        // upgrade is sitting here.
        let retired_right = Rect {
            x: SECOND.x + SECOND.w - SLIVER,
            y: SECOND.y + SECOND.h - 33.0,
            w: 1528.0,
            h: 923.0,
        };
        let frames: HashMap<WindowId, (Pid, Rect)> = [
            (w(1), (Pid(42), sliver)),
            (w(2), (Pid(42), rect(50.0, 60.0))),
            (w(3), (Pid(42), deep)),
            (w(4), (Pid(42), legacy)),
            (w(5), (Pid(42), retired_right)),
        ]
        .into();

        // A window already at a park artifact — the current corner, a deep
        // clamp, or either retired corner: bookkept as parked, but its
        // (unknown) real frame is never fabricated from the artifact.
        for id in [w(1), w(3), w(4), w(5)] {
            assert_eq!(b.park(id, None, &frames, &geo()), None, "{id:?}");
            assert!(b.parked.contains(&id));
            assert!(!b.saved.contains_key(&id), "{id:?} captured an artifact");
        }

        // A window at a real position parks normally.
        let write = b.park(w(2), None, &frames, &geo()).unwrap();
        assert_eq!(b.saved[&w(2)], rect(50.0, 60.0));
        assert_eq!(write.2, park_frame(rect(50.0, 60.0), &geo()));
    }

    /// The park request must clear every display: an earlier corner
    /// (x = main's right edge - 1) hung the window's body across whatever sat
    /// to the right of main, and its successor left the body on the rightmost
    /// display's own screen. macOS will not hide a title bar vertically, so
    /// the horizontal escape is the only thing doing any hiding, and how much
    /// of it survives on ANY screen is the whole question.
    #[test]
    fn a_parked_window_is_invisible_on_every_display() {
        for size in [(1470.0, 900.0), (800.0, 600.0), (MAIN.w + 500.0, 900.0)] {
            for y in [0.0, 100.0, MAIN.h - 200.0] {
                let f = Rect {
                    x: 100.0,
                    y,
                    w: size.0,
                    h: size.1,
                };
                let d = FakeDesktop::new(&[(w(1), Pid(10), f)]);
                let want = park_frame(f, &geo());
                d.move_windows(&[(
                    Pid(10),
                    w(1),
                    Point {
                        x: want.x,
                        y: want.y,
                    },
                )]);
                let landed = d.frame(w(1));
                let seen = visible_width(&landed);
                assert!(
                    seen <= SLIVER,
                    "a parked {}x{} window at y={y} still shows a {seen}pt strip at {landed:?}",
                    size.0,
                    size.1
                );
                assert_eq!(landed, want, "and it landed exactly");
            }
        }
    }

    /// The ratchet, pinned: hiding a window must never RESIZE it. The park
    /// write carries the window's size, so aiming it at a corner on the short
    /// second display asked for a frame that display could not hold; the
    /// WindowServer granted a shorter one, and the next park recorded THAT as
    /// the window's real frame. Michael's Chrome lost 58pt and his kitty 59pt
    /// this way, permanently, a few points per switch.
    #[test]
    fn parking_a_tall_window_never_shrinks_it() {
        // Taller than the second display can hold (923), comfortably within
        // main's 1050 — the shape of every window the ratchet ate.
        let tall = Rect {
            x: 0.0,
            y: 157.0,
            w: 1528.0,
            h: 1050.0,
        };
        let d = FakeDesktop::new(&[(w(1), Pid(10), tall), (w(2), Pid(20), rect(300.0, 200.0))]);
        let mut b = EmulatedWorkspaces::new(3);
        b.note_scan(&d, &d.scan());

        b.move_window_to_workspace(&d, w(1), ws(2)).unwrap();
        for round in 1..=2 {
            assert_eq!(
                d.frame(w(1)).h,
                tall.h,
                "the park shrank the window (round {round})"
            );
            assert_eq!(b.saved[&w(1)], tall, "the promise shrank (round {round})");
            b.switch_workspace(&d, ws(2)); // restore
            assert_eq!(d.frame(w(1)), tall, "came back short (round {round})");
            b.switch_workspace(&d, ws(1)); // park again
        }
        assert_eq!(d.frame(w(1)).h, tall.h);
    }

    /// The last of the ratchet, closed: a park is a MOVE, so not even a window
    /// taller than the display owning the park origin comes back shorter. No
    /// window on Michael's rig can be that tall — main is the tallest screen
    /// there — but a rig whose external display is taller than main has them,
    /// and this used to file the difference off them a park at a time.
    ///
    /// The second half is the counterfactual, and it is the point: the same
    /// request as a whole-frame write still loses 150pt to the cap and its y
    /// to main's top inset. Nothing about the WindowServer got kinder; the
    /// write stopped asking.
    #[test]
    fn a_park_moves_an_over_tall_window_without_shortening_it() {
        let over_tall = Rect {
            x: 0.0,
            y: 200.0,
            w: 900.0,
            h: 1200.0,
        };
        let want = park_frame(over_tall, &geo());

        let d = FakeDesktop::new(&[(w(1), Pid(10), over_tall)]);
        let mut b = EmulatedWorkspaces::new(3);
        b.note_scan(&d, &d.scan());
        b.move_window_to_workspace(&d, w(1), ws(2)).unwrap();
        assert_eq!(d.frame(w(1)), want, "parked exactly, height and all");
        assert_eq!(
            b.saved[&w(1)],
            over_tall,
            "and the promise is the real frame"
        );

        let d = FakeDesktop::new(&[(w(1), Pid(10), over_tall)]);
        d.write_whole_frame(Pid(10), w(1), want);
        assert_eq!(
            (d.frame(w(1)).h, d.frame(w(1)).y),
            (MAIN.h - 30.0, 30.0),
            "a size-carrying write is still capped to what main can hold"
        );
    }

    /// The promise records a whole frame but a restore writes only its origin,
    /// so a window whose size changed while it was parked — an app resizing
    /// itself, or a window an older build already ratcheted short — comes home
    /// to the right place at the size it actually has.
    ///
    /// This is the deliberate cost of the move-only write, and the shape of the
    /// recovery is what makes it acceptable: the model does not fabricate the
    /// lost height, it just stops carrying a stale one, and the next park
    /// records what the window really is.
    #[test]
    fn a_restore_puts_a_window_back_at_its_size_not_its_promises() {
        let tall = Rect {
            x: 200.0,
            y: 157.0,
            w: 1000.0,
            h: 1050.0,
        };
        let d = FakeDesktop::new(&[(w(1), Pid(10), tall)]);
        let mut b = EmulatedWorkspaces::new(3);
        b.note_scan(&d, &d.scan());
        b.move_window_to_workspace(&d, w(1), ws(2)).unwrap();
        let parked_at = d.frame(w(1));

        // While parked, the window becomes shorter by a hand that is not ours.
        let shrunk = Rect {
            h: 923.0,
            ..parked_at
        };
        d.place(w(1), shrunk);

        b.switch_workspace(&d, ws(2));
        assert_eq!(
            d.frame(w(1)),
            Rect { h: 923.0, ..tall },
            "home to the promised position, at the height it now has"
        );
        assert_eq!(b.saved[&w(1)], tall, "the promise itself is not rewritten");

        // Parking again captures the window as it is — nothing keeps dragging
        // the stale height along, so one user resize is all the repair takes.
        b.switch_workspace(&d, ws(1));
        assert_eq!(b.saved[&w(1)], Rect { h: 923.0, ..tall });
    }

    #[test]
    fn a_partial_scan_never_reassigns_a_living_window() {
        // The deterministic phantom-maker, replayed: a parked window's app
        // blows the AX timeout, so ONE scan misses it while the window server
        // still knows it. Its declaration and restore promise must survive.
        let d = FakeDesktop::new(&[
            (w(1), Pid(10), rect(100.0, 100.0)),
            (w(2), Pid(20), rect(300.0, 200.0)),
        ]);
        let mut b = EmulatedWorkspaces::new(3);
        b.note_scan(&d, &d.scan());
        b.move_window_to_workspace(&d, w(1), ws(3)).unwrap(); // parked

        b.note_scan(&d, &[(w(2), Pid(20))]); // partial: w1 missing, alive
        assert_eq!(b.window_ws()[&w(1)], ws(3), "declaration kept");
        assert_eq!(b.saved[&w(1)], rect(100.0, 100.0), "promise kept");

        // Re-sighted next scan: same identity, nothing to adopt.
        b.note_scan(&d, &d.scan());
        assert_eq!(b.window_ws()[&w(1)], ws(3));

        // A failed CG read is not evidence either.
        d.cg_down.set(true);
        d.close(w(1));
        b.note_scan(&d, &[(w(2), Pid(20))]);
        assert_eq!(b.window_ws()[&w(1)], ws(3), "no evidence, no forgetting");

        // CG back up and the window really is gone: forgotten everywhere.
        d.cg_down.set(false);
        b.note_scan(&d, &[(w(2), Pid(20))]);
        assert!(!b.window_ws().contains_key(&w(1)));
        assert!(!b.saved.contains_key(&w(1)));
        assert!(!b.parked.contains(&w(1)));
    }

    #[test]
    fn a_recycled_id_never_inherits_the_dead_windows_promise() {
        let d = FakeDesktop::new(&[
            (w(1), Pid(10), rect(100.0, 100.0)),
            (w(2), Pid(20), rect(300.0, 200.0)),
        ]);
        let mut b = EmulatedWorkspaces::new(3);
        b.note_scan(&d, &d.scan());
        b.move_window_to_workspace(&d, w(1), ws(3)).unwrap(); // parked, saved

        // The id comes back in the same scan under a different app: it is a
        // NEW window on the current workspace, with no inherited teleport.
        b.note_scan(&d, &[(w(1), Pid(99)), (w(2), Pid(20))]);
        assert_eq!(b.window_ws()[&w(1)], ws(1));
        assert!(!b.saved.contains_key(&w(1)));
        assert!(!b.parked.contains(&w(1)));
    }

    fn frames_of(d: &FakeDesktop) -> HashMap<WindowId, (Pid, Rect)> {
        d.windows()
            .into_iter()
            .map(|(id, p, f)| (id, (p, f)))
            .collect()
    }

    #[test]
    fn an_assignment_never_touches_the_frame_and_the_switch_carries_it() {
        let d = FakeDesktop::new(&[
            (w(1), Pid(10), rect(100.0, 100.0)),
            (w(2), Pid(20), rect(300.0, 200.0)),
        ]);
        let mut b = EmulatedWorkspaces::new(3);
        b.note_scan(&d, &d.scan());

        // Prologue: a workspace round trip leaves w1 with a lingering saved
        // promise (saved outlives parked), and the user then moves it. The
        // carry must respect where the user put it — honoring the stale
        // promise teleported carried windows back to their old frame.
        b.move_window_to_workspace(&d, w(2), ws(2)).unwrap();
        b.switch_workspace(&d, ws(2));
        b.switch_workspace(&d, ws(1)); // w1 parked and restored; promise lingers
        let placed = rect(700.0, 400.0);
        d.place(w(1), placed);

        // The carry path: reassign, no frame write of any kind…
        b.assign_window_to_workspace(w(1), ws(2)).unwrap();
        assert_eq!(b.window_ws()[&w(1)], ws(2));
        assert_eq!(d.frame(w(1)), placed, "stayed put");
        assert!(!b.parked.contains(&w(1)));

        // …then the switch finds it already a resident: nothing moves it.
        b.switch_workspace(&d, ws(2));
        assert_eq!(d.frame(w(1)), placed, "still where the user put it");

        // And leaving again parks it with a FRESH promise.
        b.switch_workspace(&d, ws(1));
        assert!(in_park_corner(&d.frame(w(1)), &geo()));
        assert_eq!(b.saved[&w(1)], placed);
    }

    #[test]
    fn enforcement_asserts_the_declaration_not_the_parked_set() {
        // The blind spot: a window declared hidden while its frame was
        // unreadable never got a park write, never entered the parked set,
        // and the old parked-set walk could never see it.
        let d = FakeDesktop::new(&[(w(2), Pid(20), rect(300.0, 200.0))]);
        let mut b = EmulatedWorkspaces::new(3);
        b.note_scan(&d, &d.scan());
        b.move_window_to_workspace(&d, w(1), ws(2)).unwrap(); // no frame yet

        // The window appears, visible on the wrong workspace.
        d.windows
            .borrow_mut()
            .insert(w(1), (Pid(10), rect(100.0, 100.0)));
        b.enforce_placement(&d, &frames_of(&d));
        assert!(in_park_corner(&d.frame(w(1)), &geo()), "parked at last");
        assert_eq!(b.saved[&w(1)], rect(100.0, 100.0), "promise captured");
    }

    #[test]
    fn a_stale_restore_never_drains_the_enforcement_budget() {
        let d = FakeDesktop::new(&[
            (w(1), Pid(10), rect(100.0, 100.0)),
            (w(2), Pid(20), rect(300.0, 200.0)),
        ]);
        let mut b = EmulatedWorkspaces::new(3);
        b.note_scan(&d, &d.scan());
        b.move_window_to_workspace(&d, w(1), ws(2)).unwrap(); // parked
        let corner = d.frame(w(1)); // where the park write lands

        // Our own restore lands late: the window sits at exactly its promise.
        // Re-park it — without counting, and without repeating the write on
        // every pass while it is in flight (that was a self-sustaining loop:
        // 8 identical writes in 17s against a window that never moved).
        d.place(w(1), rect(100.0, 100.0));
        b.enforce_placement(&d, &frames_of(&d));
        assert_eq!(d.frame(w(1)), corner, "one re-park issued");

        d.place(w(1), rect(100.0, 100.0)); // still reads as our write in flight
        for _ in 0..(ENFORCE_LIMIT as usize + 3) {
            b.enforce_placement(&d, &frames_of(&d));
            assert_eq!(
                d.frame(w(1)),
                rect(100.0, 100.0),
                "no second write until the first is seen landing"
            );
        }
        assert_eq!(b.enforce_attempts.get(&w(1)), None, "budget untouched");
        assert_eq!(b.window_ws()[&w(1)], ws(2), "declaration intact");

        // The in-flight write finally lands; the episode closes…
        d.place(w(1), corner);
        b.enforce_placement(&d, &frames_of(&d));
        // …so the NEXT stale landing earns a fresh re-park, still uncounted.
        d.place(w(1), rect(100.0, 100.0));
        b.enforce_placement(&d, &frames_of(&d));
        assert_eq!(d.frame(w(1)), corner, "new episode, new write");
        assert_eq!(b.enforce_attempts.get(&w(1)), None, "still never counted");
    }

    /// The invariant enforcement exists to protect: a declaration is NEVER
    /// rewritten from the screen. A window that keeps escaping keeps its
    /// declared workspace no matter how many passes run; at the limit
    /// enforcement stands down loudly and leaves it visibly misplaced — the
    /// misplacement is obvious and the next switch heals it, while a
    /// rewritten declaration is a silent, permanent loss of where the user
    /// filed the window.
    #[test]
    fn a_window_that_keeps_escaping_keeps_its_declaration() {
        let d = FakeDesktop::new(&[
            (w(1), Pid(10), rect(100.0, 100.0)),
            (w(2), Pid(20), rect(300.0, 200.0)),
        ]);
        let mut b = EmulatedWorkspaces::new(3);
        b.note_scan(&d, &d.scan());
        b.move_window_to_workspace(&d, w(1), ws(2)).unwrap();
        b.take_trace();

        // A foreign hand insists the window stays visible; our park writes
        // never take (frozen desktop).
        let foreign = rect(500.0, 400.0);
        d.place(w(1), foreign);
        d.freeze();
        for _ in 0..(ENFORCE_LIMIT as usize + 5) {
            b.enforce_placement(&d, &frames_of(&d));
            assert_eq!(b.window_ws()[&w(1)], ws(2), "declaration never rewritten");
        }
        assert_eq!(b.saved[&w(1)], rect(100.0, 100.0), "promise kept too");
        assert!(b.parked.contains(&w(1)));

        // Past the limit enforcement holds fire: even with writes landing
        // again, the window is left where it visibly stands.
        d.thaw();
        for _ in 0..3 {
            b.enforce_placement(&d, &frames_of(&d));
            assert_eq!(d.frame(w(1)), foreign, "no writes past the limit");
        }

        // The stand-down is loud, and said once — not once per pass.
        let standoffs: Vec<_> = b
            .take_trace()
            .into_iter()
            .filter(|t| t.kind == ParkTraceKind::Standoff)
            .collect();
        assert_eq!(standoffs.len(), 1);
        assert_eq!(standoffs[0].window, w(1));

        // The kept declaration is what makes the user's next switch heal it.
        b.switch_workspace(&d, ws(2));
        assert_eq!(d.frame(w(1)), rect(100.0, 100.0), "restored to its promise");
    }

    /// The bottom clamp is what made the emulated backend unusable: macOS
    /// landed a parked window pulled back up from the corner by an
    /// app-dependent 27-124pt, so a compliant window read as an escapee on
    /// every pass — re-parked forever, budget drained, the raw sliver fed to
    /// the core. Parking sideways at the window's own y takes the clamp out
    /// of the mechanism entirely: however deep this WindowServer's pull-back
    /// is, the park never goes near the bottom edge and lands exactly.
    #[test]
    fn a_park_lands_exactly_however_deep_the_bottom_clamp_is() {
        // 124pt is the deepest landing in the production logs — well past any
        // title-bar-sized allowance.
        for pull in [28.0, 65.0, 124.0] {
            let d = FakeDesktop::new(&[
                (w(1), Pid(10), rect(100.0, 100.0)),
                (w(2), Pid(20), rect(300.0, 200.0)),
            ]);
            d.pull.set(pull);
            let mut b = EmulatedWorkspaces::new(3);
            b.note_scan(&d, &d.scan());
            b.move_window_to_workspace(&d, w(1), ws(2)).unwrap();

            let landed = d.frame(w(1));
            assert_eq!(
                landed,
                park_frame(rect(100.0, 100.0), &geo()),
                "the park landed where it asked, size and all, at pull {pull}"
            );

            // Enforcement must leave it alone indefinitely: no budget spent,
            // no rewrite of anything, no write churn against a window that is
            // already obeying.
            for _ in 0..(ENFORCE_LIMIT as usize + 5) {
                b.enforce_placement(&d, &frames_of(&d));
            }
            assert_eq!(b.window_ws()[&w(1)], ws(2), "declaration survives");
            assert!(
                b.enforce_attempts.is_empty(),
                "no violation was ever counted at pull {pull}"
            );
            assert_eq!(d.frame(w(1)), landed, "and it was never rewritten");

            // The core is fed the promise, not the sliver, at every depth —
            // when this failed, MRU scoping and monitor attribution reasoned
            // about a window living at the display corner.
            let believed = b.believed_frames(&d, &frames_of(&d));
            assert_eq!(believed.get(&w(1)), Some(&rect(100.0, 100.0)));

            // The promise stayed the window's real frame, so it comes back
            // whole.
            b.switch_workspace(&d, ws(2));
            assert_eq!(d.frame(w(1)), rect(100.0, 100.0));
        }
    }

    /// Ordo's own non-park writes (a rehome of a promise-less window) landing
    /// late — or clamped, e.g. pushed down under the menu bar — must never be
    /// charged to the window: in the incident, every charged attempt after
    /// the single genuinely foreign observation was Ordo counting its own
    /// write landing.
    #[test]
    fn a_window_at_ordos_own_last_write_is_never_charged() {
        // w1 starts as a corner artifact with no history: parked by the
        // sliver guard with NO promise, so its restore re-homes it.
        let artifact = park_frame(rect(0.0, 0.0), &geo());
        let d = FakeDesktop::new(&[
            (w(1), Pid(10), artifact),
            (w(2), Pid(20), rect(300.0, 200.0)),
        ]);
        let mut b = EmulatedWorkspaces::new(3);
        b.note_scan(&d, &d.scan());
        b.move_window_to_workspace(&d, w(1), ws(2)).unwrap(); // sliver guard: no promise
        assert!(!b.saved.contains_key(&w(1)));

        b.switch_workspace(&d, ws(2)); // restore re-homes it to main's origin
        let rehomed = d.frame(w(1));
        assert!(!in_park_corner(&rehomed, &geo()));

        // The user carries it to a hidden workspace, and the OS nudges the
        // rehome landing (the menu-bar pushdown): the observed frame now
        // matches Ordo's last WRITE, not any promise.
        b.assign_window_to_workspace(w(1), ws(3)).unwrap();
        d.place(
            w(1),
            Rect {
                y: rehomed.y + 25.0,
                ..rehomed
            },
        );
        b.take_trace();
        b.enforce_placement(&d, &frames_of(&d));
        assert_eq!(
            b.enforce_attempts.get(&w(1)),
            None,
            "our own write is not an app fighting back"
        );
        // The pass classified it as ours (the discriminating fact: without
        // the last-write exemption this reads as a foreign violation)…
        assert!(b
            .take_trace()
            .iter()
            .any(|t| t.window == w(1) && t.kind == ParkTraceKind::Suppressed));
        // …but exemption suppresses the COUNT, not the correction: it still
        // gets parked, and the frame it stood at becomes the promise.
        assert!(in_park_corner(&d.frame(w(1)), &geo()));
        assert_eq!(b.saved[&w(1)].x, rehomed.x);
    }

    /// The restart / corner-migration wrinkle, pinned: park requests are not
    /// persisted, so after a restart a parked window has no anchor and reads
    /// as ONE violation — it is re-parked once (which also migrates windows
    /// parked at a retired corner onto the current one) and then reads
    /// compliant, with its declaration and promise untouched.
    #[test]
    fn a_restart_reasserts_each_parked_window_once() {
        // As the upgrade finds things: bookkept parked with a good promise,
        // physically at the corner the previous build used (the rightmost
        // display's bottom right, pulled back up by its clamp), no
        // park_request memory.
        let legacy_landing = Rect {
            x: SECOND.x + SECOND.w - SLIVER,
            y: SECOND.y + SECOND.h - 65.0,
            w: 800.0,
            h: 600.0,
        };
        let d = FakeDesktop::new(&[
            (w(1), Pid(10), legacy_landing),
            (w(2), Pid(20), rect(300.0, 200.0)),
        ]);
        let mut b = EmulatedWorkspaces::new(3);
        b.note_scan(&d, &d.scan());
        b.ledger.assign_window(w(1), ws(2));
        b.parked.insert(w(1));
        b.saved.insert(w(1), rect(100.0, 100.0));

        b.enforce_placement(&d, &frames_of(&d));
        assert_eq!(b.enforce_attempts.get(&w(1)), Some(&1), "one violation");
        let migrated = d.frame(w(1));
        assert!(
            visible_width(&migrated) <= SLIVER,
            "re-parked out of sight: {migrated:?}"
        );
        assert_eq!(migrated.h, legacy_landing.h, "and not resized on the way");

        // With the anchor re-established, the next passes are quiet.
        for _ in 0..3 {
            b.enforce_placement(&d, &frames_of(&d));
        }
        assert_eq!(b.enforce_attempts.get(&w(1)), None, "budget cleared");
        assert_eq!(d.frame(w(1)), migrated, "no further writes");
        assert_eq!(b.window_ws()[&w(1)], ws(2));
        assert_eq!(b.saved[&w(1)], rect(100.0, 100.0), "promise untouched");
    }

    /// State files written before the corner was recognizable carry promises
    /// that ARE park positions. Honoring one re-parks the window the moment
    /// its workspace comes up: the window you cannot switch to.
    #[test]
    fn a_promise_that_is_itself_a_park_position_re_homes_instead_of_re_parking() {
        let d = FakeDesktop::new(&[(w(1), Pid(10), rect(100.0, 100.0))]);
        let mut b = EmulatedWorkspaces::new(3);
        b.note_scan(&d, &d.scan());
        b.move_window_to_workspace(&d, w(1), ws(2)).unwrap();

        // Exactly the residue found in a live state.json: origin at the park
        // corner, the window's own size preserved.
        let poisoned = Rect {
            x: MAIN.w - SLIVER,
            y: MAIN.h - 28.0,
            w: 1424.0,
            h: 906.0,
        };
        b.saved.insert(w(1), poisoned);

        b.switch_workspace(&d, ws(2));
        let restored = d.frame(w(1));
        assert!(
            !in_park_corner(&restored, &geo()),
            "must not restore into the corner it was parked in"
        );
        assert!(
            restored.x >= MAIN.x && restored.y >= MAIN.y && restored.x + restored.w <= MAIN.w,
            "and must land somewhere reachable: {restored:?}"
        );
    }

    #[test]
    fn fresh_session_merge_keeps_unseen_declarations_and_revives_slivered_ones() {
        let pw = |id: u32, wsn: u8, saved: Option<Rect>| PersistedWindow {
            id: w(id),
            workspace: ws(wsn),
            monitor: vm(1),
            owner: Pid(10),
            saved,
            home: None,
        };
        let file = PersistedState {
            version: statefile::VERSION,
            boot_time_sec: 1,
            current: ws(1),
            viewed: vm(1),
            virtual_monitors_enabled: true,
            virtual_monitor_count: 2,
            windows: vec![
                // Parked pre-R, never seen by the fresh session.
                pw(10, 3, Some(rect(10.0, 20.0))),
                // Parked pre-R, adopted by the fresh session but still a sliver.
                pw(11, 2, Some(rect(30.0, 40.0))),
                // Parked pre-R, pulled out and placed by the user during R.
                pw(12, 2, Some(rect(70.0, 80.0))),
                // Parked pre-R, and RE-parked by the user during R (so it is
                // physically a sliver, but by this session's own hand).
                pw(14, 2, Some(rect(90.0, 95.0))),
            ],
        };
        let claim = |wsn: u8| Claim {
            ws: ws(wsn),
            monitor: vm(1),
            owner: Pid(10),
        };
        let model: BTreeMap<WindowId, Claim> = [
            (w(11), claim(1)),
            (w(12), claim(1)),
            (w(13), claim(1)),
            (w(14), claim(3)),
        ]
        .into();
        let own_saved: HashMap<WindowId, Rect> = [(w(14), rect(91.0, 96.0))].into();
        let slivered = |id: &WindowId| *id == w(11) || *id == w(14);

        let m = merge_fresh_session(&model, &own_saved, &file, slivered);
        // Unseen: the file's declaration survives S untouched.
        assert_eq!(m.assign[&w(10)].ws, ws(3));
        // Slivered adoptee: adoption was noise, the file's promise wins.
        assert_eq!(m.assign[&w(11)].ws, ws(2));
        // User-placed: the live arrangement is the new truth.
        assert_eq!(m.assign[&w(12)].ws, ws(1));
        // Genuinely new in the fresh session: kept.
        assert_eq!(m.assign[&w(13)].ws, ws(1));
        // Slivered by the session's OWN park: a deliberate placement, not
        // noise — the fresh model and its captured frame win.
        assert_eq!(m.assign[&w(14)].ws, ws(3));
        // Only the noise slivers and unseen windows get file promises back.
        let saved: BTreeMap<_, _> = m.saved.into_iter().collect();
        assert_eq!(saved.get(&w(10)), Some(&rect(10.0, 20.0)));
        assert_eq!(saved.get(&w(11)), Some(&rect(30.0, 40.0)));
        assert_eq!(saved.get(&w(12)), None);
        assert_eq!(saved.get(&w(14)), None);
    }

    // --- virtual monitors ----------------------------------------------------
    // Michael's rig: w1 on the main display (monitor 1), w2 on the second
    // (monitor 2). The external display goes away and comes back.

    fn rig() -> (FakeDesktop, EmulatedWorkspaces) {
        let d = FakeDesktop::new(&[
            (w(1), Pid(10), rect(100.0, 100.0)),
            (w(2), Pid(20), rect(2000.0, 100.0)),
        ]);
        let mut b = EmulatedWorkspaces::new(3);
        b.note_scan(&d, &d.scan());
        assert_eq!(b.monitors().count, 2, "two displays, two monitors");
        assert_eq!(b.window_monitors()[&w(2)], vm(2), "adopted where it stands");
        (d, b)
    }

    /// One rescan's worth of the shell: learn the displays, then assert the
    /// projection — what a plug event turns into once the world settles.
    fn rescan(d: &FakeDesktop, b: &mut EmulatedWorkspaces) {
        b.note_scan(d, &d.scan());
        b.enforce_placement(d, &frames_of(d));
    }

    #[test]
    fn unplugging_parks_the_hidden_monitor_and_viewing_it_swaps_the_screen() {
        let (d, mut b) = rig();
        d.set_displays(&[MAIN]);
        let rehomed = d.frame(w(2));
        assert!(MAIN.contains(rehomed.center()), "macOS moved it onto the laptop");

        rescan(&d, &mut b);
        assert_eq!(b.monitors().count, 2, "the count never shrinks");
        assert_eq!(b.monitors().viewed, vm(1));
        assert!(in_park_corner(&d.frame(w(2)), &geo()), "monitor 2 is hidden");
        assert_eq!(b.window_monitors()[&w(2)], vm(2), "declaration kept");
        assert_eq!(d.frame(w(1)), rect(100.0, 100.0), "monitor 1 untouched");
        assert!(d.is_hidden(Pid(20)), "an app with nothing on screen is dimmed");

        // J/K: the other monitor's windows come up, this one's go down.
        b.view_monitor(&d, vm(2)).unwrap();
        assert_eq!(d.frame(w(2)), rehomed, "back where the laptop had it");
        assert!(in_park_corner(&d.frame(w(1)), &geo()));
        assert!(!d.is_hidden(Pid(20)));
        assert_eq!(b.view_monitor(&d, vm(3)), Err(MonitorOutOfRange(vm(3))));
        b.view_monitor(&d, vm(1)).unwrap();
        assert_eq!(d.frame(w(1)), rect(100.0, 100.0));
        assert!(in_park_corner(&d.frame(w(2)), &geo()));

        // Ctrl+Alt+Cmd+V: collapse shows both; enabling hides monitor 2 again.
        b.set_virtual_monitors(&d, false);
        assert_eq!(d.frame(w(1)), rect(100.0, 100.0));
        assert_eq!(d.frame(w(2)), rehomed);
        b.set_virtual_monitors(&d, true);
        assert!(in_park_corner(&d.frame(w(2)), &geo()));
        assert_eq!(d.frame(w(1)), rect(100.0, 100.0));
        // Enforcement has nothing to add to a settled screen.
        for _ in 0..3 {
            b.enforce_placement(&d, &frames_of(&d));
        }
        assert!(in_park_corner(&d.frame(w(2)), &geo()));
        assert!(b.enforce_attempts.is_empty());
    }

    /// Monitor memory. The frame macOS re-homed the window to is what `saved`
    /// can know; `home` is the docked frame, and it is what the window comes
    /// back to when its display returns.
    #[test]
    fn replugging_brings_a_window_home_to_its_docked_frame() {
        let (d, mut b) = rig();
        assert_eq!(b.home[&w(2)], rect(2000.0, 100.0), "docked frame remembered");
        d.set_displays(&[MAIN]);
        rescan(&d, &mut b);
        // Undocked, the user views monitor 2 and moves w2 around on the laptop:
        // the docked frame must survive all of it.
        b.view_monitor(&d, vm(2)).unwrap();
        d.place(w(2), rect(300.0, 300.0));
        rescan(&d, &mut b);
        assert_eq!(b.home[&w(2)], rect(2000.0, 100.0), "not overwritten undocked");
        b.view_monitor(&d, vm(1)).unwrap();
        b.take_trace();

        d.set_displays(&[MAIN, SECOND]);
        rescan(&d, &mut b);
        assert_eq!(d.frame(w(2)), rect(2000.0, 100.0), "exactly where it was");
        assert_eq!(d.frame(w(1)), rect(100.0, 100.0));
        assert!(!d.is_hidden(Pid(20)));
        assert!(b
            .take_trace()
            .iter()
            .any(|t| t.kind == ParkTraceKind::Rehost && t.window == w(2)));
        // Belief and screen agree, so the core sees no promise to re-host.
        assert!(b.believed_frames(&d, &frames_of(&d)).is_empty());
    }

    #[test]
    fn a_replacement_display_of_another_size_gets_the_frame_carried_over() {
        let (d, mut b) = rig();
        d.set_displays(&[MAIN]);
        rescan(&d, &mut b);
        // A different panel stands in the second position: too small to hold
        // the old docked frame, so the laptop frame is carried over instead.
        let small = Rect {
            x: 1920.0,
            y: 0.0,
            w: 1000.0,
            h: 700.0,
        };
        d.set_displays(&[MAIN, small]);
        rescan(&d, &mut b);
        let f = d.frame(w(2));
        assert!(
            small.contains(f.center()),
            "landed on the new display: {f:?}"
        );
        assert_eq!((f.w, f.h), (800.0, 600.0), "size is the window's own");
    }

    /// A display change is systemic: the view follows the window the user is
    /// in, rather than that window being parked because the anchor pointed
    /// elsewhere.
    #[test]
    fn the_view_follows_the_focused_window_through_an_unplug() {
        let (d, mut b) = rig();
        d.focused.set(Some(w(2)));
        d.set_displays(&[MAIN]);
        rescan(&d, &mut b);
        assert_eq!(b.monitors().viewed, vm(2));
        assert!(in_park_corner(&d.frame(w(1)), &geo()), "monitor 1 hidden instead");
        assert!(MAIN.contains(d.frame(w(2)).center()));
    }

    #[test]
    fn a_monitor_assignment_is_a_declaration_only_and_enforcement_asserts_it() {
        let (d, mut b) = rig();
        d.set_displays(&[MAIN]);
        rescan(&d, &mut b);
        // The core assigns w1 to the hidden monitor without viewing it (a
        // corral, say): no write here…
        b.assign_window_to_monitor(w(1), vm(2)).unwrap();
        assert_eq!(d.frame(w(1)), rect(100.0, 100.0));
        assert_eq!(b.window_monitors()[&w(1)], vm(2));
        // …and the standing check parks it on the next pass.
        b.enforce_placement(&d, &frames_of(&d));
        assert!(in_park_corner(&d.frame(w(1)), &geo()));
        assert_eq!(
            b.assign_window_to_monitor(w(1), vm(9)),
            Err(MonitorOutOfRange(vm(9)))
        );
    }

    #[test]
    fn the_state_file_carries_the_layout_and_the_docked_frames() {
        let dir = std::env::temp_dir().join(format!("ordo-vm-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.json");

        let d = FakeDesktop::new(&[
            (w(1), Pid(10), rect(100.0, 100.0)),
            (w(2), Pid(20), rect(2000.0, 100.0)),
        ]);
        let mut b = EmulatedWorkspaces::with_persistence(3, path.clone());
        b.note_scan(&d, &d.scan());
        d.set_displays(&[MAIN]);
        rescan(&d, &mut b);
        b.view_monitor(&d, vm(2)).unwrap();
        b.set_virtual_monitors(&d, false);

        // A restart: the layout, the declarations and the docked frames are
        // all back before the first scan.
        let again = EmulatedWorkspaces::with_persistence(3, path);
        assert_eq!(again.monitors().count, 2);
        assert_eq!(again.monitors().viewed, vm(2));
        assert!(!again.monitors().enabled);
        assert_eq!(again.window_monitors()[&w(2)], vm(2));
        assert_eq!(again.home[&w(2)], rect(2000.0, 100.0));

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
