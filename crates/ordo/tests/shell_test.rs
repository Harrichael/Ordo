//! Integration tests for the shell, in the house style: the engine, the real
//! SQLite logger, the real pure core, and the real replay checker all run
//! together against an in-memory database. The only fake is the OS itself — a
//! scripted world at the [`WorldSource`] seam — because a live WindowServer
//! can't be driven reproducibly. Everything above that seam is the real thing.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::rc::Rc;
use std::time::{Duration, Instant};

use ordo::clock::Clock;
use ordo::engine::{Engine, Msg};
use ordo::logger::Logger;
use ordo::menubar::{MenuBarView, MonitorEntry, MonitorsView, WorkspaceEntry};
use ordo::ports::{Effector, NullEffector, SnapshotStats, WorldSource};
use ordo::replay::replay;
use ordo_core::{
    after_merge, anchor_after_add, AxHintKind, Effect, Event, FocusIntent, Gesture, HotkeyAction, MonitorId, MonitorSnap, MonitorWs, OpOutcome, Pid,
    Rect, RescanTrigger, VirtualMonitorId, VirtualMonitors, VirtualMonitorsWord, WindowId,
    WindowSnap, WorkspaceId, WorkspaceSnap, WorldSnapshot,
};
use rusqlite::Connection;

// --- fakes at the OS seam --------------------------------------------------

/// A world that hands out a scripted sequence of snapshots, one per rescan,
/// repeating the last forever (a real machine keeps answering after the script
/// runs out).
struct ScriptedWorld {
    snaps: Vec<WorldSnapshot>,
    at: Rc<Cell<usize>>,
}

impl WorldSource for ScriptedWorld {
    fn snapshot(&mut self) -> WorldSnapshot {
        let i = self.at.get().min(self.snaps.len() - 1);
        self.at.set(i + 1);
        self.snaps[i].clone()
    }

    /// Scripted snapshots come from fixtures, not a parking mechanism.
    fn take_park_trace(&mut self) -> Vec<ordo_emulated::ParkTrace> {
        Vec::new()
    }

    fn take_snapshot_stats(&mut self) -> Option<SnapshotStats> {
        None
    }
}

/// Reports every workspace switch as OS-successful, so the engine's cascade
/// (effect -> result -> confirming rescan) runs end to end.
struct OkEffector;

impl Effector for OkEffector {
    fn execute(&mut self, effect: &Effect) -> Option<OpOutcome> {
        match effect {
            Effect::WarpMouse { .. }
            | Effect::SetIntercepting { .. }
            | Effect::RequestRescan { .. } => None,
            _ => Some(OpOutcome::Ok),
        }
    }
}

/// How a desktop answers a focus grant. The scripted world above always
/// agrees with the core, which is exactly why the suite never saw a grant
/// that did not land: real apps ignore grants, or key a sibling instead.
#[derive(Clone, Copy)]
enum FocusPolicy {
    Lands,
    Ignored,
    /// Every grant to a window of this sibling's app lands on the sibling.
    Sibling(WindowId),
}

/// A desktop that REACTS to effects — the way the real one does, imperfectly
/// — shared between a world source and an effector so the engine's cascade
/// (effect -> result -> confirming rescan) runs against one consistent fake.
/// Frames and assignments follow the effects; focus follows `policy`; and a
/// test can fling focus wherever an app might, between observations.
struct FakeOs {
    active: WorkspaceId,
    assignments: BTreeMap<WindowId, WorkspaceId>,
    windows: Vec<WindowSnap>,
    focused: Option<WindowId>,
    policy: FocusPolicy,
    /// The emulated backend's word on the virtual layer: two monitors, one per
    /// display while both are plugged in.
    view: VirtualMonitors,
    monitors: BTreeMap<WindowId, VirtualMonitorId>,
    /// How many displays are plugged in (1 or 2).
    displays: usize,
}

impl FakeOs {
    fn snapshot(&self) -> WorldSnapshot {
        let monitors: Vec<MonitorSnap> = [mon(1, 0.0), mon(2, 1920.0)]
            .into_iter()
            .take(self.displays)
            .collect();
        WorldSnapshot {
            workspaces: WorkspaceSnap {
                monitors: monitors
                    .iter()
                    .map(|m| {
                        (
                            m.id,
                            MonitorWs {
                                active: self.active,
                                count: 3,
                            },
                        )
                    })
                    .collect(),
                assignments: self.assignments.clone(),
                virtual_monitors: Some(VirtualMonitorsWord {
                    view: self.view,
                    assignments: self.monitors.clone(),
                }),
            },
            monitors,
            windows: self.windows.clone(),
            focused: self.focused,
        }
    }
}

struct FakeWorld(Rc<RefCell<FakeOs>>);

impl WorldSource for FakeWorld {
    fn snapshot(&mut self) -> WorldSnapshot {
        self.0.borrow().snapshot()
    }

    fn take_park_trace(&mut self) -> Vec<ordo_emulated::ParkTrace> {
        Vec::new()
    }

