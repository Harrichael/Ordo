//! The macOS FFI edge — the only part of Ordo that talks to the OS.
//!
//! Assembly of a [`WorldSnapshot`] pulls from three sources and joins them on
//! stable ids: display geometry from Core Graphics ([`display`]), windows from
//! the Accessibility API ([`ax`]), and workspace assignment from the backend
//! ([`native_backend`] over [`skylight`], or [`emulated_backend`] adapting the
//! `ordo-emulated` crate). The backend is shared with the
//! effector via `Rc<RefCell<…>>`; that is sound because the backend, the
//! world source and the effector live on the single engine thread — none of
//! these handles are `Send`, and none ever leave it. What does cross threads
//! is plain data behind locks: the apps' queues ([`crate::app_queue`], each
//! app's `ax::AxApp` living on that app's thread), the restack worker's slot,
//! and the menu bar's list of unreachable apps.
//!
//! NOTE: the FFI here compiles and follows each API's documented shape, but the
//! private SkyLight schema parsing in particular wants validation on-device
//! against the running macOS version (see [`skylight`]).

pub mod ax;
pub mod cf;
pub mod display;
pub mod display_watch;
pub mod effector;
pub mod emulated_backend;
pub mod look_gate;
pub mod mission_control;
pub mod monitor_map;
pub mod workspace_list;
pub mod mouse;
pub mod native_backend;
pub mod observer;
pub mod rescue_gather;
pub mod restack_worker;
pub mod skylight;
pub mod status_item;
pub mod tap;
pub mod ws_events;
pub mod zorder;

pub use effector::MacEffector;

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ordo_core::{
    MonitorId, MonitorSnap, MonitorWs, Pid, Rect, WindowId, WindowSnap, WorkspaceSnap,
    WorldSnapshot,
};

use ordo_emulated::{ParkTrace, ParkTraceKind};

use crate::app_queue::AppQueues;
use crate::backend::WorkspaceBackend;
use crate::ports::{SnapshotStats, WorldSource};

pub type SharedBackend = Rc<RefCell<dyn WorkspaceBackend>>;

/// The apps the last scan found refusing Ordo ([`ax::Walk::refused`]), for
/// the menu bar, which draws on another thread.
pub type Unreachable = Arc<std::sync::Mutex<Vec<Pid>>>;

pub fn native_backend() -> SharedBackend {
    Rc::new(RefCell::new(native_backend::NativeBackend::new()))
}

/// `state_path`: where the ledger's promises persist across restarts;
/// `None` (e.g. `--fresh`) starts empty and stays ephemeral.
pub fn emulated_backend(
    workspaces: u8,
    state_path: Option<std::path::PathBuf>,
    queues: AppQueues,
) -> SharedBackend {
    Rc::new(RefCell::new(match state_path {
        Some(p) => emulated_backend::EmulatedBackend::with_persistence(workspaces, p, queues),
        None => emulated_backend::EmulatedBackend::new(workspaces, queues),
    }))
}

/// The apps' queues, each app's thread working through Accessibility.
pub fn ax_queues() -> AppQueues {
    AppQueues::new(|pid| Box::new(ax::AxApp::open(pid)))
}

/// Builds a full snapshot each call: displays + AX windows + backend workspace
/// classification, joined into the core's vocabulary.
pub struct MacWorldSource {
    backend: SharedBackend,
    /// The tap/effector's shared engagement flag. Placement enforcement rides
    /// the rescan cadence, so it must stop the moment Ordo is paused or
    /// rescued — a rescue gather frees windows the ledger still calls parked,
    /// and fighting the gather would be worse than any phantom.
    intercepting: Arc<AtomicBool>,
    /// Display reconfiguration in progress: the world is unobservable until
    /// it settles (see [`display_watch`]).
    settle: display_watch::DisplaySettle,
    /// Last RAW frame logged per window, so the trace records movement rather
    /// than repeating a still picture every snapshot. Without this the table
    /// grows by every tracked window every two seconds; with it, a settled
    /// desktop writes nothing.
    last_raw: HashMap<WindowId, Rect>,
    /// Drained by the engine after each `snapshot()`.
    trace: Vec<ParkTrace>,
    stats: Option<SnapshotStats>,
    /// Each window's layer and parent, asked once, when it is first seen: they
    /// are fixed at creation, and the asking costs a few milliseconds a
    /// snapshot that re-asking every window would pay forever.
    facts: HashMap<WindowId, zorder::ServerFacts>,
    unreachable: Unreachable,
    /// Every window the last believed snapshot had, with its app: what a
    /// scan that misses windows is checked against.
    last_windows: HashMap<WindowId, Pid>,
    /// Blind scans are being discarded; said once per episode.
    blind: bool,
    /// The windows now kept as `unread`: since when, and whether they've
    /// been called ghosts yet.
    unread_since: HashMap<WindowId, Unread>,
    own_menu: status_item::OwnMenu,
    /// What the focus read stood on at the last scan with Ordo's menu open,
    /// logged on change: opening that menu has moved focus onto a hidden
    /// workspace's window, and why is not yet known.
    menu_focus: Option<(Vec<Pid>, Option<WindowId>)>,
}

