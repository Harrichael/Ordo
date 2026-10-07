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
use std::time::Instant;

use ordo_core::{
    project, Pid, Point, Rect, VirtualMonitorId, VirtualMonitors, WindowId,
    WorkspaceId, FRAME_EPSILON,
};

use crate::ledger::{Claim, Ledger, SwitchPlan};
use crate::statefile::{self, PersistedState, PersistedWindow};
use crate::trace::{ParkTrace, ParkTraceKind, SwitchCost};
use crate::{Desktop, HideWhen, Hiding, Idle, Move, Unhide};

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

/// Each app's windows that are off screen, with where each is parked.
type ParkedByApp = HashMap<Pid, Vec<(WindowId, Point)>>;

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
    /// The display holding the largest share of `f`, by area; none if it
    /// overlaps none. Not the one under its centre: a window the user hung
    /// off a display's bottom edge has its centre on no display, and was
    /// taken for one made on another display and clamped back in (run 58).
    fn display_of(&self, f: &Rect) -> Option<Rect> {
        self.displays
            .iter()
            .copied()
            .map(|d| (overlap(&d, f), d))
            .filter(|(a, _)| *a > 0.0)
            .max_by(|a, b| a.0.total_cmp(&b.0))
            .map(|(_, d)| d)
    }

    fn display_index_of(&self, f: &Rect) -> Option<usize> {
        let d = self.display_of(f)?;
        self.displays.iter().position(|x| *x == d)
    }
}