    /// A fixed cost per snapshot, so the log's side channel can be checked
    /// against the snapshots it describes.
    fn take_snapshot_stats(&mut self) -> Option<SnapshotStats> {
        Some(SnapshotStats {
            total: Duration::from_millis(9),
            walk: Duration::from_millis(6),
            enforce: Duration::from_millis(2),
            apps: 2,
            windows: self.0.borrow().windows.len(),
            slowest: Some((Pid(200), Duration::from_millis(6))),
        })
    }
}

struct FakeEffector(Rc<RefCell<FakeOs>>);

impl Effector for FakeEffector {
    fn execute(&mut self, effect: &Effect) -> Option<OpOutcome> {
        let mut os = self.0.borrow_mut();
        match effect {
            Effect::SwitchWorkspace { target, .. } => os.active = *target,
            Effect::MoveWindowToWorkspace { window, target, .. }
            | Effect::AssignWindowToWorkspace { window, target, .. } => {
                os.assignments.insert(*window, *target);
            }
            Effect::SetWindowFrame { window, frame, .. } => {
                if let Some(w) = os.windows.iter_mut().find(|w| w.id == *window) {
                    w.frame = *frame;
                }
            }
            Effect::AssignWindowToMonitor { window, target, .. } => {
                os.monitors.insert(*window, *target);
            }
            Effect::ViewMonitor { target, .. } => os.view.viewed = *target,
            // The desktop is no window of the model.
            Effect::FocusDesktop { .. } => os.focused = None,
            Effect::SetVirtualMonitors { enabled, .. } => os.view.enabled = *enabled,
            Effect::MergeMonitors { from, into, .. } => {
                for m in os.monitors.values_mut() {
                    *m = after_merge(*m, *from, *into);
                }
                os.view.viewed = after_merge(os.view.viewed, *from, *into);
                os.view.count -= 1;
            }
            Effect::AddMonitor { .. } => {
                os.view.viewed = anchor_after_add(os.view.count, os.view.viewed, os.view.enabled, os.displays);
                os.view.count += 1;
            }
            Effect::FocusWindow { window, .. } => match os.policy {
                FocusPolicy::Lands => os.focused = Some(*window),
                FocusPolicy::Ignored => {}
                FocusPolicy::Sibling(sib) => {
                    let same_app = os.windows.iter().find(|w| w.id == *window).map(|w| w.app)
                        == os.windows.iter().find(|w| w.id == sib).map(|w| w.app);
                    os.focused = Some(if same_app { sib } else { *window });
                }
            },
            Effect::WarpMouse { .. }
            | Effect::RestackWindows { .. }
            | Effect::SetIntercepting { .. }
            | Effect::RequestRescan { .. } => return None,
        }
        // The executor's own view: the write was accepted. Whether it took is
        // the next snapshot's business — the whole point of the fake.
        Some(OpOutcome::Ok)
    }
}

/// w1 (pid 100) and w3 (pid 100) on workspace 1, w2 (pid 200) and w4 (pid
/// 200) on workspace 2; the user is on workspace 1 with w1 focused.
fn fake_os(policy: FocusPolicy) -> Rc<RefCell<FakeOs>> {
    Rc::new(RefCell::new(FakeOs {
        active: WorkspaceId(1),
        assignments: [
            (WindowId(1), WorkspaceId(1)),
            (WindowId(2), WorkspaceId(2)),
            (WindowId(3), WorkspaceId(1)),
            (WindowId(4), WorkspaceId(2)),
        ]
        .into(),
        windows: vec![
            win(1, 100, 100.0),
            win(2, 200, 2000.0),
            win(3, 100, 600.0),
            win(4, 200, 2400.0),
        ],
        focused: Some(WindowId(1)),
        policy,
        view: VirtualMonitors {
            count: 2,
            viewed: VirtualMonitorId(1),
            enabled: true,
        },
        // By display: w1/w3 on the left one, w2/w4 on the right.
        monitors: [
            (WindowId(1), VirtualMonitorId(1)),
            (WindowId(2), VirtualMonitorId(2)),
            (WindowId(3), VirtualMonitorId(1)),
            (WindowId(4), VirtualMonitorId(2)),
        ]
        .into(),
        displays: 2,
    }))
}

fn engine_on(os: &Rc<RefCell<FakeOs>>, logger: Logger) -> Engine {
    Engine::new(
        logger,
        Box::new(FakeWorld(os.clone())),
        Box::new(FakeEffector(os.clone())),
        Box::new(StepClock { n: Cell::new(0) }),
    )
}

fn at(n: i64) -> ordo_core::Ts {
    ordo_core::Ts {
        wall_ms: n,
        mono_ns: n as u64,
    }
}

/// A monotonic fake clock: reproducible timestamps for reproducible logs. The
/// step is a plausible inter-event gap rather than one tick, because the core
/// ages expectations by elapsed `mono_ns` — a clock that barely moved would
/// leave every op pending forever and quietly retire the expiry paths below.
struct StepClock {
    n: Cell<u64>,
}

const STEP_MS: u64 = 100;

