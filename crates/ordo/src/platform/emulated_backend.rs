//! The emulated workspace backend: Ordo owns workspaces outright.
//!
//! Everything lives in one native Space. A window on a hidden workspace is
//! parked off-screen (bottom-right corner, a sliver left visible because macOS
//! forcibly re-homes fully-off-screen windows). Switching parks the outgoing
//! workspace's windows and restores the incoming one's to their saved frames.
//! The decisions are the pure [`Ledger`]; this type just applies them with AX.
//!
//! Chosen via `--backend emulated`. Its tradeoffs vs. native (Mission Control
//! clutter, Cmd-Tab showing every app, the visible sliver) are the price of
//! unlimited instant workspaces with no private Space APIs — see the research.
//! Best paired with "Displays have separate Spaces" off.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;

use ordo_core::{Pid, Rect, WindowId, WorkspaceId};

use crate::backend::{
    BackendError, BackendTopology, Capabilities, MonitorWorkspace, Result, WorkspaceBackend,
};
use crate::ledger::{Ledger, MoveAction};
use crate::statefile::{self, PersistedState, PersistedWindow};

use super::{ax, display};

/// How much of a parked window stays on-screen. macOS refuses to keep a fully
/// off-screen window where you put it, so we leave a 1px handle — also the
/// manual escape hatch if Ordo dies mid-park.
const SLIVER: f64 = 1.0;

/// Re-parks of a phantom before we stop fighting it (mirrors the core's
/// correction damping): an app that insists on being visible wins, loudly.
const ENFORCE_LIMIT: u8 = 3;

pub struct EmulatedBackend {
    ledger: Ledger,
    /// On-screen frame to restore each parked window to.
    saved: HashMap<WindowId, Rect>,
    /// Windows currently parked off-screen. Tracked so re-parking an
    /// already-parked window (a hidden->hidden move) doesn't overwrite its real
    /// saved frame with the sliver position.
    parked: HashSet<WindowId>,
    /// Phantom re-parks issued per window since it last sat parked correctly.
    enforce_attempts: HashMap<WindowId, u8>,
    /// Where the ledger's promises persist across restarts (None = ephemeral,
    /// e.g. `--fresh`). Written through on every mutation; see statefile.rs
    /// for the trust model.
    state_path: Option<PathBuf>,
    boot_time: i64,
    /// True during a fresh session (the R chord): the state file is neither
    /// read nor written, so O can still bring the pre-R organization back.
    suspended: bool,
}

