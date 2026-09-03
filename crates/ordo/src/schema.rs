//! The log database's schema, as a versioned chain of migrations.
//!
//! The version lives in SQLite's own `PRAGMA user_version` header word, so the
//! schema needs no bookkeeping table of its own. Opening a log runs every
//! migration above the recorded version, in order, and each one commits its own
//! transaction whose last act is to bump the version: an interrupted upgrade
//! rolls back whole, and the next open resumes at exactly the step that failed.
//!
//! Rules for whoever adds the next migration:
//!   - **Append only.** `MIGRATIONS[i]` carries a database from version `i` to
//!     version `i + 1`. A migration that has shipped is history: editing it
//!     changes what a stranger's database becomes, which is the one thing this
//!     scheme exists to prevent. Fix a mistake with a further migration.
//!   - **Never fold new tables or columns into the baseline.** A fresh database
//!     runs the same chain from 0 that an old one runs from its own version,
//!     which is what keeps the two schemas identical; teaching the baseline
//!     about a later column would make them diverge.
//!   - The runner owns the transaction and the version bump. A migration just
//!     does its DDL and its data transform.
//!   - **Flag the expensive ones.** `events` is the big table (hundreds of MB on
//!     a machine that has been running Ordo for weeks). A plain `ADD COLUMN` is
//!     free, but anything that rewrites rows — a new `NOT NULL` without a
//!     constant default, a table rebuild, a backfill `UPDATE` — stalls daemon
//!     startup for as long as it takes. Say so in a comment on the migration,
//!     and remember that retention has already run by then (see
//!     [`crate::logger::Logger::from_conn`]), so the rewrite only pays for rows
//!     inside the retention window.
//!
//! One table in a real log is not from here: `rescue_events` is created on
//! demand by [`crate::rescue::record_rescue`], because the rescue CLI opens the
//! database without a [`crate::logger::Logger`] and so without this chain.

use rusqlite::{Connection, Transaction};

/// The schema version this build writes and understands.
pub const CURRENT_VERSION: i32 = 2;

type Migration = fn(&Transaction) -> rusqlite::Result<()>;

const MIGRATIONS: &[Migration] = &[m1_baseline, m2_restack_telemetry];

/// Read the version of an existing log, refusing one written by a newer Ordo.
///
/// Split out from [`migrate`] so the caller can act on the old schema first —
/// retention wants to delete doomed rows before a migration pays to rewrite
/// them. A return of 1 or more means the baseline tables are already there.
pub fn open_version(conn: &Connection) -> rusqlite::Result<i32> {
    let recorded: i32 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if recorded > CURRENT_VERSION {
        // Downgrades stop here rather than opening read-only: the daemon's
        // whole use of this file is to write to it, so a read-only handle would
        // just fail on the first insert instead — and a best-effort read-write
        // open would let this build's INSERTs run against columns and
        // constraints it has never heard of. Refusing with the two version
        // numbers is the only outcome the user can act on.
        return Err(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_ERROR),
            Some(format!(
                "log db is schema v{recorded}, but this Ordo understands v{CURRENT_VERSION}; \
                 upgrade Ordo, or move the database aside to start a new log"
            )),
        ));
    }
    if recorded == 0 && has_baseline(conn)? {
        // Version 0 is ambiguous, and only here. It is what SQLite reports for
        // an empty file, and equally what every database written before this
        // chain existed reports — those were grown by an ad-hoc `ALTER TABLE`
        // loop that stamped no version. The presence of `runs` tells the two
        // apart; stamping it now means the rest of the system, and every future
        // migration, can trust the version alone.
        conn.pragma_update(None, "user_version", 1)?;
        return Ok(1);
    }
    Ok(recorded)
}

/// Bring a log from `from` (as reported by [`open_version`]) to the current
/// version, one committed step at a time.
pub fn migrate(conn: &Connection, from: i32) -> rusqlite::Result<()> {
    apply(conn, from, MIGRATIONS)
}

fn apply(conn: &Connection, from: i32, migrations: &[Migration]) -> rusqlite::Result<()> {
    for (i, migration) in migrations.iter().enumerate() {
        let to = i as i32 + 1;
        if to <= from {
            continue;
        }
        let tx = conn.unchecked_transaction()?;
        migration(&tx)?;
        // Last act, inside the same transaction: the version is only true once
        // the work it describes has committed.
        tx.execute_batch(&format!("PRAGMA user_version = {to};"))?;
        tx.commit()?;
    }
    Ok(())
}

