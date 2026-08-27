//! On-device probe for Space-switching mechanisms (dev tool, not shipped).
//!
//! The dock-swipe gesture synthesis is ignored on Tahoe, so this tries the
//! candidates in preference order and reports what actually moves the world:
//!   A. SLSManagedDisplaySetCurrentSpace — direct, per-display, no animation.
//!   B. Synthesized Ctrl+Right/Left key events — Mission Control's own shortcut.
//! Every attempt is verified by re-reading SLSCopyManagedDisplaySpaces, and the
//! probe switches everything back to where it started.
//!
//! Run: cargo run --example switch_probe

use std::thread::sleep;
use std::time::Duration;

use objc2_core_graphics::{CGEvent, CGEventFlags, CGEventTapLocation};
use ordo::platform::skylight;
use ordo_skylight_sys as sys;

const SETTLE: Duration = Duration::from_millis(600);

fn cf_string(s: &str) -> sys::CFStringRef {
    let c = std::ffi::CString::new(s).unwrap();
    unsafe {
        sys::CFStringCreateWithCString(std::ptr::null(), c.as_ptr(), sys::kCFStringEncodingUTF8)
    }
}

fn current_of(cid: sys::CgsConnectionId, identifier: &str) -> Option<sys::CgsSpaceId> {
    skylight::managed_display_spaces(cid)
        .into_iter()
        .find(|d| d.identifier == identifier)
        .map(|d| d.current_space)
}

fn set_space(cid: sys::CgsConnectionId, identifier: &str, space: sys::CgsSpaceId) -> i32 {
    let ident = cf_string(identifier);
    let err = unsafe { sys::SLSManagedDisplaySetCurrentSpace(cid, ident, space) };
    unsafe { sys::CFRelease(ident) };
    err
}

fn press_ctrl_arrow(keycode: u16) {
    for down in [true, false] {
        let Some(ev) = CGEvent::new_keyboard_event(None, keycode, down) else {
            continue;
        };
        CGEvent::set_flags(Some(&ev), CGEventFlags::MaskControl);
        CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&ev));
        sleep(Duration::from_millis(30));
    }
}

fn main() {
    let cid = skylight::connection();
    let displays = skylight::managed_display_spaces(cid);
    println!("=== topology ===");
    for d in &displays {
        let idx = d.spaces.iter().position(|s| *s == d.current_space);
        println!(
            "display {} — {} spaces, current {:?} (index {:?})",
            d.identifier,
            d.spaces.len(),
            d.current_space,
            idx
        );
    }

    println!("\n=== mechanism A: SLSManagedDisplaySetCurrentSpace ===");
    let mut a_worked = true;
    for d in &displays {
        let Some(cur_idx) = d.spaces.iter().position(|s| *s == d.current_space) else {
            println!(
                "display {}: current space not in list, skipping",
                d.identifier
            );
            continue;
        };
        if d.spaces.len() < 2 {
            println!("display {}: only one space, skipping", d.identifier);
            continue;
        }
        let target_idx = if cur_idx == 0 { 1 } else { cur_idx - 1 };
        let target = d.spaces[target_idx];
        let origin = d.current_space;

        let err = set_space(cid, &d.identifier, target);
        sleep(SETTLE);
        let now = current_of(cid, &d.identifier);
        let moved = now == Some(target);
        println!(
            "display {}: {} -> {} | CGError {} | observed {:?} | {}",
            d.identifier,
            origin,
            target,
            err,
            now,
            if moved {
                "MOVED ✓"
            } else {
                "did not move ✗"
            }
        );
        if moved {
            // Put it back.
            let _ = set_space(cid, &d.identifier, origin);
            sleep(SETTLE);
            let back = current_of(cid, &d.identifier) == Some(origin);
            println!(
                "display {}: restored -> {}",
                d.identifier,
                if back { "ok" } else { "FAILED TO RESTORE" }
            );
        } else {
            a_worked = false;
        }
    }

    if a_worked && !displays.is_empty() {
        println!("\nverdict: mechanism A works — use SLSManagedDisplaySetCurrentSpace.");
        return;
    }

    println!("\n=== mechanism B: synthesized Ctrl+Right / Ctrl+Left ===");
    let before = skylight::managed_display_spaces(cid);
    press_ctrl_arrow(0x7C); // right arrow
    sleep(SETTLE);
    let after = skylight::managed_display_spaces(cid);
    let mut moved_any = false;
    for (b, a) in before.iter().zip(after.iter()) {
        let m = b.current_space != a.current_space;
        moved_any |= m;
        println!(
            "display {}: {} -> {} | {}",
            b.identifier,
            b.current_space,
            a.current_space,
            if m { "MOVED ✓" } else { "unchanged" }
        );
    }
    if moved_any {
        press_ctrl_arrow(0x7B); // left arrow: back to where we were
        sleep(SETTLE);
        println!("restored with Ctrl+Left.");
        println!("\nverdict: mechanism B works — synthesize Mission Control's shortcut.");
    } else {
        println!("\nverdict: neither mechanism moved a space. More digging needed.");
    }
}
