//! Probe: do AppKit calls on a plain spawned thread pile up in an autorelease
//! pool that never drains? Calls `runningApplications` N times on a std
//! thread, reading the same per-app fields the scan reads, with or without an
//! autorelease pool around each call. The thread then stays alive instead of
//! exiting (a thread's exit drains its pool) while `heap` counts what is
//! still alive.
//!
//! Measured (Tahoe 26.6): NSWorkspace hands out the same NSRunningApplication
//! objects while they are alive, and a whole fresh set once any app's record
//! changed (activation, hide, policy); the daemon sees such a change every
//! few seconds, from the user and from its own hides and raises. Without a
//! pool every superseded set is kept forever. `app` runs NSApplication on the
//! main thread as the daemon does (the change notifications need it);
//! `toggle` changes this process's own activation policy every pass, so each
//! pass gets a fresh set; `ax` adds one AXWindows read per regular app per
//! pass, as the scan does.
//!
//!   cargo run --release --example pool_probe -- nopool 12 60 2000 app toggle &
//!   heap <pid> | grep -E 'NSRunningApplication|autoreleasepool'
//!   cargo run --release --example pool_probe -- pool 12 60 2000 app toggle &
//!   heap <pid> | grep -E 'NSRunningApplication|autoreleasepool'

use std::time::Duration;

use std::ptr::NonNull;

use objc2::rc::autoreleasepool;
use objc2_app_kit::{NSApplicationActivationPolicy, NSWorkspace};
use objc2_application_services::{AXError, AXUIElement};
use objc2_core_foundation::{CFString, CFType};
use ordo_skylight_sys as sys;

/// One scan's worth of AX traffic to an app: its window list, as the walk
/// reads it.
fn ax_windows(pid: i32) {
    let el = unsafe { AXUIElement::new_application(pid) };
    unsafe { el.set_messaging_timeout(0.5) };
    let attr = CFString::from_str("AXWindows");
    let mut out: *const CFType = std::ptr::null();
    let err = unsafe { el.copy_attribute_value(&attr, NonNull::from(&mut out)) };
    if err == AXError::Success && !out.is_null() {
        unsafe { sys::CFRelease(out as *const _) };
    }
}

fn one_pass(ax: bool, front: bool) -> usize {
    if front {
        // The restack worker's read: a sweep of the same app list.
        return usize::from(ordo::platform::ax::frontmost_app().is_some());
    }
    let pids: Vec<i32> = NSWorkspace::sharedWorkspace()
        .runningApplications()
        .iter()
        .filter(|a| a.activationPolicy() == NSApplicationActivationPolicy::Regular)
        .filter(|a| a.bundleIdentifier().is_some())
        .map(|a| a.processIdentifier())
        .filter(|pid| *pid > 0)
        .collect();
    if ax {
        for pid in &pids {
            ax_windows(*pid);
        }
    }
    pids.len()
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mode = args.first().map_or("nopool", |s| s.as_str());
    let n: usize = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(100);
    let hold: u64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(60);
    let interval_ms: u64 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(0);
    let app = args.iter().any(|a| a == "app");
    let ax = args.iter().any(|a| a == "ax");
    let toggle = args.iter().any(|a| a == "toggle");
    let front = args.iter().any(|a| a == "front");
    let pooled = match mode {
        "pool" => true,
        "nopool" => false,
        _ => panic!(
            "usage: pool_probe <pool|nopool> [passes] [hold-seconds] [interval-ms] [app] [ax] [toggle] [front]"
        ),
    };
    println!("pid {}", std::process::id());

    std::thread::spawn(move || {
        let mut regular = 0;
        for k in 0..n {
            regular = if pooled {
                autoreleasepool(|_| one_pass(ax, front))
            } else {
                one_pass(ax, front)
            };
            if toggle {
                // Change this process's own LaunchServices record (nothing
                // visible: neither policy shows a Dock icon or takes focus).
                dispatch2::DispatchQueue::main().exec_async(move || {
                    use objc2_app_kit::NSApplication;
                    let mtm = objc2::MainThreadMarker::new().expect("main queue");
                    let policy = if k % 2 == 0 {
                        NSApplicationActivationPolicy::Accessory
                    } else {
                        NSApplicationActivationPolicy::Prohibited
                    };
                    NSApplication::sharedApplication(mtm).setActivationPolicy(policy);
                });
            }
            std::thread::sleep(Duration::from_millis(interval_ms));
        }
        println!(
            "done: {n} passes {interval_ms}ms apart, {regular} {} per pass, {}, NSApp {}, AX reads {}, policy toggling {} — holding {hold}s for `heap {}`",
            if front { "frontmost app found" } else { "regular apps" },
            if pooled { "pool per pass" } else { "no pool" },
            if app { "running" } else { "absent" },
            if ax { "on" } else { "off" },
            if toggle { "on" } else { "off" },
            std::process::id()
        );
        // Stay alive, pool and all, so `heap` sees what the loop left behind.
        std::thread::sleep(Duration::from_secs(hold));
        std::process::exit(0);
    });
    if app {
        use objc2_app_kit::NSApplication;
        let mtm = objc2::MainThreadMarker::new().expect("main thread");
        let app = NSApplication::sharedApplication(mtm);
        app.setActivationPolicy(NSApplicationActivationPolicy::Prohibited);
        app.run();
    } else {
        loop {
            std::thread::park();
        }
    }
}