struct Unread {
    app: Pid,
    since: Instant,
    ghost: bool,
}

/// A window its app stops listing, though it answers, while the window server
/// still shows the window on screen, for longer than a dropped read lasts: an
/// anomaly worth a line (see `still_there`, which no longer keeps a window
/// that is merely still allocated).
const GHOST_AFTER: Duration = Duration::from_secs(10);

impl MacWorldSource {
    fn note_menu_focus(&mut self, focused: Option<WindowId>) {
        if !self.own_menu.is_open() {
            if self.menu_focus.take().is_some() {
                eprintln!("ordo: own menu closed; focus read {:?}", focused.map(|w| w.0));
            }
            return;
        }
        let seen = (ax::frontmost_claims(), focused);
        if self.menu_focus.as_ref() != Some(&seen) {
            let wall_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_millis());
            let claims: Vec<i32> = seen.0.iter().map(|p| p.0).collect();
            eprintln!(
                "ordo: own menu open at wall {wall_ms}: apps claiming frontmost {claims:?}, focus read {:?}",
                focused.map(|w| w.0)
            );
            self.menu_focus = Some(seen);
        }
    }

    pub fn new(
        backend: SharedBackend,
        intercepting: Arc<AtomicBool>,
        settle: display_watch::DisplaySettle,
        unreachable: Unreachable,
        own_menu: status_item::OwnMenu,
    ) -> Self {
        MacWorldSource {
            backend,
            intercepting,
            settle,
            unreachable,
            last_raw: HashMap::new(),
            trace: Vec::new(),
            stats: None,
            facts: HashMap::new(),
            last_windows: HashMap::new(),
            blind: false,
            unread_since: HashMap::new(),
            own_menu,
            menu_focus: None,
        }
    }
}

impl MacWorldSource {
    /// One `Unread` row when a window starts being kept unread, one if it
    /// turns ghost, and one when it is seen again or gone.
    fn note_unread(&mut self, unread: &[WindowId], scan: &ax::AxScan) {
        let now = Instant::now();
        let row = |w: WindowId, app: Pid, detail: String| {
            let mut t = ParkTrace::new(w, ParkTraceKind::Unread).detail(detail);
            t.pid = Some(app);
            t
        };
        let ended: Vec<WindowId> = self
            .unread_since
            .keys()
            .filter(|w| !unread.contains(w))
            .copied()
            .collect();
        for w in ended {
            let Some(u) = self.unread_since.remove(&w) else {
                continue;
            };
            let fate = if scan.windows.iter().any(|s| s.id == w) {
                "seen again"
            } else {
                "gone"
            };
            let ms = now.duration_since(u.since).as_millis();
            self.trace.push(row(w, u.app, format!("{fate} after {ms} ms")));
        }
        for w in unread {
            let Some(app) = self.last_windows.get(w).copied() else {
                continue;
            };
            let answered = !scan.walk.unanswered.contains(&app);
            let u = self.unread_since.entry(*w).or_insert_with(|| {
                let said = if answered { "app answered" } else { "app didn't answer" };
                self.trace.push(row(*w, app, format!("missed, {said}")));
                Unread {
                    app,
                    since: now,
                    ghost: false,
                }
            });
            if !u.ghost && answered && now.duration_since(u.since) >= GHOST_AFTER {
                u.ghost = true;
                eprintln!(
                    "ordo: window {} of app {} is unlisted by its app for {} s but still in the window server's list; kept as a ghost",
                    w.0,
                    app.0,
                    GHOST_AFTER.as_secs()
                );
                self.trace.push(row(*w, app, "ghost".into()));
            }
        }
    }
}