impl Clock for StepClock {
    fn now(&self) -> ordo_core::Ts {
        let n = self.n.get();
        self.n.set(n + 1);
        ordo_core::Ts {
            wall_ms: 1_000 + (n * STEP_MS) as i64,
            mono_ns: n * STEP_MS * 1_000_000,
        }
    }
}

// --- snapshot fixtures -----------------------------------------------------

fn mon(id: u8, x: f64) -> MonitorSnap {
    MonitorSnap {
        id: MonitorId(id as u128),
        frame: Rect {
            x,
            y: 0.0,
            w: 1920.0,
            h: 1080.0,
        },
        is_main: id == 1,
    }
}

fn win(id: u32, pid: i32, x: f64) -> WindowSnap {
    WindowSnap {
        id: WindowId(id),
        app: Pid(pid),
        bundle_id: Some(format!("app.{pid}")),
        title: format!("w{id}"),
        frame: Rect {
            x,
            y: 100.0,
            w: 400.0,
            h: 300.0,
        },
        subrole: None,
        layer: None,
        parent: None,
    }
}

fn snap(focused: Option<u32>, a_ws: u8, b_ws: u8) -> WorldSnapshot {
    WorldSnapshot {
        monitors: vec![mon(1, 0.0), mon(2, 1920.0)],
        windows: vec![win(1, 100, 100.0), win(2, 200, 2000.0), win(3, 100, 600.0)],
        focused: focused.map(WindowId),
        workspaces: WorkspaceSnap {
            monitors: [
                (
                    MonitorId(1),
                    MonitorWs {
                        active: WorkspaceId(a_ws),
                        count: 3,
                    },
                ),
                (
                    MonitorId(2),
                    MonitorWs {
                        active: WorkspaceId(b_ws),
                        count: 3,
                    },
                ),
            ]
            .into(),
            assignments: [
                (WindowId(1), WorkspaceId(a_ws)),
                (WindowId(2), WorkspaceId(b_ws)),
                (WindowId(3), WorkspaceId(a_ws)),
            ]
            .into(),
            // A backend with no virtual layer, as every log before it existed.
            virtual_monitors: None,
        },
    }
}

fn in_memory_logger(backend: &str) -> Logger {
    Logger::from_conn(
        Connection::open_in_memory().unwrap(),
        "test",
        backend,
        1_000,
    )
    .unwrap()
}

fn count(conn: &Connection, sql: &str) -> i64 {
    conn.query_row(sql, [], |r| r.get(0)).unwrap()
}

// --- tests -----------------------------------------------------------------

#[test]
fn observe_mode_logs_the_world_without_emitting_actions() {
    // Boot, then observe two more times: the log should hold the events and
    // (since NullEffector executes nothing) no workspace/focus effects.
    let world = ScriptedWorld {
        snaps: vec![snap(Some(1), 1, 1), snap(Some(2), 1, 1)],
        at: Rc::new(Cell::new(0)),
    };
    let logger = in_memory_logger("native");
    let mut engine = Engine::new(
        logger,
        Box::new(world),
        Box::new(NullEffector),
        Box::new(StepClock { n: Cell::new(0) }),
    );

    // Drive observations directly — this is exactly what run()'s loop does.
    engine.observe(ordo_core::RescanTrigger::Startup);
    engine.observe(ordo_core::RescanTrigger::Periodic);

    let s = engine.state();
    assert_eq!(s.windows.len(), 3);
    assert_eq!(s.focused, Some(WindowId(2)));
    // Focus moved 1 -> 2 across the two scans, but observe mode acts on nothing.
    assert_eq!(s.monitor_ws[&MonitorId(1)], WorkspaceId(1));
}

