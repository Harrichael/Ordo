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
//! Some side channels are telemetry, not record, there to answer a future
//! statistics question rather than to replay: `restacks`/`raises` hold the
//! z-order reassert's timing breakdown (see [`crate::ports::RestackStats`]),
//! `hotkey_batches` holds how long presses queued behind the engine and how
//! many a burst folded away (see [`HotkeyBatch`]), and `snapshots` holds where
//! each world snapshot's time went (see [`crate::ports::SnapshotStats`]).

use std::path::{Path, PathBuf};

use ordo_emulated::ParkTrace;

use crate::ports::{RestackStats, SnapshotStats};
use crate::schema;
use ordo_core::{Effect, Event, Note, OpId, OpOutcome, State};
use rusqlite::{params, Connection};

/// A `State` checkpoint every this many events. Small enough that replay never
/// re-runs much history; large enough that checkpoint blobs stay a rounding
/// error against the event stream.
const CHECKPOINT_EVERY: u64 = 200;

/// Runs older than this are pruned on startup. Long enough to investigate a
/// bug you noticed a week and a half ago; short enough to bound disk use.
const RETENTION_DAYS: i64 = 14;

/// One batch of queued hotkeys, as the engine dequeued it. The core's hotkey
/// events are stamped at dequeue, so this is the only record of time spent
/// waiting behind the engine's previous work.
pub struct HotkeyBatch {
    /// The first hotkey event the batch pumped; None when it folded to
    /// nothing (a bounce that returned to where it started).
    pub first_seq: Option<u64>,
    pub presses: usize,
    pub pumped: usize,
    pub oldest_wait: std::time::Duration,
    pub newest_wait: std::time::Duration,
}

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

        let schema_version = schema::open_version(&conn)?;
        // Retention first, on whatever schema the file already has: a migration
        // that rewrites a table should never pay for rows that are about to be
        // deleted anyway. Version 0 is an empty file with nothing to prune.
        if schema_version >= 1 {
            let cutoff = now_wall_ms - RETENTION_DAYS * 24 * 60 * 60 * 1000;
            conn.execute("DELETE FROM runs WHERE started_wall < ?1", params![cutoff])?;
        }
        schema::migrate(&conn, schema_version)?;

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

    /// The sequence number the next logged step will get.
    pub fn next_seq(&self) -> u64 {
        self.seq
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

    /// Record the workspace mechanism's own account of itself. Kind and window
    /// are columns because every question starts by filtering on them; the rest
    /// rides as JSON, since this is a diagnostic channel whose shape should be
    /// free to change without a migration.
    pub fn log_park_trace(
        &mut self,
        traces: &[ParkTrace],
        now_wall_ms: i64,
    ) -> rusqlite::Result<()> {
        if traces.is_empty() {
            return Ok(());
        }
        let tx = self.conn.unchecked_transaction()?;
        for t in traces {
            tx.execute(
                "INSERT INTO park_trace (run_id, wall_ms, window, kind, payload)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    self.run_id,
                    now_wall_ms,
                    t.window.0 as i64,
                    format!("{:?}", t.kind),
                    serde_json::to_string(t).unwrap_or_else(|_| "{}".into()),
                ],
            )?;
        }
        tx.commit()
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

    pub fn log_hotkey_batch(&mut self, b: &HotkeyBatch, now_wall_ms: i64) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT INTO hotkey_batches (run_id, wall_ms, seq, presses, pumped,
                 oldest_wait_ms, newest_wait_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                self.run_id,
                now_wall_ms,
                b.first_seq.map(|s| s as i64),
                b.presses as i64,
                b.pumped as i64,
                b.oldest_wait.as_millis() as i64,
                b.newest_wait.as_millis() as i64,
            ],
        )?;
        Ok(())
    }

    /// `seq` is the snapshot's own `world_observed` event.
    pub fn log_snapshot_stats(
        &mut self,
        seq: u64,
        s: &SnapshotStats,
        now_wall_ms: i64,
    ) -> rusqlite::Result<()> {
        let ms = |d: std::time::Duration| d.as_secs_f64() * 1000.0;
        self.conn.execute(
            "INSERT INTO snapshots (run_id, wall_ms, seq, total_ms, walk_ms, enforce_ms,
                 apps, windows, slowest_pid, slowest_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                self.run_id,
                now_wall_ms,
                seq as i64,
                ms(s.total),
                ms(s.walk),
                ms(s.enforce),
                s.apps as i64,
                s.windows as i64,
                s.slowest.map(|(p, _)| p.0),
                s.slowest.map(|(_, d)| ms(d)),
            ],
        )?;
        Ok(())
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
        | Event::Engaged { at }
        | Event::Gesture { at, .. } => at,
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
        Event::Gesture { .. } => "gesture",
    }
}

fn effect_kind(e: &Effect) -> &'static str {
    match e {
        Effect::SwitchWorkspace { .. } => "switch_workspace",
        Effect::MoveWindowToWorkspace { .. } => "move_window_to_workspace",
        Effect::AssignWindowToWorkspace { .. } => "assign_window_to_workspace",
        Effect::AssignWindowToMonitor { .. } => "assign_window_to_monitor",
        Effect::ViewMonitor { .. } => "view_monitor",
        Effect::SetVirtualMonitors { .. } => "set_virtual_monitors",
        Effect::MergeMonitors { .. } => "merge_monitors",
        Effect::AddMonitor { .. } => "add_monitor",
        Effect::SetWindowFrame { .. } => "set_window_frame",
        Effect::FocusWindow { .. } => "focus_window",
        Effect::FocusDesktop { .. } => "focus_desktop",
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
        | Effect::AssignWindowToMonitor { op, .. }
        | Effect::ViewMonitor { op, .. }
        | Effect::SetVirtualMonitors { op, .. }
        | Effect::MergeMonitors { op, .. }
        | Effect::AddMonitor { op }
        | Effect::SetWindowFrame { op, .. }
        | Effect::FocusWindow { op, .. }
        | Effect::FocusDesktop { op, .. } => Some(*op),
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
        Note::GestureClassified { .. } => "gesture_classified",
        Note::FollowedFocus { .. } => "followed_focus",
        Note::MonitorAdopted { .. } => "monitor_adopted",
        Note::HeldFocus { .. } => "held_focus",
        Note::FocusReasserted { .. } => "focus_reasserted",
        Note::FocusDiverged { .. } => "focus_diverged",
        Note::DesktopReasserted { .. } => "desktop_reasserted",
        Note::DesktopDiverged { .. } => "desktop_diverged",
        Note::TearDetected { .. } => "tear_detected",
        Note::TearPersisting => "tear_persisting",
        Note::Diverged { .. } => "diverged",
    }
}
