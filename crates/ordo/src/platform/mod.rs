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
}

impl MacWorldSource {
    pub fn new(
        backend: SharedBackend,
        intercepting: Arc<AtomicBool>,
        settle: display_watch::DisplaySettle,
        unreachable: Unreachable,
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
            };
        }
        let started = Instant::now();
        let displays = display::active_displays();
        let known: Vec<(MonitorId, Rect, bool)> = displays
            .iter()
            .map(|d| (d.id, d.frame, d.is_main))
            .collect();

        let scan = ax::scan();
        let missed: Vec<WindowId> = self
            .last_windows
            .keys()
            .filter(|w| !scan.windows.iter().any(|s| s.id == **w))
            .copied()
            .collect();
        let listed = (!missed.is_empty()).then(zorder::all_windows);
        let unlisted = matches!(listed, Some(None));
        let alive = still_listed(&missed, listed.flatten());
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
            };
        }
        self.blind = false;
        // A window its app didn't answer for is kept only while the window
        // server has it: an app that keeps timing out must not keep its
        // closed windows alive.
        let unread: Vec<WindowId> = alive
            .into_iter()
            .filter(|w| {
                self.last_windows
                    .get(w)
                    .is_some_and(|pid| scan.walk.unanswered.contains(pid))
            })
            .collect();
        let last_windows: HashMap<WindowId, Pid> = scan
            .windows
            .iter()
            .map(|w| (w.id, w.app))
            .chain(unread.iter().filter_map(|w| self.last_windows.get(w).map(|p| (*w, *p))))
            .collect();
        self.last_windows = last_windows;
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

        let unseen: Vec<WindowId> = scan
            .windows
            .iter()
            .map(|w| w.id)
            .filter(|w| !self.facts.contains_key(w))
            .collect();
        if !unseen.is_empty() {
            self.facts.extend(zorder::server_facts(&unseen));
        }
        self.facts.retain(|w, _| frames.contains_key(w));
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
}
