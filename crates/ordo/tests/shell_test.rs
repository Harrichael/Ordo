//! Integration tests for the shell, in the house style: the engine, the real
//! SQLite logger, the real pure core, and the real replay checker all run
//! together against an in-memory database. The only fake is the OS itself — a
//! scripted world at the [`WorldSource`] seam — because a live WindowServer
//! can't be driven reproducibly. Everything above that seam is the real thing.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::rc::Rc;

use ordo::clock::Clock;
use ordo::engine::{Engine, Msg};
use ordo::logger::Logger;
use ordo::ports::{Effector, NullEffector, WorldSource};
use ordo::replay::replay;
use ordo_core::{
    Effect, FocusIntent, Gesture, HotkeyAction, MonitorId, MonitorSnap, MonitorWs, OpOutcome, Pid,
    Rect, RescanTrigger, WindowId, WindowSnap, WorkspaceId, WorkspaceSnap, WorldSnapshot,
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
}

impl FakeOs {
    fn snapshot(&self) -> WorldSnapshot {
        WorldSnapshot {
            monitors: vec![mon(1, 0.0), mon(2, 1920.0)],
            windows: self.windows.clone(),
            focused: self.focused,
            workspaces: WorkspaceSnap {
                monitors: [MonitorId(1), MonitorId(2)]
                    .into_iter()
                    .map(|m| {
                        (
                            m,
                            MonitorWs {
                                active: self.active,
                                count: 3,
                            },
                        )
                    })
                    .collect(),
                assignments: self.assignments.clone(),
            },
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

/// A monotonic fake clock: reproducible timestamps for reproducible logs.
struct StepClock {
    n: Cell<u64>,
}

impl Clock for StepClock {
    fn now(&self) -> ordo_core::Ts {
        let n = self.n.get();
        self.n.set(n + 1);
        ordo_core::Ts {
            wall_ms: 1_000 + n as i64,
            mono_ns: n,
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
            assert!(observations <= 12, "never stood down");
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
    for _ in 0..3 {
        engine.observe(RescanTrigger::Periodic);
    }
    os.borrow_mut().policy = FocusPolicy::Lands;
    for _ in 0..3 {
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
        tx.send(Msg::Hotkey(HotkeyAction::WorkspaceNext)).unwrap();
        tx.send(Msg::Gesture(Gesture::MouseDown {
            at: ordo_core::Point {
                x: 960.0,
                y: 1075.0,
            },
        }))
        .unwrap();
        tx.send(Msg::Hotkey(HotkeyAction::WorkspaceNext)).unwrap();
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