fn has_baseline(conn: &Connection) -> rusqlite::Result<bool> {
    conn.prepare("SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'runs'")?
        .exists([])
}

fn m1_baseline(tx: &Transaction) -> rusqlite::Result<()> {
    tx.execute_batch(BASELINE)
}

/// The restack/raise telemetry columns, which shipped as ad-hoc `ALTER`s before
/// there was a chain to put them in.
fn m2_restack_telemetry(tx: &Transaction) -> rusqlite::Result<()> {
    // Uniquely among migrations, this one tolerates its work being already
    // done: a database adopted at version 1 may be a genuine pre-column log, or
    // one the shipped ad-hoc loop already grew — both report the same version,
    // and nothing recorded which. Every later migration may assume the schema
    // is exactly what the previous version defined.
    for (table, column) in [
        ("restacks", "aborted"),
        ("restacks", "ghost_pass"),
        ("raises", "via_event"),
    ] {
        let exists = tx
            .prepare(&format!(
                "SELECT 1 FROM pragma_table_info('{table}') WHERE name = '{column}'"
            ))?
            .exists([])?;
        if !exists {
            tx.execute_batch(&format!(
                "ALTER TABLE {table} ADD COLUMN {column} INTEGER NOT NULL DEFAULT 0;"
            ))?;
        }
    }
    Ok(())
}

/// Version 1: the schema as it first shipped. Frozen — see the module rules.
const BASELINE: &str = r#"
CREATE TABLE runs (
    run_id       INTEGER PRIMARY KEY,
    started_wall INTEGER NOT NULL,
    ended_wall   INTEGER,
    version      TEXT NOT NULL,
    backend      TEXT NOT NULL
);
CREATE TABLE events (
    run_id  INTEGER NOT NULL REFERENCES runs(run_id) ON DELETE CASCADE,
    seq     INTEGER NOT NULL,
    wall_ms INTEGER NOT NULL,
    mono_ns INTEGER NOT NULL,
    kind    TEXT NOT NULL,
    payload TEXT NOT NULL,
    PRIMARY KEY (run_id, seq)
);
CREATE TABLE effects (
    run_id  INTEGER NOT NULL REFERENCES runs(run_id) ON DELETE CASCADE,
    seq     INTEGER NOT NULL,
    ord     INTEGER NOT NULL,
    op_id   INTEGER,
    kind    TEXT NOT NULL,
    payload TEXT NOT NULL,
    PRIMARY KEY (run_id, seq, ord)
);
CREATE TABLE notes (
    run_id  INTEGER NOT NULL REFERENCES runs(run_id) ON DELETE CASCADE,
    seq     INTEGER NOT NULL,
    ord     INTEGER NOT NULL,
    kind    TEXT NOT NULL,
    payload TEXT NOT NULL,
    PRIMARY KEY (run_id, seq, ord)
);
CREATE TABLE op_results (
    run_id  INTEGER NOT NULL REFERENCES runs(run_id) ON DELETE CASCADE,
    op_id   INTEGER NOT NULL,
    wall_ms INTEGER NOT NULL,
    outcome TEXT NOT NULL,
    detail  TEXT,
    PRIMARY KEY (run_id, op_id, wall_ms)
);
CREATE TABLE checkpoints (
    run_id  INTEGER NOT NULL REFERENCES runs(run_id) ON DELETE CASCADE,
    seq     INTEGER NOT NULL,
    payload TEXT NOT NULL,
    PRIMARY KEY (run_id, seq)
);
CREATE TABLE park_trace (
    run_id   INTEGER NOT NULL REFERENCES runs(run_id) ON DELETE CASCADE,
    wall_ms  INTEGER NOT NULL,
    window   INTEGER NOT NULL,
    kind     TEXT NOT NULL,
    payload  TEXT NOT NULL
);
CREATE INDEX park_trace_window ON park_trace (run_id, window, wall_ms);
CREATE TABLE restacks (
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
    converged        INTEGER NOT NULL
);
CREATE TABLE raises (
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
    PRIMARY KEY (restack_id, ord)
);
CREATE INDEX events_by_time ON events(wall_ms);
CREATE INDEX events_by_kind ON events(run_id, kind);
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logger::Logger;

    /// Verbatim the DDL the pre-versioning build wrote, three ad-hoc `ALTER`s
    /// already applied. This is a historical fact about databases on real
    /// machines, not a description of the current schema: never edit it.
    const SHIPPED_UNVERSIONED: &str = r#"
