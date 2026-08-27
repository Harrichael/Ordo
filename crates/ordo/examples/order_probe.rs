//! Probe: can a daemon connection lower another app's window with
//! `SLSOrderWindow`? WindowServer historically only honors ordering from the
//! window's own connection (yabai needs its Dock payload for this), but Tahoe
//! has surprised us in both directions — so measure, don't assume.
//!
//! Self-verifying: reads the layer-0 z-order via `CGWindowListCopyWindowInfo`
//! (which returns windows front-to-back), sends the frontmost normal window to
//! the back, and reads the order again.
//!
//!   cargo run --example order_probe            # target the frontmost window
//!   cargo run --example order_probe -- <wid>   # target a specific window id

use ordo::platform::cf;
use ordo_skylight_sys as sys;

const ON_SCREEN_ONLY: u32 = 1 << 0;
const EXCLUDE_DESKTOP: u32 = 1 << 4;

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGWindowListCopyWindowInfo(option: u32, relative_to: u32) -> sys::CFArrayRef;
}

/// (window id, owner, layer) for every on-screen window, front-to-back.
fn z_order() -> Vec<(u32, String, i64)> {
    let mut out = Vec::new();
    unsafe {
        let arr = CGWindowListCopyWindowInfo(ON_SCREEN_ONLY | EXCLUDE_DESKTOP, 0);
        if arr.is_null() {
            return out;
        }
        for i in 0..cf::array_len(arr) {
            let d = cf::array_get(arr, i) as sys::CFDictionaryRef;
            let wid = cf::number_i64(cf::dict_get(d, "kCGWindowNumber")).unwrap_or(0) as u32;
            let layer = cf::number_i64(cf::dict_get(d, "kCGWindowLayer")).unwrap_or(-1);
            let owner = cf::string_value(cf::dict_get(d, "kCGWindowOwnerName")).unwrap_or_default();
            out.push((wid, owner, layer));
        }
        sys::CFRelease(arr);
    }
    out
}

fn print_normals(label: &str, order: &[(u32, String, i64)]) {
    println!("{label} (front → back, layer 0 only):");
    for (wid, owner, layer) in order.iter().filter(|(_, _, l)| *l == 0).take(8) {
        println!("  {wid:>6}  {owner}  (layer {layer})");
    }
}

fn main() {
    let before = z_order();
    print_normals("BEFORE", &before);

    let target = std::env::args()
        .nth(1)
        .and_then(|a| a.parse::<u32>().ok())
        .or_else(|| {
            before
                .iter()
                .find(|(_, _, l)| *l == 0)
                .map(|(wid, _, _)| *wid)
        });
    let Some(target) = target else {
        println!("no layer-0 window found to probe");
        return;
    };

    let cid = unsafe { sys::SLSMainConnectionID() };
    let err = unsafe { sys::SLSOrderWindow(cid, target, -1, 0) };
    println!("\nSLSOrderWindow(cid, {target}, below, 0) -> {err}");

    std::thread::sleep(std::time::Duration::from_millis(300));
    let after = z_order();
    print_normals("\nAFTER", &after);

    let pos = |o: &[(u32, String, i64)]| {
        o.iter()
            .filter(|(_, _, l)| *l == 0)
            .position(|(w, _, _)| *w == target)
    };
    match (pos(&before), pos(&after)) {
        (Some(b), Some(a)) if a > b => println!("\nMOVED BACK ✓ ({b} → {a})"),
        (Some(b), Some(a)) => println!("\nno movement ({b} → {a}) — call is a no-op from here"),
        _ => println!("\ntarget vanished from the list — inconclusive"),
    }
}
