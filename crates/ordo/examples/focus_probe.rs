//! On-device probe for cross-app focus (dev tool, not shipped).
//!
//! Finds the currently focused window and the most recently scanned window of a
//! DIFFERENT app, focuses that one via the shipped `ax::focus` (the SLPS
//! path), verifies focus actually moved by re-scanning, then focuses back.
//!
//! Run: cargo run --example focus_probe

use std::thread::sleep;
use std::time::Duration;

use ordo::platform::ax;

fn main() {
    let scan = ax::scan();
    let Some(origin) = scan.focused else {
        println!("nothing focused; aborting");
        return;
    };
    let origin_app = scan.windows.iter().find(|w| w.id == origin).map(|w| w.app);
    let Some(target) = scan
        .windows
        .iter()
        .find(|w| Some(w.app) != origin_app)
        .map(|w| w.id)
    else {
        println!("no window of another app to test with; aborting");
        return;
    };

    println!("focused now: {:?}; cross-app target: {:?}", origin, target);
    ax::focus(target);
    sleep(Duration::from_millis(500));
    let mid = ax::scan().focused;
    println!(
        "after focus({:?}): focused = {:?} | {}",
        target,
        mid,
        if mid == Some(target) {
            "CROSS-APP FOCUS WORKS ✓"
        } else {
            "focus did not move ✗"
        }
    );

    ax::focus(origin);
    sleep(Duration::from_millis(500));
    let end = ax::scan().focused;
    println!(
        "restored: focused = {:?} | {}",
        end,
        if end == Some(origin) {
            "ok"
        } else {
            "NOT RESTORED"
        }
    );
}
