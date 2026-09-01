//! The emulated backend's orchestration: applying the [`Ledger`]'s decisions
//! to the desktop through the [`Desktop`] port, persisting its promises, and
//! policing that reality keeps matching them.
//!
//! The model's data splits in two, and the split is the architecture.
//! DECLARATIONS — which workspace a window belongs to, which workspace is
//! visible — are written only by Ordo's own commands: a user switch or move,
//! rescue, and a window's birth (a brand-new window has no prior intent to
//! preserve). OBSERVATIONS — frames, existence, focus — are authoritative
//! about the world, never about intent. An observation that contradicts a
//! declaration is a violation to correct on screen or to surface to the
//! user, NEVER to absorb into the declaration: a declaration must not travel
//! through the observation channel.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;

use ordo_core::{Pid, Rect, WindowId, WorkspaceId};

use crate::ledger::{Claim, Ledger, MoveAction};
use crate::statefile::{self, PersistedState, PersistedWindow};
use crate::trace::{ParkTrace, ParkTraceKind};
use crate::Desktop;

/// How much of a parked window stays on-screen. macOS refuses to keep a fully
/// off-screen window where you put it, so we leave a 1px handle — also the
/// manual escape hatch if Ordo dies mid-park.
const SLIVER: f64 = 1.0;

/// How far the WindowServer may move a frame from where a write asked, on the
/// clamped (vertical) axis. Measured park landings: the bulk pulled back
/// 27-65pt, with a 123-124pt tail from the era when the corner straddled a
/// second display — and a 123pt strip is plainly readable, so this must never
/// be an absolute "that's parked" box. It is only ever slack around a frame
/// ORDO ITSELF REQUESTED (the x axis stays exact), where the worst
/// misclassification is far cheaper than a missed one.
const CLAMP_SLACK: f64 = 160.0;

/// Foreign-attributed re-parks of an escaped window before enforcement stands
/// down: no more writes, stderr + a Standoff trace, and the declaration LEFT
/// ALONE. Never adoption — losing where the user filed a window is silent and
/// permanent, while a visibly misplaced window is obvious and self-heals on
/// the next switch or command.
const ENFORCE_LIMIT: u8 = 3;

/// A requested workspace outside the configured range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkspaceOutOfRange(pub WorkspaceId);

pub struct EmulatedWorkspaces {
    ledger: Ledger,
    /// On-screen frame to restore each parked window to.
    saved: HashMap<WindowId, Rect>,
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
    /// back — charging those drained the enforcement budget against us.
    last_requested: HashMap<WindowId, Rect>,
    /// Windows with an uncounted re-park issued and not yet observed
    /// compliant. Gates the suppressed path's WRITES, not just its counting:
    /// re-issuing an identical corner write every pass while the first is
    /// still in flight was a self-sustaining write loop.
    pending_repark: HashSet<WindowId>,
    /// The main display as of the last enforcement pass. A change moves the
    /// park corner itself — a systemic event no window should be blamed for.
    last_main: Option<Rect>,
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
            parked: HashSet::new(),
            enforce_attempts: HashMap::new(),
            park_request: HashMap::new(),
            last_requested: HashMap::new(),
            pending_repark: HashSet::new(),
            last_main: None,
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

    pub fn window_ws(&self) -> BTreeMap<WindowId, WorkspaceId> {
        self.ledger.window_ws()
    }

    /// Fold a completed window scan into the model: adopt the genuinely new,
    /// forget the provably dead, keep the park bookkeeping in lockstep with
    /// the ledger.
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
        if windows.is_empty() {
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
        // A recycled id's saved frame belongs to a dead stranger; the new
        // window must not inherit a teleport to it.
        let recycled = self.ledger.note_seen(windows);
        self.drop_park_bookkeeping(&recycled);
        if self.ledger.window_claims() != before {
            self.persist();
        }
    }

    fn drop_park_bookkeeping(&mut self, ids: &[WindowId]) {
        for id in ids {
            self.saved.remove(id);
            self.parked.remove(id);
            self.enforce_attempts.remove(id);
            self.park_request.remove(id);
            self.last_requested.remove(id);
            self.pending_repark.remove(id);
        }
    }

