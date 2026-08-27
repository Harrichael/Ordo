//! The macOS FFI edge — the only part of Ordo that talks to the OS.
//!
//! Assembly of a [`WorldSnapshot`] pulls from three sources and joins them on
//! stable ids: display geometry from Core Graphics ([`display`]), windows from
//! the Accessibility API ([`ax`]), and workspace assignment from the backend
//! ([`native_backend`] over [`skylight`]). The backend is shared with the
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
pub mod skylight;
pub mod tap;
pub mod zorder;

pub use effector::MacEffector;

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use ordo_core::{MonitorId, MonitorSnap, WindowId, WindowSnap, WorkspaceId, WorldSnapshot};

use crate::backend::WorkspaceBackend;
use crate::ports::WorldSource;

pub type SharedBackend = Rc<RefCell<dyn WorkspaceBackend>>;

pub fn native_backend() -> SharedBackend {
    Rc::new(RefCell::new(native_backend::NativeBackend::new()))
}

pub fn emulated_backend(workspaces: u8) -> SharedBackend {
    Rc::new(RefCell::new(emulated_backend::EmulatedBackend::new(
        workspaces,
    )))
}

/// Builds a full snapshot each call: displays + AX windows + backend workspace
/// classification, joined into the core's vocabulary.
pub struct MacWorldSource {
    backend: SharedBackend,
}

impl MacWorldSource {
    pub fn new(backend: SharedBackend) -> Self {
        MacWorldSource { backend }
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

        let per_monitor: HashMap<MonitorId, (WorkspaceId, u8)> = topo
            .monitors
            .iter()
            .map(|m| (m.monitor, (m.active, m.count)))
            .collect();
        // The usable workspace count spans all displays, so it's the min.
        let global_count = topo
            .monitors
            .iter()
            .map(|m| m.count)
            .min()
            .unwrap_or(1)
            .max(1);

        let monitors = displays
            .iter()
            .map(|d| {
                let (active, count) = per_monitor
                    .get(&d.id)
                    .copied()
                    .unwrap_or((WorkspaceId(1), global_count));
                MonitorSnap {
                    id: d.id,
                    frame: d.frame,
                    is_main: d.is_main,
                    active_workspace: active,
                    workspace_count: count,
                }
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
                frame: w.frame,
                // A window SkyLight didn't resolve defaults to workspace 1.
                // Known limitation: a window on another space that the query
                // missed will look mislocated until the next resolved scan.
                workspace: topo.window_ws.get(&w.id).copied().unwrap_or(WorkspaceId(1)),
            })
            .collect();

        WorldSnapshot {
            monitors,
            windows,
            focused: scan.focused,
        }
    }
}
