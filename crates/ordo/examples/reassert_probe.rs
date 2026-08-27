//! Probe: does zorder::reassert_stack impose an EXACT cross-app order on the
//! live desktop — including slow-AX apps like Chrome — now that each raise is
//! confirmed landed before the next is issued?
//!
//! Scrambles the current layer-0 stack to its reverse, verifies, then puts
//! the original order back, verifying again. Desktop ends as it started.
//!
//!   cargo run --example reassert_probe

use ordo::platform::{ax, zorder};
use ordo_core::WindowId;

fn read() -> Vec<WindowId> {
    zorder::stack_front_to_back()
}

fn check(label: &str, desired: &[WindowId]) -> bool {
    // The focused window is exempt from ordering (it's key, always on top),
    // so judge only the order of everything else.
    let focused = ax::focused_window();
    let want: Vec<WindowId> = desired
        .iter()
        .copied()
        .filter(|w| Some(*w) != focused)
        .collect();
    let actual: Vec<WindowId> = read().into_iter().filter(|w| want.contains(w)).collect();
    let ok = actual == want;
    println!(
        "{label}: {}\n  want {:?}\n  got  {:?}",
        if ok { "EXACT ✓" } else { "MISMATCH ✗" },
        want.iter().map(|w| w.0).collect::<Vec<_>>(),
        actual.iter().map(|w| w.0).collect::<Vec<_>>(),
    );
    ok
}

fn main() {
    let original = read();
    if original.len() < 3 {
        println!("need at least 3 windows to make this interesting");
        return;
    }
    println!(
        "original (front→back): {:?}",
        original.iter().map(|w| w.0).collect::<Vec<_>>()
    );

    let mut reversed = original.clone();
    reversed.reverse();

    // Mirror the daemon's contract: desired[0] is the designated top and the
    // core always focuses it before restacking (a key window inside the rest
    // of the order would be physically impossible to stack under).
    ax::focus(reversed[0]);
    let t = std::time::Instant::now();
    zorder::reassert_stack(&reversed);
    println!("\nreverse pass took {:?}", t.elapsed());
    let ok1 = check("reversed", &reversed);

    ax::focus(original[0]);
    let t = std::time::Instant::now();
    zorder::reassert_stack(&original);
    println!("\nrestore pass took {:?}", t.elapsed());
    let ok2 = check("restored", &original);

    println!(
        "\n{}",
        if ok1 && ok2 {
            "SEQUENCED REASSERT WORKS ✓"
        } else {
            "still racy ✗"
        }
    );
}
