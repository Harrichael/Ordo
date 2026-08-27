//! Global window z-order: read it, and push a window to the back of it.
//!
//! Reading is public CoreGraphics (`CGWindowListCopyWindowInfo` returns
//! on-screen windows front-to-back). Writing is not: WindowServer refuses
//! `SLSOrderWindow` on a foreign window from a daemon connection (error 1000,
//! probed in examples/order_probe.rs — yabai needs its SIP-off Dock payload
//! for the direct route). What *is* honored from a daemon is `AXRaise`, which
//! moves a window in the global stack (examples/raise_probe.rs) — so "send to
//! back" is spelled "raise everything else, back-to-front", which reproduces
//! the existing order with the victim underneath.
//!
//! One Tahoe quirk is a feature here: raises land BELOW the active app's key
//! window, so callers should hand focus to the intended top window *before*
//! lowering — the raises then slot in under it.

use ordo_core::WindowId;
use ordo_skylight_sys as sys;

use super::{ax, cf};

const ON_SCREEN_ONLY: u32 = 1 << 0;
const EXCLUDE_DESKTOP: u32 = 1 << 4;

#[cfg_attr(target_os = "macos", link(name = "CoreGraphics", kind = "framework"))]
extern "C" {
    fn CGWindowListCopyWindowInfo(option: u32, relative_to: u32) -> sys::CFArrayRef;
}

/// On-screen normal (layer-0) windows, front to back.
pub fn stack_front_to_back() -> Vec<WindowId> {
    let mut out = Vec::new();
    unsafe {
        let arr = CGWindowListCopyWindowInfo(ON_SCREEN_ONLY | EXCLUDE_DESKTOP, 0);
        if arr.is_null() {
            return out;
        }
        for i in 0..cf::array_len(arr) {
            let d = cf::array_get(arr, i) as sys::CFDictionaryRef;
            if cf::number_i64(cf::dict_get(d, "kCGWindowLayer")) != Some(0) {
                continue;
            }
            if let Some(wid) = cf::number_i64(cf::dict_get(d, "kCGWindowNumber")) {
                out.push(WindowId(wid as u32));
            }
        }
        sys::CFRelease(arr);
    }
    out
}

/// Visually bury `window`: every other on-screen window is raised back-to-
/// front, restoring their relative order with `window` left at the bottom.
pub fn send_to_back(window: WindowId) {
    let mut order: Vec<WindowId> = stack_front_to_back()
        .into_iter()
        .filter(|w| *w != window)
        .collect();
    order.reverse(); // raise back-to-front so the front-most is raised last
    ax::raise_ordered(&order);
}