fn overlap(a: &Rect, b: &Rect) -> f64 {
    let w = (a.x + a.w).min(b.x + b.w) - a.x.max(b.x);
    let h = (a.y + a.h).min(b.y + b.h) - a.y.max(b.y);
    if w > 0.0 && h > 0.0 {
        w * h
    } else {
        0.0
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
    /// The loaded file carried no monitor declarations (see
    /// `PersistedState::monitors_assigned`); the next sighted scan learns
    /// them from where the windows stand, once.
    learn_monitors: bool,
    /// Docked frames written on the pass the full rig came back, stood in for
    /// in belief for that pass only: its snapshot was read before the writes,
    /// and the core, seeing the windows where the laptop had them, would
    /// carry them over proportionally itself.
    homecoming: HashMap<WindowId, Rect>,
    /// The display set changed during a scan that saw no windows (a locked
    /// screen): the next scan that does see them is still the replug's pass.
    replug_unseen: bool,
    /// When the apps left with nothing on screen are to be hidden: a switch
    /// asks for its un-hides straight away but hides only once the user
    /// settles, and the apps are done with the switches' writes.
    hides_due: Option<Instant>,
    /// When, and for lack of what, an app is hidden: the user's setting.
    hiding: Hiding,
    /// The setting changed since the last pass judged which apps to hide.
    hiding_changed: bool,
    /// The apps hidden right now, as far as this model knows: the ones it
    /// hid, and ones found hidden with nothing on screen. Hiding is Ordo's
    /// alone. An app hidden any other way while it has a window on screen is
    /// shown again ([`Self::note_app_visibility`]), which is what lets a
    /// switch ask only these apps whether they are hidden. `None` until
    /// filled, by asking each app once, on the first pass that sees windows.
    hidden_apps: Option<HashSet<Pid>>,
    /// Hide and show notifications not yet acted on, oldest first.
    visibility_news: Vec<(Pid, bool)>,
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
            learn_monitors: false,
            homecoming: HashMap::new(),
            replug_unseen: false,
            hides_due: None,
            hiding: Hiding::default(),
            hiding_changed: false,
            hidden_apps: None,
            visibility_news: Vec::new(),
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
    /// `windows` is the scan's own read — every window with its app and frame
    /// — so nothing here walks the apps again.
    pub fn note_scan(&mut self, d: &dyn Desktop, windows: &HashMap<WindowId, (Pid, Rect)>) {
        let g = Geometry::read(d);
        self.homecoming.clear();
        let known = self.ledger.physical() > 0;
        let mut dirty = self.note_displays(d, &g);
        let replugged = dirty && known;
        if windows.is_empty() {
            self.replug_unseen |= replugged;
            if dirty {
                self.persist();
            }
            return;
        }
        let replugged = replugged || std::mem::take(&mut self.replug_unseen);
        let before = self.ledger.window_claims();
        let absent: Vec<WindowId> = self
            .ledger
            .window_ws()
            .keys()
            .filter(|w| !windows.contains_key(w))
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
        let frames = windows;
        let proj = self.ledger.projection();
        let viewed = self.ledger.monitors().viewed;
        let adopt = |id: WindowId| {
            frames
                .get(&id)
                .and_then(|(_, f)| g.display_index_of(f))
                .and_then(|i| proj.canonical_vm(i))
                .unwrap_or(viewed)
        };
        // A recycled id's saved frame belongs to a dead stranger; the new
        // window must not inherit a teleport to it.
        let mut seen: Vec<(WindowId, Pid)> = windows.iter().map(|(w, (p, _))| (*w, *p)).collect();
        seen.sort_by_key(|(w, _)| w.0);
        let recycled = self.ledger.note_seen(&seen, adopt);
        self.drop_park_bookkeeping(&recycled);
        if self.learn_monitors {
            self.learn_monitors_from_placement(frames, &g);
            self.learn_monitors = false;
            dirty = true;
        }
        dirty |= self.ledger.window_claims() != before;
        // Right after the display set changes, windows stand wherever macOS
        // re-homed them, which is nobody's placement: recorded as home, it
        // replaced the very frames the full rig is about to need.
        if !replugged {
            dirty |= self.refresh_home(frames, &g);
        }
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

    /// One-time: the file this model came from had no monitor declarations,
    /// so every window's monitor is a placeholder, and asserting a placeholder
    /// moves windows (it pulled a second-display window onto the first). Each
    /// window's monitor is read off the display it actually occupies — its
    /// promise's display while parked, since the sliver says nothing — and
    /// only then does it become a declaration. Windows with no frame to read
    /// keep the placeholder; their first sighting is a birth to the ledger's
    /// adoption anyway.
    fn learn_monitors_from_placement(
        &mut self,
        frames: &HashMap<WindowId, (Pid, Rect)>,
        g: &Geometry,
    ) {
        let proj = self.ledger.projection();
        for (id, claim) in self.ledger.window_claims() {
            let standing = match frames.get(&id) {
                Some((_, f)) if self.parked.contains(&id) || self.reads_parked(id, f, g) => {
                    self.saved.get(&id).copied()
                }
                Some((_, f)) => Some(*f),
                None => self.saved.get(&id).copied(),
            };
            let Some(f) = standing else { continue };
            let Some(i) = g.display_index_of(&f) else {
                continue;
            };
            if let Some(vm) = proj.canonical_vm(i) {
                if vm != claim.monitor {
                    self.ledger.assign_monitor(id, vm);
                }
            }
        }
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
        // Decided before this is called, so `parked` already says which way
        // each write goes.
        let moves: Vec<Move> = writes
            .iter()
            .map(|(pid, w, f)| Move {
                pid: *pid,
                window: *w,
                to: Point { x: f.x, y: f.y },
                parks: self.parked.contains(w),
            })
            .collect();
        if !moves.is_empty() {
            d.move_windows(&moves);
        }
    }

    /// Carry out a plan: park what left the screen, restore what entered it.
    /// Frames first, then visibility — a window must already be at the corner
    /// before its app is un-hidden, or the unhide reveals it where it still
    /// stands. The port keeps that order per app: each app's un-hide follows
    /// its own moves. It cannot stop an app from restoring its own geometry
    /// in response to being un-hidden; the un-hide's hold is for that.
    ///
    /// Stacking is NOT this backend's problem: the core follows every switch
    /// or view with a RestackWindows effect derived from the MRU history,
    /// which the effector reasserts after this returns.
    fn apply_plan(&mut self, d: &dyn Desktop, plan: SwitchPlan, boundary: ParkTrace) {
        if plan.is_empty() {
            self.persist(); // the declaration may still have changed
            return;
        }
        let started = d.now();
        let frames = current_frames(d, self.ledger.window_ws().into_keys());
        let g = Geometry::read(d);
        let read = d.now();
        let boundary_at = self.trace.len();
        self.note(boundary.detail(format!(
            "parking {}, restoring {}, rehosting {}",
            plan.park.len(),
            plan.restore.len(),
            plan.rehost.len()
        )));
        self.note_stack(d, "before");
        let mut writes = Vec::new();
        for w in plan.park {
            writes.extend(self.park(w, None, &frames, &g, d.in_flight(w)));
        }
        for w in plan.restore {
            writes.extend(self.restore(w, &frames, &g));
        }
        for w in plan.rehost {
            writes.extend(self.rehost(w, &frames, &g));
        }
        let persist_began = d.now();
        self.persist();
        let queue_began = d.now();
        self.move_windows(d, &writes);
        self.apply_app_visibility(d, &frames, &g);
        let ms = |a: std::time::Instant, b: std::time::Instant| (b - a).as_secs_f64() * 1000.0;
        if let Some(t) = self.trace.get_mut(boundary_at) {
            t.cost = Some(SwitchCost {
                read_ms: ms(started, read),
                persist_ms: ms(persist_began, queue_began),
                queue_ms: ms(queue_began, d.now()),
            });
        }
    }

    fn note_stack(&mut self, d: &dyn Desktop, moment: &str) {
        if !d.traces_stacks() {
            return;
        }
        let ids: Vec<String> = d
            .stack()
            .into_iter()
            .filter(|w| self.ledger.claim(*w).is_some())
            .map(|w| w.0.to_string())
            .collect();
        let current = self.ledger.current();
        self.note(
            ParkTrace::new(WindowId(0), ParkTraceKind::Stack)
                .ws(current, current)
                .detail(format!("{moment}: {}", ids.join(" "))),
        );
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

    pub fn merge_monitors(
        &mut self,
        d: &dyn Desktop,
        from: VirtualMonitorId,
        into: VirtualMonitorId,
    ) -> Result<(), MonitorOutOfRange> {
        let plan = self
            .ledger
            .merge_monitor(from, into)
            .ok_or(MonitorOutOfRange(from))?;
        let current = self.ledger.current();
        let boundary = ParkTrace::new(WindowId(0), ParkTraceKind::View)
            .ws(current, current)
            .detail(format!("monitor {} merged into {}", from.0, into.0));
        self.apply_plan(d, plan, boundary);
        Ok(())
    }

    /// One more, empty monitor. The plan should be empty — the screen stays
    /// as it was — but it runs like any other, so a mistake would still leave
    /// the screen matching the ledger.
    pub fn add_monitor(&mut self, d: &dyn Desktop) -> Result<(), MonitorOutOfRange> {
        let count = self.ledger.monitors().count;
        let plan = self
            .ledger
            .add_monitor()
            .ok_or(MonitorOutOfRange(VirtualMonitorId(count)))?;
        let current = self.ledger.current();
        let boundary = ParkTrace::new(WindowId(0), ParkTraceKind::View)
            .ws(current, current)
            .detail(format!("monitor {} added", count as u16 + 1));
        self.apply_plan(d, plan, boundary);
        Ok(())
    }

    pub fn move_workspace(
        &mut self,
        d: &dyn Desktop,
        from: WorkspaceId,
        to: WorkspaceId,
    ) -> Result<(), WorkspaceOutOfRange> {
        let plan = self
            .ledger
            .move_workspace(from, to)
            .ok_or(WorkspaceOutOfRange(from))?;
        let current = self.ledger.current();
        let boundary = ParkTrace::new(WindowId(0), ParkTraceKind::View)
            .ws(current, current)
            .detail(format!("workspace {} moved to {}", from.0, to.0));
        self.apply_plan(d, plan, boundary);
        Ok(())
    }

    pub fn move_monitor(
        &mut self,
        d: &dyn Desktop,
        from: VirtualMonitorId,
        to: VirtualMonitorId,
    ) -> Result<(), MonitorOutOfRange> {
        let plan = self
            .ledger
            .move_monitor(from, to)
            .ok_or(MonitorOutOfRange(from))?;
        let current = self.ledger.current();
        let boundary = ParkTrace::new(WindowId(0), ParkTraceKind::View)
            .ws(current, current)
            .detail(format!("monitor {} moved to {}", from.0, to.0));
        self.apply_plan(d, plan, boundary);
        Ok(())
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
                let known = self.ledger.window_ws().into_keys();
                let frames = current_frames(d, known.chain(ps.windows.iter().map(|w| w.id)));
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
        if self.saved.is_empty() && self.homecoming.is_empty() {
            return HashMap::new();
        }
        let g = Geometry::read(d);
        let mut believed: HashMap<WindowId, Rect> = frames
            .iter()
            .filter_map(|(w, (_, f))| {
                let saved = self.saved.get(w)?;
                self.reads_parked(*w, f, &g).then_some((*w, *saved))
            })
            .collect();
        believed.extend(self.homecoming.iter().map(|(w, f)| (*w, *f)));
        believed
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
        // would read as hidden. Not a world to assert anything against. Nor is
        // a scan that saw no windows at all — a locked screen reports displays
        // but no app's windows — and it must not use up a display change
        // either: plugged in while locked, the change is noticed blind, and
        // the windows it moves are only seen once the screen unlocks.
        if g.displays.is_empty() || frames.is_empty() {
            return;
        }
        let systemic = self.last_geometry.as_ref().is_some_and(|prev| *prev != g);
        self.last_geometry = Some(g.clone());
        if systemic {
            self.enforce_attempts.clear();
            if self.full_rig(&g) {
                let writes = self.come_home(frames, &g);
                self.move_windows(d, &writes);
            }
        }
        self.reconcile_visibility(d, frames, &g);
        if std::mem::take(&mut self.hiding_changed) {
            self.apply_app_visibility(d, frames, &g);
        }
        if self.hides_due.is_some_and(|due| d.now() >= due) && !d.busy() {
            self.hides_due = None;
            self.hide_idle_apps(d, frames, &g);
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
        let mut writes = Vec::new();
        let mut newly_parked = false;
        // Asked only once something has left its park, and then once a pass.
        let mut front: Option<Option<Pid>> = None;
        for w in hidden {
            let Some((pid, f)) = frames.get(&w) else {
                continue;
            };
            // This scan may predate our own write to it; judge it once that
            // write has had its chance to land.
            if d.in_flight(w) {
                continue;
            }
            if self.at_park(w, f) {
                self.enforce_attempts.remove(&w);
                self.pending_repark.remove(&w);
                continue;
            }
            // Parked windows turn up at the display's edge, their app's own
            // doing. Whether that app was still hidden (nothing showed) or
            // had come back (it flashed), and who was in front, say which.
            let front_app = *front.get_or_insert_with(|| d.frontmost_app());
            let seen = format!(
                "app {} hidden: {}; front app: {}",
                pid.0,
                match d.app_hidden(*pid) {
                    Some(true) => "yes",
                    Some(false) => "no",
                    None => "unknown",
                },
                front_app.map_or("none".to_string(), |p| p.0.to_string())
            );
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
                    .detail(format!(
                        "{}; {seen}",
                        if systemic {
                            "display set changed; whole pass uncounted"
                        } else if withheld {
                            "our own write; re-park already in flight, none issued"
                        } else {
                            "frame matches our own write; re-park uncounted"
                        }
                    ));
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
                    .attempt(self.enforce_attempts.get(&w).copied().unwrap_or(0))
                    .detail(seen);
                self.note(t);
                self.park_request.insert(w, want);
                if own_write && !systemic {
                    self.pending_repark.insert(w);
                }
                writes.push((*pid, w, want));
            } else {
                let attempt = self.enforce_attempts.get(&w).copied();
                let write = self.park(w, attempt, frames, &g, false);
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

    /// The full rig is back: every window on screen returns to its docked
    /// frame. macOS piled them onto the laptop, and what is left of that pile
    /// is not anyone's placement.
    fn come_home(
        &mut self,
        frames: &HashMap<WindowId, (Pid, Rect)>,
        g: &Geometry,
    ) -> Vec<(Pid, WindowId, Rect)> {
        let proj = self.ledger.projection();
        let mut writes = Vec::new();
        for (w, (pid, f)) in frames {
            let on_screen = self.ledger.visible(*w, &proj)
                && !self.parked.contains(w)
                && !self.reads_parked(*w, f, g);
            if !on_screen {
                continue;
            }
            let Some(host) = self.host_rect(*w, g) else {
                continue;
            };
            let Some(hm) = self.home.get(w).copied().filter(|hm| g.display_of(hm) == Some(host)) else {
                continue;
            };
            if same_position(f, &hm) {
                continue;
            }
            let want = Rect { x: hm.x, y: hm.y, ..*f };
            self.note(
                ParkTrace::new(*w, ParkTraceKind::Rehost)
                    .observed(*f)
                    .requested(want)
                    .detail("the full rig is back; to its docked frame"),
            );
            self.homecoming.insert(*w, want);
            writes.push((*pid, *w, want));
        }
        writes
    }

    pub fn rescue_window(&mut self, d: &dyn Desktop, window: WindowId) {
        // Claim it for the visible workspace and the anchor monitor, and bring
        // it back on-screen. No visibility pass here: rescue must only ever
        // reveal, and the gather already unhides every app up front.
        let frames = current_frames(d, [window]);
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
            statefile::set_aside(path);
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
        self.learn_monitors = !ps.monitors_assigned;
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
                // Once learned (or never in doubt), they are declarations.
                monitors_assigned: !self.learn_monitors,
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
    ///
    /// `in_flight`: a write to this window is still on its way, so `frames`
    /// may show where it stood before that write rather than where it is
    /// going. Its promise, which that write was made from, is kept, and the
    /// park is written whatever the frame says: skipping it because the
    /// window read as parked would let the write on its way land and leave
    /// the window on screen.
    fn park(
        &mut self,
        window: WindowId,
        attempt: Option<u8>,
        frames: &HashMap<WindowId, (Pid, Rect)>,
        g: &Geometry,
        in_flight: bool,
    ) -> Option<(Pid, WindowId, Rect)> {
        // Save the real frame only on the transition onto-screen -> parked; a
        // window already parked keeps its original saved frame rather than
        // recording the sliver position.
        if self.parked.contains(&window) {
            return None;
        }
        let (pid, f) = frames.get(&window)?;
        let stale = in_flight && self.saved.contains_key(&window);
        // Never canonicalize a sliver as a window's real frame. Gaps in the
        // model (a partial scan dropping the ledger entry, R-mode
        // re-adoption at birth) can hand this path a window that is already
        // physically parked; capturing that frame would turn a transient
        // wrong belief into a durable one — the window's recorded "real
        // position" becomes the corner artifact, and persist() writes it to
        // disk. A missing promise is recoverable (rescue); a lying one is
        // not.
        if !stale && self.reads_parked(window, f, g) {
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
        if !stale {
            self.saved.insert(window, f);
            if self.full_rig(g) {
                self.home.insert(window, f);
            }
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
        if stale {
            t = t.detail("a write to it is still on its way; promise kept");
        }
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
    /// The display gap is the rig changing while the window was hidden. With
    /// the full rig present, the window's `home` frame on its host comes first
    /// — land exactly where it was docked, whatever the laptop made of the
    /// promise. Otherwise: the promise itself, when it already lies on the
    /// host; then [`Self::carry_over`].
    fn restore(
        &mut self,
        window: WindowId,
        frames: &HashMap<WindowId, (Pid, Rect)>,
        g: &Geometry,
    ) -> Option<(Pid, WindowId, Rect)> {
        // Unseen is not restored: forgetting it was parked would leave it at
        // the corner with nothing left to say it needs bringing back.
        let (pid, f) = frames.get(&window)?;
        let was_parked = self.parked.remove(&window);
        self.enforce_attempts.remove(&window);
        self.pending_repark.remove(&window);
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
                // With the full rig present the docked frame is where the user
                // put the window. A promise made on the laptop is only what
                // macOS left of it, and it can fit the host too: the laptop
                // and the main display share an origin.
                Some(h) if self.full_rig(g) => {
                    let home = self
                        .home
                        .get(&window)
                        .copied()
                        .filter(|hm| g.display_of(hm) == Some(h) && !same_position(hm, &s));
                    match home {
                        Some(hm) => {
                            let want = Rect { x: hm.x, y: hm.y, ..s };
                            self.saved.insert(window, want);
                            (ParkTraceKind::Rehost, want)
                        }
                        None if g.display_of(&s) == Some(h) => (ParkTraceKind::Restore, s),
                        None => self.carry_over(window, s, h, g),
                    }
                }
                Some(h) if g.display_of(&s) == Some(h) => (ParkTraceKind::Restore, s),
                Some(h) => self.carry_over(window, s, h, g),
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

    /// Carry a window that stays on screen onto the display its monitor now
    /// stands on: its `home` frame when that is on the new host, else its
    /// frame carried over proportionally from the display it stands on —
    /// `restore`'s rule, for a window that was never parked.
    fn rehost(
        &mut self,
        window: WindowId,
        frames: &HashMap<WindowId, (Pid, Rect)>,
        g: &Geometry,
    ) -> Option<(Pid, WindowId, Rect)> {
        let (pid, f) = frames.get(&window).copied()?;
        let host = self.host_rect(window, g)?;
        if g.display_of(&f) == Some(host) {
            return None;
        }
        let home = self.home.get(&window).filter(|hm| g.display_of(hm) == Some(host));
        let want = match (home, g.display_of(&f)) {
            (Some(hm), _) => Rect { x: hm.x, y: hm.y, ..f },
            (None, Some(from)) => {
                let t = f.translate_between(&from, &host);
                Rect { x: t.x, y: t.y, ..f }
            }
            (None, None) => clamp_into(&f, &host),
        };
        self.note(ParkTrace::new(window, ParkTraceKind::Rehost).observed(f).requested(want));
        Some((pid, window, want))
    }

    /// A promise made on another display than its host: to the window's
    /// `home` when that is on the host, else carried over proportionally from
    /// the display it was made on, else clamped in. The target replaces the
    /// promise, so what the core is told and what the write asks for stay one
    /// frame.
    fn carry_over(
        &mut self,
        window: WindowId,
        s: Rect,
        host: Rect,
        g: &Geometry,
    ) -> (ParkTraceKind, Rect) {
        let home = self.home.get(&window).filter(|hm| g.display_of(hm) == Some(host));
        let want = match (home, g.display_of(&s)) {
            (Some(hm), _) => Rect { x: hm.x, y: hm.y, ..s },
            (None, Some(from)) => {
                let t = s.translate_between(&from, &host);
                Rect { x: t.x, y: t.y, ..s }
            }
            (None, None) => clamp_into(&s, &host),
        };
        self.saved.insert(window, want);
        (ParkTraceKind::Rehost, want)
    }

    /// Dock dimming: unhide every app with something to show now, and hide
    /// (Cmd+H-style) every app with nothing to show once the user has
    /// settled — both as [`Hiding`] says. With the Dock's
    /// `showhidden` pref, "hidden" renders as a translucent icon — the
    /// closest macOS gets to a per-workspace Dock.
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
        let (here_by_app, elsewhere) = self.apps_on_screen(frames, g);
        let wanted = self.wanted_apps(frames, &here_by_app);
        let hidden = self.hidden_apps(d, frames).clone();
        // A hide Ordo wasn't told of (an app its observer never attached to)
        // shows here as an app none of whose windows on screen are in the
        // window server's list: a hidden app's windows drop out of it. One
        // list read for every app, against a round trip to each.
        let listed: HashSet<WindowId> = d.stack().into_iter().collect();
        let proj = self.ledger.projection();
        let unlisted = |pid: &Pid| {
            frames.iter().all(|(w, (p, _))| {
                p != pid || !self.ledger.visible(*w, &proj) || !listed.contains(w)
            })
        };
        // An app Ordo never hides needs no showing; its windows may float
        // above the layer the window server's list is read at, which would
        // read as a hide nobody announced.
        // An app with nothing on screen is out of that list hidden or not,
        // so for it only this model's own record counts.
        let (shows, showing): (Vec<Pid>, Vec<Pid>) = wanted
            .into_iter()
            .filter(|(pid, wanted)| *wanted && (here_by_app[pid] || hidden.contains(pid)))
            .map(|(pid, _)| pid)
            .partition(|pid| {
                d.can_hide(*pid) && (hidden.contains(pid) || (here_by_app[pid] && unlisted(pid)))
            });
        // Any other app is showing, and nothing un-hidden needs holding.
        for pid in showing {
            self.note(
                ParkTrace::app(pid, ParkTraceKind::AppShown)
                    .ws(current, current)
                    .detail("showing; not asked"),
            );
        }
        let shows: Vec<Unhide> = shows
            .into_iter()
            .map(|pid| Unhide {
                pid,
                hold: elsewhere.get(&pid).cloned().unwrap_or_default(),
            })
            .collect();
        for u in &shows {
            self.hidden_apps(d, frames).remove(&u.pid);
            self.note(
                ParkTrace::app(u.pid, ParkTraceKind::AppShown)
                    .ws(current, current)
                    .detail(format!(
                        "un-hiding; holds {} window(s) parked for other workspaces",
                        u.hold.len()
                    )),
            );
        }
        // Each app's un-hide runs on its own queue, overlapping the others, so
        // a switch costs the slowest app's reveal.
        if !shows.is_empty() {
            d.show_apps(&shows);
        }
        self.hides_due = self.hiding.when.delay().map(|delay| d.now() + delay);
    }

    /// The deferred half of [`Self::apply_app_visibility`], judged against
    /// the screen as it is now rather than as the last switch left it.
    ///
    /// The front app is never hidden: hiding the active app makes macOS fling
    /// focus somewhere arbitrary. That is the front app, not the owner of the
    /// key window — Finder holding the desktop has no key window, and hiding
    /// it threw the desktop's focus away. When the exemption bites, the app
    /// just stays undimmed.
    fn hide_idle_apps(
        &mut self,
        d: &dyn Desktop,
        frames: &HashMap<WindowId, (Pid, Rect)>,
        g: &Geometry,
    ) {
        let current = self.ledger.current();
        let (here_by_app, elsewhere) = self.apps_on_screen(frames, g);
        let wanted = self.wanted_apps(frames, &here_by_app);
        let focused_app = d.frontmost_app();
        let hidden = self.hidden_apps(d, frames).clone();
        for (pid, wanted) in wanted {
            if wanted
                || Some(pid) == focused_app
                || hidden.contains(&pid)
                || !d.can_hide(pid)
            {
                continue;
            }
            let parked = elsewhere.get(&pid).map_or(0, |h| h.len());
            d.hide_app(pid);
            self.hidden_apps(d, frames).insert(pid);
            self.note(
                ParkTrace::app(pid, ParkTraceKind::AppHidden)
                    .ws(current, current)
                    .detail(format!("hidden; {parked} window(s) parked")),
            );
        }
    }

    /// The apps hidden right now, filled on first use by asking each app the
    /// ledger has a window of.
    fn hidden_apps(
        &mut self,
        d: &dyn Desktop,
        frames: &HashMap<WindowId, (Pid, Rect)>,
    ) -> &mut HashSet<Pid> {
        if self.hidden_apps.is_none() {
            let apps: HashSet<Pid> = frames
                .iter()
                .filter(|(w, _)| self.ledger.claim(**w).is_some())
                .map(|(_, (pid, _))| *pid)
                .collect();
            let hidden = apps
                .into_iter()
                .filter(|pid| d.app_hidden(*pid) == Some(true))
                .collect();
            self.hidden_apps = Some(hidden);
        }
        self.hidden_apps.as_mut().unwrap()
    }

    /// The user changed when apps are hidden. Judged at the next pass, which
    /// has the frames and runs only while Ordo is engaged: an app the new
    /// setting wants shown is un-hidden then, and hides wait the new delay.
    pub fn set_hiding(&mut self, hiding: Hiding) {
        if hiding != self.hiding {
            self.hiding = hiding;
            self.hiding_changed = true;
        }
    }

    /// The shell is about to bring this app to the front, which un-hides it.
    /// If this model hid it, it's no longer hidden, and here are its parked
    /// windows to hold through the reveal. The desktop is the case: focusing
    /// an empty workspace fronts Finder, which may have been hidden with a
    /// window parked elsewhere.
    pub fn reveal_for_focus(&mut self, d: &dyn Desktop, pid: Pid) -> Option<Vec<(WindowId, Point)>> {
        if !self.hidden_apps.as_ref().is_some_and(|h| h.contains(&pid)) {
            return None;
        }
        let frames = current_frames(d, self.ledger.window_ws().into_keys());
        let g = Geometry::read(d);
        let (_, elsewhere) = self.apps_on_screen(&frames, &g);
        self.hidden_apps(d, &frames).remove(&pid);
        let current = self.ledger.current();
        let hold = elsewhere.get(&pid).cloned().unwrap_or_default();
        self.note(
            ParkTrace::app(pid, ParkTraceKind::AppShown)
                .ws(current, current)
                .detail(format!("un-hiding to focus it; holds {} parked window(s)", hold.len())),
        );
        Some(hold)
    }

    /// An app was hidden or shown, by anyone: Ordo's own hides and un-hides
    /// arrive here too. Acted on at the next pass, which has the frames.
    pub fn note_app_visibility(&mut self, pid: Pid, hidden: bool) {
        self.visibility_news.push((pid, hidden));
    }

    /// Hiding is Ordo's alone. An app hidden some other way (Cmd+H, Hide
    /// Others, the app itself) with a window on screen is shown again, its
    /// parked windows held as a switch's un-hide holds them; one with nothing
    /// on screen is already as Ordo wants it, and is kept hidden as if Ordo
    /// had hidden it. A shown app is simply no longer hidden.
    fn reconcile_visibility(
        &mut self,
        d: &dyn Desktop,
        frames: &HashMap<WindowId, (Pid, Rect)>,
        g: &Geometry,
    ) {
        let news = std::mem::take(&mut self.visibility_news);
        if news.is_empty() {
            return;
        }
        let current = self.ledger.current();
        let (here_by_app, elsewhere) = self.apps_on_screen(frames, g);
        // The latest word per app wins.
        let mut latest: Vec<(Pid, bool)> = Vec::new();
        for (pid, hidden) in news {
            latest.retain(|(p, _)| *p != pid);
            latest.push((pid, hidden));
        }
        let mut shows = Vec::new();
        for (pid, hidden) in latest {
            let known = self.hidden_apps(d, frames);
            if !hidden {
                known.remove(&pid);
            } else if known.contains(&pid) {
                // Ordo's own hide.
            } else if here_by_app.get(&pid) == Some(&true) {
                shows.push(Unhide {
                    pid,
                    hold: elsewhere.get(&pid).cloned().unwrap_or_default(),
                });
            } else if here_by_app.contains_key(&pid) {
                known.insert(pid);
                self.note(
                    ParkTrace::app(pid, ParkTraceKind::AppHidden)
                        .ws(current, current)
                        .detail("hidden outside Ordo; kept hidden, nothing of it is on screen"),
                );
            }
        }
        if shows.is_empty() {
            return;
        }
        for u in &shows {
            self.note(
                ParkTrace::app(u.pid, ParkTraceKind::AppShown)
                    .ws(current, current)
                    .detail(format!(
                        "hidden outside Ordo; shown again, {} parked window(s) held",
                        u.hold.len()
                    )),
            );
        }
        d.show_apps(&shows);
    }

    /// Per app: whether any of its ledger windows is on screen, and where
    /// each of its other ledger windows is parked. The park REQUEST is the
    /// anchor when there is one; without it (a promise loaded from disk, a
    /// window this process never parked) the corner is recomputed, which is
    /// the same answer because a park depends only on the window's width and
    /// keeps its y.
    fn apps_on_screen(
        &self,
        frames: &HashMap<WindowId, (Pid, Rect)>,
        g: &Geometry,
    ) -> (HashMap<Pid, bool>, ParkedByApp) {
        let proj = self.ledger.projection();
        let mut here_by_app: HashMap<Pid, bool> = HashMap::new();
        let mut elsewhere: ParkedByApp = HashMap::new();
        for (window, (pid, f)) in frames {
            if self.ledger.claim(*window).is_none() {
                continue;
            }
            let visible = self.ledger.visible(*window, &proj);
            *here_by_app.entry(*pid).or_insert(false) |= visible;
            if !visible {
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
        (here_by_app, elsewhere)
    }

    /// Per app with a declared window, whether [`Hiding`] wants it shown,
    /// given whether it has a window on screen.
    fn wanted_apps(
        &self,
        frames: &HashMap<WindowId, (Pid, Rect)>,
        here_by_app: &HashMap<Pid, bool>,
    ) -> HashMap<Pid, bool> {
        match (self.hiding.when, self.hiding.idle) {
            (HideWhen::Never, _) => here_by_app.keys().map(|pid| (*pid, true)).collect(),
            (_, Idle::OffScreen) => here_by_app.clone(),
            (_, Idle::OffWorkspace) => {
                let current = self.ledger.current();
                let mut wanted: HashMap<Pid, bool> =
                    here_by_app.keys().map(|pid| (*pid, false)).collect();
                for (window, (pid, _)) in frames {
                    if self.ledger.claim(*window).is_some_and(|c| c.ws == current) {
                        wanted.insert(*pid, true);
                    }
                }
                wanted
            }
        }
    }
}

fn current_frames(
    d: &dyn Desktop,
    windows: impl IntoIterator<Item = WindowId>,
) -> HashMap<WindowId, (Pid, Rect)> {
    let ids: Vec<WindowId> = windows.into_iter().collect();
    d.frames(&ids)
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
    // The retired corners, both bottom-flush: the rightmost display's bottom
    // right (through 2026-09-02), and the main display's right edge. y confines
    // the window's TOP edge to the bottom CLAMP_SLACK points of the display,
    // where it shows less than a title bar's worth of itself. (The one used
    // briefly on 2026-09-01, right-aligned INSIDE the main display, is no
    // longer recognized: a wide window dragged flush right and low lands
    // exactly there, and was taken for parked, run 58.)
    let retired = [
        (
            g.legacy_host.x + g.legacy_host.w - SLIVER,
            g.legacy_host.y + g.legacy_host.h - SLIVER,
        ),
        (g.main.x + g.main.w - SLIVER, g.main.y + g.main.h - SLIVER),
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
        /// A hide drags the app's windows parked off the left edge back to
        /// it, keeping their y — what AppKit was caught doing, sometimes.
        yank_on_hide: std::cell::Cell<bool>,
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
        /// An active app with no key window (Finder on the desktop); else the
        /// front app is the focused window's.
        front: std::cell::Cell<Option<Pid>>,
        now: std::cell::Cell<Instant>,
        /// Times an app was asked whether it is hidden: a round trip to the
        /// app's main thread each, which a busy app is slow to answer.
        asked: std::cell::Cell<u32>,
        /// Background apps whose windows are managed but which Ordo may not hide.
        unhideable: std::cell::RefCell<HashSet<Pid>>,
        /// While set, moves wait in `queue` until `land_queued`, as the real
        /// port's writes wait on each app's thread; a queued window is in
        /// flight.
        queueing: std::cell::Cell<bool>,
        queue: std::cell::RefCell<Vec<Move>>,
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
                yank_on_hide: std::cell::Cell::new(false),
                hidden: std::cell::RefCell::new(HashSet::new()),
                displays: std::cell::RefCell::new(vec![MAIN, SECOND]),
                focused: std::cell::Cell::new(None),
                front: std::cell::Cell::new(None),
                now: std::cell::Cell::new(Instant::now()),
                asked: std::cell::Cell::new(0),
                unhideable: std::cell::RefCell::new(HashSet::new()),
                queueing: std::cell::Cell::new(false),
                queue: std::cell::RefCell::new(Vec::new()),
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

        /// The user stays put long enough for the deferred hides, and the
        /// next snapshot carries them out.
        fn settle(&self, b: &mut EmulatedWorkspaces) {
            let delay = HideWhen::Settled.delay().expect("Settled hides");
            self.now.set(self.now.get() + delay);
            b.enforce_placement(self, &self.scan());
        }

        fn thaw(&self) {
            self.frozen.set(false);
        }

        /// The apps get round to every queued move, in order.
        fn land_queued(&self) {
            self.queueing.set(false);
            let queued = std::mem::take(&mut *self.queue.borrow_mut());
            self.move_windows(&queued);
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
        fn scan(&self) -> HashMap<WindowId, (Pid, Rect)> {
            self.windows.borrow().clone().into_iter().collect()
        }

        /// A scan that saw only these windows, under these apps — what an
        /// app blowing its AX timeout, or an id recycled by another app,
        /// hands the model.
        fn scan_only(&self, seen: &[(WindowId, Pid)]) -> HashMap<WindowId, (Pid, Rect)> {
            let ws = self.windows.borrow();
            seen.iter().map(|(w, p)| (*w, (*p, ws[w].1))).collect()
        }
    }

    impl Desktop for FakeDesktop {
        fn frames(&self, windows: &[WindowId]) -> Vec<(WindowId, Pid, Rect)> {
            let ws = self.windows.borrow();
            windows
                .iter()
                .filter_map(|w| ws.get(w).map(|(p, f)| (*w, *p, *f)))
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
        fn move_windows(&self, moves: &[Move]) {
            if self.frozen.get() {
                return;
            }
            if self.queueing.get() {
                self.queue.borrow_mut().extend_from_slice(moves);
                return;
            }
            for m in moves {
                let size_is_the_windows_own = self.windows.borrow()[&m.window].1;
                self.land(
                    m.pid,
                    m.window,
                    Rect {
                        x: m.to.x,
                        y: m.to.y,
                        ..size_is_the_windows_own
                    },
                );
            }
        }

        fn in_flight(&self, window: WindowId) -> bool {
            self.queue.borrow().iter().any(|m| m.window == window)
        }

        fn busy(&self) -> bool {
            !self.queue.borrow().is_empty()
        }

        fn hide_app(&self, pid: Pid) {
            self.hidden.borrow_mut().insert(pid);
            if self.yank_on_hide.get() {
                let edge = self.displays.borrow().iter().map(|d| d.x).fold(f64::INFINITY, f64::min);
                for (p, f) in self.windows.borrow_mut().values_mut() {
                    if *p == pid && f.x < edge {
                        f.x = edge;
                    }
                }
            }
        }

        fn app_hidden(&self, pid: Pid) -> Option<bool> {
            self.asked.set(self.asked.get() + 1);
            Some(self.hidden.borrow().contains(&pid))
        }

        fn can_hide(&self, pid: Pid) -> bool {
            !self.unhideable.borrow().contains(&pid)
        }

        fn now(&self) -> Instant {
            self.now.get()
        }

        fn traces_stacks(&self) -> bool {
            true
        }

        fn stack(&self) -> Vec<WindowId> {
            let hidden = self.hidden.borrow();
            let mut ids: Vec<WindowId> = self
                .windows
                .borrow()
                .iter()
                .filter(|(_, (p, _))| !hidden.contains(p))
                .map(|(w, _)| *w)
                .collect();
            ids.sort_by_key(|w| w.0);
            ids
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
        fn show_apps(&self, apps: &[Unhide]) {
            for a in apps {
                // The port asks each app before deciding to un-hide it.
                self.asked.set(self.asked.get() + 1);
                let unhid = self.hidden.borrow().contains(&a.pid);
                let revealed = a.hold.iter().any(|(w, at)| {
                    let f = self.windows.borrow()[w].1;
                    !same_position(&f, &Rect { x: at.x, y: at.y, ..f })
                });
                // A frozen app applies nothing — neither our writes nor its
                // own reveal — so it stays hidden and everything holds still.
                if (unhid || revealed) && !self.frozen.get() {
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
            }
        }

        fn focused_window(&self) -> Option<WindowId> {
            self.focused.get()
        }

        fn frontmost_app(&self) -> Option<Pid> {
            self.front.get().or_else(|| {
                let w = self.focused.get()?;
                self.windows.borrow().get(&w).map(|(p, _)| *p)
            })
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
    /// A parked window found off its park says whether its app was still
    /// hidden — nothing showed — or had come back, which is the flash.
    #[test]
    fn a_parked_window_found_off_its_park_says_whether_its_app_was_hidden() {
        let d = FakeDesktop::new(&[
            (w(1), Pid(10), rect(100.0, 100.0)),
            (w(2), Pid(20), rect(300.0, 200.0)),
        ]);
        let mut b = EmulatedWorkspaces::new(3);
        b.note_scan(&d, &d.scan());
        b.assign_window_to_workspace(w(2), ws(2)).unwrap();
        b.switch_workspace(&d, ws(2));
        d.yank_on_hide.set(true);
        d.hide_app(Pid(10)); // AppKit's pull, while the app stays hidden
        b.take_trace();

        rescan(&d, &mut b);

        let found = b
            .take_trace()
            .into_iter()
            .find(|t| t.window == w(1) && t.kind == ParkTraceKind::Reassert)
            .expect("the pulled window is re-parked");
        assert!(found.detail.unwrap().contains("app 10 hidden: yes"));
    }

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
        // And where the switch's own time went.
        assert!(sw.cost.is_some());

        // The app owns a window here and Ordo never hid it, so it is left
        // alone, not even asked — an un-hide sent to a showing app brings its
        // parked windows forward — and the record says so.
        let shown = trace
            .iter()
            .find(|t| t.kind == ParkTraceKind::AppShown)
            .expect("the visibility pass is attributable");
        assert_eq!(shown.pid, Some(Pid(10)));
        assert_eq!(shown.detail.as_deref(), Some("showing; not asked"));
        assert!(in_park_corner(&d.frame(w(1)), &geo()));
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
        d.settle(&mut b);
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

        let frames = d.scan();
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
        let frames = d.scan();
        let believed = b.believed_frames(&d, &frames);
        assert_eq!(believed.get(&w(1)), Some(&rect(100.0, 100.0)));
    }

    #[test]
    fn park_never_captures_a_sliver_as_the_saved_frame() {
        let mut b = EmulatedWorkspaces::new(3);
        let sliver = park_frame(rect(100.0, 100.0), &geo());
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
            (w(4), (Pid(42), legacy)),
            (w(5), (Pid(42), retired_right)),
        ]
        .into();

        // A window already at a park artifact — the current corner or either
        // retired corner: bookkept as parked, but its (unknown) real frame is
        // never fabricated from the artifact.
        for id in [w(1), w(4), w(5)] {
            assert_eq!(b.park(id, None, &frames, &geo(), false), None, "{id:?}");
            assert!(b.parked.contains(&id));
            assert!(!b.saved.contains_key(&id), "{id:?} captured an artifact");
        }

        // A window at a real position parks normally.
        let write = b.park(w(2), None, &frames, &geo(), false).unwrap();
        assert_eq!(b.saved[&w(2)], rect(50.0, 60.0));
        assert_eq!(write.2, park_frame(rect(50.0, 60.0), &geo()));
    }

    /// A window the user drags low, hanging off its display's bottom edge, is
    /// put back exactly there after a round trip (run 58: the screenshot
    /// window, 486 tall, was clamped back up, and dragged flush right as well
    /// it was taken for parked and put back where it had been before).
    #[test]
    fn a_window_hung_off_the_bottom_edge_comes_back_where_the_user_left_it() {
        let low_right = Rect {
            x: MAIN.w - 1560.0,
            y: MAIN.h - 86.0,
            w: 1560.0,
            h: 486.0,
        };
        let low_left = Rect {
            x: 316.0,
            y: 941.0,
            w: 1560.0,
            h: 486.0,
        };
        for spot in [low_right, low_left] {
            let d = FakeDesktop::new(&[(w(1), Pid(10), rect(100.0, 100.0)), (w(4), Pid(40), spot)]);
            let mut b = EmulatedWorkspaces::new(3);
            rescan(&d, &mut b);
            b.switch_workspace(&d, ws(2));
            assert!(in_park_corner(&d.frame(w(4)), &geo()));
            b.switch_workspace(&d, ws(1));
            assert_eq!(d.frame(w(4)), spot);
        }
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
                d.move_windows(&[Move {
                    pid: Pid(10),
                    window: w(1),
                    to: Point {
                        x: want.x,
                        y: want.y,
                    },
                    parks: true,
                }]);
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

        b.note_scan(&d, &d.scan_only(&[(w(2), Pid(20))])); // partial: w1 missing, alive
        assert_eq!(b.window_ws()[&w(1)], ws(3), "declaration kept");
        assert_eq!(b.saved[&w(1)], rect(100.0, 100.0), "promise kept");

        // Re-sighted next scan: same identity, nothing to adopt.
        b.note_scan(&d, &d.scan());
        assert_eq!(b.window_ws()[&w(1)], ws(3));

        // A failed CG read is not evidence either.
        d.cg_down.set(true);
        d.close(w(1));
        b.note_scan(&d, &d.scan_only(&[(w(2), Pid(20))]));
        assert_eq!(b.window_ws()[&w(1)], ws(3), "no evidence, no forgetting");

        // CG back up and the window really is gone: forgotten everywhere.
        d.cg_down.set(false);
        b.note_scan(&d, &d.scan_only(&[(w(2), Pid(20))]));
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
        b.note_scan(&d, &d.scan_only(&[(w(1), Pid(99)), (w(2), Pid(20))]));
        assert_eq!(b.window_ws()[&w(1)], ws(1));
        assert!(!b.saved.contains_key(&w(1)));
        assert!(!b.parked.contains(&w(1)));
    }

    fn frames_of(d: &FakeDesktop) -> HashMap<WindowId, (Pid, Rect)> {
        d.scan()
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
            monitors_assigned: true,
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

    /// Writes land after the switch that asked for them. Going there and
    /// straight back, the second switch parks a window whose restore hasn't
    /// landed: it still reads as parked. Skipping its park on that reading
    /// let the restore land afterwards and leave it on the wrong workspace,
    /// and saving the sliver it read would have lost where it belongs.
    #[test]
    fn a_window_whose_restore_is_still_on_its_way_is_parked_when_the_user_turns_back() {
        let d = FakeDesktop::new(&[
            (w(1), Pid(10), rect(100.0, 100.0)),
            (w(2), Pid(20), rect(300.0, 200.0)),
        ]);
        let mut b = EmulatedWorkspaces::new(3);
        rescan(&d, &mut b);
        b.assign_window_to_workspace(w(2), ws(2)).unwrap();
        b.switch_workspace(&d, ws(2));
        rescan(&d, &mut b);

        d.queueing.set(true);
        b.switch_workspace(&d, ws(1));
        b.switch_workspace(&d, ws(2));
        d.land_queued();

        assert!(in_park_corner(&d.frame(w(1)), &geo()), "{:?}", d.frame(w(1)));
        assert_eq!(d.frame(w(2)), rect(300.0, 200.0));
        assert_eq!(b.saved[&w(1)], rect(100.0, 100.0), "the promise, not the sliver");
    }

    /// The mirror: leave and turn straight back before the parks land. The
    /// restore is decided while the window still reads where it stands, and
    /// must be written anyway, after the park, or the park lands last and
    /// strands the window off screen on the workspace the user is on.
    #[test]
    fn a_window_whose_park_is_still_on_its_way_comes_back_when_the_user_turns_back() {
        let d = FakeDesktop::new(&[
            (w(1), Pid(10), rect(100.0, 100.0)),
            (w(2), Pid(20), rect(300.0, 200.0)),
        ]);
        let mut b = EmulatedWorkspaces::new(3);
        rescan(&d, &mut b);
        b.assign_window_to_workspace(w(2), ws(2)).unwrap();
        b.switch_workspace(&d, ws(2));
        b.switch_workspace(&d, ws(1));
        rescan(&d, &mut b);

        d.queueing.set(true);
        b.switch_workspace(&d, ws(2));
        b.switch_workspace(&d, ws(1));
        d.land_queued();
        rescan(&d, &mut b);

        assert_eq!(d.frame(w(1)), rect(100.0, 100.0));
        assert!(in_park_corner(&d.frame(w(2)), &geo()), "{:?}", d.frame(w(2)));
        assert!(!b.parked.contains(&w(1)));
    }

    /// A burst through one app's workspaces, faster than its writes land:
    /// once they have, each window is where the last press put it.
    #[test]
    fn a_burst_leaves_each_window_where_the_last_press_put_it() {
        let d = FakeDesktop::new(&[
            (w(1), Pid(10), rect(100.0, 100.0)),
            (w(2), Pid(10), rect(300.0, 200.0)),
            (w(3), Pid(10), rect(500.0, 300.0)),
        ]);
        let mut b = EmulatedWorkspaces::new(3);
        rescan(&d, &mut b);
        b.assign_window_to_workspace(w(2), ws(2)).unwrap();
        b.assign_window_to_workspace(w(3), ws(3)).unwrap();
        b.switch_workspace(&d, ws(2));
        b.switch_workspace(&d, ws(1));
        rescan(&d, &mut b);

        d.queueing.set(true);
        b.switch_workspace(&d, ws(2));
        b.switch_workspace(&d, ws(3));
        b.switch_workspace(&d, ws(2));
        d.land_queued();
        rescan(&d, &mut b);

        assert_eq!(d.frame(w(2)), rect(300.0, 200.0));
        assert!(in_park_corner(&d.frame(w(1)), &geo()), "{:?}", d.frame(w(1)));
        assert!(in_park_corner(&d.frame(w(3)), &geo()), "{:?}", d.frame(w(3)));
    }

    /// Focusing an empty workspace fronts the desktop's owner, which un-hides
    /// it if Ordo hid it. The model hands over what to hold through that, once.
    #[test]
    fn fronting_a_hidden_app_hands_over_its_parked_windows_once() {
        let d = FakeDesktop::new(&[
            (w(1), Pid(10), rect(100.0, 100.0)),
            (w(2), Pid(20), rect(300.0, 200.0)),
        ]);
        let mut b = EmulatedWorkspaces::new(3);
        rescan(&d, &mut b);
        b.assign_window_to_workspace(w(2), ws(2)).unwrap();
        b.switch_workspace(&d, ws(2));
        d.settle(&mut b);
        assert!(d.is_hidden(Pid(10)));

        let hold = b.reveal_for_focus(&d, Pid(10)).expect("Ordo hid it");
        assert_eq!(hold.iter().map(|(w, _)| *w).collect::<Vec<_>>(), [w(1)]);
        assert!(b.reveal_for_focus(&d, Pid(10)).is_none(), "no longer hidden");
        assert!(b.reveal_for_focus(&d, Pid(20)).is_none(), "never hidden");
    }

    /// The port is told which moves park, so an un-hide still to come can
    /// hold them.
    #[test]
    fn a_switch_says_which_of_its_moves_park() {
        let d = FakeDesktop::new(&[
            (w(1), Pid(10), rect(100.0, 100.0)),
            (w(2), Pid(20), rect(300.0, 200.0)),
        ]);
        let mut b = EmulatedWorkspaces::new(3);
        rescan(&d, &mut b);
        b.assign_window_to_workspace(w(2), ws(2)).unwrap();
        b.switch_workspace(&d, ws(2));
        rescan(&d, &mut b);

        d.queueing.set(true);
        b.switch_workspace(&d, ws(1));
        let parks: Vec<(WindowId, bool)> =
            d.queue.borrow().iter().map(|m| (m.window, m.parks)).collect();
        assert!(parks.contains(&(w(2), true)), "{parks:?}");
        assert!(parks.contains(&(w(1), false)), "{parks:?}");
    }

    /// A scan taken while our own park is on its way shows the window where
    /// it was. That is not the app fighting the park: no second write, and
    /// nothing charged against the window.
    #[test]
    fn a_scan_older_than_our_own_park_neither_re_parks_nor_charges_the_window() {
        let d = FakeDesktop::new(&[(w(1), Pid(10), rect(100.0, 100.0))]);
        let mut b = EmulatedWorkspaces::new(3);
        rescan(&d, &mut b);
        d.queueing.set(true);
        b.move_window_to_workspace(&d, w(1), ws(2)).unwrap();
        b.take_trace();

        rescan(&d, &mut b);

        assert_eq!(d.queue.borrow().len(), 1, "the one park, not a second");
        assert!(b.enforce_attempts.is_empty());
        assert!(!b.take_trace().iter().any(|t| matches!(
            t.kind,
            ParkTraceKind::Reassert | ParkTraceKind::Suppressed
        )));
        d.land_queued();
        assert!(in_park_corner(&d.frame(w(1)), &geo()));
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
        d.settle(&mut b);
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

    /// On one display, the monitor not being viewed is parked, and by default
    /// its apps are dimmed like any with nothing on screen. Counting the
    /// workspace instead, they stay shown until the user leaves it.
    #[test]
    fn hiding_by_workspace_spares_apps_on_the_monitor_not_viewed() {
        let (d, mut b) = rig();
        d.set_displays(&[MAIN]);
        rescan(&d, &mut b);
        b.set_hiding(Hiding { when: HideWhen::Settled, idle: Idle::OffWorkspace });
        rescan(&d, &mut b);
        d.settle(&mut b);
        assert!(in_park_corner(&d.frame(w(2)), &geo()));
        assert!(!d.is_hidden(Pid(20)));

        b.switch_workspace(&d, ws(2));
        d.settle(&mut b);
        assert!(d.is_hidden(Pid(10)));
        assert!(d.is_hidden(Pid(20)));
    }

    /// Turned off, hiding un-hides what Ordo hid, still holding the windows
    /// parked for other workspaces, and hides nothing after that.
    #[test]
    fn hiding_off_shows_what_was_hidden_and_hides_nothing_more() {
        let d = FakeDesktop::new(&[
            (w(1), Pid(10), rect(100.0, 100.0)),
            (w(2), Pid(20), rect(300.0, 200.0)),
        ]);
        let mut b = EmulatedWorkspaces::new(3);
        rescan(&d, &mut b);
        b.assign_window_to_workspace(w(2), ws(2)).unwrap();
        b.switch_workspace(&d, ws(2));
        d.settle(&mut b);
        assert!(d.is_hidden(Pid(10)));

        b.set_hiding(Hiding { when: HideWhen::Never, ..Hiding::default() });
        rescan(&d, &mut b);
        assert!(!d.is_hidden(Pid(10)));
        assert!(in_park_corner(&d.frame(w(1)), &geo()), "held where it was parked");

        b.switch_workspace(&d, ws(1));
        d.settle(&mut b);
        assert!(!d.is_hidden(Pid(20)));
        assert!(in_park_corner(&d.frame(w(2)), &geo()));
    }

    /// Delayed, a pause that would have hidden an app doesn't: only a longer
    /// stay does.
    #[test]
    fn delayed_hiding_waits_out_a_short_pause() {
        let d = FakeDesktop::new(&[
            (w(1), Pid(10), rect(100.0, 100.0)),
            (w(2), Pid(20), rect(300.0, 200.0)),
        ]);
        let mut b = EmulatedWorkspaces::new(3);
        rescan(&d, &mut b);
        b.set_hiding(Hiding { when: HideWhen::Delayed, ..Hiding::default() });
        b.assign_window_to_workspace(w(2), ws(2)).unwrap();
        b.switch_workspace(&d, ws(2));
        d.settle(&mut b);
        assert!(!d.is_hidden(Pid(10)));

        d.now.set(d.now.get() + HideWhen::Delayed.delay().unwrap());
        b.enforce_placement(&d, &d.scan());
        assert!(d.is_hidden(Pid(10)));
    }

    /// A switch to an empty workspace hands focus to the desktop: Finder is
    /// then the front app with no window key. Hiding spares the front app, so
    /// Finder stays; spared by key window instead, it was hidden and the
    /// desktop's focus thrown somewhere arbitrary.
    #[test]
    fn the_front_app_is_never_hidden_even_with_no_window_key() {
        let d = FakeDesktop::new(&[
            (w(1), Pid(10), rect(100.0, 100.0)),
            (w(3), Pid(30), rect(300.0, 300.0)),
        ]);
        let mut b = EmulatedWorkspaces::new(3);
        rescan(&d, &mut b);
        d.front.set(Some(Pid(30)));
        b.switch_workspace(&d, ws(2));
        d.settle(&mut b);
        assert!(d.is_hidden(Pid(10)));
        assert!(!d.is_hidden(Pid(30)), "the front app is spared");
    }

    /// The screenshot tool is a background app: no Dock icon would show it
    /// again, so it is never hidden, nor sent an un-hide. Its window is parked
    /// and brought back like any other.
    #[test]
    fn a_background_app_is_parked_but_never_hidden() {
        let d = FakeDesktop::new(&[
            (w(1), Pid(10), rect(100.0, 100.0)),
            (w(4), Pid(40), rect(300.0, 300.0)),
        ]);
        d.unhideable.borrow_mut().insert(Pid(40));
        let mut b = EmulatedWorkspaces::new(3);
        rescan(&d, &mut b);
        b.switch_workspace(&d, ws(2));
        d.settle(&mut b);
        assert!(in_park_corner(&d.frame(w(4)), &geo()));
        assert!(d.is_hidden(Pid(10)));
        assert!(!d.is_hidden(Pid(40)));
        b.take_trace();

        b.switch_workspace(&d, ws(1));
        assert_eq!(d.frame(w(4)), rect(300.0, 300.0));
        let unhid_40 = b.take_trace().iter().any(|t| {
            t.kind == ParkTraceKind::AppShown
                && t.pid == Some(Pid(40))
                && t.detail.as_deref().is_some_and(|d| d.starts_with("un-hiding"))
        });
        assert!(!unhid_40);
    }

    /// A switch focuses the destination's window before it un-hides, and
    /// focusing a window of a hidden app un-hides that app with nothing
    /// holding its parked windows: AppKit drags them on screen. Arriving, the
    /// app reads as showing — so it is not sent an un-hide, which would bring
    /// all its windows forward — but its parked window is held back anyway.
    #[test]
    fn an_app_revealed_behind_the_switch_still_has_its_parked_windows_held() {
        let d = FakeDesktop::new(&[
            (w(1), Pid(10), rect(100.0, 100.0)),
            (w(2), Pid(10), rect(300.0, 200.0)),
            (w(3), Pid(20), rect(500.0, 300.0)),
        ]);
        let mut b = EmulatedWorkspaces::new(3);
        rescan(&d, &mut b);
        b.assign_window_to_workspace(w(2), ws(2)).unwrap();
        b.assign_window_to_workspace(w(3), ws(3)).unwrap();
        b.switch_workspace(&d, ws(2));
        b.switch_workspace(&d, ws(3));
        d.settle(&mut b);
        assert!(d.is_hidden(Pid(10)));

        // The focus request's un-hide, which nothing held.
        d.hidden.borrow_mut().remove(&Pid(10));
        d.place(w(2), rect(0.0, 200.0));
        b.take_trace();

        b.switch_workspace(&d, ws(1));

        assert_eq!(d.frame(w(1)), rect(100.0, 100.0));
        assert!(in_park_corner(&d.frame(w(2)), &geo()), "{:?}", d.frame(w(2)));
    }

    /// Hiding is Ordo's alone. Cmd+H on an app with a window on screen is
    /// undone, and the un-hide holds its windows parked for other workspaces,
    /// which revealing the app would otherwise drag back on screen.
    #[test]
    fn an_app_hidden_outside_ordo_with_a_window_on_screen_is_shown_again() {
        let d = FakeDesktop::new(&[
            (w(1), Pid(10), rect(100.0, 100.0)),
            (w(2), Pid(10), rect(300.0, 200.0)),
        ]);
        let mut b = EmulatedWorkspaces::new(3);
        rescan(&d, &mut b);
        b.assign_window_to_workspace(w(2), ws(2)).unwrap();
        rescan(&d, &mut b);

        d.hidden.borrow_mut().insert(Pid(10));
        b.note_app_visibility(Pid(10), true);
        rescan(&d, &mut b);

        assert!(!d.is_hidden(Pid(10)));
        assert_eq!(d.frame(w(1)), rect(100.0, 100.0));
        assert!(in_park_corner(&d.frame(w(2)), &geo()));
    }

    /// Cmd+H on an app with nothing on screen leaves it as Ordo wants it.
    /// Ordo takes it as its own hide, so switching to the app's workspace
    /// shows it.
    #[test]
    fn an_app_hidden_outside_ordo_with_nothing_on_screen_stays_hidden_until_needed() {
        let d = FakeDesktop::new(&[
            (w(1), Pid(10), rect(100.0, 100.0)),
            (w(2), Pid(20), rect(300.0, 200.0)),
        ]);
        let mut b = EmulatedWorkspaces::new(3);
        rescan(&d, &mut b);
        b.assign_window_to_workspace(w(2), ws(2)).unwrap();
        rescan(&d, &mut b);

        d.hidden.borrow_mut().insert(Pid(20));
        b.note_app_visibility(Pid(20), true);
        rescan(&d, &mut b);
        assert!(d.is_hidden(Pid(20)));

        b.switch_workspace(&d, ws(2));
        assert!(!d.is_hidden(Pid(20)));
        assert_eq!(d.frame(w(2)), rect(300.0, 200.0));
    }

    /// Some apps never get an observer, so a hide of theirs goes unheard.
    /// Their windows are missing from the window server's list, which is what
    /// gives it away: the switch asks that app after all, and shows it.
    #[test]
    fn an_unheard_hide_is_still_undone_by_the_next_switch() {
        let d = FakeDesktop::new(&[
            (w(1), Pid(10), rect(100.0, 100.0)),
            (w(2), Pid(20), rect(300.0, 200.0)),
        ]);
        let mut b = EmulatedWorkspaces::new(3);
        rescan(&d, &mut b);
        b.assign_window_to_workspace(w(2), ws(2)).unwrap();
        rescan(&d, &mut b);
        d.hidden.borrow_mut().insert(Pid(10));

        b.switch_workspace(&d, ws(2));
        b.switch_workspace(&d, ws(1));

        assert!(!d.is_hidden(Pid(10)));
    }

    /// Asking an app whether it is hidden is a round trip to its main thread,
    /// slow when the app is busy. A switch asks only the apps Ordo hid.
    #[test]
    fn a_switch_asks_only_the_apps_ordo_hid() {
        let d = FakeDesktop::new(&[
            (w(1), Pid(10), rect(100.0, 100.0)),
            (w(2), Pid(10), rect(300.0, 200.0)),
            (w(3), Pid(20), rect(500.0, 300.0)),
        ]);
        let mut b = EmulatedWorkspaces::new(3);
        rescan(&d, &mut b);
        b.assign_window_to_workspace(w(2), ws(2)).unwrap();
        b.assign_window_to_workspace(w(3), ws(2)).unwrap();
        b.switch_workspace(&d, ws(2));
        b.switch_workspace(&d, ws(1));
        d.settle(&mut b);
        assert!(d.is_hidden(Pid(20)));
        d.asked.set(0);

        b.switch_workspace(&d, ws(2));

        assert_eq!(d.asked.get(), 1, "Pid(20), which Ordo hid; not Pid(10)");
        assert!(!d.is_hidden(Pid(20)));
    }

    /// An app is dimmed only once the user stays away from its windows. A
    /// quick trip out and back never hides it, so it never has to come back
    /// through an un-hide.
    #[test]
    fn a_quick_round_trip_never_hides_the_app_it_left() {
        let d = FakeDesktop::new(&[
            (w(1), Pid(10), rect(100.0, 100.0)),
            (w(2), Pid(20), rect(300.0, 200.0)),
        ]);
        let mut b = EmulatedWorkspaces::new(3);
        rescan(&d, &mut b);
        b.assign_window_to_workspace(w(2), ws(2)).unwrap();

        b.switch_workspace(&d, ws(2));
        rescan(&d, &mut b);
        assert!(!d.is_hidden(Pid(10)), "not yet: the user may be passing through");
        b.switch_workspace(&d, ws(1));
        d.settle(&mut b);
        assert!(!d.is_hidden(Pid(10)));
        assert!(d.is_hidden(Pid(20)), "the app left behind once settled is");
        assert!(in_park_corner(&d.frame(w(2)), &geo()));
        assert_eq!(d.frame(w(1)), rect(100.0, 100.0));
    }

    /// Unplugging piles windows onto the laptop: two staggered windows on
    /// the main display end up at the same spot, one hiding the other. The
    /// laptop shares the main display's origin, so the pile also fits the
    /// main display — and both the window left on screen and the one hidden
    /// while undocked went back to the pile, not to where they were docked.
    /// The first scan after the replug must not record the pile as home
    /// either, and the core, reading that scan, must see the windows home.
    #[test]
    fn replugging_unpiles_the_windows_macos_stacked_on_the_laptop() {
        let pile = rect(0.0, 33.0);
        let d = FakeDesktop::new(&[
            (w(1), Pid(10), rect(100.0, 100.0)),
            (w(2), Pid(20), rect(2000.0, 100.0)),
            (w(3), Pid(10), rect(300.0, 300.0)),
        ]);
        let mut b = EmulatedWorkspaces::new(3);
        rescan(&d, &mut b);

        d.set_displays(&[MAIN]);
        d.place(w(1), pile);
        d.place(w(3), pile);
        rescan(&d, &mut b);
        b.move_window_to_workspace(&d, w(3), ws(2)).unwrap();

        d.set_displays(&[MAIN, SECOND]);
        let stale = frames_of(&d);
        b.note_scan(&d, &d.scan());
        b.enforce_placement(&d, &stale);
        assert_eq!(d.frame(w(1)), rect(100.0, 100.0), "left on screen, back home");
        assert_eq!(b.believed_frames(&d, &stale)[&w(1)], rect(100.0, 100.0));
        assert_eq!(d.frame(w(2)), rect(2000.0, 100.0));

        rescan(&d, &mut b);
        b.move_window_to_workspace(&d, w(3), ws(1)).unwrap();
        assert_eq!(d.frame(w(3)), rect(300.0, 300.0), "hidden while undocked, back home");
        assert_eq!(b.home[&w(1)], rect(100.0, 100.0));
        assert_eq!(b.home[&w(3)], rect(300.0, 300.0));
    }

    /// Undocked while viewing monitor 2, so the laptop shows monitor 2 and
    /// monitor 1 waits parked. The monitors go in while the screen is locked:
    /// the displays change during scans that see no windows at all. That
    /// blindness is not an empty desktop — once the windows are seen again,
    /// monitor 1 comes up on the laptop and monitor 2 goes back to its
    /// display, not left for a switch away and back to replay.
    #[test]
    fn replugging_while_blind_brings_both_monitors_back_once_seen() {
        let d = FakeDesktop::new(&[
            (w(1), Pid(10), rect(100.0, 100.0)),
            (w(2), Pid(20), rect(2000.0, 100.0)),
        ]);
        let mut b = EmulatedWorkspaces::new(3);
        rescan(&d, &mut b);
        d.focused.set(Some(w(2)));
        d.set_displays(&[MAIN]);
        rescan(&d, &mut b);
        assert_eq!(b.monitors().viewed, vm(2));
        assert!(in_park_corner(&d.frame(w(1)), &geo()), "monitor 1 waits parked");

        d.set_displays(&[MAIN, SECOND]);
        let blind = HashMap::new();
        b.note_scan(&d, &blind);
        b.enforce_placement(&d, &blind);
        rescan(&d, &mut b);

        assert!(on(MAIN, d.frame(w(1))), "monitor 1 revealed: {:?}", d.frame(w(1)));
        assert!(on(SECOND, d.frame(w(2))), "monitor 2 home: {:?}", d.frame(w(2)));
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

    /// The migration pinned. A state file from before windows carried a
    /// monitor — or from the build that first wrote one as a default of 1 —
    /// must not have those placeholders asserted: on a two-display rig that
    /// hauled a second-display window onto the first. The first scan reads
    /// each window's monitor off where it stands (its promise, while parked),
    /// and only what it learned is written back as declarations.
    #[test]
    fn a_file_without_monitor_declarations_learns_them_from_where_windows_stand() {
        let dir = std::env::temp_dir().join(format!("ordo-learn-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.json");
        let parked_promise = rect(2000.0, 100.0); // w3 lived on the second display
        let pw = |id: u32, wsn: u8, saved: Option<Rect>| PersistedWindow {
            id: w(id),
            workspace: ws(wsn),
            monitor: vm(1), // the placeholder every old window carries
            owner: Pid(10),
            saved,
            home: None,
        };
        statefile::save(
            &path,
            &PersistedState {
                version: statefile::VERSION,
                boot_time_sec: statefile::boot_time_sec(),
                current: ws(1),
                viewed: vm(1),
                virtual_monitors_enabled: true,
                virtual_monitor_count: 2,
                monitors_assigned: false,
                windows: vec![
                    pw(1, 1, None),
                    pw(2, 1, None),
                    pw(3, 2, Some(parked_promise)),
                ],
            },
        );
        let d = FakeDesktop::new(&[
            (w(1), Pid(10), rect(100.0, 100.0)),
            (w(2), Pid(10), rect(2100.0, 100.0)),
            (w(3), Pid(10), park_frame(parked_promise, &geo())),
        ]);
        let mut b = EmulatedWorkspaces::with_persistence(3, path.clone());
        rescan(&d, &mut b);

        let monitors = b.window_monitors();
        assert_eq!(monitors[&w(1)], vm(1));
        assert_eq!(monitors[&w(2)], vm(2), "read off the display it stands on");
        assert_eq!(monitors[&w(3)], vm(2), "read off its promise while parked");
        assert_eq!(d.frame(w(2)), rect(2100.0, 100.0), "nothing was hauled anywhere");
        assert!(in_park_corner(&d.frame(w(3)), &geo()));

        // Written back as declarations: a restart does not learn again, even
        // if the user has since moved w2 elsewhere.
        let again = EmulatedWorkspaces::with_persistence(3, path);
        assert!(!again.learn_monitors);
        assert_eq!(again.window_monitors()[&w(2)], vm(2));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A state file from another boot is rightly ignored, but the blank
    /// start must not destroy it: if the judgement was wrong, those promises
    /// are all that says which workspace each parked window belongs to.
    #[test]
    fn a_rejected_state_file_is_set_aside_before_a_blank_start_writes_over_it() {
        let dir = std::env::temp_dir().join(format!("ordo-reject-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.json");
        let stale = PersistedState {
            version: statefile::VERSION,
            boot_time_sec: statefile::boot_time_sec() - 3600,
            current: ws(2),
            viewed: vm(1),
            virtual_monitors_enabled: true,
            virtual_monitor_count: 2,
            monitors_assigned: true,
            windows: vec![PersistedWindow {
                id: w(7),
                workspace: ws(2),
                monitor: vm(1),
                owner: Pid(10),
                saved: Some(rect(100.0, 100.0)),
                home: None,
            }],
        };
        statefile::save(&path, &stale);

        let d = FakeDesktop::new(&[(w(1), Pid(10), rect(100.0, 100.0))]);
        let mut b = EmulatedWorkspaces::with_persistence(3, path.clone());
        rescan(&d, &mut b);

        assert_eq!(b.current(), ws(1), "the stale file was not believed");
        let started = statefile::load(&path, statefile::boot_time_sec()).unwrap();
        assert_eq!(started.windows.iter().map(|w| w.id).collect::<Vec<_>>(), vec![w(1)]);
        let rejected = std::fs::read_to_string(path.with_extension("json.rejected")).unwrap();
        assert_eq!(serde_json::from_str::<PersistedState>(&rejected).unwrap(), stale);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Three monitors, docked on three displays, then the third unplugged:
    /// monitor 3 is hidden and w3 parked, viewing 1+2. w1 and w4 share
    /// monitor 1.
    fn three_on_two() -> (FakeDesktop, EmulatedWorkspaces) {
        let d = FakeDesktop::new(&[
            (w(1), Pid(10), rect(100.0, 100.0)),
            (w(2), Pid(20), rect(2000.0, 100.0)),
            (w(3), Pid(30), rect(3500.0, 100.0)),
            (w(4), Pid(40), rect(200.0, 200.0)),
        ]);
        d.set_displays(&[MAIN, SECOND, THIRD]);
        let mut b = EmulatedWorkspaces::new(3);
        b.note_scan(&d, &d.scan());
        d.set_displays(&[MAIN, SECOND]);
        rescan(&d, &mut b);
        assert_eq!(b.monitors().count, 3);
        assert!(in_park_corner(&d.frame(w(3)), &geo()));
        (d, b)
    }

    const THIRD: Rect = Rect {
        x: 3390.0,
        y: 66.0,
        w: 1470.0,
        h: 956.0,
    };

    fn on(display: Rect, frame: Rect) -> bool {
        display.contains(frame.center())
    }

    /// Two displays showing 2+3 put monitor 2 on the LEFT display. Its
    /// windows must go with it: left where they stood, monitors 2 and 3
    /// shared the right display, and the core then re-filed w2 onto monitor
    /// 3 because that is what its display stood for.
    #[test]
    fn sliding_the_view_carries_a_monitor_that_stays_on_screen_to_its_new_display() {
        let (d, mut b) = three_on_two();
        b.view_monitor(&d, vm(3)).unwrap();
        assert!(on(MAIN, d.frame(w(2))), "monitor 2 moved left: {:?}", d.frame(w(2)));
        assert!(on(SECOND, d.frame(w(3))), "monitor 3 came up on the right");
        assert!(in_park_corner(&d.frame(w(1)), &geo()));

        b.view_monitor(&d, vm(1)).unwrap();
        assert_eq!(d.frame(w(2)), rect(2000.0, 100.0), "home again, exactly");
        assert_eq!(d.frame(w(1)), rect(100.0, 100.0));
        assert!(in_park_corner(&d.frame(w(3)), &geo()));
    }

    /// Dragging monitor 1 onto monitor 3: 1's windows join 3 on every
    /// workspace, and the monitors after 1 step down — old 2 is the new 1,
    /// old 3 the new 2. Two monitors on two displays then show everything,
    /// each window on the display its new number stands for. Nothing is
    /// spare after that, so another merge is refused.
    #[test]
    fn merging_a_monitor_folds_it_into_another_on_every_workspace() {
        let (d, mut b) = three_on_two();
        let w4 = w(4);
        b.move_window_to_workspace(&d, w4, ws(2)).unwrap();

        b.merge_monitors(&d, vm(1), vm(3)).unwrap();

        assert_eq!(b.monitors().count, 2);
        let m = b.window_monitors();
        assert_eq!((m[&w(1)], m[&w(2)], m[&w(3)], m[&w4]), (vm(2), vm(1), vm(2), vm(2)));
        assert!(on(SECOND, d.frame(w(1))), "went right with its monitor");
        assert!(on(MAIN, d.frame(w(2))), "monitor 2, now 1, moved left");
        assert!(on(SECOND, d.frame(w(3))), "revealed on the right");
        assert!(in_park_corner(&d.frame(w4), &geo()), "workspace 2 stays hidden");
        assert!(b.merge_monitors(&d, vm(2), vm(1)).is_err());
    }

    /// Dragging hidden monitor 3 between 1 and 2 makes it the second, in
    /// view on the right display, and the old 2, now third, goes out of
    /// view. The anchor stays with its monitor.
    #[test]
    fn moving_a_monitor_shows_what_the_new_order_puts_in_view() {
        let (d, mut b) = three_on_two();

        b.move_monitor(&d, vm(3), vm(2)).unwrap();

        let m = b.window_monitors();
        assert_eq!((m[&w(1)], m[&w(2)], m[&w(3)]), (vm(1), vm(3), vm(2)));
        assert_eq!(b.monitors().viewed, vm(1));
        assert!(on(SECOND, d.frame(w(3))), "came into view on the right");
        assert!(in_park_corner(&d.frame(w(2)), &geo()), "went out of view");
        assert_eq!(d.frame(w(1)), rect(100.0, 100.0));
    }

    /// Moving workspace 2 to the front renumbers: the current workspace is
    /// now 2, and the window that lived on 2 is on 1. Not a window moves.
    #[test]
    fn moving_a_workspace_renumbers_and_moves_no_window() {
        let (d, mut b) = three_on_two();
        b.move_window_to_workspace(&d, w(4), ws(2)).unwrap();
        let before: Vec<Rect> = (1..=4).map(|n| d.frame(w(n))).collect();

        b.move_workspace(&d, ws(2), ws(1)).unwrap();

        assert_eq!(b.current(), ws(2));
        let on_ws = b.window_ws();
        assert_eq!((on_ws[&w(1)], on_ws[&w(4)]), (ws(2), ws(1)));
        let after: Vec<Rect> = (1..=4).map(|n| d.frame(w(n))).collect();
        assert_eq!(after, before);
        assert!(b.move_workspace(&d, ws(4), ws(1)).is_err());
    }

    /// Viewing monitor 3 of three on two displays pins the viewport against
    /// the end. A fourth monitor unpins it; the anchor steps back to 2 so the
    /// displays keep showing 2 and 3, and not a window moves.
    #[test]
    fn adding_a_monitor_moves_no_window() {
        let (d, mut b) = three_on_two();
        b.view_monitor(&d, vm(3)).unwrap();
        let before: Vec<Rect> = (1..=4).map(|n| d.frame(w(n))).collect();

        b.add_monitor(&d).unwrap();

        assert_eq!(b.monitors().count, 4);
        assert_eq!(b.monitors().viewed, vm(2));
        let after: Vec<Rect> = (1..=4).map(|n| d.frame(w(n))).collect();
        assert_eq!(after, before);
    }
}
