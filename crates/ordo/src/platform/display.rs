//! Displays, straight from Core Graphics.
//!
//! We read display geometry from CG (`CGDisplayBounds`), whose coordinate space
//! is the global top-left-origin, y-down space that the Accessibility API also
//! uses. Reading from CG rather than `NSScreen` is deliberate: NSScreen's frame
//! is y-flipped, and mixing the two spaces is the classic multi-monitor bug.
//! Identity is the stable display UUID, not the CGDirectDisplayID, which churns
//! across hot-plug and sleep.
//!
//! A mirror set counts ONCE: two panels showing the same pixels are one
//! monitor to the user and one position to the projection. CG reports the
//! secondaries of a hardware mirror set as inactive already; software mirrors
//! and any edge case that slips through are caught by the mirror query and,
//! belt and braces, by identical bounds (nothing but a mirror shares them).

use objc2_core_graphics::{
    CGDisplayBounds, CGDisplayIsMain, CGDisplayMirrorsDisplay, CGGetActiveDisplayList,
    CGMainDisplayID,
};
use ordo_core::{MonitorId, Rect};
use ordo_skylight_sys as sys;

pub struct DisplayInfo {
    pub id: MonitorId,
    pub frame: Rect,
    pub is_main: bool,
}

pub fn active_displays() -> Vec<DisplayInfo> {
    let ids = active_display_ids();
    let main = CGMainDisplayID();
    let mut out: Vec<DisplayInfo> = Vec::new();
    for cg_id in ids {
        // A display mirroring another is that other's pixels, not a monitor.
        if CGDisplayMirrorsDisplay(cg_id) != 0 {
            continue;
        }
        let Some(uuid) = display_uuid(cg_id) else { continue };
        let b = CGDisplayBounds(cg_id);
        let frame = Rect {
            x: b.origin.x,
            y: b.origin.y,
            w: b.size.width,
            h: b.size.height,
        };
        let is_main = cg_id == main || CGDisplayIsMain(cg_id);
        if let Some(twin) = out.iter_mut().find(|d| d.frame == frame) {
            twin.is_main |= is_main;
            continue;
        }
        out.push(DisplayInfo {
            id: uuid,
            frame,
            is_main,
        });
    }
    out
}

fn active_display_ids() -> Vec<u32> {
    // First call learns the count, second fills the buffer.
    let mut count: u32 = 0;
    let err = unsafe { CGGetActiveDisplayList(0, std::ptr::null_mut(), &mut count) };
    if err.0 != 0 || count == 0 {
        return Vec::new();
    }
    let mut ids = vec![0u32; count as usize];
    let mut got: u32 = 0;
    let err = unsafe { CGGetActiveDisplayList(count, ids.as_mut_ptr(), &mut got) };
    if err.0 != 0 {
        return Vec::new();
    }
    ids.truncate(got as usize);
    ids
}

/// The display's UUID as a `u128`, via the private-but-stable CG UUID call.
/// Returns None if CG hands back no UUID (should not happen for an active
/// display, but belief must not depend on it).
fn display_uuid(cg_id: u32) -> Option<MonitorId> {
    let uuid_ref = unsafe { sys::CGDisplayCreateUUIDFromDisplayID(cg_id) };
    if uuid_ref.is_null() {
        return None;
    }
    let bytes = unsafe { sys::CFUUIDGetUUIDBytes(uuid_ref) };
    unsafe { sys::CFRelease(uuid_ref) };
    Some(MonitorId(u128::from_be_bytes(bytes.0)))
}
