//! Probe: does writing the app element's `AXHidden` attribute hide AND unhide
//! an app from a daemon, both directions verified? (The AppKit route —
//! `NSRunningApplication.hide()/unhide()` guarded by `isHidden` — shipped a
//! one-way trap: `isHidden` is a KVO cache that never refreshes without a run
//! loop, so every unhide was skipped as "already visible".)
//!
//! Verifies via the CG on-screen list: hidden apps' windows leave it.
//!
//!   cargo run --example hide_probe -- <pid>

use ordo::platform::{ax, zorder};
use ordo_core::Pid;

fn on_screen_count(pid: i32) -> usize {
    // zorder's stack is on-screen layer-0 windows; cross-check by AX pid map.
    let by_ax: std::collections::HashMap<_, _> =
        ax::windows().into_iter().map(|w| (w.id, w.app.0)).collect();
    zorder::stack_front_to_back()
        .iter()
        .filter(|w| by_ax.get(w) == Some(&pid))
        .count()
}

fn main() {
    let pid: i32 = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .expect("usage: hide_probe <pid>");

    println!("on-screen windows of {pid}: {}", on_screen_count(pid));

    println!("AXHidden := true …");
    ax::set_app_hidden(Pid(pid), true);
    std::thread::sleep(std::time::Duration::from_millis(500));
    let hidden_count = on_screen_count(pid);
    println!("on-screen windows now: {hidden_count}");

    println!("AXHidden := false …");
    ax::set_app_hidden(Pid(pid), false);
    std::thread::sleep(std::time::Duration::from_millis(500));
    let restored_count = on_screen_count(pid);
    println!("on-screen windows now: {restored_count}");

    if hidden_count == 0 && restored_count > 0 {
        println!("HIDE + UNHIDE BOTH WORK ✓");
    } else {
        println!("asymmetry: hide->{hidden_count}, unhide->{restored_count}");
    }
}
