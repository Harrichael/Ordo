//! Windows, via the Accessibility API.
//!
//! AX is the only public way to see and move other apps' windows, and it is
//! cranky: every call is synchronous IPC to the target app, notifications lie,
//! and handles go stale. Two habits from the research are baked in here:
//!   - a short messaging timeout on every app element, so one hung app stalls
//!     us for 0.2s, not indefinitely;
//!   - no handle cache shared across threads or kept past a failure:
//!     enumeration is done fresh, and one-off writes re-find their target by
//!     window id. The one cache is [`AxApp`]'s, private to an app's queue
//!     thread, re-read when a window is missing from it or a write through
//!     it fails, which keeps the stale-`AXUIElement` class of bugs out.
//!
//! Window identity is the CGWindowID, obtained from the private
//! `_AXUIElementGetWindow`; elements without one (sheets, transient overlays)
//! are skipped and never enter the model.

use std::collections::HashMap;
use std::ffi::c_void;
use std::ptr::NonNull;
use std::time::{Duration, Instant};

use objc2_app_kit::{NSApplicationActivationPolicy, NSRunningApplication, NSWorkspace};
use objc2_application_services::{AXError, AXUIElement, AXValue, AXValueType};
use objc2_core_foundation::{CFBoolean, CFString, CFType, CGPoint, CGSize};
use ordo_core::{Pid, Point, Rect, WindowId};
use ordo_emulated::HoldStat;
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
    /// `AXSubrole`: AXStandardWindow, AXDialog, AXFloatingWindow,
    /// AXSystemFloatingWindow… Read but not yet acted on. Ordo currently
    /// manages every window an app exposes through AXWindows, which is how a
    /// transient Outlook reminder toast acquired a permanent workspace claim.
    /// Logged first so the classifier is chosen against what apps actually
    /// report rather than what the conventions say they should.
    pub subrole: Option<String>,
    /// Its app has a Dock icon; otherwise it is one of the named background
    /// apps (see [`manages`]).
    pub regular: bool,
}

pub struct AxScan {
    pub windows: Vec<AxWindow>,
    pub focused: Option<WindowId>,
    pub walk: Walk,
}

/// What a window walk cost: the walk as a whole, and the app it waited on
/// longest — with apps read in parallel, that app IS the walk.
pub struct Walk {
    pub elapsed: Duration,
    pub apps: usize,
    pub slowest: Option<(Pid, Duration)>,
    /// Apps that answered "API disabled": they refuse this process outright,
    /// though macOS calls it trusted, so none of their windows are seen and
    /// none can be moved. Measured once for Outlook, running for weeks,
    /// against an Ordo started from a different terminal than the last.
    pub refused: Vec<Pid>,
    /// Apps that did not answer for their window list within the messaging
    /// timeout, busy with something (often Ordo's own writes). Their windows
    /// are unknown this time, not gone: in run 46 all six of kitty's windows
    /// dropped out of one scan this way, and came back 1.8 s later.
    pub unanswered: Vec<Pid>,
}

/// How an app answered for its window list.
#[derive(Clone, Copy, PartialEq)]
enum Listing {
    Answered,
    Refused,
    Unanswered,
}

/// Enumerate every standard window of every regular (Dock-visible) app, plus
/// which window currently has focus.
pub fn scan() -> AxScan {
    let focused = focused_window();
    let (windows, walk) = walk();
    AxScan {
        focused,
        windows,
        walk,
    }
}

/// The window half of [`scan`], for callers who don't need focus (asking every
/// app "are you frontmost?" is a second full round of IPC).
pub fn windows() -> Vec<AxWindow> {
    walk().0
}

/// Every app is asked on its own thread. Each read is a round trip to that
/// app and waits on its main thread, and mid-switch the apps are busy with
/// the very hides and moves Ordo just sent: read one after another, the walk
/// cost the sum of every app's delay; in parallel it costs the slowest one.
/// Results keep the running-apps order, so the snapshot reads the same.
fn walk() -> (Vec<AxWindow>, Walk) {
    let started = Instant::now();
    // AppKit objects stay on this thread; only plain data crosses.
    let apps: Vec<(i32, Option<String>, bool)> = NSWorkspace::sharedWorkspace()
        .runningApplications()
        .iter()
        .filter(|a| managed(a))
        .map(|a| {
            let regular = a.activationPolicy() == NSApplicationActivationPolicy::Regular;
            (a.processIdentifier(), a.bundleIdentifier().map(|b| b.to_string()), regular)
        })
        .filter(|(pid, _, _)| *pid > 0)
        .collect();
    let per_app: Vec<(Vec<AxWindow>, Duration, Listing)> = std::thread::scope(|scope| {
        let handles: Vec<_> = apps
            .iter()
            .map(|(pid, bundle_id, regular)| scope.spawn(move || app_windows(*pid, bundle_id, *regular)))
            .collect();
        handles
            .into_iter()
            .map(|h| {
                h.join()
                    .unwrap_or((Vec::new(), Duration::ZERO, Listing::Unanswered))
            })
            .collect()
    });
    let slowest = apps
        .iter()
        .zip(&per_app)
        .max_by_key(|(_, (_, took, _))| *took)
        .map(|((pid, _, _), (_, took, _))| (Pid(*pid), *took));
    let listed = |how: Listing| -> Vec<Pid> {
        apps.iter()
            .zip(&per_app)
            .filter(|(_, (_, _, listing))| *listing == how)
            .map(|((pid, _, _), _)| Pid(*pid))
            .collect()
    };
    let refused = listed(Listing::Refused);
    let unanswered = listed(Listing::Unanswered);
    let windows = per_app.into_iter().flat_map(|(w, _, _)| w).collect();
    let walk = Walk {
        elapsed: started.elapsed(),
        apps: apps.len(),
        slowest,
        refused,
        unanswered,
    };
    (windows, walk)
}