    /// Every frame write funnels through here so the model remembers what it
    /// asked for — the only way to later tell "our write landing" apart from
    /// "an app fighting back".
    fn write_frames(&mut self, d: &dyn Desktop, writes: &[(Pid, WindowId, Rect)]) {
        for (_, w, f) in writes {
            self.last_requested.insert(*w, *f);
        }
        d.set_frames(writes);
    }

    pub fn switch_workspace(&mut self, d: &dyn Desktop, target: WorkspaceId) {
        // Stacking is NOT this backend's problem: the core follows every
        // switch with a RestackWindows effect derived from the MRU history,
        // which the effector reasserts after this returns.
        // Before the ledger moves: afterwards `current` IS the target.
        let from = self.ledger.current();
        let plan = self.ledger.switch(target);
        if plan.park.is_empty() && plan.restore.is_empty() {
            self.persist(); // `current` may still have changed
            return;
        }
        let frames = current_frames(d);
        let main = d.main_display();
        let t = ParkTrace::new(WindowId(0), ParkTraceKind::Switch)
            .ws(from, target)
            .detail(format!(
                "parking {}, restoring {}",
                plan.park.len(),
                plan.restore.len()
            ));
        self.note(t);
        let mut writes = Vec::new();
        for w in plan.park {
            writes.extend(self.park(w, None, &frames, main));
        }
        for w in plan.restore {
            writes.extend(self.restore(w, &frames, main));
        }
        self.persist();
        // Frames first, then visibility — a window must already be at the
        // corner before its app is un-hidden, or the unhide reveals it where it
        // still stands. Whether that ordering is SUFFICIENT is the open
        // question: it cannot stop an app from restoring its own geometry in
        // response to being un-hidden, which is what the trace is here to show.
        self.write_frames(d, &writes);
        self.apply_app_visibility(d, &frames);
    }