impl WorldSource for MacWorldSource {
    fn snapshot(&mut self) -> WorldSnapshot {
        // Mid-reconfiguration, frames are wherever macOS has got to with
        // re-homing them and the display list may be half-updated. Reported
        // as no displays at all — the existing "unobservable, not empty" path
        // — so nothing is believed, corrected, or parked off a transient.
        if self.settle.settling() {
            return WorldSnapshot {
                monitors: Vec::new(),
                windows: Vec::new(),
                focused: None,
                workspaces: WorkspaceSnap::default(),
                unread: Vec::new(),
                key_unmanaged: false,
            };
        }
        let started = Instant::now();
        let displays = display::active_displays();
        let known: Vec<(MonitorId, Rect, bool)> = displays
            .iter()
            .map(|d| (d.id, d.frame, d.is_main))
            .collect();

        let mut scan = ax::scan();
        // Facts first: a window's layer decides whether Ordo manages it.
        let unseen: Vec<WindowId> = scan
            .windows
            .iter()
            .map(|w| w.id)
            .filter(|w| !self.facts.contains_key(w))
            .collect();
        if !unseen.is_empty() {
            self.facts.extend(zorder::server_facts(&unseen));
        }
        self.facts.retain(|w, _| scan.windows.iter().any(|s| s.id == *w));
        // Missed against everything the apps listed, so a window Ordo stops
        // managing is not taken for one a read dropped.
        let missed: Vec<WindowId> = self
            .last_windows
            .keys()
            .filter(|w| !scan.windows.iter().any(|s| s.id == **w))
            .copied()
            .collect();
        let facts = &self.facts;
        let (admitted, refused): (Vec<_>, Vec<_>) = std::mem::take(&mut scan.windows)
            .into_iter()
            .partition(|w| ax::admits(w.regular, facts.get(&w.id).and_then(|f| f.layer)));
        scan.windows = admitted;
        let key_unmanaged = scan.focused.is_some_and(|f| refused.iter().any(|w| w.id == f));
        if key_unmanaged {
            scan.focused = None;
        }
        let listed = (!missed.is_empty()).then(zorder::all_windows);
        let unlisted = matches!(listed, Some(None));
        let listed = listed.flatten();
        let ids = listed.as_ref().map(|l| l.iter().map(|x| x.id).collect());
        let alive = still_listed(&missed, ids);
        if is_blind(scan.windows.len(), &alive) {
            if !std::mem::replace(&mut self.blind, true) {
                eprintln!(
                    "ordo: no app lists a window, {}; ignoring scans until windows reappear",
                    if unlisted {
                        "and the window server's list can't be read"
                    } else {
                        "while the window server still has them (screen locked?)"
                    }
                );
            }
            return WorldSnapshot {
                monitors: Vec::new(),
                windows: Vec::new(),
                focused: None,
                workspaces: WorkspaceSnap::default(),
                unread: Vec::new(),
                key_unmanaged: false,
            };
        }
        self.blind = false;
        let on_screen: Option<Vec<WindowId>> =
            listed.map(|l| l.into_iter().filter(|x| x.on_screen).map(|x| x.id).collect());
        let hidden = std::cell::RefCell::new(HashMap::<Pid, bool>::new());
        let app_of = |w: &WindowId| self.last_windows.get(w).copied();
        let unread = still_there(
            alive,
            on_screen.as_deref(),
            |w| app_of(w).is_some_and(|pid| scan.walk.unanswered.contains(&pid)),
            |w| {
                app_of(w).is_some_and(|pid| {
                    *hidden
                        .borrow_mut()
                        .entry(pid)
                        .or_insert_with(|| ax::app_hidden(pid) == Some(true))
                })
            },
        );
        let last_windows: HashMap<WindowId, Pid> = scan
            .windows
            .iter()
            .map(|w| (w.id, w.app))
            .chain(unread.iter().filter_map(|w| self.last_windows.get(w).map(|p| (*w, *p))))
            .collect();
        self.last_windows = last_windows;
        self.note_unread(&unread, &scan);
        self.note_menu_focus(scan.focused);
        let frames: HashMap<WindowId, (Pid, Rect)> = scan
            .windows
            .iter()
            .map(|w| (w.id, (w.app, w.frame)))
            .collect();

        let topo = self
            .backend
            .borrow_mut()
            .topology(&frames, &known)
            .unwrap_or_default();

        let enforce_started = Instant::now();
        if self.intercepting.load(Ordering::Relaxed) {
            // The corrective write lands after this snapshot was read, so the
            // snapshot still shows the phantom; the next rescan absorbs the
            // fix as an (unattributed) external delta. Acceptable for a
            // standing-invariant check.
            self.backend.borrow_mut().enforce_placement(&frames);
        }
        let enforce = enforce_started.elapsed();

        // Mechanism artifacts (park slivers) never reach the core: the
        // backend substitutes the promise each one encodes.
        let believed = self.backend.borrow().believed_frames(&frames);

        // The substitution above is why no other channel can see parking, so
        // capture the raw truth here, before it is laundered — on movement
        // only, and paired with the belief it was replaced by.
        self.trace
            .extend(self.backend.borrow_mut().take_park_trace());
        for (w, (_, raw)) in &frames {
            if self.last_raw.get(w).is_some_and(|p| p == raw) {
                continue;
            }
            self.last_raw.insert(*w, *raw);
            let mut t = ParkTrace::new(*w, ParkTraceKind::Moved).observed(*raw);
            if let Some(b) = believed.get(w) {
                t = t.believed(*b);
            }
            if let (Some(ws), Some(active)) = (
                topo.window_ws.get(w).copied(),
                topo.monitors.first().map(|m| m.active),
            ) {
                t = t.ws(ws, active);
            }
            self.trace.push(t);
        }
        self.last_raw.retain(|w, _| frames.contains_key(w));

        let monitors = displays
            .iter()
            .map(|d| MonitorSnap {
                id: d.id,
                frame: d.frame,
                is_main: d.is_main,
            })
            .collect();

        let facts = &self.facts;
        let windows = scan
            .windows
            .iter()
            .map(|w| WindowSnap {
                id: w.id,
                app: w.app,
                bundle_id: w.bundle_id.clone(),
                title: w.title.clone(),
                frame: believed.get(&w.id).copied().unwrap_or(w.frame),
                subrole: w.subrole.clone(),
                layer: facts.get(&w.id).and_then(|f| f.layer),
                parent: facts.get(&w.id).and_then(|f| f.parent),
            })
            .collect();

        // The workspace layer travels on its own channel, exactly as the
        // backend told it: a monitor or window it didn't resolve is absent —
        // UNKNOWN to the core — never defaulted (the old join fabricated
        // workspace 1 for unresolved windows).
        let workspaces = WorkspaceSnap {
            monitors: topo
                .monitors
                .iter()
                .map(|m| {
                    (
                        m.monitor,
                        MonitorWs {
                            active: m.active,
                            count: m.count,
                        },
                    )
                })
                .collect(),
            assignments: topo.window_ws.iter().map(|(w, ws)| (*w, *ws)).collect(),
            virtual_monitors: topo.virtual_monitors.clone(),
        };

        let mut unreachable = self.unreachable.lock().unwrap();
        for pid in scan.walk.refused.iter().filter(|p| !unreachable.contains(p)) {
            eprintln!(
                "ordo: app {} refuses Accessibility (API disabled); its windows can't be seen or parked until it is reopened",
                pid.0
            );
        }
        *unreachable = scan.walk.refused.clone();
        drop(unreachable);

        self.stats = Some(SnapshotStats {
            total: started.elapsed(),
            walk: scan.walk.elapsed,
            enforce,
            apps: scan.walk.apps,
            windows: scan.windows.len(),
            slowest: scan.walk.slowest,
            held: Duration::ZERO,
        });
        WorldSnapshot {
            monitors,
            windows,
            focused: scan.focused,
            workspaces,
            unread,
            key_unmanaged,
        }
    }

