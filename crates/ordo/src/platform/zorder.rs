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

use std::time::Instant;

use ordo_core::WindowId;
use ordo_skylight_sys as sys;

use crate::ports::{RaiseKind, RaiseStat, RestackStats};

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
/// `desired[0]` is the DESIGNATED top — the window the core wants focused —
/// not whatever AX says is focused right now. Asking AX mid-reveal races the
/// focus effect's own landing (and the outgoing app may already be hidden),
/// which returned the old window or nothing; that put the real top window
/// into the ordering set, made every "first among want" gate unsatisfiable
/// (backgrounds can't rise above the key window), burned all the timeouts,
/// and let the unsequenced raises land coin-flip — the lived "scrambled,
/// then the top window ping-pongs each round trip" bug. Intent is the
/// authority; the actual key window, whichever it transiently is, sits in no
/// gate's scope and settles on top by itself.
pub fn reassert_stack(desired: &[WindowId]) -> Option<RestackStats> {
    const PRESENCE_TIMEOUT_MS: u64 = 600;
    // Generous on purpose: a landed gate exits in single-digit ms, so this is
    // paid only by an app genuinely slower than it — and a timeout means an
    // unaccounted-for raise is in flight, which is exactly what the second
    // pass exists to absorb. (Chrome has been measured past 400ms.)
    const LANDING_TIMEOUT_MS: u64 = 1000;
    const POLL_MS: u64 = 5;

    let t_total = Instant::now();
    let (&top, rest) = desired.split_first()?;
    if rest.is_empty() {
        return None;
    }

    let observed = |scope: &[WindowId]| -> Vec<WindowId> {
        stack_front_to_back()
            .into_iter()
            .filter(|w| scope.contains(w))
            .collect()
    };

    // Wait for un-hides to finish resurfacing every window we're about to
    // order; a window that pops back mid-pass would land wherever it left off.
    let t_presence = Instant::now();
    let mut waited = 0;
    while observed(desired).len() < desired.len() && waited < PRESENCE_TIMEOUT_MS {
        std::thread::sleep(std::time::Duration::from_millis(POLL_MS));
        waited += POLL_MS;
    }
    let presence_wait_ms = t_presence.elapsed().as_millis() as u64;
    // Anything still missing isn't coming (closed, or a stuck app): order
    // what's actually there.
    let present = observed(desired);
    let missing = (desired.len() - present.len()) as u32;
    let mut want: Vec<WindowId> = rest
        .iter()
        .copied()
        .filter(|w| present.contains(w))
        .collect();
    if want.is_empty() {
        return None;
    }

    // While a window we must order HOLDS key status, nothing can be raised
    // above it — so wait out the in-flight focus handoff (the effect always
    // precedes the restack) until the key window is out of `want`. This is
    // AX-as-wait-condition, not AX-as-authority: the condition is monotone
    // (once the handoff lands it stays landed), unlike the racing "who's
    // focused" read this function used to key its physics on. Alt+End is the
    // flow that needs it: the demoted window stays key until the handoff.
    let t_handoff = Instant::now();
    let mut waited = 0;
    let mut key_in_want = None;
    while waited < LANDING_TIMEOUT_MS {
        match ax::focused_window() {
            Some(f) if want.contains(&f) => {
                key_in_want = Some(f);
                std::thread::sleep(std::time::Duration::from_millis(POLL_MS));
                waited += POLL_MS;
            }
            _ => {
                key_in_want = None;
                break;
            }
        }
    }
    let handoff_wait_ms = t_handoff.elapsed().as_millis() as u64;
    // Still key after the wait — the handoff isn't coming (an external focus
    // grab, or a caller whose desired[0] isn't the window it focused).
    // Physics wins: nothing can be raised above the key window, so exempt it
    // and order the rest instead of burning every gate's timeout against an
    // unsatisfiable condition (once a 13-second engine freeze per switch).
    if let Some(f) = key_in_want {
        want.retain(|w| *w != f);
        if want.is_empty() {
            return None;
        }
    }
    let scope: Vec<WindowId> = std::iter::once(top).chain(want.iter().copied()).collect();

    // Two passes: the first does the work; the second exists solely to absorb
    // a ghost — a raise that outlived its landing timeout and touched down
    // after the pass finished, breaking the order behind our back. The settle
    // sleep gives such a straggler time to land where the re-pass can see it;
    // the suffix-skip keeps the re-pass to just the windows it displaced.
    let mut raises = Vec::new();
    let mut skipped_suffix = 0;
    let mut second_pass = false;
    for pass in 0..2u8 {
        if pass > 0 {
            std::thread::sleep(std::time::Duration::from_millis(150));
        }
        if observed(&scope) == scope {
            break; // exact (also the common repeated-switch case: zero raises)
        }
        if pass > 0 {
            second_pass = true;
        }
        let keep = raise_pass(top, &want, &scope, &observed, pass, &mut raises);
        if pass == 0 {
            skipped_suffix = keep as u32;
        }
    }

    Some(RestackStats {
        total_ms: t_total.elapsed().as_millis() as u64,
        presence_wait_ms,
        handoff_wait_ms,
        desired: desired.len() as u32,
        missing,
        skipped_suffix,
        second_pass,
        converged: observed(&scope) == scope,
        raises,
    })
}

