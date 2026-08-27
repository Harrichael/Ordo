//! Window-created and focus-change hints, via per-app AXObservers.
//!
//! AX notifications are unreliable, so Ordo never treats them as truth — a hint
//! only prompts the engine to take a fresh full snapshot. But one hint carries
//! information a rescan cannot reconstruct: that a window is *new*. A periodic
//! scan can't tell "just created" from "previously missed", so only the
//! `kAXWindowCreated` notification authorizes new-window corralling (see the
//! core's handling of `RescanTrigger::AxHint`). This thread turns that
//! notification into `Msg::Rescan(AxHint { WindowCreated, pid })`.
//!
//! Focus changes (`AXFocusedWindowChanged`, `AXApplicationActivated`) are hints
//! too. Without them the MRU history learns external focus only from the
//! periodic scan — click a window and press Alt+Tab within the ~2s gap and the
//! core acts on a stale "most recent", sending focus somewhere wrong. The hint
//! collapses that gap to one rescan.
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

/// Watch for Space changes by cheaply polling the SkyLight topology.
///
/// A push channel would be better, but none is reachable from a daemon on
/// Tahoe: AX has no space notification, NSWorkspace's
/// ActiveSpaceDidChangeNotification is only delivered to processes with a real
/// app identity, and no distributed notification fires on a switch at all
/// (probed — see examples/notify_probe.rs). This read is a single WindowServer
/// round-trip with no AX involved, so a 150ms cadence costs ~nothing and cuts
/// external-switch reaction from the 2s full scan to near-instant. The full
/// scan the hint triggers is where the real cost stays.
pub fn spawn_space_watcher(tx: Sender<Msg>) {
    const CADENCE: std::time::Duration = std::time::Duration::from_millis(150);
    std::thread::spawn(move || {
        let cid = super::skylight::connection();
        let mut last: Option<Vec<(String, u64)>> = None;
        loop {
            let now: Vec<(String, u64)> = super::skylight::managed_display_spaces(cid)
                .into_iter()
                .map(|d| (d.identifier, d.current_space))
                .collect();
            if last.as_ref().is_some_and(|l| *l != now)
                && tx
                    .send(Msg::Rescan(RescanTrigger::BackendHint {
                        kind: "space_changed".into(),
                    }))
                    .is_err()
            {
                return; // engine gone; the daemon is shutting down
            }
            last = Some(now);
            std::thread::sleep(CADENCE);
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
    let created = CFString::from_str("AXWindowCreated");
    if unsafe { observer.add_notification(&app, &created, ctx) } != AXError::Success {
        return None;
    }
    // Focus hints are additive: an app that rejects them (some AX-poor apps do)
    // still gets window-created coverage, and the periodic scan remains the
    // safety net for its focus changes.
    for name in ["AXFocusedWindowChanged", "AXApplicationActivated"] {
        let notif = CFString::from_str(name);
        let _ = unsafe { observer.add_notification(&app, &notif, ctx) };
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
    notification: NonNull<CFString>,
    refcon: *mut c_void,
) {
    let ctx = &*(refcon as *const Ctx);
    // The element is the affected window/app; its pid narrows corralling.
    let mut pid: i32 = 0;
    let got = element.as_ref().pid(NonNull::from(&mut pid)) == AXError::Success && pid > 0;
    let name = notification.as_ref().to_string();
    // Only the real creation notification may claim WindowCreated — that kind
    // authorizes corralling in the core, and a focus hint must never do that.
    let kind = if name == "AXWindowCreated" {
        AxHintKind::WindowCreated
    } else {
        AxHintKind::Other(name)
    };
    let _ = ctx.tx.send(Msg::Rescan(RescanTrigger::AxHint {
        pid: got.then_some(Pid(pid)),
        kind,
    }));
}