    pub fn move_window_to_workspace(
        &mut self,
        d: &dyn Desktop,
        window: WindowId,
        target: WorkspaceId,
    ) -> Result<(), WorkspaceOutOfRange> {
        let frames = current_frames(d);
        let main = d.main_display();
        let write = match self.ledger.assign_window(window, target) {
            Some(MoveAction::Park) => self.park(window, None, &frames, main),
            Some(MoveAction::Restore) => self.restore(window, &frames, main),
            None => return Err(WorkspaceOutOfRange(target)),
        };
        self.persist();
        self.write_frames(d, write.as_slice());
        self.apply_app_visibility(d, &frames);
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

    pub fn bring_up(&mut self, use_state: bool) {
        if use_state {
            // O: reload the file. With write-through active this is the model
            // we already have; after a fresh session it's the restore.
            self.load_state();
            self.suspended = false;
        } else {
            // R: blank model on the same workspace ordinal, file untouched
            // and unused. Parked slivers keep sitting where they are — O can
            // always bring their meaning back from the file.
            self.ledger =
                Ledger::restore(self.ledger.count(), self.ledger.current(), BTreeMap::new());
            self.saved.clear();
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
                let main = d.main_display();
                let merged =
                    merge_fresh_session(&self.ledger.window_claims(), &self.saved, &ps, |id| {
                        frames
                            .get(id)
                            .is_some_and(|(_, f)| self.reads_parked(*id, f, main))
                    });
                self.ledger =
                    Ledger::restore(self.ledger.count(), self.ledger.current(), merged.assign);
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
        let main = d.main_display();
        frames
            .iter()
            .filter_map(|(w, (_, f))| {
                let saved = self.saved.get(w)?;
                self.reads_parked(*w, f, main).then_some((*w, *saved))
            })
            .collect()
    }

    /// Is this window's observed frame at (a clamp of) the park corner Ordo
    /// asked it to occupy? The request is the anchor: macOS pulls a parked
    /// window back up from the corner by an amount that varies by app and
    /// display, and guessing that pull-back with a constant misread 123pt
    /// landings as escapes — spurious re-parks, a drained budget, and the raw
    /// sliver frame fed to the core (which then mis-scoped MRU and monitor
    /// attribution around a window "living" at the display corner).
    fn at_park(&self, w: WindowId, f: &Rect) -> bool {
        self.park_request
            .get(&w)
            .is_some_and(|req| clamp_of_request(f, req))
    }

    /// `at_park`, plus the geometric fallback for frames with no request to
    /// anchor on (a restarted daemon, an R-mode blank, promises from disk).
    fn reads_parked(&self, w: WindowId, f: &Rect, main: Rect) -> bool {
        self.at_park(w, f) || in_park_corner(f, main)
    }

    /// Assert the declarations: every window assigned to a hidden workspace
    /// must sit at the park corner. Iterates the LEDGER, not the parked set —
    /// a window whose park write never happened (its frame was unreadable at
    /// park time) is still declared hidden, and a parked-set walk was blind
    /// to it forever.
    ///
    /// Enforcement asserts declarations by writing FRAMES; it never writes a
    /// declaration. Violations are classified by frame before they cost
    /// budget:
    /// - at a clamp of the corner we last requested: compliant.
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
    /// A main-display change is a systemic event: the park corner moves,
    /// so every parked window reads as in violation at once for a reason
    /// that has nothing to do with opposition. That pass re-asserts without
    /// counting and clears every budget.
    ///
    /// `frames` arrives from the caller rather than `d.windows()` because the
    /// shell's enumerator has always just scanned when this runs — no backend
    /// re-enumerates on its own.
    pub fn enforce_placement(&mut self, d: &dyn Desktop, frames: &HashMap<WindowId, (Pid, Rect)>) {
        let current = self.ledger.current();
        let hidden: Vec<WindowId> = self
            .ledger
            .window_ws()
            .into_iter()
            .filter(|(_, ws)| *ws != current)
            .map(|(w, _)| w)
            .collect();
        // This runs on every snapshot; don't pay the display query when
        // there's nothing to enforce.
        if hidden.is_empty() {
            return;
        }
        let main = d.main_display();
        let systemic = self.last_main.is_some_and(|m| m != main);
        self.last_main = Some(main);
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
            let own_write = self
                .saved
                .get(&w)
                .is_some_and(|s| (f.x - s.x).abs() <= 1.0 && (f.y - s.y).abs() <= 1.0)
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
                        "main display changed; whole pass uncounted"
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
                let want = park_frame(*f, main);
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
                let write = self.park(w, self.enforce_attempts.get(&w).copied(), frames, main);
                newly_parked |= write.is_some();
                writes.extend(write);
            }
        }
        // A blind-spot park changed durable promises.
        if newly_parked {
            self.persist();
        }
        self.write_frames(d, &writes);
    }