impl EmulatedBackend {
    pub fn new(count: u8) -> Self {
        EmulatedBackend {
            ledger: Ledger::new(count),
            saved: HashMap::new(),
            parked: HashSet::new(),
            enforce_attempts: HashMap::new(),
            state_path: None,
            boot_time: statefile::boot_time_sec(),
            suspended: false,
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

    /// Replace the in-memory model with the state file's, when it validates.
    fn load_state(&mut self) {
        let Some(path) = &self.state_path else { return };
        let Some(ps) = statefile::load(path, self.boot_time) else {
            return;
        };
        let count = self.ledger.count();
        let assign: BTreeMap<WindowId, WorkspaceId> =
            ps.windows.iter().map(|w| (w.id, w.workspace)).collect();
        self.ledger = Ledger::restore(count, ps.current, assign);
        self.saved.clear();
        self.parked.clear();
        self.enforce_attempts.clear();
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
            .window_ws()
            .into_iter()
            .map(|(id, workspace)| PersistedWindow {
                id,
                workspace,
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

    fn current_frames() -> HashMap<WindowId, (Pid, Rect)> {
        ax::windows()
            .into_iter()
            .map(|w| (w.id, (w.app, w.frame)))
            .collect()
    }

    /// Bottom-right corner of the main display, keeping the window's size — so
    /// only `SLIVER` points remain visible.
    fn park_frame(size: Rect, main: Rect) -> Rect {
        Rect {
            x: main.x + main.w - SLIVER,
            y: main.y + main.h - SLIVER,
            w: size.w,
            h: size.h,
        }
    }

    /// Is this frame sitting at the park corner? Position is the parked
    /// invariant; size is the window's own.
    fn at_park_position(f: &Rect, main: Rect) -> bool {
        let want = Self::park_frame(*f, main);
        (f.x - want.x).abs() <= 1.0 && (f.y - want.y).abs() <= 1.0
    }

    /// Bookkeep a park and return the frame write it requires, so a switch can
    /// batch every write into one parallel pass instead of moving windows one
    /// by one (which made multi-monitor switches visibly ripple).
    fn park(
        &mut self,
        window: WindowId,
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
        // Never canonicalize a sliver as a window's real frame. Adoption gaps
        // (a partial scan dropping the ledger entry, R-mode re-adoption) can
        // hand this path a window that is already physically parked; capturing
        // that frame would turn a transient wrong belief into a durable one —
        // the window's recorded "real position" becomes the 1px corner, and
        // persist() writes it to disk. A missing promise is recoverable
        // (rescue); a lying one is not.
        if Self::at_park_position(f, main) {
            self.parked.insert(window);
            self.enforce_attempts.remove(&window);
            return None;
        }
        self.saved.insert(window, *f);
        self.parked.insert(window);
        self.enforce_attempts.remove(&window);
        Some((*pid, window, Self::park_frame(*f, main)))
    }

    fn restore(
        &mut self,
        window: WindowId,
        frames: &HashMap<WindowId, (Pid, Rect)>,
        main: Rect,
    ) -> Option<(Pid, WindowId, Rect)> {
        self.parked.remove(&window);
        self.enforce_attempts.remove(&window);
        let (pid, f) = frames.get(&window)?;
        match self.saved.get(&window) {
            Some(s) => Some((*pid, window, *s)),
            // Parked with no promise (its real frame was never trustworthily
            // seen — see park()'s sliver guard): don't leave it a 1px sliver
            // on the now-visible workspace. Re-home it somewhere reachable;
            // the next park captures its real frame and it self-heals.
            None if Self::at_park_position(f, main) => {
                Some((*pid, window, crate::rescue::clamp_into_visible(f, &main, 0)))
            }
            None => None,
        }
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
    fn apply_app_visibility(&self, frames: &HashMap<WindowId, (Pid, Rect)>) {
        let current = self.ledger.current();
        let assignments = self.ledger.window_ws();
        let mut here_by_app: HashMap<Pid, bool> = HashMap::new();
        for (window, (pid, _)) in frames {
            if let Some(ws) = assignments.get(window) {
                *here_by_app.entry(*pid).or_insert(false) |= *ws == current;
            }
        }
        let focused_app = ax::focused_window().and_then(|w| frames.get(&w).map(|(p, _)| *p));
        for (pid, has_window_here) in here_by_app {
            if has_window_here {
                ax::set_app_hidden(pid, false);
            } else if Some(pid) != focused_app {
                ax::set_app_hidden(pid, true);
            }
        }
    }
}

impl WorkspaceBackend for EmulatedBackend {
    fn topology(
        &mut self,
        windows: &[WindowId],
        monitors: &[(ordo_core::MonitorId, bool)],
    ) -> Result<BackendTopology> {
        // An empty scan is not an observation of emptiness: displays asleep or
        // apps too suspended to answer AX look exactly like "every window
        // closed", and forgetting on that erased every assignment over a
        // weekend (issues.txt, 2026-08-31 — everything re-adopted onto one
        // workspace at wake). Forgetting requires positive evidence: a scan
        // that found SOMETHING but not this window.
        if !windows.is_empty() {
            let before = self.ledger.window_ws();
            self.ledger.forget_missing(windows);
            self.ledger.note_seen(windows);
            // The park bookkeeping must forget in lockstep: a stale `parked`
            // entry for a window the ledger re-adopted turns placement
            // enforcement against the user (it re-slivers a window that now
            // belongs on the visible workspace).
            self.saved.retain(|id, _| windows.contains(id));
            self.parked.retain(|id| windows.contains(id));
            self.enforce_attempts.retain(|id, _| windows.contains(id));
            if self.ledger.window_ws() != before {
                self.persist();
            }
        }

        let mons = monitors
            .iter()
            .map(|(id, _)| MonitorWorkspace {
                monitor: *id,
                active: self.ledger.current(),
                count: self.ledger.count(),
            })
            .collect();
        let window_ws = self.ledger.window_ws().into_iter().collect();

        Ok(BackendTopology {
            monitors: mons,
            window_ws,
        })
    }

    fn switch_workspace(&mut self, target: WorkspaceId) -> Result<()> {
        // Stacking is NOT this backend's problem: the core follows every
        // switch with a RestackWindows effect derived from the MRU history,
        // which the effector reasserts after this returns.
        let plan = self.ledger.switch(target);
        if plan.park.is_empty() && plan.restore.is_empty() {
            self.persist(); // `current` may still have changed
            return Ok(());
        }
        let frames = Self::current_frames();
        let main = main_frame();
        let mut writes = Vec::new();
        for w in plan.park {
            writes.extend(self.park(w, &frames, main));
        }
        for w in plan.restore {
            writes.extend(self.restore(w, &frames, main));
        }
        self.persist();
        ax::set_frames(&writes);
        self.apply_app_visibility(&frames);
        Ok(())
    }

    fn move_window_to_workspace(&mut self, window: WindowId, target: WorkspaceId) -> Result<()> {
        let frames = Self::current_frames();
        let main = main_frame();
        let write = match self.ledger.assign_window(window, target) {
            Some(MoveAction::Park) => self.park(window, &frames, main),
            Some(MoveAction::Restore) => self.restore(window, &frames, main),
            None => return Err(BackendError(format!("workspace {} out of range", target.0))),
        };
        self.persist();
        ax::set_frames(write.as_slice());
        self.apply_app_visibility(&frames);
        Ok(())
    }

    fn bring_up(&mut self, use_state: bool) {
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
            self.suspended = true;
        }
    }

    fn resume_persistence(&mut self) {
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
                let frames = Self::current_frames();
                let main = main_frame();
                let merged =
                    merge_fresh_session(&self.ledger.window_ws(), &self.saved, &ps, |id| {
                        frames
                            .get(id)
                            .is_some_and(|(_, f)| Self::at_park_position(f, main))
                    });
                self.ledger =
                    Ledger::restore(self.ledger.count(), self.ledger.current(), merged.assign);
                // Ledger::restore drops out-of-range claims; the park
                // bookkeeping must not outlive them (a parked entry with no
                // assignment arms enforcement against a workspace-less window).
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

    fn enforce_placement(&mut self, frames: &HashMap<WindowId, (Pid, Rect)>) {
        // This runs on every snapshot; don't pay the display query when
        // there's nothing to enforce.
        if self.parked.is_empty() {
            return;
        }
        let assignments = self.ledger.window_ws();
        let current = self.ledger.current();
        let main = main_frame();
        let mut writes = Vec::new();
        for &w in &self.parked {
            // The ledger outranks the parked set: a window assigned to the
            // visible workspace must never be slivered, whatever stale
            // bookkeeping says (a weekend of ledger amnesia left `parked`
            // full of windows the ledger had re-adopted, and this pass spent
            // the morning re-hiding windows the user had just placed).
            if assignments.get(&w) == Some(&current) {
                continue;
            }
            let Some((pid, f)) = frames.get(&w) else {
                continue;
            };
            if Self::at_park_position(f, main) {
                self.enforce_attempts.remove(&w);
                continue;
            }
            let want = Self::park_frame(*f, main);
            // A freshly issued park looks like a phantom until the app applies
            // it, so the first "attempt" is usually just that write landing;
            // the budget exists for the window that never complies.
            let n = self.enforce_attempts.entry(w).or_insert(0);
            if *n >= ENFORCE_LIMIT {
                continue;
            }
            *n += 1;
            if *n > 1 {
                eprintln!("ordo: re-parking phantom window {} (attempt {n})", w.0);
            }
            writes.push((*pid, w, want));
        }
        ax::set_frames(&writes);
    }

    fn rescue_window(&mut self, window: WindowId) -> Result<()> {
        // Claim it for the visible workspace and bring it back on-screen.
        // No visibility pass here: rescue must only ever reveal, and the
        // gather already unhides every app up front.
        let frames = Self::current_frames();
        self.ledger.assign_window(window, self.ledger.current());
        ax::set_app_hidden(
            frames.get(&window).map(|(p, _)| *p).unwrap_or(Pid(0)),
            false,
        );
        let write = self.restore(window, &frames, main_frame());
        self.persist();
        ax::set_frames(write.as_slice());
        Ok(())
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            // The whole point of emulation: we can mint workspaces freely.
            fixed_workspace_count: false,
            max_workspaces: self.ledger.count(),
        }
    }
}

fn main_frame() -> Rect {
    let displays = display::active_displays();
    displays
        .iter()
        .find(|d| d.is_main)
        .or_else(|| displays.first())
        .map(|d| d.frame)
        .unwrap_or(Rect {
            x: 0.0,
            y: 0.0,
            w: 1920.0,
            h: 1080.0,
        })
}

/// The merged model an S-after-R resume adopts.
struct FreshMerge {
    assign: BTreeMap<WindowId, WorkspaceId>,
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
    model: &BTreeMap<WindowId, WorkspaceId>,
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
            assign.insert(w.id, w.workspace);
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

    #[test]
    fn park_never_captures_a_sliver_as_the_saved_frame() {
        let mut b = EmulatedBackend::new(3);
        let sliver = EmulatedBackend::park_frame(rect(100.0, 100.0), MAIN);
        let frames: HashMap<WindowId, (Pid, Rect)> = [
            (w(1), (Pid(42), sliver)),
            (w(2), (Pid(42), rect(50.0, 60.0))),
        ]
        .into();

        // A window already at the park corner: bookkept as parked, but its
        // (unknown) real frame is never fabricated from the sliver.
        assert_eq!(b.park(w(1), &frames, MAIN), None);
        assert!(b.parked.contains(&w(1)));
        assert!(!b.saved.contains_key(&w(1)));

        // A window at a real position parks normally.
        let write = b.park(w(2), &frames, MAIN).unwrap();
        assert_eq!(b.saved[&w(2)], rect(50.0, 60.0));
        assert!((write.2.x - (MAIN.w - SLIVER)).abs() < 0.5);
    }

    #[test]
    fn fresh_session_merge_keeps_unseen_declarations_and_revives_slivered_ones() {
        let file = PersistedState {
            version: statefile::VERSION,
            boot_time_sec: 1,
            current: ws(1),
            windows: vec![
                // Parked pre-R, never seen by the fresh session.
                PersistedWindow {
                    id: w(10),
                    workspace: ws(3),
                    saved: Some(rect(10.0, 20.0)),
                },
                // Parked pre-R, adopted by the fresh session but still a sliver.
                PersistedWindow {
                    id: w(11),
                    workspace: ws(2),
                    saved: Some(rect(30.0, 40.0)),
                },
                // Parked pre-R, pulled out and placed by the user during R.
                PersistedWindow {
                    id: w(12),
                    workspace: ws(2),
                    saved: Some(rect(70.0, 80.0)),
                },
                // Parked pre-R, and RE-parked by the user during R (so it is
                // physically a sliver, but by this session's own hand).
                PersistedWindow {
                    id: w(14),
                    workspace: ws(2),
                    saved: Some(rect(90.0, 95.0)),
                },
            ],
        };
        let model: BTreeMap<WindowId, WorkspaceId> = [
            (w(11), ws(1)),
            (w(12), ws(1)),
            (w(13), ws(1)),
            (w(14), ws(3)),
        ]
        .into();
        let own_saved: HashMap<WindowId, Rect> = [(w(14), rect(91.0, 96.0))].into();
        let slivered = |id: &WindowId| *id == w(11) || *id == w(14);

        let m = merge_fresh_session(&model, &own_saved, &file, slivered);
        // Unseen: the file's declaration survives S untouched.
        assert_eq!(m.assign[&w(10)], ws(3));
        // Slivered adoptee: adoption was noise, the file's promise wins.
        assert_eq!(m.assign[&w(11)], ws(2));
        // User-placed: the live arrangement is the new truth.
        assert_eq!(m.assign[&w(12)], ws(1));
        // Genuinely new in the fresh session: kept.
        assert_eq!(m.assign[&w(13)], ws(1));
        // Slivered by the session's OWN park: a deliberate placement, not
        // noise — the fresh model and its captured frame win.
        assert_eq!(m.assign[&w(14)], ws(3));
        // Only the noise slivers and unseen windows get file promises back.
        let saved: BTreeMap<_, _> = m.saved.into_iter().collect();
        assert_eq!(saved.get(&w(10)), Some(&rect(10.0, 20.0)));
        assert_eq!(saved.get(&w(11)), Some(&rect(30.0, 40.0)));
        assert_eq!(saved.get(&w(12)), None);
        assert_eq!(saved.get(&w(14)), None);
    }
}