/// One sequenced, landing-gated raise pass. See [`reassert_stack`] for why
/// this shape is the only deterministic one available. Returns the length of
/// the already-correct suffix it skipped, and appends one [`RaiseStat`] per
/// issued raise.
fn raise_pass(
    top: WindowId,
    want: &[WindowId],
    scope: &[WindowId],
    observed: &dyn Fn(&[WindowId]) -> Vec<WindowId>,
    pass: u8,
    raises: &mut Vec<RaiseStat>,
) -> usize {
    const LANDING_TIMEOUT_MS: u64 = 1000;
    const POLL_MS: u64 = 5;

    // The designated top's OWN app's windows obey different raise physics
    // than everyone else's (probed in slps_sibling_probe.rs, refuting
    // AeroSpace #395 on Tahoe): a background app's window raises to just
    // below the key window, but a sibling of the key window raises ABOVE it.
    //
    // Both still amount to "insert on top of the processed region" if the
    // stack is built back-to-front and every sibling raise is immediately
    // followed by re-raising the designated top: the sibling briefly covers
    // it, the re-raise freezes the sibling in as the new top of the below-
    // top region, and later background raises insert above it as usual. That
    // one extra same-app raise per sibling makes ANY interleaving of apps in
    // the desired order achievable — no focus changes, key status never
    // moves. A final unconditional raise of the designated top makes the
    // result independent of when its focus effect happens to land.
    let with_pids = stack_with_pids();
    let fpid = with_pids.iter().find(|(w, _)| *w == top.0).map(|(_, p)| *p);
    let is_sibling = |w: WindowId| {
        fpid.is_some()
            && with_pids
                .iter()
                .find(|(x, _)| *x == w.0)
                .map(|(_, p)| Some(*p) == fpid)
                .unwrap_or(false)
    };

    // Skip the already-correct bottom of the stack.
    let actual = observed(want);
    let mut keep = 0;
    while keep < want.len()
        && keep < actual.len()
        && want[want.len() - 1 - keep] == actual[actual.len() - 1 - keep]
    {
        keep += 1;
    }

    let mut sequence: Vec<(WindowId, RaiseKind)> = Vec::new();
    for i in (0..want.len() - keep).rev() {
        let kind = if is_sibling(want[i]) {
            RaiseKind::Sibling
        } else {
            RaiseKind::Background
        };
        sequence.push((want[i], kind));
        if kind == RaiseKind::Sibling {
            sequence.push((top, RaiseKind::Top));
        }
    }
    if sequence.last().map(|&(w, _)| w) != Some(top) {
        sequence.push((top, RaiseKind::Top));
    }

    // Per-raise stats use this pass-start read (same one the sibling
    // classification is from): "how buried was it", not exact hop counts.
    let pid_of = |w: WindowId| {
        with_pids
            .iter()
            .find(|(x, _)| *x == w.0)
            .map(|&(_, p)| p)
            .unwrap_or(-1)
    };
    let above = |w: WindowId| -> (u32, u32) {
        let mut in_scope = 0;
        for (i, (x, _)) in with_pids.iter().enumerate() {
            if *x == w.0 {
                return (in_scope, i as u32);
            }
            if scope.iter().any(|s| s.0 == *x) {
                in_scope += 1;
            }
        }
        (in_scope, with_pids.len() as u32)
    };

    // One landing at a time. Landed is the raise's own observable, never a
    // vacuous ordering check (the first window's "suffix in order" is
    // trivially true, which once let its in-flight raise leapfrog two later
    // ones), and always RELATIVE to the windows being ordered — never "the
    // absolute top", which the transient key window owns: a landed raise
    // reads back above every other window in its scope. The designated top's
    // scope includes everything; the others' excludes the designated top,
    // which legitimately sits above them.
    let ids: Vec<WindowId> = sequence.iter().map(|&(w, _)| w).collect();
    let mut step = 0;
    ax::raise_sequenced(&ids, |w| {
        let kind = sequence[step].1;
        step += 1;
        let landed = |w: WindowId| {
            if w == top {
                observed(scope).first() == Some(&w)
            } else {
                observed(want).first() == Some(&w)
            }
        };
        let t = Instant::now();
        let mut waited = 0;
        while !landed(w) && waited < LANDING_TIMEOUT_MS {
            std::thread::sleep(std::time::Duration::from_millis(POLL_MS));
            waited += POLL_MS;
        }
        let (above_scope, above_all) = above(w);
        raises.push(RaiseStat {
            window: w,
            pid: pid_of(w),
            kind,
            pass,
            above_scope,
            above_all,
            wait_ms: t.elapsed().as_millis() as u64,
            timed_out: waited >= LANDING_TIMEOUT_MS,
        });
    });
    keep
}