    pub fn rescue_window(&mut self, d: &dyn Desktop, window: WindowId) {
        // Claim it for the visible workspace and bring it back on-screen.
        // No visibility pass here: rescue must only ever reveal, and the
        // gather already unhides every app up front.
        let frames = current_frames(d);
        self.ledger.assign_window(window, self.ledger.current());
        d.set_app_hidden(
            frames.get(&window).map(|(p, _)| *p).unwrap_or(Pid(0)),
            false,
        );
        let write = self.restore(window, &frames, d.main_display());
        self.persist();
        self.write_frames(d, write.as_slice());
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
                        owner: w.owner,
                    },
                )
            })
            .collect();
        self.ledger = Ledger::restore(count, ps.current, claims);
        self.saved.clear();
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
                owner: claim.owner,
                saved: self
                    .parked
                    .contains(&id)
                    .then(|| self.saved.get(&id).copied())
                    .flatten(),
            })
            .collect();
        statefile::save(
            path,
            &PersistedState {
                version: statefile::VERSION,
                boot_time_sec: self.boot_time,
                current: self.ledger.current(),
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
        main: Rect,
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
        if self.reads_parked(window, f, main) {
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
        self.parked.insert(window);
        self.enforce_attempts.remove(&window);
        self.pending_repark.remove(&window);
        let want = park_frame(f, main);
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

    fn restore(
        &mut self,
        window: WindowId,
        frames: &HashMap<WindowId, (Pid, Rect)>,
        main: Rect,
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
        if !was_parked && !self.reads_parked(window, f, main) {
            return None;
        }
        let (pid, f) = (*pid, *f);
        let (kind, want) = match self.saved.get(&window).copied() {
            // A promise that is itself a park artifact is not a promise. It is
            // residue from before the corner was recognizable, persisted to
            // disk, so it outlives the bug that wrote it. Honoring it re-parks
            // the window the instant its workspace comes up — the window you
            // cannot switch to. A promise has no request to anchor on (it may
            // predate this process), so this is the geometric test.
            Some(s) if in_park_corner(&s, main) => {
                (ParkTraceKind::PoisonedPromise, rehome_into(&s, main))
            }
            Some(s) => (ParkTraceKind::Restore, s),
            // Parked with no promise (its real frame was never trustworthily
            // seen — see park()'s sliver guard): don't leave it a 1px sliver
            // on the now-visible workspace. Re-home it somewhere reachable;
            // the next park captures its real frame and it self-heals.
            None if self.reads_parked(window, &f, main) => {
                (ParkTraceKind::Rehome, rehome_into(&f, main))
            }
            None => return None,
        };
        let t = ParkTrace::new(window, kind)
            .observed(f)
            .requested(want)
            .at_park(self.reads_parked(window, &f, main));
        self.note(t);
        Some((pid, window, want))
    }

    /// Dock dimming: hide (Cmd+H-style) every app whose known windows are all
    /// parked on hidden workspaces, unhide every app with a window here. With
    /// the Dock's `showhidden` pref, "hidden" renders as a translucent icon —
    /// the closest macOS gets to a per-workspace Dock.
    ///
    /// The app owning the focused window is never hidden: hiding the active
    /// app makes macOS fling focus somewhere arbitrary. Core-side, switches
    /// hand focus to the destination before this runs, so the exemption
    /// almost never bites; when it does, the app just stays undimmed.
    fn apply_app_visibility(&mut self, d: &dyn Desktop, frames: &HashMap<WindowId, (Pid, Rect)>) {
        let current = self.ledger.current();
        let assignments = self.ledger.window_ws();
        let mut here_by_app: HashMap<Pid, bool> = HashMap::new();
        for (window, (pid, _)) in frames {
            if let Some(ws) = assignments.get(window) {
                *here_by_app.entry(*pid).or_insert(false) |= *ws == current;
            }
        }
        let focused_app = d
            .focused_window()
            .and_then(|w| frames.get(&w).map(|(p, _)| *p));
        // Count each app's windows that are NOT on the current workspace: those
        // are the ones an unhide reveals along with the wanted one, and the
        // number worth having when reading a flash back.
        let mut elsewhere: HashMap<Pid, usize> = HashMap::new();
        for (window, (pid, _)) in frames {
            if assignments.get(window).is_some_and(|ws| *ws != current) {
                *elsewhere.entry(*pid).or_insert(0) += 1;
            }
        }
        let mut notes = Vec::new();
        for (pid, has_window_here) in here_by_app {
            let parked_too = elsewhere.get(&pid).copied().unwrap_or(0);
            if has_window_here {
                d.set_app_hidden(pid, false);
                notes.push(
                    ParkTrace::app(pid, ParkTraceKind::AppShown)
                        .ws(current, current)
                        .detail(format!(
                            "unhidden; also reveals {parked_too} window(s) parked for other workspaces"
                        )),
                );
            } else if Some(pid) != focused_app {
                d.set_app_hidden(pid, true);
                notes.push(
                    ParkTrace::app(pid, ParkTraceKind::AppHidden)
                        .ws(current, current)
                        .detail(format!("hidden; {parked_too} window(s) parked")),
                );
            }
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

/// The park spot: flush to the bottom of the main display, right-aligned,
/// keeping the window's size — so only `SLIVER` points remain visible.
///
/// The whole horizontal span stays on the main display. The old corner
/// (x = right edge - SLIVER) hung the window's body across whatever sits to
/// the right of main: the WindowServer then clamped one identical request
/// against different displays — the same write landed anywhere from 27 to
/// 124pt back up depending on which edge it consulted — and the overhang was
/// a visible window-wide band on the second display even on a good landing.
/// A window WIDER than main cannot fit; it is floored at main's left edge so
/// its origin (and the left of its title bar) stays on main, accepting the
/// unavoidable overhang on the right — the bottom-flush y still reduces it
/// to a strip.
///
/// MIGRATION: windows persisted while parked at the old corner read as one
/// violation on the first enforcement pass after this change and are
/// re-parked once onto this corner (see `park_request` for why the same
/// happens after any restart).
fn park_frame(size: Rect, main: Rect) -> Rect {
    Rect {
        x: (main.x + main.w - size.w).max(main.x),
        y: main.y + main.h - SLIVER,
        w: size.w,
        h: size.h,
    }
}

/// Did `observed` land where `requested` asked, modulo the WindowServer's
/// clamp? Position is the parked invariant; size is the window's own.
///
/// GOTCHA: a parked window does NOT land where we asked. The WindowServer
/// refuses to push a title bar off the bottom of the screen and pulls the
/// window back UP by an amount that varies by app and display (measured:
/// 27-65pt for most apps, 123-124pt in the straddling-corner era). x is the
/// tight axis — nothing clamps horizontally once the span fits the display —
/// so x must match the request; y may only sit at or above it, within
/// CLAMP_SLACK. Anchoring on the REQUEST is what keeps this from guessing:
/// an absolute corner-region constant either misses deep clamps (declaring
/// compliant windows escaped — spurious re-parks, then a rewritten
/// declaration) or grows wide enough to swallow readable windows.
fn clamp_of_request(observed: &Rect, requested: &Rect) -> bool {
    (observed.x - requested.x).abs() <= 1.0
        && observed.y <= requested.y + 1.0
        && observed.y >= requested.y - CLAMP_SLACK
}

/// Like [`clamp_of_request`], but direction-agnostic in y: the clamp pulls up
/// at the bottom edge and pushes DOWN under the menu bar, and this comparator
/// serves the enforcement budget's own-write exemption, where the writes in
/// question (restores, rehomes) can land against either edge. A false match
/// merely leaves one violation uncounted (it is still corrected); a false
/// mismatch charges Ordo's own write to the window — the leak that drained
/// budgets.
fn near_own_request(observed: &Rect, requested: &Rect) -> bool {
    (observed.x - requested.x).abs() <= 1.0 && (observed.y - requested.y).abs() <= CLAMP_SLACK
}

/// Does this frame LOOK like a park artifact, with no request to anchor on?
/// The fallback for frames whose park (if any) this process never issued:
/// promises loaded from disk, windows already slivered when an R-mode blank
/// or a model gap meets them.
///
/// x must right-align exactly to the window's own width (or sit at the
/// pre-2026-09 1px corner) — an alignment no user placement hits by accident
/// at this height, since y confines the window's TOP edge to the bottom
/// CLAMP_SLACK points of the display: a window there shows less than a
/// title bar's worth of itself. The consumers of this test share an
/// asymmetry that tolerates its generosity: a false "parked" skips a promise
/// capture or re-homes a window (recoverable, self-heals on the next park);
/// a false "not parked" canonicalizes a park artifact as a window's real
/// frame — a silent, durable lie persisted to disk.
fn in_park_corner(f: &Rect, main: Rect) -> bool {
    let near = |a: f64, b: f64| (a - b).abs() <= 1.0;
    let corner_x = (main.x + main.w - f.w).max(main.x);
    let legacy_x = main.x + main.w - SLIVER;
    let bottom_y = main.y + main.h - SLIVER;
    (near(f.x, corner_x) || near(f.x, legacy_x))
        && f.y <= bottom_y + 1.0
        && f.y >= bottom_y - CLAMP_SLACK
}

/// Re-home a frame fully inside `area` (top-left, shrunk if oversized) — the
/// no-cascade twin of the rescue gather's clamp, for the lone promise-less
/// window a restore must not leave as a sliver.
fn rehome_into(f: &Rect, area: Rect) -> Rect {
    Rect {
        x: area.x,
        y: area.y,
        w: f.w.min(area.w),
        h: f.h.min(area.h),
    }
}

/// The merged model an S-after-R resume adopts.
struct FreshMerge {
    assign: BTreeMap<WindowId, Claim>,
    /// Restore promises revived from the file (window is parked again).
    saved: Vec<(WindowId, Rect)>,
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
    for w in &file.windows {
        let model_knows = model.contains_key(&w.id);
        let noise_sliver =
            w.saved.is_some() && is_slivered(&w.id) && !own_saved.contains_key(&w.id);
        if !model_knows || noise_sliver {
            assign.insert(
                w.id,
                Claim {
                    ws: w.workspace,
                    owner: w.owner,
                },
            );
            if let Some(f) = w.saved {
                saved.push((w.id, f));
            }
        }
    }
    FreshMerge { assign, saved }
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

    /// A desktop where frame writes land instantly — enough to drive the whole
    /// backend through the port without a real window. `freeze()` makes later
    /// writes vanish, simulating an app that hasn't applied them yet.
    struct FakeDesktop {
        windows: std::cell::RefCell<BTreeMap<WindowId, (Pid, Rect)>>,
        frozen: std::cell::Cell<bool>,
        cg_down: std::cell::Cell<bool>,
        /// How far this WindowServer pulls a bottom-clamped write back up.
        /// Defaults to a title bar; tests raise it to the deepest landing
        /// measured in production (124pt), which no constant-box predicate
        /// survived.
        pull: std::cell::Cell<f64>,
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
            }
        }

        fn frame(&self, w: WindowId) -> Rect {
            self.windows.borrow()[&w].1
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

        /// Writes land CLAMPED, as the real WindowServer lands them: a window
        /// may not be positioned with its title bar below the bottom of the
        /// display. A fake that stores frames verbatim is a fake that cannot
        /// reproduce parking, which is how an exact-point park predicate
        /// passed every test here and fought every window in production.
        fn set_frames(&self, writes: &[(Pid, WindowId, Rect)]) {
            if self.frozen.get() {
                return;
            }
            let floor = MAIN.y + MAIN.h - self.pull.get();
            let mut ws = self.windows.borrow_mut();
            for (pid, w, f) in writes {
                let landed = Rect {
                    y: f.y.min(floor),
                    ..*f
                };
                ws.insert(*w, (*pid, landed));
            }
        }

        fn set_app_hidden(&self, _pid: Pid, _hidden: bool) {}

        fn focused_window(&self) -> Option<WindowId> {
            None
        }

        fn main_display(&self) -> Rect {
            MAIN
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
        assert!(in_park_corner(&d.frame(w(2)), MAIN));

        // …and switching there parks w1 and puts w2 back where it was.
        b.switch_workspace(&d, ws(2));
        assert_eq!(b.current(), ws(2));
        assert!(in_park_corner(&d.frame(w(1)), MAIN));
        assert_eq!(d.frame(w(2)), rect(300.0, 200.0));

        // Round home: both windows end at their original frames.
        b.switch_workspace(&d, ws(1));
        assert_eq!(d.frame(w(1)), rect(100.0, 100.0));
        assert!(in_park_corner(&d.frame(w(2)), MAIN));
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
            MAIN
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
            in_park_corner(&want, MAIN),
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
        let sliver = park_frame(rect(100.0, 100.0), MAIN);
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
        let frames: HashMap<WindowId, (Pid, Rect)> = [
            (w(1), (Pid(42), sliver)),
            (w(2), (Pid(42), rect(50.0, 60.0))),
            (w(3), (Pid(42), deep)),
            (w(4), (Pid(42), legacy)),
        ]
        .into();

        // A window already at a park artifact — the ideal corner, a deep
        // clamp, or the legacy corner: bookkept as parked, but its (unknown)
        // real frame is never fabricated from the artifact.
        for id in [w(1), w(3), w(4)] {
            assert_eq!(b.park(id, None, &frames, MAIN), None, "{id:?}");
            assert!(b.parked.contains(&id));
            assert!(!b.saved.contains_key(&id), "{id:?} captured an artifact");
        }

        // A window at a real position parks normally.
        let write = b.park(w(2), None, &frames, MAIN).unwrap();
        assert_eq!(b.saved[&w(2)], rect(50.0, 60.0));
        assert_eq!(write.2, park_frame(rect(50.0, 60.0), MAIN));
    }

    /// The park request must not straddle a neighboring display: the old
    /// corner (x = right edge - 1) hung the window's body across whatever sat
    /// to the right of main, where the WindowServer clamped the identical
    /// request against different displays (same write, landings 27-124pt
    /// apart) and the overhang was a visible window-wide band.
    #[test]
    fn the_park_corner_keeps_the_window_on_the_main_display() {
        let f = Rect {
            x: 100.0,
            y: 100.0,
            w: 1470.0,
            h: 900.0,
        };
        let want = park_frame(f, MAIN);
        assert_eq!(want.y, MAIN.y + MAIN.h - SLIVER);
        assert!(
            want.x >= MAIN.x && want.x + want.w <= MAIN.x + MAIN.w,
            "horizontal span must stay on main: {want:?}"
        );

        // Wider than the display: it cannot fit, so it anchors at main's left
        // edge and overhangs the right — the one unavoidable case.
        let wide = Rect {
            x: 0.0,
            y: 0.0,
            w: MAIN.w + 500.0,
            h: 900.0,
        };
        assert_eq!(park_frame(wide, MAIN).x, MAIN.x);
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
        assert!(in_park_corner(&d.frame(w(1)), MAIN));
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
        assert!(in_park_corner(&d.frame(w(1)), MAIN), "parked at last");
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

    /// The regression that made the emulated backend unusable: macOS lands a
    /// parked window pulled back up from the corner we asked for — measured
    /// anywhere from a title bar (27pt) to 124pt, varying by app and display —
    /// so a compliant window read as an escapee on every pass: re-parked
    /// forever, its budget drained, the raw sliver fed to the core. The
    /// landing must read as compliance at ANY clamp depth, because the
    /// landing is a clamp of what WE requested, not a spot to guess at.
    #[test]
    fn a_park_that_lands_where_macos_puts_it_is_compliance_not_escape() {
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

            // The landed frame is NOT the requested corner — that is the
            // whole point — but it is still parked.
            let landed = d.frame(w(1));
            assert!(landed.y <= MAIN.h - pull, "macOS pulled it back up");

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
        let artifact = park_frame(rect(0.0, 0.0), MAIN);
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
        assert!(!in_park_corner(&rehomed, MAIN));

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
        assert!(in_park_corner(&d.frame(w(1)), MAIN));
        assert_eq!(b.saved[&w(1)].x, rehomed.x);
    }

    /// The restart / corner-migration wrinkle, pinned: park requests are not
    /// persisted, so after a restart a parked window has no anchor and reads
    /// as ONE violation — it is re-parked once (which also migrates windows
    /// parked at the legacy straddling corner onto the in-display corner) and
    /// then reads compliant, with its declaration and promise untouched.
    #[test]
    fn a_restart_reasserts_each_parked_window_once() {
        // As a restart finds things: bookkept parked with a good promise,
        // physically at the LEGACY corner (x at the display edge), no
        // park_request memory.
        let legacy_landing = Rect {
            x: MAIN.w - SLIVER,
            y: MAIN.h - 65.0,
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
            migrated.x + migrated.w <= MAIN.x + MAIN.w,
            "re-parked onto the in-display corner: {migrated:?}"
        );

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
            !in_park_corner(&restored, MAIN),
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
            owner: Pid(10),
            saved,
        };
        let file = PersistedState {
            version: statefile::VERSION,
            boot_time_sec: 1,
            current: ws(1),
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
}
