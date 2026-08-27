//! Probe: the two claims that decide how a reveal orders the FOCUSED app's
//! own windows (AeroSpace #395: "unfocused windows of the focused application
//! cannot be raised" via AXRaise; alt-tab source: SLPSSetFrontProcessWithOptions
//! with a wid raises just that window, WindowServer-side, no key change).
//!
//! Run from a terminal whose app has a second window. Steps:
//!   0. AXRaise the sibling — expected NO-OP (the AeroSpace limitation).
//!   1. SLPS-raise the sibling — expected: sibling above the focused window,
//!      key unchanged (typing still goes to the focused one).
//!   2. SLPS-raise the focused window — expected: original order restored.
//!
//!   cargo run --example slps_sibling_probe

use ordo::platform::{ax, zorder};
use ordo_skylight_sys as sys;

fn stack() -> Vec<(u32, i32)> {
    zorder::stack_with_pids()
}

fn pos(s: &[(u32, i32)], wid: u32) -> Option<usize> {
    s.iter().position(|(w, _)| *w == wid)
}

fn slps_raise(pid: i32, wid: u32) -> bool {
    let mut psn = sys::ProcessSerialNumber::default();
    unsafe {
        if sys::GetProcessForPID(pid, &mut psn) != 0 {
            return false;
        }
        sys::SLPSSetFrontProcessWithOptions(&psn, wid, sys::kCPSUserGenerated) == 0
    }
}

fn main() {
    let original_focus = ax::focused_window();

    // Pick any app with two on-screen windows and make it frontmost, so the
    // test exercises the focused-app case the AeroSpace limitation is about.
    let s0 = stack();
    let Some(&(_, fpid)) = s0
        .iter()
        .find(|(_, p)| s0.iter().filter(|(_, q)| q == p).count() >= 2)
    else {
        println!("no app with two on-screen windows — open one and rerun");
        return;
    };
    let pair: Vec<u32> = s0
        .iter()
        .filter(|(_, p)| *p == fpid)
        .map(|(w, _)| *w)
        .collect();
    let (focused, sibling) = (ordo_core::WindowId(pair[0]), pair[1]);
    println!(
        "adopting pid {fpid}: focusing {} over sibling {sibling}",
        focused.0
    );
    ax::focus(focused);
    std::thread::sleep(std::time::Duration::from_millis(300));
    let s0 = stack();
    println!(
        "start: focused at {:?}, sibling at {:?}",
        pos(&s0, focused.0),
        pos(&s0, sibling)
    );

    println!("\n[0] AXRaise on sibling (expected no-op per AeroSpace #395) …");
    ax::raise(ordo_core::WindowId(sibling));
    std::thread::sleep(std::time::Duration::from_millis(200));
    let s1 = stack();
    println!(
        "    sibling: {:?} -> {:?}",
        pos(&s0, sibling),
        pos(&s1, sibling)
    );

    println!("[1] SLPS-raise sibling (immediate read, then +100ms) …");
    let ok = slps_raise(fpid, sibling);
    let s2a = stack();
    std::thread::sleep(std::time::Duration::from_millis(100));
    let s2b = stack();
    println!(
        "    call ok: {ok}; sibling: {:?} -> {:?} (imm) -> {:?} (+100ms); focused now at {:?}",
        pos(&s1, sibling),
        pos(&s2a, sibling),
        pos(&s2b, sibling),
        pos(&s2b, focused.0)
    );

    println!("[2] SLPS-raise focused back on top …");
    let ok = slps_raise(fpid, focused.0);
    let s3a = stack();
    std::thread::sleep(std::time::Duration::from_millis(100));
    let s3b = stack();
    println!(
        "    call ok: {ok}; focused: {:?} (imm) -> {:?} (+100ms); sibling at {:?}",
        pos(&s3a, focused.0),
        pos(&s3b, focused.0),
        pos(&s3b, sibling)
    );

    let verdict =
        pos(&s3b, focused.0) < pos(&s3b, sibling) && pos(&s2b, sibling) < pos(&s1, sibling);
    println!(
        "\n{}",
        if verdict {
            "SLPS CAN ORDER THE FOCUSED APP'S SIBLINGS ✓"
        } else {
            "no dice"
        }
    );

    if let Some(w) = original_focus {
        ax::focus(w);
    }
}
