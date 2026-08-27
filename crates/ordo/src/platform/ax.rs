//! Windows, via the Accessibility API.
//!
//! AX is the only public way to see and move other apps' windows, and it is
//! cranky: every call is synchronous IPC to the target app, notifications lie,
//! and handles go stale. Two habits from the research are baked in here:
//!   - a short messaging timeout on every app element, so one hung app stalls
//!     us for 0.2s, not indefinitely;
//!   - no persistent handle cache — enumeration is done fresh each time and
//!     writes re-find their target by window id, sidestepping the entire class
//!     of stale-`AXUIElement` bugs at the cost of an O(windows) walk per write.
//!
//! Window identity is the CGWindowID, obtained from the private
//! `_AXUIElementGetWindow`; elements without one (sheets, transient overlays)
//! are skipped and never enter the model.

use std::ffi::c_void;
use std::ptr::NonNull;

use objc2_app_kit::{NSApplicationActivationOptions, NSApplicationActivationPolicy, NSWorkspace};
use objc2_application_services::{AXError, AXUIElement, AXValue, AXValueType};
use objc2_core_foundation::{CFString, CFType, CGPoint, CGSize};
use ordo_core::{Pid, Rect, WindowId};
use ordo_skylight_sys as sys;

/// A quarter second: long enough for a healthy app to answer, short enough that
/// a wedged one doesn't wedge us.
const MESSAGING_TIMEOUT_SECS: f32 = 0.2;

pub struct AxWindow {
    pub id: WindowId,
    pub app: Pid,
    pub bundle_id: Option<String>,
    pub title: String,
    pub frame: Rect,
}

pub struct AxScan {
    pub windows: Vec<AxWindow>,
    pub focused: Option<WindowId>,
}

/// Enumerate every standard window of every regular (Dock-visible) app, plus
/// which window currently has focus.
pub fn scan() -> AxScan {
    let mut windows = Vec::new();
    let apps = NSWorkspace::sharedWorkspace().runningApplications();
    for app in apps.iter() {
        if app.activationPolicy() != NSApplicationActivationPolicy::Regular {
            continue;
        }
        let pid = app.processIdentifier();
        if pid <= 0 {
            continue;
        }
        let bundle_id = app.bundleIdentifier().map(|b| b.to_string());
        let el = unsafe { AXUIElement::new_application(pid) };
        unsafe { el.set_messaging_timeout(MESSAGING_TIMEOUT_SECS) };

        // The window elements are borrowed from this array, so every read must
        // happen before it's released — releasing first would leave dangling
        // AXUIElement pointers (a use-after-free that only surfaces once the
        // app actually has windows to enumerate).
        let Some(raw) = (unsafe { copy_attr(&el, "AXWindows") }) else {
            continue;
        };
        unsafe {
            for i in 0..super::cf::array_len(raw) {
                let win = super::cf::array_get(raw, i) as *const AXUIElement;
                if win.is_null() {
                    continue;
                }
                if let Some(w) = read_window(win, Pid(pid), bundle_id.clone()) {
                    windows.push(w);
                }
            }
            sys::CFRelease(raw);
        }
    }

    AxScan {
        focused: focused_window(),
        windows,
    }
}

fn read_window(win: *const AXUIElement, app: Pid, bundle_id: Option<String>) -> Option<AxWindow> {
    let id = window_id(win)?;
    let win_ref = unsafe { &*win };
    let pos = unsafe { copy_point(win_ref, "AXPosition", AXValueType::CGPoint) }?;
    let size = unsafe { copy_size(win_ref, "AXSize") }?;
    let title = unsafe {
        copy_attr(win_ref, "AXTitle")
            .and_then(|p| {
                let s = super::cf::string_value(p);
                sys::CFRelease(p);
                s
            })
            .unwrap_or_default()
    };
    Some(AxWindow {
        id,
        app,
        bundle_id,
        title,
        frame: Rect {
            x: pos.x,
            y: pos.y,
            w: size.width,
            h: size.height,
        },
    })
}

