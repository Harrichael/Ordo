//! Probe: what does one app answer over Accessibility? Read-only. For when an
//! app's windows are on screen but missing from Ordo's scans: prints each
//! step's error code and timing instead of the scan's silent skip.
//!
//!   cargo run --release --example ax_app_probe -- <pid>

use std::ffi::c_void;
use std::ptr::NonNull;
use std::time::Instant;

use objc2_application_services::{AXError, AXIsProcessTrusted, AXUIElement};
use objc2_core_foundation::{CFString, CFType};
use ordo::platform::cf;
use ordo_skylight_sys as sys;

fn copy(el: &AXUIElement, name: &str, timeout: f32) -> (AXError, Option<*const c_void>, f64) {
    unsafe { el.set_messaging_timeout(timeout) };
    let attr = CFString::from_str(name);
    let mut out: *const CFType = std::ptr::null();
    let t = Instant::now();
    let err = unsafe { el.copy_attribute_value(&attr, NonNull::from(&mut out)) };
    let ms = t.elapsed().as_secs_f64() * 1000.0;
    let v = (err == AXError::Success && !out.is_null()).then_some(out as *const c_void);
    (err, v, ms)
}

fn text(v: Option<*const c_void>) -> String {
    v.and_then(|p| unsafe { cf::string_value(p) })
        .unwrap_or_else(|| "-".into())
}

fn main() {
    let pid: i32 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .expect("usage: ax_app_probe <pid>");
    println!("trusted: {}", unsafe { AXIsProcessTrusted() });
    let app = unsafe { AXUIElement::new_application(pid) };
    for timeout in [0.2f32, 2.0] {
        println!("-- messaging timeout {timeout}s");
        let (err, role, ms) = copy(&app, "AXRole", timeout);
        println!("app AXRole: {err:?} {} ({ms:.1} ms)", text(role));
        let (err, title, ms) = copy(&app, "AXTitle", timeout);
        println!("app AXTitle: {err:?} {} ({ms:.1} ms)", text(title));
        for name in ["AXFocusedWindow", "AXMainWindow"] {
            let (err, _, ms) = copy(&app, name, timeout);
            println!("app {name}: {err:?} ({ms:.1} ms)");
        }
        let (err, windows, ms) = copy(&app, "AXWindows", timeout);
        let n = windows.map_or(0, |w| unsafe { cf::array_len(w) });
        println!("app AXWindows: {err:?}, {n} element(s) ({ms:.1} ms)");
        let Some(windows) = windows else { continue };
        for i in 0..n {
            let win = unsafe { cf::array_get(windows, i) } as *const AXUIElement;
            let mut wid: u32 = 0;
            let id_err = unsafe { sys::_AXUIElementGetWindow(win as *const c_void, &mut wid) };
            let w = unsafe { &*win };
            let (role_err, role, _) = copy(w, "AXRole", timeout);
            let (_, subrole, _) = copy(w, "AXSubrole", timeout);
            let (pos_err, _, pos_ms) = copy(w, "AXPosition", timeout);
            let (size_err, _, _) = copy(w, "AXSize", timeout);
            let (_, title, _) = copy(w, "AXTitle", timeout);
            println!(
                "  [{i}] window id {wid} (err {id_err}); role {role_err:?} {} / {}; position {pos_err:?} ({pos_ms:.1} ms); size {size_err:?}; title {}",
                text(role),
                text(subrole),
                text(title)
            );
        }
        unsafe { sys::CFRelease(windows) };
    }
}
