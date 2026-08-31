//! The structured log — the whole point of which is that a bug report months
//! from now ("windows went weird around 3pm") is answerable by query.
//!
//! The log is not a diary; it is the event/effect/note stream that produced
//! every decision, stored so it can be replayed (see [`crate::replay`]). Four
//! things get recorded per engine step:
//!   - the `Event` that arrived (full JSON, including any world snapshot — this
//!     is replay's source of truth),
//!   - every `Effect` the core emitted, linked to that event,
//!   - every `Note` the core produced (its own explanation: self-confirmed vs
//!     external, ops lost, divergence), linked to that event,
//!   - periodic `State` checkpoints so replay can start near a point of
//!     interest instead of from boot.
//!
//! Executor results land in `op_results`. Snapshots live inline in the event
//! payload rather than a side table: it keeps replay a single read, and a
//! personal-scale DB does not need the denormalization.
//!
//! One side channel is telemetry, not record: `restacks`/`raises` hold the
//! z-order reassert's timing breakdown (see [`crate::ports::RestackStats`]),
//! there to answer a future statistics question, not to replay.

use std::path::{Path, PathBuf};

use crate::ports::RestackStats;
use ordo_core::{Effect, Event, Note, OpId, OpOutcome, State};
use rusqlite::{params, Connection};

/// A `State` checkpoint every this many events. Small enough that replay never
/// re-runs much history; large enough that checkpoint blobs stay a rounding
/// error against the event stream.
const CHECKPOINT_EVERY: u64 = 200;

/// Runs older than this are pruned on startup. Long enough to investigate a
/// bug you noticed a week and a half ago; short enough to bound disk use.
const RETENTION_DAYS: i64 = 14;

pub struct Logger {
    conn: Connection,
    run_id: i64,
    seq: u64,
}

/// The canonical location for Ordo's data on macOS.
pub fn default_db_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    Path::new(&home)
        .join("Library")
        .join("Application Support")
        .join("Ordo")
        .join("log.db")
}

impl Logger {
    /// Open (creating if needed) the log at `path`, prune old runs, and begin a
    /// fresh run. `now_wall_ms` is passed in rather than read here so the shell
    /// owns every clock read.
    pub fn open(
        path: &Path,
        version: &str,
        backend: &str,
        now_wall_ms: i64,
    ) -> rusqlite::Result<Self> {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let conn = Connection::open(path)?;
        Self::from_conn(conn, version, backend, now_wall_ms)
    }

    /// Used by tests against an in-memory DB and by the same code path as
    /// [`Logger::open`].
    pub fn from_conn(
        conn: Connection,
        version: &str,
        backend: &str,
        now_wall_ms: i64,
    ) -> rusqlite::Result<Self> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.execute_batch(SCHEMA)?;
        // Columns added after a table shipped: CREATE IF NOT EXISTS won't grow
        // an existing table, and the telemetry history is worth keeping (the
        // raise-overlap decision wants weeks of it), so alter in place.
        for (table, column) in [
            ("restacks", "aborted"),
            ("restacks", "ghost_pass"),
            ("raises", "via_event"),
        ] {
            let exists = conn
                .prepare(&format!(
                    "SELECT 1 FROM pragma_table_info('{table}') WHERE name = '{column}'"
                ))?
                .exists([])?;
            if !exists {
                conn.execute_batch(&format!(
                    "ALTER TABLE {table} ADD COLUMN {column} INTEGER NOT NULL DEFAULT 0;"
                ))?;
            }
        }

        let cutoff = now_wall_ms - RETENTION_DAYS * 24 * 60 * 60 * 1000;
        conn.execute("DELETE FROM runs WHERE started_wall < ?1", params![cutoff])?;

        conn.execute(
            "INSERT INTO runs (started_wall, version, backend) VALUES (?1, ?2, ?3)",
            params![now_wall_ms, version, backend],
        )?;
        let run_id = conn.last_insert_rowid();