fn focused_window() -> Option<WindowId> {
    let front = NSWorkspace::sharedWorkspace().frontmostApplication()?;
    let pid = front.processIdentifier();
    if pid <= 0 {
        return None;
    }
    let el = unsafe { AXUIElement::new_application(pid) };
    unsafe { el.set_messaging_timeout(MESSAGING_TIMEOUT_SECS) };
    let focused = unsafe { copy_attr(&el, "AXFocusedWindow") }?;
    let id = window_id(focused as *const AXUIElement);
    unsafe { sys::CFRelease(focused) };
    id
}

fn window_id(el: *const AXUIElement) -> Option<WindowId> {
    if el.is_null() {
        return None;
    }
    let mut wid: u32 = 0;
    let err = unsafe { sys::_AXUIElementGetWindow(el as *const c_void, &mut wid) };
    if err == 0 && wid != 0 {
        Some(WindowId(wid))
    } else {
        None
    }
}

/// Copy an attribute, returning the owned CF value as a raw pointer (caller
/// releases). `None` on any AX error or a null result.
unsafe fn copy_attr(el: &AXUIElement, name: &str) -> Option<*const c_void> {
    let attr = CFString::from_str(name);
    let mut out: *const CFType = std::ptr::null();
    let err = el.copy_attribute_value(&attr, NonNull::from(&mut out));
    if err == AXError::Success && !out.is_null() {
        Some(out as *const c_void)
    } else {
        None
    }
}

unsafe fn copy_point(el: &AXUIElement, name: &str, ty: AXValueType) -> Option<CGPoint> {
    let raw = copy_attr(el, name)?;
    let mut p = CGPoint { x: 0.0, y: 0.0 };
    let ok = (*(raw as *const objc2_application_services::AXValue))
        .value(ty, NonNull::new(&mut p as *mut _ as *mut c_void)?);
    sys::CFRelease(raw);
    if ok {
        Some(p)
    } else {
        None
    }
}

unsafe fn copy_size(el: &AXUIElement, name: &str) -> Option<CGSize> {
    let raw = copy_attr(el, name)?;
    let mut s = CGSize {
        width: 0.0,
        height: 0.0,
    };
    let ok = (*(raw as *const objc2_application_services::AXValue)).value(
        AXValueType::CGSize,
        NonNull::new(&mut s as *mut _ as *mut c_void)?,
    );
    sys::CFRelease(raw);
    if ok {
        Some(s)
    } else {
        None
    }
}

// --- writes ----------------------------------------------------------------

/// Raise `target`, make it its app's main/focused window, and activate the app.
/// Returns whether the window was found (its AX writes are best-effort — some
/// apps refuse `kAXRaise` but still come forward on activation).
pub fn focus(target: WindowId) -> bool {
    with_window(target, |_app, win, pid| {
        let win = unsafe { &*win };
        unsafe {
            set_bool(win, "AXMain", true);
            set_bool(win, "AXFocused", true);
            let raise = CFString::from_str("AXRaise");
            let _ = win.perform_action(&raise);
        }
        activate_pid(pid);
    })
    .is_some()
}

/// Move/resize `target` to `frame`. Uses the position -> size -> position idiom
/// (apps clamp size to the *current* screen before a cross-display move lands,
/// so a single set often misses) and brackets the writes with
/// `AXEnhancedUserInterface` disabled — without which Chromium/Electron windows
/// move slowly and land wrong.
pub fn set_frame(target: WindowId, frame: Rect) -> bool {
    with_window(target, |app, win, _pid| {
        let win = unsafe { &*win };
        let restore_eui = unsafe { disable_enhanced_ui(app) };
        unsafe {
            set_point(win, "AXPosition", frame.x, frame.y);
            set_size_attr(win, frame.w, frame.h);
            set_point(win, "AXPosition", frame.x, frame.y);
            if restore_eui {
                set_bool(app, "AXEnhancedUserInterface", true);
            }
        }
    })
    .is_some()
}

