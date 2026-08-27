//! Probe: can SLPS event records impose z-order synchronously (WindowServer-
//! side), unlike AXRaise which each app processes on its own schedule?
//!
//! Three escalating questions, each answered by immediate CG read-back:
//!   1. Do the two 0xf8 "make key window" records ALONE raise a background
//!      app's window globally? (If yes: ordering with zero app-activation
//!      churn.)
//!   2. Does SLPSSetFrontProcessWithOptions + records raise it? (Known to
//!      work for focus; here we measure whether the reorder is immediate.)
//!   3. Can a full back-to-front pass impose an EXACT order across apps?
//!
//!   cargo run --example slps_order_probe

use ordo::platform::zorder;
use ordo_skylight_sys as sys;

fn stack() -> Vec<(u32, i32)> {
    zorder::stack_with_pids()
}

fn print_stack(label: &str, s: &[(u32, i32)]) {
    println!("{label}:");
    for (wid, pid) in s.iter().take(10) {
        println!("  {wid:>6}  pid {pid}");
    }
}

fn make_key(psn: &sys::ProcessSerialNumber, wid: u32) {
    let mut bytes = [0u8; 0xf8];
    bytes[0x04] = 0xf8;
    bytes[0x3a] = 0x10;
    bytes[0x3c..0x40].copy_from_slice(&wid.to_le_bytes());
    for b in &mut bytes[0x20..0x30] {
        *b = 0xff;
    }
    unsafe {
        bytes[0x08] = 0x01;
        let _ = sys::SLPSPostEventRecordTo(psn, bytes.as_ptr());
        bytes[0x08] = 0x02;
        let _ = sys::SLPSPostEventRecordTo(psn, bytes.as_ptr());
    }
}

fn psn_of(pid: i32) -> Option<sys::ProcessSerialNumber> {
    let mut psn = sys::ProcessSerialNumber::default();
    (unsafe { sys::GetProcessForPID(pid, &mut psn) } == 0).then_some(psn)
}

fn pos(s: &[(u32, i32)], wid: u32) -> Option<usize> {
    s.iter().position(|(w, _)| *w == wid)
}

fn main() {
    let before = stack();
    print_stack("BEFORE", &before);
    let Some(&(victim, vpid)) = before.last() else {
        println!("nothing to probe");
        return;
    };

    // 1: records alone on the back-most window.
    println!("\n[1] make-key records alone on {victim} …");
    if let Some(psn) = psn_of(vpid) {
        make_key(&psn, victim);
    }
    let after1 = stack();
    println!(
        "    position {:?} -> {:?} (immediate read)",
        pos(&before, victim),
        pos(&after1, victim)
    );

    // 2: front-process + records.
    println!("[2] SLPSSetFrontProcessWithOptions + records on {victim} …");
    if let Some(psn) = psn_of(vpid) {
        unsafe {
            let _ = sys::SLPSSetFrontProcessWithOptions(&psn, victim, sys::kCPSUserGenerated);
        }
        make_key(&psn, victim);
    }
    let after2 = stack();
    println!(
        "    position {:?} -> {:?} (immediate read)",
        pos(&after1, victim),
        pos(&after2, victim)
    );

    // 3: impose the exact REVERSE of the current order, back-to-front.
    let current = stack();
    let desired: Vec<(u32, i32)> = current.iter().rev().copied().collect();
    println!("\n[3] imposing exact reverse order via [2]'s mechanism …");
    for (wid, pid) in desired.iter().rev() {
        // back-to-front over `desired`
        if let Some(psn) = psn_of(*pid) {
            unsafe {
                let _ = sys::SLPSSetFrontProcessWithOptions(&psn, *wid, sys::kCPSUserGenerated);
            }
            make_key(&psn, *wid);
        }
    }
    let after3 = stack();
    let desired_ids: Vec<u32> = desired.iter().map(|(w, _)| *w).collect();
    let actual_ids: Vec<u32> = after3
        .iter()
        .map(|(w, _)| *w)
        .filter(|w| desired_ids.contains(w))
        .collect();
    print_stack("AFTER exact-order pass", &after3);
    if actual_ids == desired_ids {
        println!("\nEXACT ORDER IMPOSED SYNCHRONOUSLY ✓");
    } else {
        println!("\nmismatch:\n  want {desired_ids:?}\n  got  {actual_ids:?}");
    }
}
