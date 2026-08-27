//! Listen to ALL distributed notifications for ~14s and print each one's name
//! (dev tool). Run it, switch a Space by hand or via backend_switch, and see
//! which notification names macOS actually broadcasts on Tahoe — the
//! NSWorkspace route needs a real app identity, so a daemon needs whatever
//! shows up here instead.
//!
//! Run: cargo run --example notify_probe

use std::ffi::c_void;
use std::time::{Duration, Instant};

use objc2_core_foundation::{
    kCFRunLoopDefaultMode, CFDictionary, CFNotificationCenter, CFNotificationName,
    CFNotificationSuspensionBehavior, CFRunLoop,
};

unsafe extern "C-unwind" fn on_note(
    _center: *mut CFNotificationCenter,
    _observer: *mut c_void,
    name: *const CFNotificationName,
    _object: *const c_void,
    _info: *const CFDictionary,
) {
    if !name.is_null() {
        println!("distributed: {}", unsafe { &*name });
    }
}

fn main() {
    let center = CFNotificationCenter::distributed_center().expect("distributed center");
    unsafe {
        center.add_observer(
            &center as *const _ as *const c_void,
            Some(on_note),
            None, // all names — discovery mode
            std::ptr::null(),
            CFNotificationSuspensionBehavior::DeliverImmediately,
        );
    }
    println!("listening for distributed notifications for 14s — switch a Space now…");
    let deadline = Instant::now() + Duration::from_secs(14);
    while Instant::now() < deadline {
        CFRunLoop::run_in_mode(unsafe { kCFRunLoopDefaultMode }, 0.5, false);
    }
    println!("done.");
}
