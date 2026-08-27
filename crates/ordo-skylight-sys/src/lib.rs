//! The project's entire undocumented-ABI surface, quarantined in one crate.
//!
//! Everything Ordo needs from macOS that Apple does not publish lives here and
//! nowhere else: the SkyLight space APIs and the private `_AXUIElementGetWindow`
//! (`AXUIElementRef -> CGWindowID`). Private symbols are what break across macOS
//! point releases, so keeping them in one auditable file turns an OS update into
//! a diff of this crate rather than a hunt across the codebase.
//!
//! Also re-exported here: the handful of *public* Core Foundation getters used
//! to walk the CFDictionary/CFArray structures SkyLight returns. They are public
//! API, but they belong next to the raw pointers they operate on, so the rest
//! of the shell never touches raw `*const c_void`.
//!
//! Everything is `unsafe` and pointer-typed on purpose; [`crate`] deliberately
//! offers no safety layer — that is the platform module's job, where retain and
//! lifetime rules can be enforced with real knowledge of each call's ownership.

#![allow(non_snake_case, non_upper_case_globals)]

use std::ffi::c_void;
use std::os::raw::c_int;

pub type CFTypeRef = *const c_void;
pub type CFArrayRef = *const c_void;
pub type CFDictionaryRef = *const c_void;
pub type CFStringRef = *const c_void;
pub type CFNumberRef = *const c_void;
pub type CFAllocatorRef = *const c_void;
pub type CFIndex = isize;
pub type Boolean = u8;

/// WindowServer connection id (`SLSMainConnectionID`).
pub type CgsConnectionId = c_int;

/// A native space id as SkyLight reports it. This type never crosses into
/// `ordo-core` — the core speaks 1-based workspace ordinals only; the native
/// backend owns the translation.
pub type CgsSpaceId = u64;

/// `CFNumberGetValue` type selectors (from CFNumber.h).
pub const kCFNumberSInt64Type: c_int = 4;
pub const kCFNumberIntType: c_int = 9;

/// UTF-8 for `CFStringCreateWithCString` (from CFString.h).
pub const kCFStringEncodingUTF8: u32 = 0x0800_0100;

/// Mask for `SLSCopySpacesForWindows`: user + fullscreen + system spaces. yabai
/// uses this value to mean "consider every space a window might live on".
pub const kCgsAllSpacesMask: c_int = 0x7;

#[cfg_attr(target_os = "macos", link(name = "SkyLight", kind = "framework"))]
extern "C" {
    pub fn SLSMainConnectionID() -> CgsConnectionId;

    /// Full topology: a CFArray of per-display CFDictionaries, each with the
    /// display's identifier, its ordered "Spaces" list, and its "Current Space".
    /// Caller owns the returned array (Copy rule) and must `CFRelease` it.
    pub fn SLSCopyManagedDisplaySpaces(cid: CgsConnectionId) -> CFArrayRef;

    /// The current space id for a display, keyed by the display's UUID string.
    pub fn SLSManagedDisplayGetCurrentSpace(
        cid: CgsConnectionId,
        display: CFStringRef,
    ) -> CgsSpaceId;

    /// For each window id in `windows`, the space(s) it occupies. Returns a
    /// CFArray parallel to the input. Caller owns the result.
    pub fn SLSCopySpacesForWindows(
        cid: CgsConnectionId,
        mask: c_int,
        windows: CFArrayRef,
    ) -> CFArrayRef;
}

#[cfg_attr(
    target_os = "macos",
    link(name = "ApplicationServices", kind = "framework")
)]
extern "C" {
    /// The one private AX symbol every macOS window manager needs: map an
    /// `AXUIElementRef` (passed as an opaque pointer) to its `CGWindowID`.
    /// Returns `kAXErrorSuccess` (0) on success.
    pub fn _AXUIElementGetWindow(element: *const c_void, out: *mut u32) -> i32;
}

/// The 16 raw bytes of a UUID, laid out identically to CoreFoundation's
/// `CFUUIDBytes` (sixteen `UInt8` fields). We fold these into a `u128` for a
/// stable, hashable monitor identity.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct CFUUIDBytes(pub [u8; 16]);

#[cfg_attr(target_os = "macos", link(name = "CoreGraphics", kind = "framework"))]
extern "C" {
    /// A stable per-display UUID (survives the CGDirectDisplayID churn that
    /// hot-plug and sleep cause). Returns a CFUUIDRef the caller owns.
    pub fn CGDisplayCreateUUIDFromDisplayID(display: u32) -> CFTypeRef;
}

#[cfg_attr(target_os = "macos", link(name = "CoreFoundation", kind = "framework"))]
extern "C" {
    pub fn CFUUIDGetUUIDBytes(uuid: CFTypeRef) -> CFUUIDBytes;
    pub fn CFRelease(cf: CFTypeRef);
    pub fn CFEqual(a: CFTypeRef, b: CFTypeRef) -> Boolean;

    pub fn CFArrayGetCount(array: CFArrayRef) -> CFIndex;
    pub fn CFArrayGetValueAtIndex(array: CFArrayRef, idx: CFIndex) -> *const c_void;
    pub fn CFArrayCreate(
        allocator: CFAllocatorRef,
        values: *const *const c_void,
        num_values: CFIndex,
        callbacks: *const c_void,
    ) -> CFArrayRef;

    pub fn CFDictionaryGetValue(dict: CFDictionaryRef, key: *const c_void) -> *const c_void;

    pub fn CFNumberGetValue(number: CFNumberRef, the_type: c_int, value: *mut c_void) -> Boolean;
    pub fn CFNumberCreate(
        allocator: CFAllocatorRef,
        the_type: c_int,
        value: *const c_void,
    ) -> CFNumberRef;

    pub fn CFStringCreateWithCString(
        allocator: CFAllocatorRef,
        c_str: *const std::os::raw::c_char,
        encoding: u32,
    ) -> CFStringRef;
    pub fn CFStringGetCString(
        the_string: CFStringRef,
        buffer: *mut std::os::raw::c_char,
        buffer_size: CFIndex,
        encoding: u32,
    ) -> Boolean;
    pub fn CFGetTypeID(cf: CFTypeRef) -> usize;
    pub fn CFNumberGetTypeID() -> usize;

    /// The callback set that makes a CFArray retain/release CF objects — needed
    /// when building the window-id array for `SLSCopySpacesForWindows`.
    pub static kCFTypeArrayCallBacks: c_void;

    /// The shared CFBoolean singletons, used to toggle the AX
    /// `AXEnhancedUserInterface` flag around window moves.
    pub static kCFBooleanTrue: CFTypeRef;
    pub static kCFBooleanFalse: CFTypeRef;
}
