//! Global window z-order: read it, and impose a desired order on it.
//!
//! Reading is public CoreGraphics (`CGWindowListCopyWindowInfo` returns
//! on-screen windows front-to-back). Writing is not: WindowServer refuses
//! `SLSOrderWindow` on a foreign window from a daemon connection (error 1000,
//! probed in examples/order_probe.rs — yabai needs its SIP-off Dock payload
//! for the direct route). What *is* honored from a daemon is `AXRaise`, which
//! moves a window in the global stack (examples/raise_probe.rs) — so an
//! arbitrary order is spelled "raise each one, back-to-front".
//!
//! One Tahoe quirk is a feature here: raises land BELOW the active app's key
//! window, so callers should hand focus to the intended top window *before*
//! restacking — the raises then slot in under it.

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

/// Impose `desired` (front-to-back) as the relative z-order of those windows,
/// verified. One raise pass is NOT enough on its own: `AXRaise` returns when
/// the app acknowledges, not when WindowServer reorders — Chromium apps
/// process AX on their own thread, so a slow app's raise (or its un-hide's
/// window resurfacing) can land after a later window's and bury it. So:
/// raise back-to-front, read the real order back, retry on mismatch.
///
/// The currently focused window is exempt from both raising and comparison:
/// raises land below the key window, so its position is macOS's to decide and
/// fighting it would just burn the retries.
pub fn reassert_stack(desired: &[WindowId]) {
    let focused = ax::focused_window();
    let want: Vec<WindowId> = desired
        .iter()
        .copied()
        .filter(|w| Some(*w) != focused)
        .collect();
    if want.len() < 2 {
        return;
    }
    let mut back_to_front = want.clone();
    back_to_front.reverse();
    for attempt in 0..3 {
        ax::raise_ordered(&back_to_front);
        // Give async reorders a beat to land before judging; slower each try.
        std::thread::sleep(std::time::Duration::from_millis(30 + attempt * 90));
        let actual: Vec<WindowId> = stack_front_to_back()
            .into_iter()
            .filter(|w| want.contains(w) && Some(*w) != focused)
            .collect();
        if actual == want {
            return;
        }
    }
}
