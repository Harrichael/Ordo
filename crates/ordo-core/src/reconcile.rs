use serde::{Deserialize, Serialize};

use crate::effect::Expectation;
use crate::event::{MonitorSnap, WorldSnapshot};
use crate::ids::{MonitorId, Rect, WindowId, WorkspaceId, FRAME_EPSILON};
use crate::state::{State, WindowRecord};

/// What changed between the previous belief and a fresh snapshot. Deltas are
/// the unit of self-vs-external attribution: one that matches a pending
/// expectation is the echo of our own effect; the rest is the user or macOS.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Delta {
    MonitorAdded(MonitorId),
    MonitorRemoved(MonitorId),
    MonitorWorkspaceChanged {
        monitor: MonitorId,
        from: WorkspaceId,
        to: WorkspaceId,
    },
    WindowCreated(WindowId),
    WindowDestroyed(WindowId),
    WindowWorkspaceChanged {
        window: WindowId,
        from: WorkspaceId,
        to: WorkspaceId,
    },
    WindowMonitorChanged {
        window: WindowId,
        from: MonitorId,
        to: MonitorId,
    },
    WindowFrameChanged {
        window: WindowId,
        from: Rect,
        to: Rect,
    },
    TitleChanged(WindowId),
    FocusChanged {
        from: Option<WindowId>,
        to: Option<WindowId>,
    },
}

pub(crate) fn diff(state: &State, snap: &WorldSnapshot) -> Vec<Delta> {
    let mut deltas = Vec::new();

    for id in state.monitors.keys() {
        if !snap.monitors.iter().any(|m| m.id == *id) {
            deltas.push(Delta::MonitorRemoved(*id));
        }
    }
    for m in &snap.monitors {
        if !state.monitors.contains_key(&m.id) {
            deltas.push(Delta::MonitorAdded(m.id));
        }
    }
    // Workspace changes come from the backend's word, never from geometry; a
    // monitor absent from the word is unknown, and unknown is not a change.
    for (id, mws) in &snap.workspaces.monitors {
        if let Some(old) = state.monitor_ws.get(id) {
            if *old != mws.active {
                deltas.push(Delta::MonitorWorkspaceChanged {
                    monitor: *id,
                    from: *old,
                    to: mws.active,
                });
            }
        }
    }

    for id in state.windows.keys() {
        if !snap.windows.iter().any(|w| w.id == *id) {
            deltas.push(Delta::WindowDestroyed(*id));
        }
    }
    for w in &snap.windows {
        match state.windows.get(&w.id) {
            None => {
                // A newcomer with no workspace word can't enter the model
                // (see apply_snapshot); reporting it created would spam an
                // unabsorbable delta every scan until the backend resolves it.
                if snap.workspaces.assignments.contains_key(&w.id) {
                    deltas.push(Delta::WindowCreated(w.id));
                }
            }
            Some(old) => {
                if let Some(new_ws) = snap.workspaces.assignments.get(&w.id) {
                    if old.workspace != *new_ws {
                        deltas.push(Delta::WindowWorkspaceChanged {
                            window: w.id,
                            from: old.workspace,
                            to: *new_ws,
                        });
                    }
                }
                if let Some(mon) = derive_monitor(&w.frame, &snap.monitors) {
                    if old.monitor != mon {
                        deltas.push(Delta::WindowMonitorChanged {
                            window: w.id,
                            from: old.monitor,
                            to: mon,
                        });
                    }
                }
                if !old.frame.approx_eq(&w.frame, FRAME_EPSILON) {
                    deltas.push(Delta::WindowFrameChanged {
                        window: w.id,
                        from: old.frame,
                        to: w.frame,
                    });
                }
                if old.title != w.title {
                    deltas.push(Delta::TitleChanged(w.id));
                }
            }
        }
    }

    if state.focused != snap.focused {
        deltas.push(Delta::FocusChanged {
            from: state.focused,
            to: snap.focused,
        });
    }

    deltas
}

