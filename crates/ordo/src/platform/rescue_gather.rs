//! The OS-side rescue gather: put displaced windows back where the user can see
//! them.
//!
//! Deliberately dumb and linear, and it trusts only the log plus a fresh look at
//! the OS — never Ordo's in-memory state, which may be exactly what went wrong.
//! For every window the log says Ordo moved, or that is currently parked
//! off-screen, it moves the window to its display's active space and clamps its
//! frame onto the main display, cascading so they don't stack invisibly. Then
//! it records that it ran.
//!
//! This runs in the `ordo rescue` CLI's own process, so it works even if the
//! daemon is wedged or dead. It is idempotent — running it twice is harmless.

use std::collections::HashSet;
use std::path::Path;

use ordo_core::Rect;
use rusqlite::Connection;

use crate::backend::WorkspaceBackend;
use crate::rescue::{clamp_into_visible, is_offscreen, latest_run, record_rescue, touched_windows};

use super::{ax, display, native_backend::NativeBackend};

/// Returns how many windows were gathered.
pub fn gather(db_path: &Path, now_wall_ms: i64) -> usize {
    let conn = match Connection::open(db_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("ordo rescue: cannot open log db: {e}");
            return 0;
        }
    };

    let touched: HashSet<u32> = latest_run(&conn)
        .ok()
        .flatten()
        .and_then(|run| touched_windows(&conn, run).ok())
        .unwrap_or_default()
        .into_iter()
        .map(|w| w.0)
        .collect();

    let displays = display::active_displays();
    if displays.is_empty() {
        return 0;
    }
    let display_rects: Vec<Rect> = displays.iter().map(|d| d.frame).collect();
    let visible = displays
        .iter()
        .find(|d| d.is_main)
        .unwrap_or(&displays[0])
        .frame;

    // The emulated backend dims workspaces by hiding apps; a rescue must undo
    // that unconditionally — a wedged daemon can't, and invisible apps are
    // exactly the kind of wreckage the kill switch exists to clean up.
    ax::unhide_all_apps();

    let mut backend = NativeBackend::new();
    let scan = ax::scan();

    let mut gathered = 0usize;
    for w in &scan.windows {
        let displaced = touched.contains(&w.id.0) || is_offscreen(&w.frame, &display_rects);
        if !displaced {
            continue;
        }
        // Bring it onto a visible space, then onto the main display's area.
        let _ = backend.rescue_window(w.id);
        let target = clamp_into_visible(&w.frame, &visible, gathered);
        ax::set_frame(w.id, target);
        gathered += 1;
    }

    let _ = record_rescue(&conn, now_wall_ms, gathered, "gather complete");
    gathered
}
