//! Probe: the exact reported failure — alt-backtick between two windows of
//! one app, workspace round trip, second window must sit directly under the
//! focused one, not buried beneath other apps.
//!
//! Focuses an app with two windows, BURIES the sibling under every other
//! window (simulating the post-reveal scramble), then calls reassert_stack
//! with the MRU-shaped desired order and verifies the exact result.
//!
//!   cargo run --example scenario_probe

use ordo::platform::{ax, zorder};
use ordo_core::WindowId;

fn stack() -> Vec<u32> {
    zorder::stack_front_to_back().iter().map(|w| w.0).collect()
}

fn main() {
    let original_focus = ax::focused_window();

    let with_pids = zorder::stack_with_pids();
    let Some(&(_, pid)) = with_pids
        .iter()
        .find(|(_, p)| with_pids.iter().filter(|(_, q)| q == p).count() >= 2)
    else {
        println!("need an app with two on-screen windows");
        return;
    };
    let pair: Vec<u32> = with_pids
        .iter()
        .filter(|(_, p)| *p == pid)
        .map(|(w, _)| *w)
        .collect();
    let (a, b) = (pair[0], pair[1]);
    println!("terminals: A={a} (will focus), B={b} (must end directly under A)");

    ax::focus(WindowId(a));
    std::thread::sleep(std::time::Duration::from_millis(300));

    // Bury B: raise every non-pair window above it (the post-reveal scramble).
    let burial: Vec<WindowId> = stack()
        .into_iter()
        .filter(|w| *w != a && *w != b)
        .rev()
        .map(WindowId)
        .collect();
    ax::raise_sequenced(&burial, |_| {
        std::thread::sleep(std::time::Duration::from_millis(30))
    });
    std::thread::sleep(std::time::Duration::from_millis(300));
    println!("buried:  {:?}", stack());

    // Desired = MRU shape: A, B, then the rest in their current order.
    let mut desired: Vec<WindowId> = vec![WindowId(a), WindowId(b)];
    desired.extend(
        stack()
            .into_iter()
            .filter(|w| *w != a && *w != b)
            .map(WindowId),
    );
    let want: Vec<u32> = desired.iter().map(|w| w.0).collect();

    let t = std::time::Instant::now();
    zorder::reassert_stack(&desired);
    let took = t.elapsed();
    std::thread::sleep(std::time::Duration::from_millis(150));
    let got = stack();
    println!("\nreassert took {took:?}\n  want {want:?}\n  got  {got:?}");
    let adjacent_ok = got == want;
    println!(
        "{}",
        if adjacent_ok {
            "ADJACENT: A on top, B directly beneath ✓"
        } else {
            "adjacent case wrong ✗"
        }
    );

    // Round two: an INTERLEAVED order — a background window between the two
    // same-app windows — reachable only via the raise-sibling-then-refocus-
    // window sequencing.
    let rest: Vec<u32> = stack().into_iter().filter(|w| *w != a && *w != b).collect();
    let mut desired: Vec<WindowId> = vec![WindowId(a), WindowId(rest[0]), WindowId(b)];
    desired.extend(rest[1..].iter().copied().map(WindowId));
    let want: Vec<u32> = desired.iter().map(|w| w.0).collect();

    let t = std::time::Instant::now();
    zorder::reassert_stack(&desired);
    let took = t.elapsed();
    std::thread::sleep(std::time::Duration::from_millis(150));
    let got = stack();
    println!("\ninterleaved reassert took {took:?}\n  want {want:?}\n  got  {got:?}");
    println!(
        "\n{}",
        if adjacent_ok && got == want {
            "BOTH SCENARIOS EXACT ✓"
        } else if got == want {
            "interleaved ✓ but adjacent ✗"
        } else {
            "interleaved wrong ✗"
        }
    );

    if let Some(w) = original_focus {
        ax::focus(w);
    }
}