        Ok(Logger {
            conn,
            run_id,
            seq: 0,
        })
    }

    pub fn run_id(&self) -> i64 {
        self.run_id
    }

    /// Record one engine step. Returns the event's sequence number so the
    /// caller can correlate follow-up rows. Everything goes in one transaction
    /// so a step is all-or-nothing in the log.
    pub fn log_step(
        &mut self,
        event: &Event,
        effects: &[Effect],
        notes: &[Note],
        state: &State,
    ) -> rusqlite::Result<u64> {
        let seq = self.seq;
        self.seq += 1;

        let (wall_ms, mono_ns) = event_time(event);
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "INSERT INTO events (run_id, seq, wall_ms, mono_ns, kind, payload)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                self.run_id,
                seq,
                wall_ms,
                mono_ns,
                event_kind(event),
                json(event)
            ],
        )?;
        for (ord, effect) in effects.iter().enumerate() {
            tx.execute(
                "INSERT INTO effects (run_id, seq, ord, op_id, kind, payload)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    self.run_id,
                    seq,
                    ord as i64,
                    effect_op(effect).map(|o| o.0 as i64),
                    effect_kind(effect),
                    json(effect)
                ],
            )?;
        }
        for (ord, note) in notes.iter().enumerate() {
            tx.execute(
                "INSERT INTO notes (run_id, seq, ord, kind, payload)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![self.run_id, seq, ord as i64, note_kind(note), json(note)],
            )?;
        }
        if seq.is_multiple_of(CHECKPOINT_EVERY) {
            tx.execute(
                "INSERT INTO checkpoints (run_id, seq, payload) VALUES (?1, ?2, ?3)",
                params![self.run_id, seq, json(state)],
            )?;
        }
        tx.commit()?;
        Ok(seq)
    }

    pub fn log_op_result(
        &mut self,
        op: OpId,
        outcome: &OpOutcome,
        now_wall_ms: i64,
    ) -> rusqlite::Result<()> {
        let (outcome_str, detail) = match outcome {
            OpOutcome::Ok => ("ok", None),
            OpOutcome::Timeout => ("timeout", None),
            OpOutcome::Failed { detail } => ("failed", Some(detail.clone())),
        };
        self.conn.execute(
            "INSERT INTO op_results (run_id, op_id, wall_ms, outcome, detail)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![self.run_id, op.0 as i64, now_wall_ms, outcome_str, detail],
        )?;
        Ok(())
    }

    pub fn log_restack_stats(
        &mut self,
        s: &RestackStats,
        now_wall_ms: i64,
    ) -> rusqlite::Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "INSERT INTO restacks (run_id, wall_ms, total_ms, presence_wait_ms,
                 handoff_wait_ms, desired, missing, skipped_suffix, second_pass, converged,
                 aborted, ghost_pass)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                self.run_id,
                now_wall_ms,
                s.total_ms as i64,
                s.presence_wait_ms as i64,
                s.handoff_wait_ms as i64,
                s.desired,
                s.missing,
                s.skipped_suffix,
                s.second_pass,
                s.converged,
                s.aborted,
                s.ghost_pass,
            ],
        )?;
        let restack_id = tx.last_insert_rowid();
        for (ord, r) in s.raises.iter().enumerate() {
            tx.execute(
                "INSERT INTO raises (restack_id, ord, window, pid, kind, pass,
                     above_scope, above_all, wait_ms, timed_out, via_event)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                params![
                    restack_id,
                    ord as i64,
                    r.window.0,
                    r.pid,
                    r.kind.as_str(),
                    r.pass,
                    r.above_scope,
                    r.above_all,
                    r.wait_ms as i64,
                    r.timed_out,
                    r.via_event,
                ],
            )?;
        }
        tx.commit()
    }

    pub fn close(&mut self, now_wall_ms: i64) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE runs SET ended_wall = ?1 WHERE run_id = ?2",
            params![now_wall_ms, self.run_id],
        )?;
        Ok(())
    }
}

fn json<T: serde::Serialize>(v: &T) -> String {
    // The core types are plain data; serialization cannot realistically fail,
    // and a log write is not worth crashing the daemon over if it somehow does.
    serde_json::to_string(v).unwrap_or_else(|_| "null".to_string())
}

fn event_time(e: &Event) -> (i64, i64) {
    let ts = match e {
        Event::Hotkey { at, .. }
        | Event::WorldObserved { at, .. }
        | Event::EffectResult { at, .. }
        | Event::RescueEngaged { at }
        | Event::Engaged { at } => at,
    };
    (ts.wall_ms, ts.mono_ns as i64)
}

fn event_kind(e: &Event) -> &'static str {
    match e {
        Event::Hotkey { .. } => "hotkey",
        Event::WorldObserved { .. } => "world_observed",
        Event::EffectResult { .. } => "effect_result",
        Event::RescueEngaged { .. } => "rescue_engaged",
        Event::Engaged { .. } => "engaged",
    }
}

fn effect_kind(e: &Effect) -> &'static str {
    match e {
        Effect::SwitchWorkspace { .. } => "switch_workspace",
        Effect::MoveWindowToWorkspace { .. } => "move_window_to_workspace",
        Effect::AssignWindowToWorkspace { .. } => "assign_window_to_workspace",
        Effect::SetWindowFrame { .. } => "set_window_frame",
        Effect::FocusWindow { .. } => "focus_window",
        Effect::WarpMouse { .. } => "warp_mouse",
        Effect::RestackWindows { .. } => "restack_windows",
        Effect::RequestRescan { .. } => "request_rescan",
        Effect::SetIntercepting { .. } => "set_intercepting",
    }
}

