//! Probe: does a bare `AXRaise` (no activation, no focus) move a background
//! app's window in the GLOBAL z-order when sent from a daemon? If yes,
//! "send window to back" is implementable as "raise everything else above it,
//! back-to-front" — the direct route (`SLSOrderWindow`) is refused from a
//! non-owner connection (error 1000, see order_probe).
//!
//! Picks the back-most layer-0 window, raises it, and diffs the z-order.
//!
//!   cargo run --example raise_probe            # target the back-most window
//!   cargo run --example raise_probe -- <wid>   # target a specific window id

use ordo::platform::{ax, cf};
use ordo_core::WindowId;
use ordo_skylight_sys as sys;

const ON_SCREEN_ONLY: u32 = 1 << 0;
const EXCLUDE_DESKTOP: u32 = 1 << 4;

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGWindowListCopyWindowInfo(option: u32, relative_to: u32) -> sys::CFArrayRef;
}

fn z_order() -> Vec<(u32, String)> {
    let mut out = Vec::new();
    unsafe {
        let arr = CGWindowListCopyWindowInfo(ON_SCREEN_ONLY | EXCLUDE_DESKTOP, 0);
        if arr.is_null() {
            return out;
        }
        for i in 0..cf::array_len(arr) {
            let d = cf::array_get(arr, i) as sys::CFDictionaryRef;
            let layer = cf::number_i64(cf::dict_get(d, "kCGWindowLayer")).unwrap_or(-1);
            if layer != 0 {
                continue;
            }
            let wid = cf::number_i64(cf::dict_get(d, "kCGWindowNumber")).unwrap_or(0) as u32;
            let owner = cf::string_value(cf::dict_get(d, "kCGWindowOwnerName")).unwrap_or_default();
            out.push((wid, owner));
        }
        sys::CFRelease(arr);
    }
    out
}

fn print_order(label: &str, order: &[(u32, String)]) {
    println!("{label} (front → back):");
    for (wid, owner) in order.iter().take(10) {
        println!("  {wid:>6}  {owner}");
    }
}

fn main() {
    let before = z_order();
    print_order("BEFORE", &before);

    let target = std::env::args()
        .nth(1)
        .and_then(|a| a.parse::<u32>().ok())
        .or_else(|| before.last().map(|(wid, _)| *wid));
    let Some(target) = target else {
        println!("no layer-0 window found to probe");
        return;
    };

    println!("\nAXRaise on {target} (back-most) …");
    let found = ax::raise(WindowId(target));
    println!("window found: {found}");

    std::thread::sleep(std::time::Duration::from_millis(400));
    let after = z_order();
    print_order("\nAFTER", &after);

    let pos = |o: &[(u32, String)]| o.iter().position(|(w, _)| *w == target);
    match (pos(&before), pos(&after)) {
        (Some(b), Some(a)) if a < b => println!("\nRAISED GLOBALLY ✓ ({b} → {a})"),
        (Some(b), Some(a)) => println!("\nno global movement ({b} → {a})"),
        _ => println!("\ntarget vanished — inconclusive"),
    }
}
