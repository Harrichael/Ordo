//! Integration tests for the shell, in the house style: the engine, the real
//! SQLite logger, the real pure core, and the real replay checker all run
//! together against an in-memory database. The only fake is the OS itself — a
//! scripted world at the [`WorldSource`] seam — because a live WindowServer
//! can't be driven reproducibly. Everything above that seam is the real thing.

use std::cell::Cell;
use std::rc::Rc;

use ordo::clock::Clock;
use ordo::engine::Engine;
use ordo::logger::Logger;
use ordo::ports::{Effector, NullEffector, WorldSource};
use ordo::replay::replay;
use ordo_core::{
    Effect, MonitorId, MonitorSnap, MonitorWs, OpOutcome, Pid, Rect, WindowId, WindowSnap,
    WorkspaceId, WorkspaceSnap, WorldSnapshot,
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
