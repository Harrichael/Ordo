//! Reading the native Spaces topology out of SkyLight.
//!
//! Two private reads, both SIP-on and permission-free:
//!   - [`managed_display_spaces`] walks `SLSCopyManagedDisplaySpaces` into, per
//!     display, its ordered space list and its current space. Workspace ordinal
//!     *i* is the *i*-th entry of that list — that mapping lives here and only
//!     here, exactly as the design requires.
//!   - [`spaces_for_windows`] maps each window id to its space via
//!     `SLSCopySpacesForWindows`.
//!
//! CAVEAT — this parser encodes the CFDictionary schema SkyLight has used
//! through recent macOS (keys "Display Identifier", "Spaces", "Current Space",
//! "ManagedSpaceID"/"id64", "type"). That schema is undocumented and has
//! shifted across releases; every lookup degrades gracefully (missing key →
//! skip) rather than crashing, and the whole thing wants validation on-device
//! against the running macOS version. It is the single most version-fragile
//! part of Ordo, which is why it is isolated here.

use std::collections::HashMap;
use std::ffi::c_void;

use ordo_core::{MonitorId, WindowId, WorkspaceId};
use ordo_skylight_sys as sys;

use super::cf;

pub struct DisplaySpaces {
    /// The SkyLight "Display Identifier": usually a UUID string, sometimes the
    /// literal "Main" for the primary display on some macOS versions.
    pub identifier: String,
    pub current_space: sys::CgsSpaceId,
    /// Ordered space ids; ordinal = index + 1.
    pub spaces: Vec<sys::CgsSpaceId>,
}

pub fn connection() -> sys::CgsConnectionId {
    unsafe { sys::SLSMainConnectionID() }
}

pub fn managed_display_spaces(cid: sys::CgsConnectionId) -> Vec<DisplaySpaces> {
    let array = unsafe { sys::SLSCopyManagedDisplaySpaces(cid) };
    if array.is_null() {
        return Vec::new();
    }
    let mut out = Vec::new();
    unsafe {
        for i in 0..cf::array_len(array) {
            let display = cf::array_get(array, i);
            let identifier =
                cf::string_value(cf::dict_get(display, "Display Identifier")).unwrap_or_default();

            let current_space = space_id(cf::dict_get(display, "Current Space")).unwrap_or(0);

            let spaces_arr = cf::dict_get(display, "Spaces");
            let mut spaces = Vec::new();
            for j in 0..cf::array_len(spaces_arr) {
                let space_dict = cf::array_get(spaces_arr, j);
                if let Some(id) = space_id(space_dict) {
                    spaces.push(id);
                }
            }

            out.push(DisplaySpaces {
                identifier,
                current_space,
                spaces,
            });
        }
        sys::CFRelease(array);
    }
    out
}

/// Fold the raw topology into what the core needs, keyed by MonitorId. `main`
/// is the main display's id, used to resolve the "Main" identifier form. The
/// per-display space-id -> ordinal map is returned too, so window assignments
/// can be translated without re-reading.
pub struct Topology {
    pub per_monitor: HashMap<MonitorId, (WorkspaceId, u8)>,
    pub space_to_ordinal: HashMap<sys::CgsSpaceId, WorkspaceId>,
}

pub fn fold_topology(displays: &[DisplaySpaces], known: &[(MonitorId, bool)]) -> Topology {
    let main = known.iter().find(|(_, is_main)| *is_main).map(|(m, _)| *m);
    let mut per_monitor = HashMap::new();
    let mut space_to_ordinal = HashMap::new();

    for d in displays {
        let Some(monitor) = resolve_monitor(&d.identifier, known, main) else {
            continue;
        };
        let count = d.spaces.len().min(u8::MAX as usize) as u8;
        let mut active = WorkspaceId(1);
        for (idx, sid) in d.spaces.iter().enumerate() {
            let ordinal = WorkspaceId((idx as u8).saturating_add(1).max(1));
            space_to_ordinal.insert(*sid, ordinal);
            if *sid == d.current_space {
                active = ordinal;
            }
        }
        per_monitor.insert(monitor, (active, count.max(1)));
    }

    Topology {
        per_monitor,
        space_to_ordinal,
    }
}