    fn take_park_trace(&mut self) -> Vec<ParkTrace> {
        std::mem::take(&mut self.trace)
    }

    fn take_snapshot_stats(&mut self) -> Option<SnapshotStats> {
        self.stats.take()
    }
}

/// Of `windows`, those the window server's full list still has; all of them
/// when the list can't be read, which is no evidence of anything.
fn still_listed(windows: &[WindowId], listed: Option<Vec<WindowId>>) -> Vec<WindowId> {
    match listed {
        Some(listed) => windows.iter().filter(|w| listed.contains(w)).copied().collect(),
        None => windows.to_vec(),
    }
}

/// Of the windows a scan missed that `still_listed` kept, those the core is
/// told are still there (`WorldSnapshot::unread`), so that a window it is
/// told closed has really gone. An app's AX read drops a window now and then
/// even when the app answers (run 45 seq 1969: a focused Slack window gone
/// for one scan), and the core hands focus on when the focused window closes.
/// Being listed is not enough: an app may order a window out and keep it
/// (Chrome's closed windows, for seconds or for good; the screenshot tool's
/// spent capture bar), and kept, a closed window was never handed on from.
/// So a missed window stays only while it is on screen (a parked one is, at
/// its corner), its app didn't answer, or its app is hidden, which takes its
/// windows off screen too. Without the list only an unanswered app keeps its
/// windows: one that keeps timing out must not keep its closed windows alive.
fn still_there(
    alive: Vec<WindowId>,
    on_screen: Option<&[WindowId]>,
    unanswered: impl Fn(&WindowId) -> bool,
    hidden: impl Fn(&WindowId) -> bool,
) -> Vec<WindowId> {
    alive
        .into_iter()
        .filter(|w| match on_screen {
            None => unanswered(w),
            Some(on) => on.contains(w) || unanswered(w) || hidden(w),
        })
        .collect()
}