CREATE TABLE runs (
    run_id       INTEGER PRIMARY KEY,
    started_wall INTEGER NOT NULL,
    ended_wall   INTEGER,
    version      TEXT NOT NULL,
    backend      TEXT NOT NULL
);
CREATE TABLE events (
    run_id  INTEGER NOT NULL REFERENCES runs(run_id) ON DELETE CASCADE,
    seq     INTEGER NOT NULL,
    wall_ms INTEGER NOT NULL,
    mono_ns INTEGER NOT NULL,
    kind    TEXT NOT NULL,
    payload TEXT NOT NULL,
    PRIMARY KEY (run_id, seq)
);
CREATE TABLE effects (
    run_id  INTEGER NOT NULL REFERENCES runs(run_id) ON DELETE CASCADE,
    seq     INTEGER NOT NULL,
    ord     INTEGER NOT NULL,
    op_id   INTEGER,
    kind    TEXT NOT NULL,
    payload TEXT NOT NULL,
    PRIMARY KEY (run_id, seq, ord)
);
CREATE TABLE notes (
    run_id  INTEGER NOT NULL REFERENCES runs(run_id) ON DELETE CASCADE,
    seq     INTEGER NOT NULL,
    ord     INTEGER NOT NULL,
    kind    TEXT NOT NULL,
    payload TEXT NOT NULL,
    PRIMARY KEY (run_id, seq, ord)
);
CREATE TABLE op_results (
    run_id  INTEGER NOT NULL REFERENCES runs(run_id) ON DELETE CASCADE,
    op_id   INTEGER NOT NULL,
    wall_ms INTEGER NOT NULL,
    outcome TEXT NOT NULL,
    detail  TEXT,
    PRIMARY KEY (run_id, op_id, wall_ms)
);
CREATE TABLE checkpoints (
    run_id  INTEGER NOT NULL REFERENCES runs(run_id) ON DELETE CASCADE,
    seq     INTEGER NOT NULL,
    payload TEXT NOT NULL,
    PRIMARY KEY (run_id, seq)
);
CREATE TABLE park_trace (
    run_id   INTEGER NOT NULL REFERENCES runs(run_id) ON DELETE CASCADE,
    wall_ms  INTEGER NOT NULL,
    window   INTEGER NOT NULL,
    kind     TEXT NOT NULL,
    payload  TEXT NOT NULL
);
CREATE INDEX park_trace_window ON park_trace (run_id, window, wall_ms);
CREATE TABLE restacks (
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
CREATE TABLE raises (
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
CREATE INDEX events_by_time ON events(wall_ms);
CREATE INDEX events_by_kind ON events(run_id, kind);
"#;

    fn upgraded(ddl: &str) -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(ddl).unwrap();
        let from = open_version(&conn).unwrap();
        migrate(&conn, from).unwrap();
        conn
    }

    fn version(conn: &Connection) -> i32 {
        conn.query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap()
    }

    fn columns(conn: &Connection, table: &str) -> Vec<String> {
        let mut stmt = conn
            .prepare(&format!("SELECT name FROM pragma_table_info('{table}')"))
            .unwrap();
        let mut cols: Vec<String> = stmt
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        cols.sort();
        cols
    }

    /// What the schema *is*, as opposed to how it was written down. Column
    /// facts rather than `sqlite_master` text, because a table that reached its
    /// shape by `ALTER` stores different DDL than one created with the columns
    /// inline while being the same schema — comparing the text would fail the
    /// two databases this scheme exists to reconcile. Ordinal position is left
    /// out for the same reason: the logger names every column in its SQL, so
    /// position is not something worth ossifying.
    fn fingerprint(conn: &Connection) -> Vec<String> {
        let mut objects: Vec<(String, String, String)> = conn
            .prepare(
                "SELECT type, name, tbl_name FROM sqlite_master
                 WHERE name NOT LIKE 'sqlite_%' ORDER BY type, name",
            )
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        objects.sort();

        let mut out = Vec::new();
        for (kind, name, tbl) in objects {
            match kind.as_str() {
                "table" => {
                    let mut stmt = conn
                        .prepare(&format!(
                            "SELECT name, type, \"notnull\", ifnull(dflt_value, ''), pk
                             FROM pragma_table_info('{name}') ORDER BY name"
                        ))
                        .unwrap();
                    let mut cols: Vec<String> = stmt
                        .query_map([], |r| {
                            Ok(format!(
                                "{name}.{} {} notnull={} dflt={} pk={}",
                                r.get::<_, String>(0)?,
                                r.get::<_, String>(1)?,
                                r.get::<_, i64>(2)?,
                                r.get::<_, String>(3)?,
                                r.get::<_, i64>(4)?,
                            ))
                        })
                        .unwrap()
                        .map(Result::unwrap)
                        .collect();
                    cols.sort();
                    out.extend(cols);
                }
                "index" => {
                    let cols: Vec<String> = conn
                        .prepare(&format!("SELECT name FROM pragma_index_info('{name}')"))
                        .unwrap()
                        .query_map([], |r| r.get(0))
                        .unwrap()
                        .map(Result::unwrap)
                        .collect();
                    out.push(format!("index {name} on {tbl}({})", cols.join(",")));
                }
                other => out.push(format!("{other} {name}")),
            }
        }
        out
    }

    #[test]
    fn a_fresh_database_ends_at_the_current_version_with_the_whole_schema() {
        let conn = upgraded("");

        assert_eq!(version(&conn), CURRENT_VERSION);
        for table in [
            "runs",
            "events",
            "effects",
            "notes",
            "op_results",
            "checkpoints",
            "park_trace",
            "restacks",
            "raises",
        ] {
            assert!(!columns(&conn, table).is_empty(), "{table} missing");
        }
        assert!(columns(&conn, "restacks").contains(&"aborted".to_string()));
        assert!(columns(&conn, "restacks").contains(&"ghost_pass".to_string()));
        assert!(columns(&conn, "raises").contains(&"via_event".to_string()));
    }

    #[test]
    fn a_pre_column_database_gains_the_columns_and_keeps_its_rows() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(BASELINE).unwrap();
        conn.execute(
            "INSERT INTO runs (run_id, started_wall, version, backend)
             VALUES (7, 1000, 'old', 'native')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO restacks (restack_id, run_id, wall_ms, total_ms, presence_wait_ms,
                 handoff_wait_ms, desired, missing, skipped_suffix, second_pass, converged)
             VALUES (1, 7, 1000, 5, 0, 0, 3, 0, 0, 0, 1)",
            [],
        )
        .unwrap();

        let from = open_version(&conn).unwrap();
        migrate(&conn, from).unwrap();

        assert_eq!(version(&conn), CURRENT_VERSION);
        // The row survives, and the new columns take their default.
        let (run_id, aborted, ghost): (i64, i64, i64) = conn
            .query_row(
                "SELECT run_id, aborted, ghost_pass FROM restacks WHERE restack_id = 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!((run_id, aborted, ghost), (7, 0, 0));
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM runs", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            1
        );
    }

    /// The case that will actually happen on a machine running Ordo today: the
    /// ad-hoc loop already added the three columns, and nothing recorded that.
    #[test]
    fn a_shipped_unversioned_database_upgrades_without_re_adding_its_columns() {
        let conn = upgraded(SHIPPED_UNVERSIONED);

        assert_eq!(version(&conn), CURRENT_VERSION);
        let restacks = columns(&conn, "restacks");
        assert_eq!(restacks.iter().filter(|c| *c == "aborted").count(), 1);
        assert_eq!(restacks.iter().filter(|c| *c == "ghost_pass").count(), 1);
        assert_eq!(
            columns(&conn, "raises")
                .iter()
                .filter(|c| *c == "via_event")
                .count(),
            1
        );
    }

    #[test]
    fn every_upgrade_path_converges_on_the_same_schema() {
        let fresh = fingerprint(&upgraded(""));
        assert_eq!(
            fingerprint(&upgraded(BASELINE)),
            fresh,
            "pre-column upgrade"
        );
        assert_eq!(
            fingerprint(&upgraded(SHIPPED_UNVERSIONED)),
            fresh,
            "shipped-unversioned upgrade"
        );
    }

    /// The promise a version number has to keep: it never describes work that
    /// only half happened, and the retry picks up exactly where it stopped.
    #[test]
    fn a_migration_that_fails_leaves_neither_its_work_nor_its_version() {
        fn adds_a_table(tx: &Transaction) -> rusqlite::Result<()> {
            tx.execute_batch("CREATE TABLE later (x INTEGER);")
        }
        fn adds_a_table_then_fails(tx: &Transaction) -> rusqlite::Result<()> {
            adds_a_table(tx)?;
            tx.execute_batch("SELECT malformed syntax here")
        }

        let conn = Connection::open_in_memory().unwrap();
        assert!(apply(&conn, 0, &[m1_baseline, adds_a_table_then_fails]).is_err());
        assert_eq!(version(&conn), 1, "step 1 stands, step 2 does not");
        assert!(
            columns(&conn, "later").is_empty(),
            "step 2 rolled back whole"
        );

        // The retry resumes at step 2 — it does not re-run the baseline, which
        // would fail on tables that already exist.
        apply(&conn, version(&conn), &[m1_baseline, adds_a_table]).unwrap();
        assert_eq!(version(&conn), 2);
        assert_eq!(columns(&conn, "later"), vec!["x".to_string()]);
    }

    #[test]
    fn a_database_from_a_newer_ordo_is_refused() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SHIPPED_UNVERSIONED).unwrap();
        conn.pragma_update(None, "user_version", CURRENT_VERSION + 1)
            .unwrap();

        let msg = match Logger::from_conn(conn, "test", "native", 1_000) {
            Ok(_) => panic!("a newer log db was opened read-write"),
            Err(e) => e.to_string(),
        };
        assert!(
            msg.contains(&format!("v{}", CURRENT_VERSION + 1))
                && msg.contains(&format!("v{CURRENT_VERSION}")),
            "unhelpful downgrade error: {msg}"
        );
    }

    #[test]
    fn re_opening_a_log_applies_nothing() {
        // Migration 1 creates its tables unconditionally, so a second open that
        // succeeds is itself the evidence that the chain did not re-run.
        let dir = std::env::temp_dir().join(format!("ordo-schema-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("reopen.db");
        let _ = std::fs::remove_file(&db);

        let before = {
            let logger = Logger::open(&db, "test", "native", 1_000).unwrap();
            let conn = Connection::open(&db).unwrap();
            assert_eq!(version(&conn), CURRENT_VERSION);
            drop(logger);
            fingerprint(&conn)
        };

        let _second = Logger::open(&db, "test", "native", 2_000).unwrap();
        let conn = Connection::open(&db).unwrap();
        assert_eq!(version(&conn), CURRENT_VERSION);
        assert_eq!(fingerprint(&conn), before);
        // Both runs are on record: the second open added to the log, it did not
        // start it over.
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM runs", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            2
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Retention runs on the old schema, before any migration, so a future
    /// migration never pays to rewrite rows that are on their way out.
    #[test]
    fn opening_an_unversioned_log_prunes_and_migrates_together() {
        let dir = std::env::temp_dir().join(format!("ordo-prune-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("unversioned.db");
        let _ = std::fs::remove_file(&db);

        {
            let seed = Connection::open(&db).unwrap();
            seed.execute_batch(BASELINE).unwrap();
            seed.execute(
                "INSERT INTO runs (run_id, started_wall, version, backend)
                 VALUES (1, 0, 'ancient', 'native')",
                [],
            )
            .unwrap();
            seed.execute(
                "INSERT INTO events (run_id, seq, wall_ms, mono_ns, kind, payload)
                 VALUES (1, 0, 0, 0, 'engaged', '{}')",
                [],
            )
            .unwrap();
        }

        let thirty_days_ms = 30i64 * 24 * 60 * 60 * 1000;
        let run_id = Logger::open(&db, "test", "native", thirty_days_ms)
            .unwrap()
            .run_id();

        let conn = Connection::open(&db).unwrap();
        assert_eq!(version(&conn), CURRENT_VERSION);
        let surviving: i64 = conn
            .query_row("SELECT run_id FROM runs", [], |r| r.get(0))
            .unwrap();
        assert_eq!(surviving, run_id, "the ancient run was pruned");
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM events", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            0,
            "its events went with it"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
