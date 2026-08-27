//! The kill switch's recovery half.
//!
//! Rescue trusts almost nothing: not the engine's in-memory `State`, not the AX
//! registry, not the backend's caches — those may be exactly what went wrong.
//! It trusts the append-only log (which windows Ordo may have displaced) and a
//! fresh look at the OS. The fast path that *disengages* interception lives in
//! the event tap (milestone M3); this module is the "now put things back where
//! I can see them" gather.
//!
//! The full OS-side gather (re-home displaced windows onto the visible area)
//! lands in M5. What is implemented and tested now is its one pure input: the
//! set of windows the log says Ordo moved, which the gather will operate on.

use ordo_core::WindowId;
use rusqlite::{params, Connection};

/// Every window named by a workspace-move or frame-set effect in the given run
/// — "windows Ordo may have displaced". The gather re-homes these first, then
/// (belt and braces) sweeps for anything off-screen regardless of the log.
pub fn touched_windows(conn: &Connection, run_id: i64) -> rusqlite::Result<Vec<WindowId>> {
    let mut stmt = conn.prepare(
        "SELECT DISTINCT payload FROM effects
         WHERE run_id = ?1 AND kind IN ('move_window_to_workspace', 'set_window_frame')",
    )?;
    let rows = stmt.query_map(params![run_id], |row| {
        let payload: String = row.get(0)?;
        Ok(payload)
    })?;

    let mut seen = std::collections::BTreeSet::new();
    for row in rows {
        if let Some(w) = window_id_of(&row?) {
            seen.insert(w.0);
        }
    }
    Ok(seen.into_iter().map(WindowId).collect())
}

/// The most recent run id in the log, or None on an empty DB. The rescue CLI
/// operates on the current/last run.
pub fn latest_run(conn: &Connection) -> rusqlite::Result<Option<i64>> {
    conn.query_row("SELECT MAX(run_id) FROM runs", [], |row| {
        row.get::<_, Option<i64>>(0)
    })
}

fn window_id_of(effect_payload: &str) -> Option<WindowId> {
    let v: serde_json::Value = serde_json::from_str(effect_payload).ok()?;
    // Both effect variants carry `{ "...": { "window": <id>, ... } }`.
    let obj = v.as_object()?;
    let inner = obj.values().next()?.as_object()?;
    let id = inner.get("window")?.as_u64()?;
    Some(WindowId(id as u32))
}
