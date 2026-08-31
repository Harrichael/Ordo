//! The macOS FFI edge — the only part of Ordo that talks to the OS.
//!
//! Assembly of a [`WorldSnapshot`] pulls from three sources and joins them on
//! stable ids: display geometry from Core Graphics ([`display`]), windows from
//! the Accessibility API ([`ax`]), and workspace assignment from the backend
//! ([`native_backend`] over [`skylight`], or [`emulated_backend`] adapting the
//! `ordo-emulated` crate). The backend is shared with the
//! effector (later milestones) via `Rc<RefCell<…>>`; that is sound because the
//! entire platform layer lives on the single engine thread — none of these
//! handles are `Send`, and none ever leave it.
//!
//! NOTE: the FFI here compiles and follows each API's documented shape, but the
//! private SkyLight schema parsing in particular wants validation on-device
//! against the running macOS version (see [`skylight`]).

pub mod ax;
pub mod cf;
pub mod display;
pub mod effector;
pub mod emulated_backend;
pub mod mission_control;
pub mod mouse;
pub mod native_backend;
pub mod observer;
pub mod rescue_gather;
pub mod restack_worker;
pub mod skylight;
pub mod tap;
pub mod ws_events;
pub mod zorder;

pub use effector::MacEffector;

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use ordo_core::{
    MonitorId, MonitorSnap, MonitorWs, Pid, Rect, WindowId, WindowSnap, WorkspaceSnap,
    WorldSnapshot,
};

use crate::backend::WorkspaceBackend;
use crate::ports::WorldSource;

pub type SharedBackend = Rc<RefCell<dyn WorkspaceBackend>>;

pub fn native_backend() -> SharedBackend {
    Rc::new(RefCell::new(native_backend::NativeBackend::new()))
}

/// `state_path`: where the ledger's promises persist across restarts;
/// `None` (e.g. `--fresh`) starts empty and stays ephemeral.
pub fn emulated_backend(workspaces: u8, state_path: Option<std::path::PathBuf>) -> SharedBackend {
    Rc::new(RefCell::new(match state_path {
        Some(p) => emulated_backend::EmulatedBackend::with_persistence(workspaces, p),
        None => emulated_backend::EmulatedBackend::new(workspaces),
    }))
}

/// Builds a full snapshot each call: displays + AX windows + backend workspace
/// classification, joined into the core's vocabulary.
pub struct MacWorldSource {
    backend: SharedBackend,
    /// The tap/effector's shared engagement flag. Placement enforcement rides
    /// the rescan cadence, so it must stop the moment Ordo is paused or
    /// rescued — a rescue gather frees windows the ledger still calls parked,
    /// and fighting the gather would be worse than any phantom.
    intercepting: Arc<AtomicBool>,
}

impl MacWorldSource {
    pub fn new(backend: SharedBackend, intercepting: Arc<AtomicBool>) -> Self {
        MacWorldSource {
            backend,
            intercepting,
        }
    }
}

impl WorldSource for MacWorldSource {
    fn snapshot(&mut self) -> WorldSnapshot {
        let displays = display::active_displays();
        let known: Vec<(MonitorId, bool)> = displays.iter().map(|d| (d.id, d.is_main)).collect();

        let scan = ax::scan();
        let window_ids: Vec<WindowId> = scan.windows.iter().map(|w| w.id).collect();

        let topo = self
            .backend
            .borrow_mut()
            .topology(&window_ids, &known)
            .unwrap_or_default();

        let frames: HashMap<WindowId, (Pid, Rect)> = scan
            .windows
            .iter()
            .map(|w| (w.id, (w.app, w.frame)))
            .collect();

        if self.intercepting.load(Ordering::Relaxed) {
            // The corrective write lands after this snapshot was read, so the
            // snapshot still shows the phantom; the next rescan absorbs the
            // fix as an (unattributed) external delta. Acceptable for a
            // standing-invariant band-aid.
            self.backend.borrow_mut().enforce_placement(&frames);
        }

        // Mechanism artifacts (park slivers) never reach the core: the
        // backend substitutes the promise each one encodes.
        let believed = self.backend.borrow().believed_frames(&frames);

        let monitors = displays
            .iter()
            .map(|d| MonitorSnap {
                id: d.id,
                frame: d.frame,
                is_main: d.is_main,
            })
            .collect();

        let windows = scan
            .windows
            .iter()
            .map(|w| WindowSnap {
                id: w.id,
                app: w.app,
                bundle_id: w.bundle_id.clone(),
                title: w.title.clone(),
                frame: believed.get(&w.id).copied().unwrap_or(w.frame),
            })
            .collect();

        // The workspace layer travels on its own channel, exactly as the
        // backend told it: a monitor or window it didn't resolve is absent —
        // UNKNOWN to the core — never defaulted (the old join fabricated
        // workspace 1 for unresolved windows).
        let workspaces = WorkspaceSnap {
            monitors: topo
                .monitors
                .iter()
                .map(|m| {
                    (
                        m.monitor,
                        MonitorWs {
                            active: m.active,
                            count: m.count,
                        },
                    )
                })
                .collect(),
            assignments: topo.window_ws.iter().map(|(w, ws)| (*w, *ws)).collect(),
        };

        WorldSnapshot {
            monitors,
            windows,
            focused: scan.focused,
            workspaces,
        }
    }
}
