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

/// On-screen normal (layer-0) windows with their owning pids, front to back.
/// Pids come from the same CG read (`kCGWindowOwnerPID`) — no AX involved.
pub fn stack_with_pids() -> Vec<(u32, i32)> {
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
            let wid = cf::number_i64(cf::dict_get(d, "kCGWindowNumber"));
            let pid = cf::number_i64(cf::dict_get(d, "kCGWindowOwnerPID"));
            if let (Some(w), Some(p)) = (wid, pid) {
                out.push((w as u32, p as i32));
            }
        }
        sys::CFRelease(arr);
    }
    out
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

/// Impose `desired` (front-to-back) as the relative z-order of those windows.
///
/// The only per-window lever a SIP-on daemon has is `AXRaise` (probed:
/// SLSOrderWindow is refused with error 1000; SLPS make-key records don't
/// reorder; a burst of SLPSSetFrontProcessWithOptions coalesces to the last
/// call — see examples/order_probe.rs and slps_order_probe.rs). AXRaise is
/// processed on the target APP's schedule (Chromium: 100ms+), so raises to
/// different apps land in arbitrary relative order. Fire-and-verify loops
/// can't fix that: a pass can verify clean and then be broken by a raise
/// still in flight.
///
/// The rule that makes it deterministic: NEVER have two raises in flight.
/// Each raise is confirmed landed — read back from WindowServer — before the
/// next is issued. Same-app raises are ordered by the app's own AX queue;
/// cross-app ordering is enforced by the landing check. When the pass ends,
/// nothing is in flight, so nothing can retroactively break it. Fast apps
/// confirm in one ~few-ms poll; a slow app costs its true latency, once.
///
/// Two prerequisites are also waited on, because they're app-async too:
/// windows still resurfacing from an un-hide (absent from the CG list) are
/// awaited before ordering starts, and windows already in correct relative
/// position at the bottom are skipped entirely — the common round trip
/// raises only what actually moved.
///
/// The currently focused window is not ordered like the rest: background
/// raises land below it for free, and it is re-raised right after any of its
/// own app's siblings (they jump above it — see the sequencing comment below).
pub fn reassert_stack(desired: &[WindowId]) {
    const PRESENCE_TIMEOUT_MS: u64 = 600;
    const LANDING_TIMEOUT_MS: u64 = 400;
    const POLL_MS: u64 = 5;

    let focused = ax::focused_window();
    let want: Vec<WindowId> = desired
        .iter()
        .copied()
        .filter(|w| Some(*w) != focused)
        .collect();
    if want.len() < 2 {
        return;
    }

    let observed = |want: &[WindowId]| -> Vec<WindowId> {
        stack_front_to_back()
            .into_iter()
            .filter(|w| want.contains(w))
            .collect()
    };

    // Wait for un-hides to finish resurfacing every window we're about to
    // order; a window that pops back mid-pass would land wherever it left off.
    let mut waited = 0;
    while observed(&want).len() < want.len() && waited < PRESENCE_TIMEOUT_MS {
        std::thread::sleep(std::time::Duration::from_millis(POLL_MS));
        waited += POLL_MS;
    }
    // Anything still missing isn't coming (closed, or a stuck app): order
    // what's actually there.
    let present = observed(&want);
    let want: Vec<WindowId> = want.into_iter().filter(|w| present.contains(w)).collect();
    if want.len() < 2 {
        return;
    }

    // The focused APP's windows obey different raise physics than everyone
    // else's (probed in slps_sibling_probe.rs, refuting AeroSpace #395 on
    // Tahoe): a background app's window raises to just below the key window,
    // but a sibling of the key window raises ABOVE it, to global top.
    //
    // Both still amount to "insert on top of the processed region" if the
    // stack is built back-to-front and every sibling raise is immediately
    // followed by re-raising the focused window: the sibling briefly covers
    // it, the re-raise freezes the sibling in as the new top of the below-
    // focus region, and later background raises insert above it as usual.
    // That one extra same-app raise per sibling makes ANY interleaving of
    // apps in the desired order achievable — no focus changes, key status
    // never moves.
    let with_pids = stack_with_pids();
    let fpid = focused.and_then(|f| with_pids.iter().find(|(w, _)| *w == f.0).map(|(_, p)| *p));
    let is_sibling = |w: WindowId| {
        fpid.is_some()
            && with_pids
                .iter()
                .find(|(x, _)| *x == w.0)
                .map(|(_, p)| Some(*p) == fpid)
                .unwrap_or(false)
    };

    // Skip the already-correct bottom of the stack.
    let actual = observed(&want);
    let mut keep = 0;
    while keep < want.len()
        && keep < actual.len()
        && want[want.len() - 1 - keep] == actual[actual.len() - 1 - keep]
    {
        keep += 1;
    }

    let mut sequence: Vec<WindowId> = Vec::new();
    for i in (0..want.len() - keep).rev() {
        sequence.push(want[i]);
        if is_sibling(want[i]) {
            sequence.extend(focused);
        }
    }

    // One landing at a time. Landed is the raise's own observable, never a
    // vacuous ordering check (the first window's "suffix in order" is
    // trivially true, which once let its in-flight raise leapfrog two later
    // ones): a background raise lands at top-of-non-key, i.e. above every
    // other window we're ordering; a sibling (or the focused re-raise) lands
    // at the absolute top.
    ax::raise_sequenced(&sequence, |w| {
        let landed = |w: WindowId| {
            if Some(w) == focused || is_sibling(w) {
                stack_front_to_back().first() == Some(&w)
            } else {
                observed(&want).first() == Some(&w)
            }
        };
        let mut waited = 0;
        while !landed(w) && waited < LANDING_TIMEOUT_MS {
            std::thread::sleep(std::time::Duration::from_millis(POLL_MS));
            waited += POLL_MS;
        }
    });
}
