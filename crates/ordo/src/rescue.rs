//! The kill switch's recovery half.
//!
//! Rescue trusts almost nothing: not the engine's in-memory `State`, not the AX
//! registry, not the backend's caches — those may be exactly what went wrong.
//! It trusts the append-only log (which windows Ordo may have displaced) and a
//! fresh look at the OS. The fast path that *disengages* interception lives in
//! the event tap (milestone M3); this module is the "now put things back where
//! I can see them" gather.
//!
//! The portable half lives here — reading the log for displaced windows and the
//! pure geometry that re-homes a frame into the visible area. The OS-side gather
//! that actually moves windows is [`crate::platform::rescue_gather`], which
//! composes these with live enumeration.

use ordo_core::{Rect, WindowId};
use rusqlite::{params, Connection};

/// Offset between successive rescued windows so they don't stack invisibly.
const CASCADE_STEP: f64 = 32.0;

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

/// Whether a window's frame lies entirely outside the union of display bounds —
/// i.e. it has been parked somewhere the user can't reach it.
pub fn is_offscreen(frame: &Rect, displays: &[Rect]) -> bool {
    !displays.iter().any(|d| intersects(frame, d))
}

fn intersects(a: &Rect, b: &Rect) -> bool {
    a.x < b.x + b.w && a.x + a.w > b.x && a.y < b.y + b.h && a.y + a.h > b.y
}

/// Re-home a frame fully inside `visible`, shrinking it if it's larger and
/// offsetting by `cascade` steps so multiple rescued windows don't overlap
/// exactly. Pure geometry — the heart of "bring it back where I can see it".
pub fn clamp_into_visible(frame: &Rect, visible: &Rect, cascade: usize) -> Rect {
    let w = frame.w.min(visible.w);
    let h = frame.h.min(visible.h);
    let offset = cascade as f64 * CASCADE_STEP;
    // Offset from the top-left, but never so far that the window leaves the
    // visible area; the max(visible.x) guards the case where offset > slack.
    let max_x = (visible.x + visible.w - w).max(visible.x);
    let max_y = (visible.y + visible.h - h).max(visible.y);
    let x = (visible.x + offset).min(max_x);
    let y = (visible.y + offset).min(max_y);
    Rect { x, y, w, h }
}

/// Record that a rescue gather ran, so the log tells the whole story. Creates
/// the table on first use (the CLI opens the DB directly, without the Logger).
pub fn record_rescue(
    conn: &Connection,
    wall_ms: i64,
    windows_gathered: usize,
    detail: &str,
) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS rescue_events (
            wall_ms          INTEGER NOT NULL,
            windows_gathered INTEGER NOT NULL,
            detail           TEXT
        );",
    )?;
    conn.execute(
        "INSERT INTO rescue_events (wall_ms, windows_gathered, detail) VALUES (?1, ?2, ?3)",
        params![wall_ms, windows_gathered as i64, detail],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(x: f64, y: f64, w: f64, h: f64) -> Rect {
        Rect { x, y, w, h }
    }

    #[test]
    fn offscreen_detection_matches_the_visible_union() {
        let displays = vec![r(0.0, 0.0, 1920.0, 1080.0), r(1920.0, 0.0, 1920.0, 1080.0)];
        assert!(is_offscreen(&r(-5000.0, 0.0, 400.0, 300.0), &displays));
        assert!(!is_offscreen(&r(100.0, 100.0, 400.0, 300.0), &displays));
        // Straddling the seam still counts as on-screen.
        assert!(!is_offscreen(&r(1800.0, 100.0, 400.0, 300.0), &displays));
    }

    #[test]
    fn clamp_brings_a_far_window_fully_onto_the_display() {
        let visible = r(0.0, 0.0, 1920.0, 1080.0);
        let parked = r(-9000.0, -9000.0, 400.0, 300.0);
        let fixed = clamp_into_visible(&parked, &visible, 0);
        assert!(fixed.x >= visible.x && fixed.x + fixed.w <= visible.x + visible.w);
        assert!(fixed.y >= visible.y && fixed.y + fixed.h <= visible.y + visible.h);
    }

    #[test]
    fn clamp_shrinks_and_still_fits_a_window_bigger_than_the_display() {
        let visible = r(0.0, 0.0, 800.0, 600.0);
        let huge = r(0.0, 0.0, 4000.0, 3000.0);
        let fixed = clamp_into_visible(&huge, &visible, 5);
        assert!(fixed.w <= visible.w && fixed.h <= visible.h);
        assert!(fixed.x + fixed.w <= visible.x + visible.w + 0.001);
        assert!(fixed.y + fixed.h <= visible.y + visible.h + 0.001);
    }
}