/// The app's windows, how long asking took, and how it answered.
fn app_windows(pid: i32, bundle_id: &Option<String>, regular: bool) -> (Vec<AxWindow>, Duration, Listing) {
    let started = Instant::now();
    let mut windows = Vec::new();
    let el = unsafe { AXUIElement::new_application(pid) };
    unsafe { el.set_messaging_timeout(MESSAGING_TIMEOUT_SECS) };

    // The window elements are borrowed from this array, so every read must
    // happen before it's released — releasing first would leave dangling
    // AXUIElement pointers (a use-after-free that only surfaces once the
    // app actually has windows to enumerate).
    let listed = unsafe { copy_attr_or_error(&el, "AXWindows") };
    let listing = match listed {
        Err(AXError::APIDisabled) => Listing::Refused,
        Err(AXError::CannotComplete) => Listing::Unanswered,
        _ => Listing::Answered,
    };
    if let Ok(raw) = listed {
        unsafe {
            for i in 0..super::cf::array_len(raw) {
                let win = super::cf::array_get(raw, i) as *const AXUIElement;
                if win.is_null() {
                    continue;
                }
                if let Some(w) = read_window(win, Pid(pid), bundle_id.clone(), regular) {
                    windows.push(w);
                }
            }
            sys::CFRelease(raw);
        }
    }
    (windows, started.elapsed(), listing)
}

fn read_window(
    win: *const AXUIElement,
    app: Pid,
    bundle_id: Option<String>,
    regular: bool,
) -> Option<AxWindow> {
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
    let subrole = unsafe {
        copy_attr(win_ref, "AXSubrole").and_then(|p| {
            let s = super::cf::string_value(p);
            sys::CFRelease(p);
            s
        })
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
        subrole,
        regular,
    })
}

pub fn focused_window() -> Option<WindowId> {
    let pid = frontmost_app()?;
    let el = unsafe { AXUIElement::new_application(pid.0) };
    unsafe { el.set_messaging_timeout(MESSAGING_TIMEOUT_SECS) };
    let focused = unsafe { copy_attr(&el, "AXFocusedWindow") }?;
    let id = window_id(focused as *const AXUIElement);
    unsafe { sys::CFRelease(focused) };
    id
}

/// Whether `w` is the key window, asking only its own app: is it frontmost,
/// and is `w` its focused window. [`focused_window`] has to ask every app to
/// find the frontmost one.
pub fn is_key(w: WindowId, pid: i32) -> bool {
    let el = unsafe { AXUIElement::new_application(pid) };
    unsafe { el.set_messaging_timeout(MESSAGING_TIMEOUT_SECS) };
    let Some(front) = (unsafe { copy_attr(&el, "AXFrontmost") }) else {
        return false;
    };
    let is_front = unsafe { &*(front as *const CFBoolean) }.value();
    unsafe { sys::CFRelease(front) };
    if !is_front {
        return false;
    }
    let Some(focused) = (unsafe { copy_attr(&el, "AXFocusedWindow") }) else {
        return false;
    };
    let id = window_id(focused as *const AXUIElement);
    unsafe { sys::CFRelease(focused) };
    id == Some(w)
}

/// Every app answering `AXFrontmost` true, where [`frontmost_app`] takes the
/// first: for logging what the focus read stands on when it surprises.
pub fn frontmost_claims() -> Vec<Pid> {
    let apps = NSWorkspace::sharedWorkspace().runningApplications();
    apps.iter()
        .filter(|a| managed(a))
        .map(|a| a.processIdentifier())
        .filter(|pid| *pid > 0)
        .filter(|pid| {
            let el = unsafe { AXUIElement::new_application(*pid) };
            unsafe { el.set_messaging_timeout(MESSAGING_TIMEOUT_SECS) };
            let Some(front) = (unsafe { copy_attr(&el, "AXFrontmost") }) else {
                return false;
            };
            let is_front = unsafe { &*(front as *const CFBoolean) }.value();
            unsafe { sys::CFRelease(front) };
            is_front
        })
        .map(Pid)
        .collect()
}

/// Background apps (no Dock icon) whose windows the user works in like any
/// app's, so Ordo manages them: named, because nothing a window reports tells
/// these from a launcher's panel or a menu-bar popover.
const MANAGED_BACKGROUND_APPS: &[&str] = &["com.apple.screencaptureui"];

/// Whether Ordo manages an app's windows: every regular app, and the named
/// background ones.
pub fn manages(regular: bool, bundle_id: Option<&str>) -> bool {
    regular || bundle_id.is_some_and(|b| MANAGED_BACKGROUND_APPS.contains(&b))
}

pub(crate) fn managed(app: &NSRunningApplication) -> bool {
    let bundle = app.bundleIdentifier().map(|b| b.to_string());
    manages(
        app.activationPolicy() == NSApplicationActivationPolicy::Regular,
        bundle.as_deref(),
    )
}

/// NSFloatingWindowLevel: the screenshot window, utility panels. Capture UI
/// and overlays sit higher.
const FLOATING_LAYER: i32 = 3;

