//! Window-created hints, via per-app AXObservers.
//!
//! AX notifications are unreliable, so Ordo never treats them as truth — a hint
//! only prompts the engine to take a fresh full snapshot. But one hint carries
//! information a rescan cannot reconstruct: that a window is *new*. A periodic
//! scan can't tell "just created" from "previously missed", so only the
//! `kAXWindowCreated` notification authorizes new-window corralling (see the
//! core's handling of `RescanTrigger::AxHint`). This thread turns that
//! notification into `Msg::Rescan(AxHint { WindowCreated, pid })`.
//!
//! Observers are per-pid. We attach to every regular app at startup and, on a
//! slow timer, to any newly-launched app — cruder than subscribing to
//! NSWorkspace launch notifications, but it needs no delegate and self-heals if
//! an app wasn't AX-ready the instant it launched.

use std::collections::HashSet;
use std::ffi::c_void;
use std::ptr::NonNull;

use crossbeam_channel::Sender;
use objc2_app_kit::{NSApplicationActivationPolicy, NSWorkspace};
use objc2_application_services::{AXError, AXObserver, AXUIElement};
use objc2_core_foundation::{kCFRunLoopDefaultMode, CFRetained, CFRunLoop, CFString};
use ordo_core::{AxHintKind, Pid, RescanTrigger};

use crate::engine::Msg;

/// Seconds the run loop pumps between passes that attach observers to
/// newly-launched apps.
const ATTACH_INTERVAL_SECS: f64 = 3.0;

struct Ctx {
    tx: Sender<Msg>,
}

/// Spawn the observer thread. Runs until the process exits.
pub fn spawn(tx: Sender<Msg>) {
    std::thread::spawn(move || {
        let ctx = Box::into_raw(Box::new(Ctx { tx }));
        let mut attached: HashSet<i32> = HashSet::new();
        // Keep observers alive for the thread's lifetime.
        let mut observers: Vec<CFRetained<AXObserver>> = Vec::new();

        loop {
            attach_new(&mut attached, &mut observers, ctx as *mut c_void);
            // Pump this thread's run loop so callbacks fire, returning
            // periodically to attach to freshly-launched apps.
            CFRunLoop::run_in_mode(
                unsafe { kCFRunLoopDefaultMode },
                ATTACH_INTERVAL_SECS,
                false,
            );
        }
    });
}

fn attach_new(
    attached: &mut HashSet<i32>,
    observers: &mut Vec<CFRetained<AXObserver>>,
    ctx: *mut c_void,
) {
    let apps = NSWorkspace::sharedWorkspace().runningApplications();
    for app in apps.iter() {
        if app.activationPolicy() != NSApplicationActivationPolicy::Regular {
            continue;
        }
        let pid = app.processIdentifier();
        if pid <= 0 || attached.contains(&pid) {
            continue;
        }
        if let Some(observer) = attach_one(pid, ctx) {
            observers.push(observer);
        }
        // Mark attached either way: a failed attach is usually an AX-less app,
        // and retrying it every 3s forever is just noise.
        attached.insert(pid);
    }
}

fn attach_one(pid: i32, ctx: *mut c_void) -> Option<CFRetained<AXObserver>> {
    let mut raw: *mut AXObserver = std::ptr::null_mut();
    let err = unsafe { AXObserver::create(pid, Some(callback), NonNull::from(&mut raw)) };
    if err != AXError::Success {
        return None;
    }
    let observer = unsafe { CFRetained::from_raw(NonNull::new(raw)?) };

    let app = unsafe { AXUIElement::new_application(pid) };
    let notif = CFString::from_str("AXWindowCreated");
    if unsafe { observer.add_notification(&app, &notif, ctx) } != AXError::Success {
        return None;
    }

    let source = unsafe { observer.run_loop_source() };
    if let Some(rl) = CFRunLoop::current() {
        unsafe { rl.add_source(Some(&source), kCFRunLoopDefaultMode) };
    }
    Some(observer)
}

unsafe extern "C-unwind" fn callback(
    _observer: NonNull<AXObserver>,
    element: NonNull<AXUIElement>,
    _notification: NonNull<CFString>,
    refcon: *mut c_void,
) {
    let ctx = &*(refcon as *const Ctx);
    // The element is the created window; its pid narrows corralling to that app.
    let mut pid: i32 = 0;
    let got = element.as_ref().pid(NonNull::from(&mut pid)) == AXError::Success && pid > 0;
    let _ = ctx.tx.send(Msg::Rescan(RescanTrigger::AxHint {
        pid: got.then_some(Pid(pid)),
        kind: AxHintKind::WindowCreated,
    }));
}