/// A scan in which no app listed a window, while windows the model holds are
/// still in the window server's list, saw nothing: the screen is locked or
/// the session asleep (run 48 seq 662: thirteen minutes of such scans erased
/// every window, and every window's place in the MRU order). One whose
/// missing windows are gone from that list too is the last window closing.
fn is_blind(scanned: usize, missed_but_listed: &[WindowId]) -> bool {
    scanned == 0 && !missed_but_listed.is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_scan_is_blind_only_while_the_window_server_still_has_the_missing_windows() {
        let (a, b) = (WindowId(1), WindowId(2));
        let locked = still_listed(&[a, b], Some(vec![a, b, WindowId(9)]));
        assert!(is_blind(0, &locked));
        let unreadable = still_listed(&[a, b], None);
        assert!(is_blind(0, &unreadable));
        let last_closed = still_listed(&[a], Some(vec![WindowId(9)]));
        assert!(!is_blind(0, &last_closed));
        assert!(!is_blind(3, &locked), "a scan that sees windows is not blind");
    }

    #[test]
    fn a_missed_window_is_still_there_only_while_on_screen_or_its_app_cannot_say() {
        // Every app answered without its window. a is on screen (a dropped
        // read, or a parked window at its corner); b is ordered out but still
        // allocated, as a closed Chrome window is; c's app is hidden; d's app
        // timed out; e is gone from the list altogether.
        let (a, b, c, d, e) = (WindowId(1), WindowId(2), WindowId(3), WindowId(4), WindowId(5));
        let unanswered = |w: &WindowId| *w == d;
        let hidden = |w: &WindowId| *w == c;
        let alive = still_listed(&[a, b, c, d, e], Some(vec![a, b, c, d, WindowId(9)]));
        assert_eq!(alive, vec![a, b, c, d]);
        let on_screen = [a, WindowId(9)];
        assert_eq!(still_there(alive, Some(&on_screen), unanswered, hidden), vec![a, c, d]);
        let alive = still_listed(&[a, b, c], None);
        assert_eq!(
            still_there(alive, None, unanswered, hidden),
            Vec::<WindowId>::new(),
            "no list, no evidence: only an unanswered app keeps its windows"
        );
    }
}