/// Whether Ordo manages a window it has been told about: a regular app's at
/// any layer, a background app's only at an ordinary one. The screenshot
/// tool's own window floats at layer 3 and is worked in like an app window;
/// its capture bar (layer 1499) and crop overlay (layer 24) are the capture
/// in progress, and move with the user as one thing (run 56).
pub fn admits(regular: bool, layer: Option<i32>) -> bool {
    regular || layer.is_some_and(|l| l <= FLOATING_LAYER)
}

/// Whether the app has a Dock icon, so a hide can be undone from there.
pub fn has_dock_icon(pid: Pid) -> bool {
    NSRunningApplication::runningApplicationWithProcessIdentifier(pid.0)
        .is_some_and(|a| a.activationPolicy() == NSApplicationActivationPolicy::Regular)
}

pub fn frontmost_app() -> Option<Pid> {
    // Ask each app's live `AXFrontmost` attribute — NOT
    // NSWorkspace.frontmostApplication, which is a cache that refreshes only
    // when a run loop pumps (the engine thread never pumps one, so it would
    // report the frontmost app from boot forever). The system-wide element's
    // AXFocusedApplication would be cleaner but returns
    // kAXErrorCannotComplete here (observed on Tahoe).
    // A background app's window being key leaves the regular app before it
    // still answering frontmost (the screenshot tool's window: probed), so the
    // background apps are asked first. Footgun: a hung background app costs
    // every focus read its messaging timeout.
    let apps = NSWorkspace::sharedWorkspace().runningApplications();
    let mut apps: Vec<_> = apps.iter().filter(|a| managed(a)).collect();
    apps.sort_by_key(|a| a.activationPolicy() == NSApplicationActivationPolicy::Regular);
    for app in apps {
        let pid = app.processIdentifier();
        if pid <= 0 {
            continue;
        }
        let el = unsafe { AXUIElement::new_application(pid) };
        unsafe { el.set_messaging_timeout(MESSAGING_TIMEOUT_SECS) };
        let Some(front) = (unsafe { copy_attr(&el, "AXFrontmost") }) else {
            continue;
        };
        let is_front = unsafe { &*(front as *const CFBoolean) }.value();
        unsafe { sys::CFRelease(front) };
        if is_front {
            return Some(Pid(pid));
        }
    }
    None
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
    copy_attr_or_error(el, name).ok()
}