fn effect_op(e: &Effect) -> Option<OpId> {
    match e {
        Effect::SwitchWorkspace { op, .. }
        | Effect::MoveWindowToWorkspace { op, .. }
        | Effect::AssignWindowToWorkspace { op, .. }
        | Effect::SetWindowFrame { op, .. }
        | Effect::FocusWindow { op, .. } => Some(*op),
        Effect::WarpMouse { .. }
        | Effect::RestackWindows { .. }
        | Effect::RequestRescan { .. }
        | Effect::SetIntercepting { .. } => None,
    }
}

fn note_kind(n: &Note) -> &'static str {
    match n {
        Note::SelfConfirmed { .. } => "self_confirmed",
        Note::OpLost { .. } => "op_lost",
        Note::OpFailed { .. } => "op_failed",
        Note::External { .. } => "external",
        Note::FollowedFocus { .. } => "followed_focus",
        Note::HeldFocusOnClose { .. } => "held_focus_on_close",
        Note::HeldFocusSettling { .. } => "held_focus_settling",
        Note::TearDetected { .. } => "tear_detected",
        Note::TearPersisting => "tear_persisting",
        Note::Diverged { .. } => "diverged",
    }
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS runs (
    run_id       INTEGER PRIMARY KEY,
    started_wall INTEGER NOT NULL,
    ended_wall   INTEGER,
    version      TEXT NOT NULL,
    backend      TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS events (
    run_id  INTEGER NOT NULL REFERENCES runs(run_id) ON DELETE CASCADE,
    seq     INTEGER NOT NULL,
    wall_ms INTEGER NOT NULL,
    mono_ns INTEGER NOT NULL,
    kind    TEXT NOT NULL,
    payload TEXT NOT NULL,
    PRIMARY KEY (run_id, seq)
);
CREATE TABLE IF NOT EXISTS effects (
    run_id  INTEGER NOT NULL REFERENCES runs(run_id) ON DELETE CASCADE,
    seq     INTEGER NOT NULL,
    ord     INTEGER NOT NULL,
    op_id   INTEGER,
    kind    TEXT NOT NULL,
    payload TEXT NOT NULL,
    PRIMARY KEY (run_id, seq, ord)
);
CREATE TABLE IF NOT EXISTS notes (
    run_id  INTEGER NOT NULL REFERENCES runs(run_id) ON DELETE CASCADE,
    seq     INTEGER NOT NULL,
    ord     INTEGER NOT NULL,
    kind    TEXT NOT NULL,
    payload TEXT NOT NULL,
    PRIMARY KEY (run_id, seq, ord)
);
CREATE TABLE IF NOT EXISTS op_results (
    run_id  INTEGER NOT NULL REFERENCES runs(run_id) ON DELETE CASCADE,
    op_id   INTEGER NOT NULL,
    wall_ms INTEGER NOT NULL,
    outcome TEXT NOT NULL,
    detail  TEXT,
    PRIMARY KEY (run_id, op_id, wall_ms)
);
CREATE TABLE IF NOT EXISTS checkpoints (
    run_id  INTEGER NOT NULL REFERENCES runs(run_id) ON DELETE CASCADE,
    seq     INTEGER NOT NULL,
    payload TEXT NOT NULL,
    PRIMARY KEY (run_id, seq)
);
CREATE TABLE IF NOT EXISTS restacks (
    restack_id       INTEGER PRIMARY KEY,
    run_id           INTEGER NOT NULL REFERENCES runs(run_id) ON DELETE CASCADE,
    wall_ms          INTEGER NOT NULL,
    total_ms         INTEGER NOT NULL,
    presence_wait_ms INTEGER NOT NULL,
    handoff_wait_ms  INTEGER NOT NULL,
    desired          INTEGER NOT NULL,
    missing          INTEGER NOT NULL,
    skipped_suffix   INTEGER NOT NULL,
    second_pass      INTEGER NOT NULL,
    converged        INTEGER NOT NULL,
    aborted          INTEGER NOT NULL DEFAULT 0,
    ghost_pass       INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS raises (
    restack_id  INTEGER NOT NULL REFERENCES restacks(restack_id) ON DELETE CASCADE,
    ord         INTEGER NOT NULL,
    window      INTEGER NOT NULL,
    pid         INTEGER NOT NULL,
    kind        TEXT NOT NULL,
    pass        INTEGER NOT NULL,
    above_scope INTEGER NOT NULL,
    above_all   INTEGER NOT NULL,
    wait_ms     INTEGER NOT NULL,
    timed_out   INTEGER NOT NULL,
    via_event   INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (restack_id, ord)
);
CREATE INDEX IF NOT EXISTS events_by_time ON events(wall_ms);
CREATE INDEX IF NOT EXISTS events_by_kind ON events(run_id, kind);
"#;