/// Walk regular apps, and when the window whose id is `target` is found, run
/// `f(app_element, window_ptr, pid)` while both handles are live. `None` if the
/// window isn't found (gone, or an app with no AX tree).
fn with_window<T>(
    target: WindowId,
    f: impl FnOnce(&AXUIElement, *const AXUIElement, i32) -> T,
) -> Option<T> {
    let apps = NSWorkspace::sharedWorkspace().runningApplications();
    for app in apps.iter() {
        if app.activationPolicy() != NSApplicationActivationPolicy::Regular {
            continue;
        }
        let pid = app.processIdentifier();
        if pid <= 0 {
            continue;
        }
        let el = unsafe { AXUIElement::new_application(pid) };
        unsafe { el.set_messaging_timeout(MESSAGING_TIMEOUT_SECS) };

        let Some(raw) = (unsafe { copy_attr(&el, "AXWindows") }) else {
            continue;
        };
        let mut found = None;
        unsafe {
            for i in 0..super::cf::array_len(raw) {
                let win = super::cf::array_get(raw, i) as *const AXUIElement;
                if window_id(win) == Some(target) {
                    found = Some((win, pid));
                    break;
                }
            }
        }
        if let Some((win, pid)) = found {
            let out = f(&el, win, pid);
            unsafe { sys::CFRelease(raw) };
            return Some(out);
        }
        unsafe { sys::CFRelease(raw) };
    }
    None
}

fn activate_pid(pid: i32) {
    let apps = NSWorkspace::sharedWorkspace().runningApplications();
    for app in apps.iter() {
        if app.processIdentifier() == pid {
            // Deprecated but still the reliable way to bring a background app's
            // window forward with keyboard focus.
            #[allow(deprecated)]
            let _ = app.activateWithOptions(NSApplicationActivationOptions::ActivateAllWindows);
            return;
        }
    }
}

unsafe fn set_bool(el: &AXUIElement, name: &str, value: bool) {
    let attr = CFString::from_str(name);
    let b = if value {
        sys::kCFBooleanTrue
    } else {
        sys::kCFBooleanFalse
    };
    if b.is_null() {
        return;
    }
    let cf: &CFType = &*(b as *const CFType);
    let _ = el.set_attribute_value(&attr, cf);
}

unsafe fn set_point(el: &AXUIElement, name: &str, x: f64, y: f64) {
    let mut p = CGPoint { x, y };
    let Some(val) = AXValue::new(
        AXValueType::CGPoint,
        NonNull::new(&mut p as *mut _ as *mut c_void).unwrap(),
    ) else {
        return;
    };
    let attr = CFString::from_str(name);
    let _ = el.set_attribute_value(&attr, val.as_ref());
}

unsafe fn set_size_attr(el: &AXUIElement, w: f64, h: f64) {
    let mut s = CGSize {
        width: w,
        height: h,
    };
    let Some(val) = AXValue::new(
        AXValueType::CGSize,
        NonNull::new(&mut s as *mut _ as *mut c_void).unwrap(),
    ) else {
        return;
    };
    let attr = CFString::from_str("AXSize");
    let _ = el.set_attribute_value(&attr, val.as_ref());
}

/// Read `AXEnhancedUserInterface`; if it's on, turn it off and report that it
/// needs restoring. Returns false if it was already off (nothing to restore).
unsafe fn disable_enhanced_ui(app: &AXUIElement) -> bool {
    let Some(cur) = copy_attr(app, "AXEnhancedUserInterface") else {
        return false;
    };
    let was_on = !sys::kCFBooleanTrue.is_null() && sys::CFEqual(cur, sys::kCFBooleanTrue) != 0;
    sys::CFRelease(cur);
    if was_on {
        set_bool(app, "AXEnhancedUserInterface", false);
    }
    was_on
}