unsafe fn copy_attr_or_error(el: &AXUIElement, name: &str) -> Result<*const c_void, AXError> {
    let attr = CFString::from_str(name);
    let mut out: *const CFType = std::ptr::null();
    let err = el.copy_attribute_value(&attr, NonNull::from(&mut out));
    match err {
        AXError::Success if !out.is_null() => Ok(out as *const c_void),
        AXError::Success => Err(AXError::NoValue),
        e => Err(e),
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

/// The "make key window" half of the focus handoff: two raw WindowServer event
/// records (yabai's reverse-engineered recipe — the 0x01/0x02 at offset 0x08
/// are an activate/deactivate pair, the window id sits at 0x3c). Without them
/// `SLPSSetFrontProcessWithOptions` fronts the app but the target window never
/// becomes key, so keyboard focus stays where it was.
fn make_key_window(psn: &sys::ProcessSerialNumber, wid: u32) {
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

/// Raise `target`, make it its app's main/focused window, and bring the app
/// frontmost. Returns whether the window was found (its AX writes are
/// best-effort — some apps refuse `kAXRaise` but still come forward).
///
/// Frontmosting uses the private `SLPSSetFrontProcessWithOptions`: AppKit's
/// cooperative activation (Sonoma+) silently refuses activation from a
/// background daemon, so the public `NSRunningApplication activate` raises the
/// window without ever moving keyboard focus — cross-app Alt+Tab looked like
/// "Slack pops up but focus stays put".
pub fn focus(target: WindowId) -> bool {
    with_window(target, |_app, win, pid| unsafe { focus_element(pid, target, &*win) }).is_some()
}

unsafe fn focus_element(pid: i32, target: WindowId, win: &AXUIElement) {
    front_process(pid, target.0);
    make_element_key(win);
}

/// The window server half of a focus: this app in front, with this window
/// key. Works for any window of the app, the desktop included.
fn front_process(pid: i32, window: u32) {
    unsafe {
        let mut psn = sys::ProcessSerialNumber::default();
        if sys::GetProcessForPID(pid, &mut psn) == 0 {
            let _ = sys::SLPSSetFrontProcessWithOptions(&psn, window, sys::kCPSUserGenerated);
            make_key_window(&psn, window);
        }
    }
}

/// The Accessibility half: the app's own idea of its main and focused
/// window, and the window raised.
unsafe fn make_element_key(win: &AXUIElement) {
    set_bool(win, "AXMain", true);
    set_bool(win, "AXFocused", true);
    let raise = CFString::from_str("AXRaise");
    let _ = win.perform_action(&raise);
}

/// Raise `target` in the global z-order without touching focus or app
/// activation — the building block for "send to back", which WindowServer
/// won't do directly for foreign windows (SLSOrderWindow → error 1000 from a
/// daemon connection): raising everything else above a window is the same
/// thing, one raise at a time.
pub fn raise(target: WindowId) -> bool {
    with_window(target, |_app, win, _pid| unsafe {
        let raise = CFString::from_str("AXRaise");
        let _ = (*win).perform_action(&raise);
    })
    .is_some()
}

/// Hide or unhide an app (the Cmd+H kind of hidden), by pid, via the app
/// element's live `AXHidden` attribute. Used for Dock dimming: with `defaults
/// write com.apple.dock showhidden -bool true`, hidden apps render translucent
/// in the Dock, giving parked-elsewhere apps a "not on this workspace" cue.
///
/// Deliberately NOT `NSRunningApplication.hide()/unhide()/isHidden`: those are
/// KVO-backed caches that refresh only when a run loop pumps, so from the
/// engine thread `isHidden` reports the boot-time value forever. That exact
/// bug shipped once — hides fired (false -> true looked like a change) but
/// every unhide was skipped as "already visible", stranding whole workspaces
/// invisible. Same lesson as `frontmostApplication` in `focused_window`.
pub fn set_app_hidden(pid: Pid, hidden: bool) {
    let el = unsafe { AXUIElement::new_application(pid.0) };
    unsafe {
        el.set_messaging_timeout(MESSAGING_TIMEOUT_SECS);
        set_bool(&el, "AXHidden", hidden);
    }
}

/// Whether the app is hidden, read live from its `AXHidden` — not
/// `NSRunningApplication.isHidden`, a cache this thread never refreshes (see
/// [`set_app_hidden`]).
pub fn app_hidden(pid: Pid) -> Option<bool> {
    let el = unsafe { AXUIElement::new_application(pid.0) };
    unsafe { el.set_messaging_timeout(MESSAGING_TIMEOUT_SECS) };
    let raw = unsafe { copy_attr(&el, "AXHidden") }?;
    let hidden = unsafe { &*(raw as *const CFBoolean) }.value();
    unsafe { sys::CFRelease(raw) };
    Some(hidden)
}

/// How long ONE un-hide may spend holding its parked windows at the corner
/// before it gives up and lets the next enforcement pass clean up.
///
/// The measurements it has to cover: a window re-enters the window server's
/// list at a median of 17-20ms after the un-hide (worst 63ms), and the hold
/// converges within 19ms of that at worst — call it 82ms end to end. 250ms is
/// three times that, and it is a per-app ceiling paid only by an app that
/// never converges, since the loop exits the moment the window server agrees.
/// The cost of exceeding it is bounded too: the un-hide simply reverts to
/// today's behaviour (the window sits visible until enforcement re-parks it),
/// which is why a generous bound is cheaper than a clever one. That relies on
/// the queues reporting an escaped window as not in flight (`app_queue`'s
/// `refused` on escape), or enforcement would wait on it.
const HOLD_BUDGET: Duration = Duration::from_millis(250);

/// What one un-hide's hold cost. `escaped` is the fact worth watching: a
/// window listed there was left on screen for enforcement to find.
pub struct HoldOutcome {
    pub pid: Pid,
    /// False when the app was already showing and was left alone.
    pub unhid: bool,
    pub writes: u32,
    pub elapsed_ms: u64,
    pub escaped: Vec<WindowId>,
    /// Held windows out of the hold's reach: gone from the window server,
    /// another app's, or not in the app's own window list.
    pub unreachable: Vec<WindowId>,
    /// Whether the app had `AXEnhancedUserInterface` on, so it was turned off
    /// for the hold and back on after.
    pub enhanced_ui: bool,
    /// The on-screen stack, front to back, at each step of this app's un-hide,
    /// in debug mode only (empty otherwise).
    /// A switch was seen lifting an app's windows above others' between its
    /// moves and the end of its un-hides; these name the step. Other apps'
    /// un-hides run alongside, so a step's read can include their effects.
    pub stacks: Vec<(&'static str, Vec<WindowId>)>,
}

/// Un-hide one app and keep writing `hold`'s origins until the window server
/// reports the windows there.
///
/// An un-hide is not the mirror of a hide. As the app's windows order back in,
/// AppKit runs `constrainFrameRect:toScreen:` over each one and drags every
/// window parked off the left edge fully back onto a display — measured
/// deterministic (96/96), and the flash on every workspace switch.
///
/// The chase must VERIFY, never assume. Issuing the re-park once, immediately
/// after the un-hide, was measured to end correctly parked in only 1-4 trials
/// of 12: the app processes our position write BEFORE it orders the window in,
/// and the order-in's constrain then overwrites it. Every one of those writes
/// returned success, so nothing but the window server's own answer can settle
/// whether the position stuck — which is exactly what a future "simplify this
/// loop into one write" would throw away.
pub fn show_app_holding(
    pid: Pid,
    hold: &[(WindowId, Point)],
    cancel: &dyn Fn() -> bool,
) -> HoldOutcome {
    let started = Instant::now();
    let mut stacks = Vec::new();
    let tracing = crate::debug::enabled();
    let mut mark = |step| {
        if tracing {
            stacks.push((step, super::zorder::stack_front_to_back()));
        }
    };
    let el = unsafe { AXUIElement::new_application(pid.0) };
    unsafe { el.set_messaging_timeout(MESSAGING_TIMEOUT_SECS) };
    // Only a window the window server has, as this app's, can be revealed by
    // this un-hide. An id the model holds for a window since closed, or
    // recycled by another app, is nothing to write to or wait for. A read
    // that comes back empty may have failed, and is no evidence.
    let ids: Vec<WindowId> = hold.iter().map(|(w, _)| *w).collect();
    let described = super::zorder::describe(&ids);
    let owned = |w: &WindowId| {
        described.is_empty()
            || described
                .iter()
                .any(|(d, owner, _)| d == w && *owner == pid.0)
    };
    let mut unreachable: Vec<WindowId> = ids.iter().filter(|w| !owned(w)).copied().collect();
    let hold: Vec<(WindowId, Point)> = hold.iter().filter(|(w, _)| owned(w)).copied().collect();
    let outcome = |unhid, writes, escaped, enhanced_ui, unreachable, stacks| HoldOutcome {
        pid,
        unhid,
        writes,
        elapsed_ms: started.elapsed().as_millis() as u64,
        escaped,
        unreachable,
        enhanced_ui,
        stacks,
    };
    // See `Desktop::show_apps`: an un-hide sent to a showing app brings all
    // its windows forward, so a showing app is never sent one. But showing
    // is not proof its parked windows stayed parked: any focus un-hides an
    // app, the restack worker's included, with nothing holding them. So
    // they are checked, and held if any left its spot. An
    // app that doesn't answer is un-hidden anyway — skipping a hidden one
    // would leave its windows invisible.
    let showing = app_hidden(pid) == Some(false);
    if showing && hold.iter().all(|(w, at)| holds_at(*w, *at)) {
        return outcome(false, 0, Vec::new(), false, unreachable, stacks);
    }
    if hold.is_empty() {
        // Nothing to lose to the reveal, and most un-hides are this: don't pay
        // an AXWindows walk (a round trip to the app) to discover it.
        mark("before un-hide");
        unsafe { set_bool(&el, "AXHidden", false) };
        mark("after un-hide");
        return outcome(true, 0, Vec::new(), false, unreachable, stacks);
    }

    // Resolve the window elements BEFORE the un-hide: an AXWindows walk is a
    // round trip to the app, and once the reveal is under way that round trip
    // queues behind the app's own order-in work — the very milliseconds the
    // hold is racing for. The arrays are borrowed from, so they stay alive
    // for the whole chase (same rule as `raise_sequenced`).
    let mut arrays: Vec<*const c_void> = Vec::new();
    let listed = listed_windows(&el, &mut arrays);
    let mut chase: Vec<(WindowId, Point, *const AXUIElement)> = Vec::new();
    let mut chased: Vec<(WindowId, Point)> = hold.clone();
    if let Some(listed) = &listed {
        resolve(listed, &mut chase, &mut chased, &mut unreachable);
    }
    if showing && chased.iter().all(|(w, at)| holds_at(*w, *at)) {
        release(arrays);
        return outcome(false, 0, Vec::new(), false, unreachable, stacks);
    }

    mark("before un-hide");
    let restore_eui = unsafe { disable_enhanced_ui(&el) };
    if restore_eui {
        mark("after enhanced UI off");
    }
    if !showing {
        unsafe { set_bool(&el, "AXHidden", false) };
        mark("after un-hide");
    }

    let deadline = started + HOLD_BUDGET;
    let mut writes = 0u32;
    // An app that didn't answer before the un-hide is asked once more now,
    // spending the hold's one re-read; failing that, nothing can be written,
    // and its windows are left to the check below.
    let mut reread = listed.is_none();
    if reread {
        if let Some(listed) = listed_windows(&el, &mut arrays) {
            resolve(&listed, &mut chase, &mut chased, &mut unreachable);
        }
    }
    loop {
        // A window absent from this read is mid-reveal and reads as "not
        // there yet", which is the right answer: keep writing. Absence is what
        // makes the loop safe to enter on a read — a hidden app's windows are
        // genuinely missing from `kCGWindowListOptionIncludingWindow` (unlike
        // the full list `existing_windows` uses, which keeps them), so the
        // first read cannot report success before the order-in has happened.
        chase.retain(|(w, at, _)| !holds_at(*w, *at));
        if chase.is_empty() || Instant::now() >= deadline || cancel() {
            break;
        }
        // No sleep between rounds — each write is a synchronous round trip to
        // the app, which is the only pacing this loop needs (~12 writes per
        // window over a converging chase).
        let mut refused: Vec<WindowId> = Vec::new();
        for (w, at, win) in &chase {
            writes += 1;
            if unsafe { set_point(&**win, "AXPosition", at.x, at.y) } != AXError::Success {
                refused.push(*w);
            }
        }
        if refused.is_empty() {
            continue;
        }
        // A refused write is a stale element or a closed window, and only a
        // fresh read tells which: the window is chased on through its fresh
        // element, or, no longer listed, it has closed. One re-read per hold,
        // as `AxApp::through` allows: against a hung app each costs a
        // messaging timeout. A window refused after that is dropped, and left
        // to the check below.
        let fresh = if reread {
            None
        } else {
            reread = true;
            listed_windows(&el, &mut arrays)
        };
        chase.retain_mut(|(w, _, win)| {
            if !refused.contains(w) {
                return true;
            }
            match fresh.as_ref().and_then(|f| f.get(w)) {
                Some(el) => {
                    *win = *el;
                    true
                }
                None => false,
            }
        });
        if let Some(fresh) = &fresh {
            for w in refused.iter().filter(|w| !fresh.contains_key(w)) {
                chased.retain(|(c, _)| c != w);
                unreachable.push(*w);
            }
        }
    }

    mark("after holds");
    unsafe {
        if restore_eui {
            set_bool(&el, "AXEnhancedUserInterface", true);
        }
    }
    release(arrays);
    if restore_eui {
        mark("after enhanced UI on");
    }

    // Asked fresh rather than inferred from the loop, so a window whose
    // writes kept being refused is reported as escaped rather than silently
    // counted as held.
    let escaped = chased
        .iter()
        .filter(|(w, at)| !holds_at(*w, *at))
        .map(|(w, _)| *w)
        .collect();
    outcome(!showing, writes, escaped, restore_eui, unreachable, stacks)
}

/// The app's window elements by id, borrowed from an AXWindows array that is
/// pushed onto `arrays` for the caller to release. `None` when the app didn't
/// answer, which says nothing about its windows, unlike an empty list.
fn listed_windows(
    el: &AXUIElement,
    arrays: &mut Vec<*const c_void>,
) -> Option<HashMap<WindowId, *const AXUIElement>> {
    let mut out = HashMap::new();
    let raw = unsafe { copy_attr(el, "AXWindows") }?;
    unsafe {
        for i in 0..super::cf::array_len(raw) {
            let win = super::cf::array_get(raw, i) as *const AXUIElement;
            if let Some(id) = window_id(win) {
                out.insert(id, win);
            }
        }
    }
    arrays.push(raw);
    Some(out)
}

/// Chase each window of `chased` through its element in the app's list. One
/// the app doesn't list can't be written, and it is not one the reveal orders
/// in: the window server keeps some helper windows alive (Chrome's omnibox
/// and find-bar popups, kitty's 64x64 one), and the model holds their ids.
/// Waited for, each read as escaped on every un-hide of its app.
fn resolve(
    listed: &HashMap<WindowId, *const AXUIElement>,
    chase: &mut Vec<(WindowId, Point, *const AXUIElement)>,
    chased: &mut Vec<(WindowId, Point)>,
    unreachable: &mut Vec<WindowId>,
) {
    chased.retain(|(w, at)| match listed.get(w) {
        Some(win) => {
            chase.push((*w, *at, *win));
            true
        }
        None => {
            unreachable.push(*w);
            false
        }
    });
}

fn release(arrays: Vec<*const c_void>) {
    for raw in arrays {
        unsafe { sys::CFRelease(raw) };
    }
}

/// Does the window server show this window at this origin? Position only, to
/// the point: a park lands exactly (see `ordo_emulated`'s `park_frame`), and
/// the window's size is its own business.
fn holds_at(w: WindowId, at: Point) -> bool {
    super::zorder::window_bounds(w)
        .is_some_and(|b| (b.x - at.x).abs() <= 1.0 && (b.y - at.y).abs() <= 1.0)
}

/// Unhide every regular app — the rescue path's counterpart to dimming, so a
/// kill switch never leaves apps invisible behind a dead daemon. Unconditional
/// writes: reading hidden-ness first would just add a failure mode.
pub fn unhide_all_apps() {
    let apps = NSWorkspace::sharedWorkspace().runningApplications();
    for app in apps.iter() {
        // Regular apps only: a background app Ordo manages is never hidden
        // (`has_dock_icon`, asked before every hide).
        if app.activationPolicy() != NSApplicationActivationPolicy::Regular {
            continue;
        }
        let pid = app.processIdentifier();
        if pid > 0 {
            set_app_hidden(Pid(pid), false);
        }
    }
}

/// Raise `targets` in exactly the given order, invoking `after_each` between
/// raises. The callback is the caller's chance to WAIT for the raise to
/// actually land (AXRaise is acknowledged by the app but applied on its own
/// schedule — issuing the next raise before the previous landed is how
/// cross-app stacking turns into a race), and its return value decides
/// whether the sequence continues — `false` stops before the next raise, so a
/// preempted caller never issues raises for an order it has abandoned. One
/// walk collects every element up front; the borrowed arrays stay alive for
/// the whole sequence.
pub fn raise_sequenced(targets: &[WindowId], mut after_each: impl FnMut(WindowId) -> bool) {
    // The window elements are borrowed from their app's AXWindows array, so
    // every array stays alive until all raises are done.
    let mut arrays: Vec<*const c_void> = Vec::new();
    let mut found: HashMap<WindowId, *const AXUIElement> = HashMap::new();
    let apps = NSWorkspace::sharedWorkspace().runningApplications();
    for app in apps.iter() {
        if !managed(&app) {
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
        arrays.push(raw);
        unsafe {
            for i in 0..super::cf::array_len(raw) {
                let win = super::cf::array_get(raw, i) as *const AXUIElement;
                if let Some(id) = window_id(win) {
                    if targets.contains(&id) {
                        found.insert(id, win);
                    }
                }
            }
        }
    }
    let raise = CFString::from_str("AXRaise");
    for t in targets {
        if let Some(win) = found.get(t) {
            unsafe {
                let _ = (**win).perform_action(&raise);
            }
            if !after_each(*t) {
                break;
            }
        }
    }
    for raw in arrays {
        unsafe { sys::CFRelease(raw) };
    }
}

/// Raises windows, collecting an app's window elements the first time one of
/// its windows is raised, so a restack asks only the apps it raises in rather
/// than every running app. The elements are borrowed from each app's
/// `AXWindows` array, which stays alive until the raiser drops.
#[derive(Default)]
pub struct Raiser {
    asked: Vec<i32>,
    arrays: Vec<*const c_void>,
    found: HashMap<WindowId, *const AXUIElement>,
}

impl Raiser {
    /// Issue an `AXRaise`; false when the window has no element (it closed).
    pub fn raise(&mut self, w: WindowId, pid: i32) -> bool {
        if !self.asked.contains(&pid) {
            let el = unsafe { AXUIElement::new_application(pid) };
            unsafe { el.set_messaging_timeout(MESSAGING_TIMEOUT_SECS) };
            // A failed read is asked again on the next raise: a busy app's
            // timeout says nothing about whether its windows exist.
            if let Some(raw) = unsafe { copy_attr(&el, "AXWindows") } {
                self.asked.push(pid);
                self.arrays.push(raw);
                unsafe {
                    for i in 0..super::cf::array_len(raw) {
                        let win = super::cf::array_get(raw, i) as *const AXUIElement;
                        if let Some(id) = window_id(win) {
                            self.found.insert(id, win);
                        }
                    }
                }
            }
        }
        let Some(&win) = self.found.get(&w) else {
            return false;
        };
        let raise = CFString::from_str("AXRaise");
        unsafe {
            let _ = (*win).perform_action(&raise);
        }
        true
    }
}

impl Drop for Raiser {
    fn drop(&mut self) {
        for raw in self.arrays.drain(..) {
            unsafe { sys::CFRelease(raw) };
        }
    }
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
            let _ = set_point(win, "AXPosition", frame.x, frame.y);
            set_size_attr(win, frame.w, frame.h);
            let _ = set_point(win, "AXPosition", frame.x, frame.y);
            if restore_eui {
                set_bool(app, "AXEnhancedUserInterface", true);
            }
        }
    })
    .is_some()
}

/// One app's handles, kept by that app's queue thread between jobs: the app
/// element, and its window elements, read once and re-read when a window is
/// missing from them or a write through one fails. A queue works through
/// many jobs for one app, and each fresh window-list read was a round trip
/// to an app that, mid-switch, is busy with the jobs before it.
pub struct AxApp {
    pid: i32,
    el: objc2_core_foundation::CFRetained<AXUIElement>,
    /// The app's AXWindows array; the elements below are borrowed from it,
    /// so it lives exactly as long as they are used.
    windows: Option<*const c_void>,
    by_id: HashMap<WindowId, *const AXUIElement>,
}

impl AxApp {
    pub fn open(pid: Pid) -> Self {
        let el = unsafe { AXUIElement::new_application(pid.0) };
        unsafe { el.set_messaging_timeout(MESSAGING_TIMEOUT_SECS) };
        AxApp {
            pid: pid.0,
            el,
            windows: None,
            by_id: HashMap::new(),
        }
    }

    fn reread(&mut self) {
        self.by_id.clear();
        if let Some(raw) = self.windows.take() {
            unsafe { sys::CFRelease(raw) };
        }
        let Some(raw) = (unsafe { copy_attr(&self.el, "AXWindows") }) else {
            return;
        };
        unsafe {
            for i in 0..super::cf::array_len(raw) {
                let win = super::cf::array_get(raw, i) as *const AXUIElement;
                if let Some(id) = window_id(win) {
                    self.by_id.insert(id, win);
                }
            }
        }
        self.windows = Some(raw);
    }

    /// The window's element, and whether the elements were just re-read to
    /// find it. An element is only trusted while it still names its window:
    /// ids are recycled, and a cached element can outlive its window.
    fn element(&mut self, w: WindowId) -> (Option<*const AXUIElement>, bool) {
        if let Some(win) = self.by_id.get(&w).copied() {
            if window_id(win) == Some(w) {
                return (Some(win), false);
            }
        }
        self.reread();
        (self.by_id.get(&w).copied(), true)
    }

    /// Run `write` through the window's element, re-reading the elements and
    /// trying once more if it fails: the element may be stale while the
    /// window lives on. Never a second re-read: against a hung app each one
    /// costs a messaging timeout.
    fn through(&mut self, w: WindowId, write: impl Fn(&AXUIElement) -> bool) -> bool {
        let (win, fresh) = self.element(w);
        if let Some(win) = win {
            if write(unsafe { &*win }) {
                return true;
            }
        }
        if fresh {
            return false;
        }
        self.reread();
        self.by_id
            .get(&w)
            .is_some_and(|win| write(unsafe { &**win }))
    }
}

impl Drop for AxApp {
    fn drop(&mut self) {
        if let Some(raw) = self.windows.take() {
            unsafe { sys::CFRelease(raw) };
        }
    }
}

impl crate::app_queue::AppSession for AxApp {
    fn move_windows(
        &mut self,
        moves: &[(WindowId, Point)],
        cancel: &dyn Fn() -> bool,
    ) -> Vec<(WindowId, bool, f64)> {
        // One enhanced-UI bracket around the whole batch, not one per window:
        // without it Chromium/Electron windows move slowly and land wrong.
        let restore_eui = unsafe { disable_enhanced_ui(&self.el) };
        let mut out = Vec::with_capacity(moves.len());
        for (w, to) in moves {
            if cancel() {
                break;
            }
            let t = Instant::now();
            let took = self.through(*w, |win| unsafe {
                set_point(win, "AXPosition", to.x, to.y) == AXError::Success
            });
            out.push((*w, took, t.elapsed().as_secs_f64() * 1000.0));
        }
        if restore_eui {
            unsafe { set_bool(&self.el, "AXEnhancedUserInterface", true) };
        }
        out
    }

    /// The position -> size -> position idiom of [`set_frame`], for the same
    /// reason: apps clamp a size to the current screen before a cross-display
    /// move lands.
    fn set_frame(&mut self, window: WindowId, to: Rect) -> bool {
        let restore_eui = unsafe { disable_enhanced_ui(&self.el) };
        let found = self.through(window, |win| unsafe {
            let first = set_point(win, "AXPosition", to.x, to.y) == AXError::Success;
            set_size_attr(win, to.w, to.h);
            let _ = set_point(win, "AXPosition", to.x, to.y);
            first
        });
        if restore_eui {
            unsafe { set_bool(&self.el, "AXEnhancedUserInterface", true) };
        }
        found
    }

    fn show(&mut self, hold: &[(WindowId, Point)], cancel: &dyn Fn() -> bool) -> HoldStat {
        let o = show_app_holding(Pid(self.pid), hold, cancel);
        if !o.escaped.is_empty() {
            eprintln!(
                "ordo: un-hiding pid {} left {} window(s) off the corner after {}ms",
                o.pid.0,
                o.escaped.len(),
                o.elapsed_ms
            );
        }
        HoldStat::new(o.pid, o.unhid, hold.len(), o.writes, o.elapsed_ms, o.escaped)
            .with_steps(
                o.enhanced_ui,
                o.stacks
                    .into_iter()
                    .map(|(step, ids)| (step.to_string(), ids))
                    .collect(),
            )
            .with_unreachable(o.unreachable)
    }

    fn hide(&mut self) {
        unsafe { set_bool(&self.el, "AXHidden", true) };
    }

    fn find(&mut self, window: WindowId) -> bool {
        self.element(window).0.is_some()
    }

    fn front(&mut self, window: WindowId) {
        front_process(self.pid, window.0);
    }

    fn make_key(&mut self, window: WindowId) {
        if let Some(win) = self.element(window).0 {
            unsafe { make_element_key(&*win) };
        }
    }

    fn front_desktop(&mut self, window: u32) {
        front_process(self.pid, window);
    }
}

/// Find the window whose id is `target` and run `f(app_element, window_ptr,
/// pid)` while both handles are live. `None` if the window isn't found (gone,
/// or an app with no AX tree).
///
/// The window server names the owner, so normally one app is asked. Asking
/// every app in turn is kept only as the fallback: each ask is a round trip
/// to an app's main thread, and mid-switch those are busy.
fn with_window<T>(
    target: WindowId,
    f: impl FnOnce(&AXUIElement, *const AXUIElement, i32) -> T,
) -> Option<T> {
    let owner = super::zorder::owner_of(target);
    let mut f = Some(f);
    if let Some(pid) = owner {
        if let Some(out) = with_window_of(pid, target, &mut f) {
            return Some(out);
        }
    }
    let apps = NSWorkspace::sharedWorkspace().runningApplications();
    for app in apps.iter() {
        if !managed(&app) {
            continue;
        }
        let pid = app.processIdentifier();
        if pid <= 0 || Some(pid) == owner {
            continue;
        }
        if let Some(out) = with_window_of(pid, target, &mut f) {
            return Some(out);
        }
    }
    None
}

/// `f` is taken only when the window is found, so a miss leaves it for the
/// next app.
fn with_window_of<T, F>(pid: i32, target: WindowId, f: &mut Option<F>) -> Option<T>
where
    F: FnOnce(&AXUIElement, *const AXUIElement, i32) -> T,
{
    let el = unsafe { AXUIElement::new_application(pid) };
    unsafe { el.set_messaging_timeout(MESSAGING_TIMEOUT_SECS) };
    let raw = unsafe { copy_attr(&el, "AXWindows") }?;
    let mut out = None;
    unsafe {
        for i in 0..super::cf::array_len(raw) {
            let win = super::cf::array_get(raw, i) as *const AXUIElement;
            if window_id(win) == Some(target) {
                out = f.take().map(|f| f(&el, win, pid));
                break;
            }
        }
        sys::CFRelease(raw);
    }
    out
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

/// The error is returned, not swallowed, for the one caller that can act on
/// it: the un-hide hold, where a refused write means the window is gone and
/// the chase should stop asking. Everywhere else a failed move is nothing a
/// caller could do better than the next enforcement pass will.
unsafe fn set_point(el: &AXUIElement, name: &str, x: f64, y: f64) -> AXError {
    let mut p = CGPoint { x, y };
    let Some(val) = AXValue::new(
        AXValueType::CGPoint,
        NonNull::new(&mut p as *mut _ as *mut c_void).unwrap(),
    ) else {
        return AXError::Failure;
    };
    let attr = CFString::from_str(name);
    el.set_attribute_value(&attr, val.as_ref())
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

#[cfg(test)]
mod tests {
    use super::manages;

    #[test]
    fn every_regular_app_and_only_the_named_background_ones_are_managed() {
        assert!(manages(true, Some("com.google.Chrome")));
        assert!(manages(true, None));
        assert!(manages(false, Some("com.apple.screencaptureui")));
        assert!(!manages(false, Some("com.raycast.macos")));
        assert!(!manages(false, None));
    }

    #[test]
    fn a_background_apps_window_is_managed_only_at_an_ordinary_layer() {
        use super::admits;
        assert!(admits(false, Some(3)), "the screenshot window");
        assert!(!admits(false, Some(1499)), "its capture bar");
        assert!(!admits(false, None), "a layer never learned");
        assert!(admits(true, Some(1499)), "a regular app's panel, as before");
    }
}