#[test]
fn logged_run_replays_without_divergence() {
    // File-backed so the replay reader opens the same bytes the engine wrote.
    let dir = std::env::temp_dir().join(format!("ordo-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("replay.db");
    let _ = std::fs::remove_file(&db);

    let run_id = {
        let logger = Logger::open(&db, "test", "native", 1_000).unwrap();
        let run_id = logger.run_id();
        let world = ScriptedWorld {
            snaps: vec![
                snap(Some(3), 1, 1),
                snap(Some(2), 1, 1),
                snap(Some(1), 1, 1),
                snap(Some(1), 2, 2),
            ],
            at: Rc::new(Cell::new(0)),
        };
        let mut engine = Engine::new(
            logger,
            Box::new(world),
            Box::new(OkEffector),
            Box::new(StepClock { n: Cell::new(0) }),
        );
        engine.observe(ordo_core::RescanTrigger::Startup);
        engine.observe(ordo_core::RescanTrigger::Periodic);
        engine.observe(ordo_core::RescanTrigger::Periodic);
        engine.pump(ordo_core::Event::Hotkey {
            at: ordo_core::Ts {
                wall_ms: 2_000,
                mono_ns: 999,
            },
            action: ordo_core::HotkeyAction::WorkspaceNext,
        });
        run_id
    };

    let conn = Connection::open(&db).unwrap();
    // The session actually did something worth logging.
    assert!(count(&conn, "SELECT COUNT(*) FROM events") >= 4);
    assert!(
        count(
            &conn,
            "SELECT COUNT(*) FROM effects WHERE kind = 'switch_workspace'"
        ) >= 1
    );

    let report = replay(&conn, run_id, None).unwrap();
    assert!(
        report.is_clean(),
        "replay diverged from the log: {:?}",
        report.mismatches
    );
    assert!(report.events_replayed >= 4);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn replay_from_a_checkpoint_is_clean_and_skips_the_checkpointed_event() {
    // F1: a checkpoint at seq S stores the state *after* event S. Resuming must
    // start at S+1, not re-apply S. A checkpoint is always written at seq 0, so
    // replay(Some(0)) resumes at seq 1 and must be clean — the off-by-one used
    // to double-apply event 0 and report a spurious mismatch.
    let dir = std::env::temp_dir().join(format!("ordo-cp-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("cp.db");
    let _ = std::fs::remove_file(&db);

    let run_id = {
        let logger = Logger::open(&db, "test", "native", 1_000).unwrap();
        let run_id = logger.run_id();
        let world = ScriptedWorld {
            snaps: vec![
                snap(Some(3), 1, 1),
                snap(Some(2), 1, 1),
                snap(Some(1), 2, 2),
            ],
            at: Rc::new(Cell::new(0)),
        };
        let mut engine = Engine::new(
            logger,
            Box::new(world),
            Box::new(OkEffector),
            Box::new(StepClock { n: Cell::new(0) }),
        );
        engine.observe(ordo_core::RescanTrigger::Startup);
        engine.observe(ordo_core::RescanTrigger::Periodic);
        engine.pump(ordo_core::Event::Hotkey {
            at: ordo_core::Ts {
                wall_ms: 2_000,
                mono_ns: 999,
            },
            action: ordo_core::HotkeyAction::WorkspaceNext,
        });
        run_id
    };

    let conn = Connection::open(&db).unwrap();
    let total = count(&conn, "SELECT COUNT(*) FROM events");
    assert!(count(&conn, "SELECT COUNT(*) FROM checkpoints WHERE seq = 0") == 1);

    let full = replay(&conn, run_id, None).unwrap();
    assert!(
        full.is_clean(),
        "full replay diverged: {:?}",
        full.mismatches
    );
    assert_eq!(
        full.events_replayed as i64, total,
        "full run verified from empty"
    );

    let from_cp = replay(&conn, run_id, Some(0)).unwrap();
    assert!(
        from_cp.is_clean(),
        "checkpoint replay diverged: {:?}",
        from_cp.mismatches
    );
    // Resumed after event 0, so it verifies one fewer event.
    assert_eq!(from_cp.events_replayed as i64, total - 1);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn retention_prunes_runs_older_than_the_window() {
    let dir = std::env::temp_dir().join(format!("ordo-ret-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("ret.db");
    let _ = std::fs::remove_file(&db);

    // An ancient run (wall time 0), then "now" is 30 days later: opening a new
    // run must sweep the ancient one away.
    {
        let mut old = Logger::open(&db, "test", "native", 0).unwrap();
        old.close(0).unwrap();
    }
    let thirty_days_ms = 30i64 * 24 * 60 * 60 * 1000;
    {
        let _fresh = Logger::open(&db, "test", "native", thirty_days_ms).unwrap();
    }

    let conn = Connection::open(&db).unwrap();
    // Only the fresh run survives.
    assert_eq!(count(&conn, "SELECT COUNT(*) FROM runs"), 1);

    let _ = std::fs::remove_dir_all(&dir);
}

// --- focus: the desktop disagrees ----------------------------------------------

#[test]
fn a_focus_grant_the_desktop_ignores_is_retried_then_stood_down() {
    // The switch grants w2 focus; this desktop never lands it, so observed
    // focus stays on w1, now parked on hidden workspace 1. Through the real
    // engine cascade (each grant's own post-effect rescan included): the
    // grant is retried under the damping limit, the standoff is logged and
    // retires the claim, and the stuck focus is never read as the user
    // navigating back to workspace 1.
    let dir = std::env::temp_dir().join(format!("ordo-ignored-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("ignored.db");
    let _ = std::fs::remove_file(&db);
    let os = fake_os(FocusPolicy::Ignored);
    {
        let mut engine = engine_on(&os, Logger::open(&db, "test", "emulated", 1_000).unwrap());
        engine.observe(RescanTrigger::Startup);
        engine.pump(ordo_core::Event::Hotkey {
            at: at(2_000),
            action: HotkeyAction::WorkspaceNext,
        });
        let mut observations = 0;
        while engine.state().focus_intent() != FocusIntent::Deferred {
            engine.observe(RescanTrigger::Periodic);
            observations += 1;
            // Generous because expiry is elapsed time now: at STEP_MS per
            // clock read, waiting out one grant's TTL costs several rescans.
            assert!(observations <= 48, "never stood down");
        }
        // The stand-down is where the interesting behaviour starts, not
        // where it ends: focus is still on a hidden window, so keep watching
        // long enough for several more damping episodes to have run.
        for _ in 0..40 {
            engine.observe(RescanTrigger::Periodic);
        }
        assert_eq!(engine.state().focused, Some(WindowId(1)));
        assert_eq!(engine.state().focus_intent(), FocusIntent::Deferred);
    }
    assert_eq!(os.borrow().active, WorkspaceId(2), "no snap-back");
    let conn = Connection::open(&db).unwrap();
    assert_eq!(
        count(
            &conn,
            "SELECT COUNT(*) FROM effects WHERE kind = 'focus_window'"
        ),
        4,
        "the command's grant + 3 re-assertions, and none after the stand-down"
    );
    assert_eq!(
        count(
            &conn,
            "SELECT COUNT(*) FROM notes WHERE kind = 'focus_diverged'"
        ),
        1
    );
    assert_eq!(
        count(
            &conn,
            "SELECT COUNT(*) FROM effects WHERE kind = 'switch_workspace'"
        ),
        1
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_grant_that_lands_on_a_sibling_is_corrected_until_the_app_gives_in() {
    // Chrome's habit: a grant to w2 keys its sibling w4. After the first
    // retry the fake relents and lands grants properly; the engine converges
    // on the declared window with the MRU order untouched by the theft.
    let os = fake_os(FocusPolicy::Sibling(WindowId(4)));
    let mut engine = engine_on(&os, in_memory_logger("emulated"));
    engine.observe(RescanTrigger::Startup);
    engine.pump(ordo_core::Event::Hotkey {
        at: at(2_000),
        action: HotkeyAction::WorkspaceNext,
    });
    assert_eq!(
        os.borrow().focused,
        Some(WindowId(4)),
        "the sibling took it"
    );
    // Enough rescans either side of the relenting for the ignored grant's
    // expectation to time out and the retry to be issued.
    for _ in 0..8 {
        engine.observe(RescanTrigger::Periodic);
    }
    os.borrow_mut().policy = FocusPolicy::Lands;
    for _ in 0..8 {
        engine.observe(RescanTrigger::Periodic);
    }
    assert_eq!(os.borrow().focused, Some(WindowId(2)));
    assert_eq!(engine.state().focused, Some(WindowId(2)));
    assert_eq!(
        engine.state().focus_history.iter().next(),
        Some(WindowId(2)),
        "w4's stolen focus never became most recent"
    );
}

#[test]
fn an_unwitnessed_fling_is_pulled_back_and_a_witnessed_switch_is_followed() {
    // The OS owns focus after start. An app flings it to parked w2: the
    // engine pulls it back to the visible MRU window and stays put. The same
    // fling right after a witnessed Cmd+Tab is navigation, and followed.
    let os = fake_os(FocusPolicy::Lands);
    let mut engine = engine_on(&os, in_memory_logger("emulated"));
    engine.observe(RescanTrigger::Startup);
    assert_eq!(engine.state().focus_intent(), FocusIntent::Deferred);

    os.borrow_mut().focused = Some(WindowId(2));
    engine.observe(RescanTrigger::Periodic);
    assert_eq!(os.borrow().active, WorkspaceId(1), "held");
    assert_eq!(os.borrow().focused, Some(WindowId(1)), "pulled back");
    assert_eq!(
        engine.state().focus_intent(),
        FocusIntent::Window(WindowId(1))
    );

    engine.pump(ordo_core::Event::Gesture {
        at: at(3_000),
        gesture: Gesture::SystemSwitch,
    });
    os.borrow_mut().focused = Some(WindowId(2));
    engine.observe(RescanTrigger::Periodic);
    assert_eq!(os.borrow().active, WorkspaceId(2), "followed");
    assert_eq!(os.borrow().focused, Some(WindowId(2)));
    assert_eq!(
        engine.state().focus_intent(),
        FocusIntent::Window(WindowId(2))
    );
}

#[test]
fn a_gesture_keeps_its_place_between_hotkeys_and_replays_clean() {
    // Through the real message loop: a click between two queued switches is
    // a fence — the presses are not folded into one jump across it, and the
    // last command owns the declaration — and the logged run, gesture
    // included, replays without divergence.
    let dir = std::env::temp_dir().join(format!("ordo-gesture-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("gesture.db");
    let _ = std::fs::remove_file(&db);
    let os = fake_os(FocusPolicy::Lands);
    let run_id = {
        let logger = Logger::open(&db, "test", "emulated", 1_000).unwrap();
        let run_id = logger.run_id();
        let engine = engine_on(&os, logger);
        let (tx, rx) = crossbeam_channel::unbounded::<Msg>();
        tx.send(Msg::hotkey(HotkeyAction::WorkspaceNext)).unwrap();
        tx.send(Msg::Gesture(Gesture::MouseDown {
            at: ordo_core::Point {
                x: 960.0,
                y: 1075.0,
            },
        }))
        .unwrap();
        tx.send(Msg::hotkey(HotkeyAction::WorkspaceNext)).unwrap();
        tx.send(Msg::Shutdown).unwrap();
        engine.run(rx);
        run_id
    };
    assert_eq!(os.borrow().active, WorkspaceId(3));
    let conn = Connection::open(&db).unwrap();
    assert_eq!(
        count(
            &conn,
            "SELECT COUNT(*) FROM effects WHERE kind = 'switch_workspace'"
        ),
        2,
        "the gesture fenced the fold"
    );
    let order: Vec<String> = conn
        .prepare("SELECT kind FROM events WHERE kind IN ('hotkey','gesture') ORDER BY seq")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(order, ["hotkey", "gesture", "hotkey"]);
    // The click's verdict must reach the DB under its kind string, or an
    // unarmed follow is undebuggable from the log.
    assert_eq!(
        count(
            &conn,
            "SELECT COUNT(*) FROM notes WHERE kind = 'gesture_classified'"
        ),
        1
    );
    let report = replay(&conn, run_id, None).unwrap();
    assert!(report.is_clean(), "{:?}", report.mismatches);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Presses that queue behind the engine are one burst: they fold into a single
/// switch, and the log keeps what the core event can't — how many there were
/// and how long they waited — since the core stamps hotkeys at dequeue.
#[test]
fn a_queued_burst_is_logged_with_its_presses_and_their_wait() {
    let dir = std::env::temp_dir().join(format!("ordo-burst-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("burst.db");
    let _ = std::fs::remove_file(&db);
    let os = fake_os(FocusPolicy::Lands);
    {
        let logger = Logger::open(&db, "test", "emulated", 1_000).unwrap();
        let engine = engine_on(&os, logger);
        let (tx, rx) = crossbeam_channel::unbounded::<Msg>();
        let now = Instant::now();
        for ago in [300, 100] {
            tx.send(Msg::Hotkey(
                HotkeyAction::WorkspaceNext,
                now - Duration::from_millis(ago),
            ))
            .unwrap();
        }
        tx.send(Msg::Shutdown).unwrap();
        engine.run(rx);
    }
    assert_eq!(os.borrow().active, WorkspaceId(3));
    let conn = Connection::open(&db).unwrap();
    let (seq, presses, pumped, oldest, newest): (Option<i64>, i64, i64, i64, i64) = conn
        .query_row(
            "SELECT seq, presses, pumped, oldest_wait_ms, newest_wait_ms FROM hotkey_batches",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .unwrap();
    assert_eq!((presses, pumped), (2, 1));
    assert!(oldest >= 300 && newest >= 100 && newest < oldest);
    let kind: String = conn
        .query_row(
            "SELECT kind FROM events WHERE seq = ?1",
            [seq.expect("a batch that pumped names its event")],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(kind, "hotkey");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Hints queued behind a busy engine are one look at the world, taken before
/// the hotkeys queued with them: a snapshot reads the present wherever its
/// hint sat, so a hint landing between two presses must not keep them from
/// folding into one switch. Creation hints are kept, one per app, because only
/// they license corralling the new window.
#[test]
fn queued_rescans_become_one_look_ahead_of_the_presses_they_would_have_split() {
    let dir = std::env::temp_dir().join(format!("ordo-collapse-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("collapse.db");
    let _ = std::fs::remove_file(&db);
    let os = fake_os(FocusPolicy::Lands);
    let hint = |pid: Option<i32>, kind: AxHintKind| {
        Msg::Rescan(RescanTrigger::AxHint {
            pid: pid.map(Pid),
            kind,
        })
    };
    let focus = || AxHintKind::Other("AXFocusedWindowChanged".into());
    let run_id = {
        let logger = Logger::open(&db, "test", "emulated", 1_000).unwrap();
        let run_id = logger.run_id();
        let engine = engine_on(&os, logger);
        let (tx, rx) = crossbeam_channel::unbounded::<Msg>();
        for m in [
            hint(Some(7), focus()),
            Msg::hotkey(HotkeyAction::WorkspaceNext),
            hint(Some(8), focus()),
            hint(Some(7), AxHintKind::WindowCreated),
            Msg::hotkey(HotkeyAction::WorkspaceNext),
            hint(Some(7), AxHintKind::WindowCreated),
            hint(Some(8), AxHintKind::WindowCreated),
            Msg::Shutdown,
        ] {
            tx.send(m).unwrap();
        }
        engine.run(rx);
        run_id
    };
    let conn = Connection::open(&db).unwrap();
    let steps: Vec<(String, String)> = conn
        .prepare("SELECT kind, payload FROM events WHERE kind IN ('world_observed', 'hotkey') ORDER BY seq")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    let hints_before_the_press: Vec<(Option<i32>, bool)> = steps
        .iter()
        .take_while(|(kind, _)| kind != "hotkey")
        .filter_map(|(_, p)| match serde_json::from_str(p).unwrap() {
            Event::WorldObserved {
                trigger: RescanTrigger::AxHint { pid, kind },
                ..
            } => Some((pid.map(|p| p.0), kind == AxHintKind::WindowCreated)),
            _ => None,
        })
        .collect();
    assert_eq!(hints_before_the_press, [(Some(7), true), (Some(8), true)]);
    assert_eq!(steps.iter().filter(|(kind, _)| kind == "hotkey").count(), 1, "the two presses folded");
    assert_eq!(os.borrow().active, WorkspaceId(3));
    let report = replay(&conn, run_id, None).unwrap();
    assert!(report.is_clean(), "{:?}", report.mismatches);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Every snapshot's cost lands beside it in the log, keyed to its own event —
/// the rescan a hotkey waited behind is findable from the hotkey's batch.
#[test]
fn each_snapshot_logs_what_it_cost_against_its_own_event() {
    let dir = std::env::temp_dir().join(format!("ordo-snapcost-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("snapcost.db");
    let _ = std::fs::remove_file(&db);
    let os = fake_os(FocusPolicy::Lands);
    {
        let logger = Logger::open(&db, "test", "emulated", 1_000).unwrap();
        let engine = engine_on(&os, logger);
        let (tx, rx) = crossbeam_channel::unbounded::<Msg>();
        tx.send(Msg::hotkey(HotkeyAction::WorkspaceNext)).unwrap();
        tx.send(Msg::Shutdown).unwrap();
        engine.run(rx);
    }
    let conn = Connection::open(&db).unwrap();
    let observed: Vec<i64> = conn
        .prepare("SELECT seq FROM events WHERE kind = 'world_observed' ORDER BY seq")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    let costed: Vec<(i64, f64, i64)> = conn
        .prepare("SELECT seq, total_ms, slowest_pid FROM snapshots ORDER BY seq")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert!(observed.len() >= 2, "the startup scan and the switch's rescan");
    assert_eq!(costed.iter().map(|c| c.0).collect::<Vec<_>>(), observed);
    assert!(costed.iter().all(|c| c.1 == 9.0 && c.2 == 200));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_view_change_on_one_display_runs_end_to_end_and_replays_clean() {
    // The laptop rig, through the real engine, logger and replay: the
    // external display is gone, so w2 and w4 (monitor 2) are hidden. Cmd+Alt+K
    // views monitor 2 — focus goes to its MRU window, the backend's word
    // confirms the anchor — and the logged run replays without divergence.
    let dir = std::env::temp_dir().join(format!("ordo-view-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("view.db");
    let _ = std::fs::remove_file(&db);
    let os = fake_os(FocusPolicy::Lands);
    {
        let mut o = os.borrow_mut();
        o.displays = 1;
        // w2 shares workspace 1 with w1 and w3, on the other monitor.
        o.assignments.insert(WindowId(2), WorkspaceId(1));
        // macOS re-homed the second display's windows onto the laptop.
        for w in o.windows.iter_mut() {
            if w.frame.x >= 1920.0 {
                w.frame.x -= 1920.0;
            }
        }
    }
    let run_id = {
        let logger = Logger::open(&db, "test", "emulated", 1_000).unwrap();
        let run_id = logger.run_id();
        let mut engine = engine_on(&os, logger);
        engine.observe(RescanTrigger::Startup);
        assert!(!engine
            .state()
            .is_visible(&engine.state().windows[&WindowId(2)]));

        engine.pump(ordo_core::Event::Hotkey {
            at: at(2_000),
            action: HotkeyAction::ViewMonitorNext,
        });
        assert_eq!(os.borrow().view.viewed, VirtualMonitorId(2));
        assert_eq!(os.borrow().focused, Some(WindowId(2)));
        assert_eq!(
            engine.state().focus_intent(),
            FocusIntent::Window(WindowId(2))
        );
        assert!(engine
            .state()
            .is_visible(&engine.state().windows[&WindowId(2)]));
        assert!(!engine
            .state()
            .is_visible(&engine.state().windows[&WindowId(1)]));

        // At the edge nothing happens; back at monitor 1 the anchor is 1.
        engine.pump(ordo_core::Event::Hotkey {
            at: at(3_000),
            action: HotkeyAction::ViewMonitorNext,
        });
        assert_eq!(os.borrow().view.viewed, VirtualMonitorId(2));
        engine.pump(ordo_core::Event::Hotkey {
            at: at(4_000),
            action: HotkeyAction::ViewMonitorPrev,
        });
        assert_eq!(os.borrow().view.viewed, VirtualMonitorId(1));
        assert_eq!(os.borrow().focused, Some(WindowId(1)));
        run_id
    };

    let conn = Connection::open(&db).unwrap();
    let report = replay(&conn, run_id, None).unwrap();
    assert!(report.is_clean(), "{:?}", report.mismatches);
    assert!(
        count(
            &conn,
            "SELECT COUNT(*) FROM effects WHERE kind = 'view_monitor'"
        ) >= 2
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Run the engine's real loop, which publishes the menu bar's view after
/// every batch, and collect each view. The "user" reads each redraw and only
/// then sends the next message, so every view is a settled one.
fn menu_bar_views(os: &Rc<RefCell<FakeOs>>, script: Vec<Msg>) -> Vec<MenuBarView> {
    let (tx, rx) = crossbeam_channel::unbounded();
    let mut script = script.into_iter().chain([Msg::Shutdown]);
    let seen = Rc::new(RefCell::new(Vec::new()));
    let engine = engine_on(os, in_memory_logger("emulated")).on_state({
        let seen = seen.clone();
        move |s| {
            seen.borrow_mut().push(MenuBarView::of(s));
            if let Some(next) = script.next() {
                tx.send(next).unwrap();
            }
        }
    });
    engine.run(rx);
    Rc::try_unwrap(seen).unwrap().into_inner()
}

fn monitors_view(
    viewed: u8,
    displays: &[u128],
    // Per monitor: its display, its windows here, its windows everywhere.
    monitors: &[(Option<usize>, usize, usize)],
) -> MonitorsView {
    MonitorsView {
        displays: displays.iter().map(|d| MonitorId(*d)).collect(),
        monitors: monitors
            .iter()
            .enumerate()
            .map(|(i, (display, windows, all_windows))| MonitorEntry {
                id: VirtualMonitorId(i as u8 + 1),
                display: *display,
                windows: *windows,
                all_windows: *all_windows,
            })
            .collect(),
        viewed: VirtualMonitorId(viewed),
        enabled: true,
    }
}

#[test]
fn the_menu_bar_shows_every_workspace_and_follows_a_pick_from_its_menu() {
    // First the startup scene, then a pick from the menu (the same
    // WorkspaceSwitchTo a chord mints) landing on workspace 2, then a rescue
    // graying the whole thing out. Two displays, a monitor on each; the
    // monitors section counts the current workspace's windows on each.
    let os = fake_os(FocusPolicy::Lands);
    let seen = menu_bar_views(
        &os,
        vec![
            Msg::hotkey(HotkeyAction::WorkspaceSwitchTo(WorkspaceId(2))),
            Msg::Rescue,
        ],
    );

    let entry = |n: u8, apps: &[i32]| WorkspaceEntry {
        id: WorkspaceId(n),
        apps: apps.iter().map(|p| Pid(*p)).collect(),
    };
    let workspaces = vec![entry(1, &[100]), entry(2, &[200]), entry(3, &[])];
    let on_1 = monitors_view(1, &[1, 2], &[(Some(0), 2, 2), (Some(1), 0, 2)]);
    let on_2 = monitors_view(1, &[1, 2], &[(Some(0), 0, 2), (Some(1), 2, 2)]);
    assert_eq!(
        seen,
        vec![
            MenuBarView {
                workspaces: workspaces.clone(),
                current: Some(WorkspaceId(1)),
                engaged: true,
                monitors: Some(on_1),
            },
            MenuBarView {
                workspaces: workspaces.clone(),
                current: Some(WorkspaceId(2)),
                engaged: true,
                monitors: Some(on_2.clone()),
            },
            MenuBarView {
                workspaces,
                current: Some(WorkspaceId(2)),
                engaged: false,
                monitors: Some(on_2),
            },
        ]
    );
    assert_eq!(os.borrow().active, WorkspaceId(2));
}

#[test]
fn the_menu_bar_shows_which_monitor_the_one_display_is_showing() {
    // The laptop rig: one display, two virtual monitors. Monitor 2 is hidden
    // with its one window on this workspace; Cmd+Alt+K brings it up and
    // monitor 1's two windows go out of sight in its place.
    let os = fake_os(FocusPolicy::Lands);
    {
        let mut o = os.borrow_mut();
        o.displays = 1;
        o.assignments.insert(WindowId(2), WorkspaceId(1));
        for w in o.windows.iter_mut() {
            if w.frame.x >= 1920.0 {
                w.frame.x -= 1920.0;
            }
        }
    }
    let seen = menu_bar_views(&os, vec![Msg::hotkey(HotkeyAction::ViewMonitorNext)]);
    let monitors: Vec<_> = seen.into_iter().map(|v| v.monitors.unwrap()).collect();
    assert_eq!(
        monitors,
        vec![
            monitors_view(1, &[1], &[(Some(0), 2, 2), (None, 1, 2)]),
            monitors_view(2, &[1], &[(None, 2, 2), (Some(0), 1, 2)]),
        ]
    );
}

/// The menu's plus, end to end: a third monitor appears in the diagram,
/// empty and off screen, and the two displays keep showing what they showed.
#[test]
fn the_menus_plus_adds_an_empty_monitor_beside_the_displays() {
    let os = fake_os(FocusPolicy::Lands);
    let seen = menu_bar_views(&os, vec![Msg::hotkey(HotkeyAction::AddMonitor)]);
    let (before, after) = (
        seen.first().unwrap().monitors.clone().unwrap(),
        seen.last().unwrap().monitors.clone().unwrap(),
    );
    assert_eq!(before.monitors.len(), 2);
    assert_eq!(after.monitors[..2], before.monitors[..]);
    let new = &after.monitors[2];
    assert_eq!((new.id, new.display, new.windows, new.all_windows), (VirtualMonitorId(3), None, 0, 0));
}
