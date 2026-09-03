//! Thin, local safety over the raw Core Foundation getters in
//! [`ordo_skylight_sys`]. The point is to keep raw `*const c_void` and the
//! get/copy ownership distinction confined to one small file, so the SkyLight
//! parser above reads like ordinary Rust.
//!
//! Ownership convention followed throughout: values obtained with a "Get" call
//! are borrowed (owned by their container — do not release); values obtained
//! with a "Copy/Create" call are owned (release when done). Callers uphold this.
//!
//! Every function here is `unsafe` for the same reason — the caller must pass a
//! pointer that is either null or a live CF object of the expected type — so the
//! per-function `# Safety` boilerplate is waived in favor of this note.
#![allow(clippy::missing_safety_doc)]

use std::ffi::{c_void, CString};

use ordo_skylight_sys as sys;

pub unsafe fn array_len(a: sys::CFArrayRef) -> isize {
    if a.is_null() {
        0
    } else {
        sys::CFArrayGetCount(a)
    }
}

pub unsafe fn array_get(a: sys::CFArrayRef, i: isize) -> *const c_void {
    sys::CFArrayGetValueAtIndex(a, i)
}

/// Look up `key` in a dictionary. Returns a borrowed value pointer (null if
/// absent). The key CFString is created and released here.
pub unsafe fn dict_get(d: sys::CFDictionaryRef, key: &str) -> *const c_void {
    if d.is_null() {
        return std::ptr::null();
    }
    let cstr = match CString::new(key) {
        Ok(c) => c,
        Err(_) => return std::ptr::null(),
    };
    let cf_key =
        sys::CFStringCreateWithCString(std::ptr::null(), cstr.as_ptr(), sys::kCFStringEncodingUTF8);
    if cf_key.is_null() {
        return std::ptr::null();
    }
    let value = sys::CFDictionaryGetValue(d, cf_key);
    sys::CFRelease(cf_key);
    value
}

/// Read a CFNumber as i64. Returns None if the pointer isn't a CFNumber.
pub unsafe fn number_i64(n: *const c_void) -> Option<i64> {
    if n.is_null() || sys::CFGetTypeID(n) != sys::CFNumberGetTypeID() {
        return None;
    }
    let mut v: i64 = 0;
    let ok = sys::CFNumberGetValue(
        n,
        sys::kCFNumberSInt64Type,
        &mut v as *mut i64 as *mut c_void,
    );
    if ok != 0 {
        Some(v)
    } else {
        None
    }
}

/// Read a CFNumber as f64. Separate from [`number_i64`] because CoreGraphics
/// window geometry is doubles, and asking for SInt64 back would truncate it.
pub unsafe fn number_f64(n: *const c_void) -> Option<f64> {
    if n.is_null() || sys::CFGetTypeID(n) != sys::CFNumberGetTypeID() {
        return None;
    }
    let mut v: f64 = 0.0;
    let ok = sys::CFNumberGetValue(
        n,
        sys::kCFNumberDoubleType,
        &mut v as *mut f64 as *mut c_void,
    );
    if ok != 0 {
        Some(v)
    } else {
        None
    }
}

/// Read a CFString into a Rust String (best effort; None if it can't be
/// materialized within a reasonable buffer).
pub unsafe fn string_value(s: *const c_void) -> Option<String> {
    if s.is_null() {
        return None;
    }
    let mut buf = vec![0i8; 512];
    let ok = sys::CFStringGetCString(
        s,
        buf.as_mut_ptr(),
        buf.len() as isize,
        sys::kCFStringEncodingUTF8,
    );
    if ok == 0 {
        return None;
    }
    let cstr = std::ffi::CStr::from_ptr(buf.as_ptr());
    cstr.to_str().ok().map(|s| s.to_string())
}
