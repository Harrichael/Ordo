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

use objc2_app_kit::{NSApplicationActivationPolicy, NSWorkspace};
use objc2_application_services::{AXError, AXUIElement, AXValueType};
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

        for win in app_windows(&el) {
            if let Some(w) = read_window(win, Pid(pid), bundle_id.clone()) {
                windows.push(w);
            }
        }
    }

    AxScan {
        focused: focused_window(),
        windows,
    }
}

/// The AXUIElement pointers for an app's windows. Borrowed from the copied
/// array, which is released before return — callers use them only within the
/// loop body, which they do.
fn app_windows(app: &AXUIElement) -> Vec<*const AXUIElement> {
    let Some(raw) = (unsafe { copy_attr(app, "AXWindows") }) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    unsafe {
        for i in 0..super::cf::array_len(raw) {
            let el = super::cf::array_get(raw, i) as *const AXUIElement;
            if !el.is_null() {
                out.push(el);
            }
        }
        sys::CFRelease(raw);
    }
    out
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