/// Window -> workspace ordinal, using a space-id -> ordinal map from
/// [`fold_topology`]. Windows whose space we can't resolve are omitted; the
/// caller treats an omitted window as "workspace unknown" and leaves belief be.
pub fn window_workspaces(
    cid: sys::CgsConnectionId,
    windows: &[WindowId],
    space_to_ordinal: &HashMap<sys::CgsSpaceId, WorkspaceId>,
) -> HashMap<WindowId, WorkspaceId> {
    let mut out = HashMap::new();
    if windows.is_empty() {
        return out;
    }
    unsafe {
        let Some(cf_windows) = make_number_array(windows) else {
            return out;
        };
        let result = sys::SLSCopySpacesForWindows(cid, sys::kCgsAllSpacesMask, cf_windows);
        sys::CFRelease(cf_windows);
        if result.is_null() {
            return out;
        }
        let n = cf::array_len(result).min(windows.len() as isize);
        for i in 0..n {
            let entry = cf::array_get(result, i);
            // Each entry is either a CFNumber (one space) or a CFArray of them;
            // the window's "home" space is the first.
            let sid = cf::number_i64(entry)
                .map(|v| v as sys::CgsSpaceId)
                .or_else(|| {
                    let first = cf::array_get(entry, 0);
                    cf::number_i64(first).map(|v| v as sys::CgsSpaceId)
                });
            if let Some(sid) = sid {
                if let Some(ordinal) = space_to_ordinal.get(&sid) {
                    out.insert(windows[i as usize], *ordinal);
                }
            }
        }
        sys::CFRelease(result);
    }
    out
}

/// The raw space id a window currently lives on (not translated to an
/// ordinal), needed to find which display's space list it belongs to before a
/// move.
pub fn raw_space_of_window(cid: sys::CgsConnectionId, window: WindowId) -> Option<sys::CgsSpaceId> {
    unsafe {
        let cf_windows = make_number_array(&[window])?;
        let result = sys::SLSCopySpacesForWindows(cid, sys::kCgsAllSpacesMask, cf_windows);
        sys::CFRelease(cf_windows);
        if result.is_null() {
            return None;
        }
        let entry = cf::array_get(result, 0);
        let sid = cf::number_i64(entry)
            .or_else(|| cf::number_i64(cf::array_get(entry, 0)))
            .map(|v| v as sys::CgsSpaceId);
        sys::CFRelease(result);
        sid
    }
}

/// Ask SkyLight to move one window to a space. Best-effort: on recent macOS
/// this may be a no-op from a non-Dock process (see the sys declaration), which
/// is why callers verify afterward.
pub fn move_window_to_space(cid: sys::CgsConnectionId, window: WindowId, space: sys::CgsSpaceId) {
    unsafe {
        let Some(cf_windows) = make_number_array(&[window]) else {
            return;
        };
        sys::SLSMoveWindowsToManagedSpace(cid, cf_windows, space);
        sys::CFRelease(cf_windows);
    }
}

/// Public wrapper over the identifier->MonitorId resolution, so the backend can
/// aim gestures at the right display.
pub fn resolve_monitor_id(identifier: &str, known: &[(MonitorId, bool)]) -> Option<MonitorId> {
    let main = known.iter().find(|(_, m)| *m).map(|(id, _)| *id);
    resolve_monitor(identifier, known, main)
}

unsafe fn space_id(space_dict: *const c_void) -> Option<sys::CgsSpaceId> {
    if space_dict.is_null() {
        return None;
    }
    cf::number_i64(cf::dict_get(space_dict, "ManagedSpaceID"))
        .or_else(|| cf::number_i64(cf::dict_get(space_dict, "id64")))
        .map(|v| v as sys::CgsSpaceId)
}

fn resolve_monitor(
    identifier: &str,
    known: &[(MonitorId, bool)],
    main: Option<MonitorId>,
) -> Option<MonitorId> {
    if identifier.eq_ignore_ascii_case("main") {
        return main;
    }
    if let Ok(parsed) = identifier.parse::<MonitorId>() {
        if known.iter().any(|(m, _)| *m == parsed) {
            return Some(parsed);
        }
    }
    // Unparseable identifier with a single display: it can only be that one.
    if known.len() == 1 {
        return Some(known[0].0);
    }
    None
}

/// Build a CFArray of CFNumbers (window ids) with proper CF callbacks, so
/// SkyLight retains them for the duration of its call.
unsafe fn make_number_array(windows: &[WindowId]) -> Option<sys::CFArrayRef> {
    let mut numbers: Vec<*const c_void> = Vec::with_capacity(windows.len());
    for w in windows {
        let id = w.0 as i64;
        let n = sys::CFNumberCreate(
            std::ptr::null(),
            sys::kCFNumberSInt64Type,
            &id as *const i64 as *const c_void,
        );
        if n.is_null() {
            for m in &numbers {
                sys::CFRelease(*m);
            }
            return None;
        }
        numbers.push(n);
    }
    let array = sys::CFArrayCreate(
        std::ptr::null(),
        numbers.as_ptr(),
        numbers.len() as isize,
        &sys::kCFTypeArrayCallBacks as *const c_void,
    );
    // CFArrayCreate retained each number; drop our references.
    for m in &numbers {
        sys::CFRelease(*m);
    }
    if array.is_null() {
        None
    } else {
        Some(array)
    }
}
