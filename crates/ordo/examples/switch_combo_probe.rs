//! On-device probe: full space-transition combo (dev tool, not shipped).
//!
//! `SLSManagedDisplaySetCurrentSpace` alone half-switches on Tahoe — the
//! record (and AX visibility) moves, the pixels don't. This probe adds the
//! compositor half, the way yabai's Dock payload does it:
//!   set current space  ->  SLSShowSpaces(targets)  ->  SLSHideSpaces(origins)
//! It flips every display to its 2nd space, holds ~1.5s so a human can confirm
//! the screen ACTUALLY changed, then runs the same combo back. The record is
//! verified by read-back either way; only eyes can verify the pixels.
//!
//! Run: cargo run --example switch_combo_probe

use std::ffi::c_void;
use std::thread::sleep;
use std::time::Duration;

use ordo::platform::skylight;
use ordo_skylight_sys as sys;

fn cf_string(s: &str) -> sys::CFStringRef {
    let c = std::ffi::CString::new(s).unwrap();
    unsafe {
        sys::CFStringCreateWithCString(std::ptr::null(), c.as_ptr(), sys::kCFStringEncodingUTF8)
    }
}

fn cf_space_array(spaces: &[sys::CgsSpaceId]) -> sys::CFArrayRef {
    let numbers: Vec<*const c_void> = spaces
        .iter()
        .map(|s| {
            let v = *s as i64;
            unsafe {
                sys::CFNumberCreate(
                    std::ptr::null(),
                    sys::kCFNumberSInt64Type,
                    &v as *const i64 as *const c_void,
                )
            }
        })
        .collect();
    let array = unsafe {
        sys::CFArrayCreate(
            std::ptr::null(),
            numbers.as_ptr(),
            numbers.len() as isize,
            &sys::kCFTypeArrayCallBacks as *const c_void,
        )
    };
    for n in &numbers {
        unsafe { sys::CFRelease(*n) };
    }
    array
}

/// The full transition for a set of (display identifier, from, to) moves.
fn combo_switch(cid: sys::CgsConnectionId, moves: &[(String, sys::CgsSpaceId, sys::CgsSpaceId)]) {
    for (ident, _, to) in moves {
        let s = cf_string(ident);
        unsafe {
            let _ = sys::SLSManagedDisplaySetCurrentSpace(cid, s, *to);
            sys::CFRelease(s);
        }
    }
    let show = cf_space_array(&moves.iter().map(|m| m.2).collect::<Vec<_>>());
    let hide = cf_space_array(&moves.iter().map(|m| m.1).collect::<Vec<_>>());
    unsafe {
        sys::SLSShowSpaces(cid, show);
        sys::SLSHideSpaces(cid, hide);
        sys::CFRelease(show);
        sys::CFRelease(hide);
    }
}

fn main() {
    let cid = skylight::connection();
    let displays = skylight::managed_display_spaces(cid);

    let moves: Vec<(String, sys::CgsSpaceId, sys::CgsSpaceId)> = displays
        .iter()
        .filter(|d| d.spaces.len() >= 2)
        .filter_map(|d| {
            let cur_idx = d.spaces.iter().position(|s| *s == d.current_space)?;
            let target_idx = if cur_idx == 0 { 1 } else { 0 };
            Some((d.identifier.clone(), d.current_space, d.spaces[target_idx]))
        })
        .collect();
    if moves.is_empty() {
        println!("no display with 2+ spaces; nothing to probe");
        return;
    }

    println!("switching {} display(s) via the full combo…", moves.len());
    combo_switch(cid, &moves);
    sleep(Duration::from_millis(1500));

    let after = skylight::managed_display_spaces(cid);
    for (ident, from, to) in &moves {
        let now = after
            .iter()
            .find(|d| &d.identifier == ident)
            .map(|d| d.current_space);
        println!(
            "display {}: record {} -> {:?} (wanted {}) | {}",
            ident,
            from,
            now,
            to,
            if now == Some(*to) {
                "record ok"
            } else {
                "record MISSED"
            }
        );
    }

    println!("switching back…");
    let back: Vec<_> = moves
        .iter()
        .map(|(i, from, to)| (i.clone(), *to, *from))
        .collect();
    combo_switch(cid, &back);
    sleep(Duration::from_millis(800));

    let end = skylight::managed_display_spaces(cid);
    let restored = moves.iter().all(|(ident, from, _)| {
        end.iter()
            .find(|d| &d.identifier == ident)
            .is_some_and(|d| d.current_space == *from)
    });
    println!(
        "restored: {}",
        if restored {
            "ok"
        } else {
            "NOT RESTORED — check Mission Control"
        }
    );
    println!("\n>>> HUMAN CHECK: did the screens visibly switch and come back?");
}
