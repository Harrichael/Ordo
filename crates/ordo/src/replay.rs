//! Re-run a logged run through the pure core and check the core still decides
//! exactly what it decided live.
//!
//! Because the core is deterministic and clock-free, replaying the logged
//! `Event` stream from the logged `State` must reproduce the logged `Effect`
//! stream byte-for-byte. A mismatch means one of two things, both worth
//! knowing: the core's logic changed since the run (expected, when you're
//! debugging a fix), or something non-deterministic leaked into the core (a
//! bug the whole architecture exists to prevent). Either way replay points at
//! the exact event where live and re-run diverged.

use ordo_core::{update, Effect, Event, State};
use rusqlite::{params, Connection};

#[derive(Debug)]
pub struct ReplayReport {
    pub run_id: i64,
    pub from_seq: u64,
    pub events_replayed: usize,
    pub mismatches: Vec<Mismatch>,
}

#[derive(Debug)]
pub struct Mismatch {
    pub seq: u64,
    pub logged: Vec<Effect>,
    pub recomputed: Vec<Effect>,
}

impl ReplayReport {
    pub fn is_clean(&self) -> bool {
        self.mismatches.is_empty()
    }
}

/// Replay `run_id`, starting from the nearest checkpoint at or before
/// `from_seq` (or from an empty state if none). `from_seq = None` replays the
/// whole run.
pub fn replay(
    conn: &Connection,
    run_id: i64,
    from_seq: Option<u64>,
) -> rusqlite::Result<ReplayReport> {
    let target = from_seq.unwrap_or(0);

    let (mut state, start_seq) = load_checkpoint(conn, run_id, target)?;

    let mut stmt = conn.prepare(
        "SELECT seq, payload FROM events
         WHERE run_id = ?1 AND seq >= ?2 ORDER BY seq",
    )?;
    let rows = stmt.query_map(params![run_id, start_seq], |row| {
        let seq: i64 = row.get(0)?;
        let payload: String = row.get(1)?;
        Ok((seq as u64, payload))
    })?;

    let mut report = ReplayReport {
        run_id,
        from_seq: start_seq,
        events_replayed: 0,
        mismatches: Vec::new(),
    };

    for row in rows {
        let (seq, payload) = row?;
        let event: Event = match serde_json::from_str(&payload) {
            Ok(e) => e,
            Err(_) => continue, // a corrupt row shouldn't abort the whole replay
        };
        let step = update(&state, &event);
        let logged = logged_effects(conn, run_id, seq)?;
        if logged != step.effects {
            report.mismatches.push(Mismatch {
                seq,
                logged,
                recomputed: step.effects.clone(),
            });
        }
        state = step.state;
        report.events_replayed += 1;
    }

    Ok(report)
}

/// The most recent checkpoint at or before `target`, and the seq to resume
/// from. Checkpoints store the state *before* their seq's event was applied, so
/// replay resumes at that seq.
fn load_checkpoint(conn: &Connection, run_id: i64, target: u64) -> rusqlite::Result<(State, u64)> {
    let found: Option<(i64, String)> = conn
        .query_row(
            "SELECT seq, payload FROM checkpoints
             WHERE run_id = ?1 AND seq <= ?2 ORDER BY seq DESC LIMIT 1",
            params![run_id, target as i64],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .ok();

    match found {
        Some((seq, payload)) => {
            let state = serde_json::from_str(&payload).unwrap_or_else(|_| State::new());
            Ok((state, seq as u64))
        }
        None => Ok((State::new(), 0)),
    }
}

fn logged_effects(conn: &Connection, run_id: i64, seq: u64) -> rusqlite::Result<Vec<Effect>> {
    let mut stmt = conn.prepare(
        "SELECT payload FROM effects
         WHERE run_id = ?1 AND seq = ?2 ORDER BY ord",
    )?;
    let rows = stmt.query_map(params![run_id, seq as i64], |row| {
        let payload: String = row.get(0)?;
        Ok(payload)
    })?;
    let mut effects = Vec::new();
    for row in rows {
        if let Ok(e) = serde_json::from_str(&row?) {
            effects.push(e);
        }
    }
    Ok(effects)
}