/// Belief follows the snapshot wholesale — records are rebuilt, not patched.
/// Only what the snapshot cannot know survives: focus history, declarations,
/// damping counters, pendings (handled by the caller). Observed focus is
/// mirrored here but never recorded into the MRU history: which window counts
/// as "used" is the caller's decision, made against the focus declaration.
pub(crate) fn apply_snapshot(s: &mut State, snap: &WorldSnapshot) {
    let old_windows = std::mem::take(&mut s.windows);

    s.monitors.clear();
    for m in &snap.monitors {
        s.monitors.insert(
            m.id,
            crate::state::MonitorRecord {
                id: m.id,
                frame: m.frame,
                is_main: m.is_main,
            },
        );
    }
    // The workspace layer updates from the backend's word alone. A monitor
    // the word doesn't mention keeps its previous workspace — unknown leaves
    // belief be — while a monitor gone from the observed geometry is gone.
    s.monitor_ws.retain(|id, _| s.monitors.contains_key(id));
    for (id, mws) in &snap.workspaces.monitors {
        if s.monitors.contains_key(id) {
            s.monitor_ws.insert(*id, mws.active);
        }
    }
    // Keep the old count through a blip (display sleep, a wordless backend)
    // rather than collapsing to a default the next snapshot would fight.
    if let Some(min) = snap.workspaces.monitors.values().map(|m| m.count).min() {
        s.workspace_count = min.max(1);
    }

    for w in &snap.windows {
        // A window whose monitor can't be derived means the snapshot has no
        // monitors at all — nothing the core decides about it would be
        // executable, so it stays out of the model.
        let Some(monitor) = derive_monitor(&w.frame, &snap.monitors) else {
            continue;
        };
        // The workspace is the backend's word; without one, the previous
        // belief stands. A newcomer with no word at all stays out of the
        // model until the backend resolves it — never fabricated onto a
        // default workspace (the old `unwrap_or(ws 1)` silently teleported
        // belief for every window a SkyLight query missed).
        let prior = old_windows.get(&w.id);
        let Some(workspace) = snap
            .workspaces
            .assignments
            .get(&w.id)
            .copied()
            .or(prior.map(|r| r.workspace))
        else {
            continue;
        };
        let (ws_corrections, frame_corrections) =
            prior.map_or((0, 0), |r| (r.ws_corrections, r.frame_corrections));
        let is_new = prior.is_none();
        s.windows.insert(
            w.id,
            WindowRecord {
                id: w.id,
                app: w.app,
                bundle_id: w.bundle_id.clone(),
                title: w.title.clone(),
                workspace,
                monitor,
                frame: w.frame,
                ws_corrections,
                frame_corrections,
            },
        );
        // Enter never-before-seen windows into the MRU history at the back — but
        // only ones that actually made it into the model, so focus_history stays
        // a subset of `windows` (an invariant the rest of the core relies on).
        if is_new {
            s.focus_history.note_created(w.id);
        }
    }

    for gone in old_windows.keys() {
        if !s.windows.contains_key(gone) {
            s.focus_history.remove(*gone);
        }
    }

    s.focused = snap.focused.filter(|w| s.windows.contains_key(w));
}

/// The monitor whose frame contains the window's center; a window straddling
/// displays belongs to whichever holds its center, matching where the user
/// perceives it. Falls back to nearest center for fully off-screen frames
/// (macOS mostly prevents these, but belief must not crash on them).
pub(crate) fn derive_monitor(frame: &Rect, monitors: &[MonitorSnap]) -> Option<MonitorId> {
    let c = frame.center();
    if let Some(m) = monitors.iter().find(|m| m.frame.contains(c)) {
        return Some(m.id);
    }
    monitors
        .iter()
        .min_by(|a, b| {
            let da = dist2(a.frame, c.x, c.y);
            let db = dist2(b.frame, c.x, c.y);
            da.total_cmp(&db)
        })
        .map(|m| m.id)
}

fn dist2(r: Rect, x: f64, y: f64) -> f64 {
    let c = r.center();
    (c.x - x).powi(2) + (c.y - y).powi(2)
}

/// Would this pending expectation account for this delta? A focus change is
/// explained only by a grant for exactly that window: a switch's fallout that
/// re-keys something else is real, and the log should show it as such.
pub(crate) fn explains(e: &Expectation, d: &Delta) -> bool {
    match (e, d) {
        (Expectation::AllMonitorsOn(t), Delta::MonitorWorkspaceChanged { to, .. }) => to == t,
        (
            Expectation::WindowOn { window, workspace },
            Delta::WindowWorkspaceChanged { window: w, to, .. },
        ) => w == window && to == workspace,
        (
            Expectation::WindowFramed { window, frame },
            Delta::WindowFrameChanged { window: w, to, .. },
        ) => w == window && to.approx_eq(frame, FRAME_EPSILON),
        (
            Expectation::WindowFramed { window, .. },
            Delta::WindowMonitorChanged { window: w, .. },
        ) => w == window,
        (Expectation::Focused(t), Delta::FocusChanged { to, .. }) => *to == Some(*t),
        _ => false,
    }
}
