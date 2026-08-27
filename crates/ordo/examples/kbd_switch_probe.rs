//! On-device probe: Space switching via synthesized Mission Control shortcuts
//! (dev tool, not shipped). Requires "Move left/right a space" enabled in
//! System Settings -> Keyboard -> Keyboard Shortcuts -> Mission Control.
//!
//! Questions this answers:
//!   1. Does a synthesized Ctrl+Right actually move a Space (pixels included —
//!      this path IS Mission Control, so the record can be trusted)?
//!   2. Which display does it act on?
//!   3. Can we steer the target display by SLPS-focusing a window there first?
//!
//! Run: cargo run --example kbd_switch_probe

use std::thread::sleep;
use std::time::Duration;

use objc2_core_graphics::{
    CGEvent, CGEventFlags, CGEventSource, CGEventSourceStateID, CGEventTapLocation,
};
use ordo::platform::{display, skylight};

/// Mission Control's slide animation needs to finish before the topology
/// read-back is meaningful.
const SETTLE: Duration = Duration::from_millis(900);

fn press_ctrl_arrow(keycode: u16) {
    // A HID-state event source: symbolic hotkeys ignore source-less events.
    let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState);
    // Ctrl+Alt+Cmd plus the secondary-fn bit arrows carry — matching the
    // stored hotkey parameters (mask 10223616) exactly.
    let flags = CGEventFlags::MaskControl
        | CGEventFlags::MaskAlternate
        | CGEventFlags::MaskCommand
        | CGEventFlags::MaskSecondaryFn;
    for down in [true, false] {
        let Some(ev) = CGEvent::new_keyboard_event(source.as_deref(), keycode, down) else {
            continue;
        };
        CGEvent::set_flags(Some(&ev), flags);
        CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&ev));
        sleep(Duration::from_millis(40));
    }
}

fn report_moves(cid: ordo_skylight_sys::CgsConnectionId, label: &str, before: &[(String, u64)]) {
    let after = skylight::managed_display_spaces(cid);
    for (ident, was) in before {
        let now = after
            .iter()
            .find(|d| &d.identifier == ident)
            .map(|d| d.current_space);
        println!(
            "  [{label}] display {}…: {} -> {:?} | {}",
            &ident[..8.min(ident.len())],
            was,
            now,
            if now == Some(*was) {
                "unchanged"
            } else {
                "MOVED ✓"
            }
        );
    }
}

fn currents(cid: ordo_skylight_sys::CgsConnectionId) -> Vec<(String, u64)> {
    skylight::managed_display_spaces(cid)
        .into_iter()
        .map(|d| (d.identifier, d.current_space))
        .collect()
}

fn main() {
    let cid = skylight::connection();

    // Q1+Q2: plain synthesized Ctrl+Right — does anything move, and where?
    let before = currents(cid);
    println!("phase 1: synthesized Ctrl+Right, no steering");
    press_ctrl_arrow(0x7C);
    sleep(SETTLE);
    report_moves(cid, "phase 1", &before);
    press_ctrl_arrow(0x7B);
    sleep(SETTLE);

    // Q3: steer by MOUSE POINTER — warp to the non-main display, then switch.
    let displays = display::active_displays();
    let saved_mouse = ordo::platform::mouse::position();
    match displays.iter().find(|d| !d.is_main) {
        Some(target_display) => {
            println!(
                "phase 2: steering — warp pointer to display {:?} center, then Ctrl+Alt+Cmd+Right",
                target_display.id
            );
            ordo::platform::mouse::warp_to(target_display.frame.center());
            sleep(Duration::from_millis(300));
            let before = currents(cid);
            press_ctrl_arrow(0x7C);
            sleep(SETTLE);
            report_moves(cid, "phase 2", &before);
            press_ctrl_arrow(0x7B);
            sleep(SETTLE);
            if let Some(p) = saved_mouse {
                ordo::platform::mouse::warp_to(p);
            }
        }
        None => println!("phase 2 skipped: single display"),
    }

    let end = currents(cid);
    println!("final state: {:?}", end);
    println!("\n>>> HUMAN CHECK: did you SEE spaces slide this time?");
}
