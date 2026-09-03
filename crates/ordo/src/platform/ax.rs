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

use std::collections::HashMap;
use std::ffi::c_void;
use std::ptr::NonNull;
use std::time::{Duration, Instant};

use objc2_app_kit::{NSApplicationActivationPolicy, NSWorkspace};
use objc2_application_services::{AXError, AXUIElement, AXValue, AXValueType};
use objc2_core_foundation::{CFBoolean, CFString, CFType, CGPoint, CGSize};
use ordo_core::{Pid, Point, Rect, WindowId};
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
}

pub struct AxScan {
    pub windows: Vec<AxWindow>,
    pub focused: Option<WindowId>,
}

/// Enumerate every standard window of every regular (Dock-visible) app, plus
/// which window currently has focus.
pub fn scan() -> AxScan {
    AxScan {
        focused: focused_window(),
        windows: windows(),
    }
}

/// The window half of [`scan`], for callers who don't need focus (asking every
/// app "are you frontmost?" is a second full round of IPC).
pub fn windows() -> Vec<AxWindow> {
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

    windows
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
    })
}

pub fn focused_window() -> Option<WindowId> {
    // Find the front app by asking each app's live `AXFrontmost` attribute —
    // NOT NSWorkspace.frontmostApplication, which is a cache that refreshes
    // only when a run loop pumps (the engine thread never pumps one, so it
    // would report the frontmost app from boot forever). The system-wide
    // element's AXFocusedApplication would be cleaner but returns
    // kAXErrorCannotComplete here (observed on Tahoe).
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
        let Some(front) = (unsafe { copy_attr(&el, "AXFrontmost") }) else {
            continue;
        };
        let is_front = unsafe { &*(front as *const CFBoolean) }.value();
        unsafe { sys::CFRelease(front) };
        if !is_front {
            continue;
        }
        let focused = unsafe { copy_attr(&el, "AXFocusedWindow") }?;
        let id = window_id(focused as *const AXUIElement);
        unsafe { sys::CFRelease(focused) };
        return id;
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
    with_window(target, |_app, win, pid| {
        unsafe {
            let mut psn = sys::ProcessSerialNumber::default();
            if sys::GetProcessForPID(pid, &mut psn) == 0 {
                let _ = sys::SLPSSetFrontProcessWithOptions(&psn, target.0, sys::kCPSUserGenerated);
                make_key_window(&psn, target.0);
            }
        }
        let win = unsafe { &*win };
        unsafe {
            set_bool(win, "AXMain", true);
            set_bool(win, "AXFocused", true);
            let raise = CFString::from_str("AXRaise");
            let _ = win.perform_action(&raise);
        }
    })
    .is_some()
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
/// which is why a generous bound is cheaper than a clever one.
const HOLD_BUDGET: Duration = Duration::from_millis(250);

/// What one un-hide's hold cost. `escaped` is the fact worth watching: a
/// window listed there was left on screen for enforcement to find.
pub struct HoldOutcome {
    pub pid: Pid,
    pub writes: u32,
    pub elapsed_ms: u64,
    pub escaped: Vec<WindowId>,
}

/// Un-hide these apps, holding each one's listed windows at the given origins
/// through the reveal.
///
/// An un-hide is not the mirror of a hide. As the app's windows order back in,
/// AppKit runs `constrainFrameRect:toScreen:` over each one and drags every
/// window parked off the left edge fully back onto a display — measured
/// deterministic (96/96), and the flash on every workspace switch.
///
/// Threaded per app for the same reason [`move_windows`] is: a switch un-hides
/// several apps at once and must cost the slowest app's reveal, not the sum of
/// them.
pub fn show_apps(apps: &[(Pid, &[(WindowId, Point)])]) -> Vec<HoldOutcome> {
    std::thread::scope(|scope| {
        let running: Vec<_> = apps
            .iter()
            .map(|(pid, hold)| scope.spawn(move || show_app_holding(*pid, hold)))
            .collect();
        running.into_iter().filter_map(|h| h.join().ok()).collect()
    })
}

/// Un-hide one app and keep writing `hold`'s origins until the window server
/// reports the windows there.
///
/// The chase must VERIFY, never assume. Issuing the re-park once, immediately
/// after the un-hide, was measured to end correctly parked in only 1-4 trials
/// of 12: the app processes our position write BEFORE it orders the window in,
/// and the order-in's constrain then overwrites it. Every one of those writes
/// returned success, so nothing but the window server's own answer can settle
/// whether the position stuck — which is exactly what a future "simplify this
/// loop into one write" would throw away.
fn show_app_holding(pid: Pid, hold: &[(WindowId, Point)]) -> HoldOutcome {
    let started = Instant::now();
    let el = unsafe { AXUIElement::new_application(pid.0) };
    unsafe { el.set_messaging_timeout(MESSAGING_TIMEOUT_SECS) };
    if hold.is_empty() {
        // Nothing to lose to the reveal, and most un-hides are this: don't pay
        // an AXWindows walk (a round trip to the app) to discover it.
        unsafe { set_bool(&el, "AXHidden", false) };
        return HoldOutcome {
            pid,
            writes: 0,
            elapsed_ms: started.elapsed().as_millis() as u64,
            escaped: Vec::new(),
        };
    }

    // Resolve the window elements BEFORE the un-hide: an AXWindows walk is a
    // round trip to the app, and once the reveal is under way that round trip
    // queues behind the app's own order-in work — the very milliseconds the
    // hold is racing for. The array is borrowed from, so it stays alive for
    // the whole chase (same rule as `raise_sequenced`).
    let raw = unsafe { copy_attr(&el, "AXWindows") };
    let mut chase: Vec<(WindowId, Point, *const AXUIElement)> = Vec::new();
    if let Some(raw) = raw {
        unsafe {
            for i in 0..super::cf::array_len(raw) {
                let win = super::cf::array_get(raw, i) as *const AXUIElement;
                let Some(id) = window_id(win) else { continue };
                if let Some((_, at)) = hold.iter().find(|(w, _)| *w == id) {
                    chase.push((id, *at, win));
                }
            }
        }
    }

    let restore_eui = unsafe { disable_enhanced_ui(&el) };
    unsafe { set_bool(&el, "AXHidden", false) };

    let deadline = started + HOLD_BUDGET;
    let mut writes = 0u32;
    loop {
        // A window absent from this read is mid-reveal and reads as "not
        // there yet", which is the right answer: keep writing. Absence is what
        // makes the loop safe to enter on a read — a hidden app's windows are
        // genuinely missing from `kCGWindowListOptionIncludingWindow` (unlike
        // the full list `existing_windows` uses, which keeps them), so the
        // first read cannot report success before the order-in has happened.
        chase.retain(|(w, at, _)| !holds_at(*w, *at));
        if chase.is_empty() || Instant::now() >= deadline {
            break;
        }
        // No sleep between rounds — each write is a synchronous round trip to
        // the app, which is the only pacing this loop needs (~12 writes per
        // window over a converging chase). A write the app REFUSES is a window
        // that no longer exists; dropping it is what keeps a dead handle from
        // spinning here until the budget runs out.
        chase.retain(|(_, at, win)| {
            writes += 1;
            unsafe { set_point(&**win, "AXPosition", at.x, at.y) == AXError::Success }
        });
    }

    unsafe {
        if restore_eui {
            set_bool(&el, "AXEnhancedUserInterface", true);
        }
        if let Some(raw) = raw {
            sys::CFRelease(raw);
        }
    }

    // Asked fresh rather than inferred from the loop, so a window that was
    // never found in AXWindows or whose handle went stale is reported as
    // escaped rather than silently counted as held.
    HoldOutcome {
        pid,
        writes,
        elapsed_ms: started.elapsed().as_millis() as u64,
        escaped: hold
            .iter()
            .filter(|(w, at)| !holds_at(*w, *at))
            .map(|(w, _)| *w)
            .collect(),
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

/// Move a batch of windows to new origins, grouped by owning app, one thread
/// per app. Sizes are never touched: unlike [`set_frame`]'s cross-display move,
/// this is the emulated backend's park/restore path, where a window's size is
/// the window's own business and a size write is only an opportunity for the
/// WindowServer to cap it (see `ordo_emulated::Desktop::move_windows`).
///
/// The batching exists because a switch is only as instant as its slowest
/// serialization: one `set_frame` per window re-walks every app's window list
/// per write, so a multi-window switch rippled visibly across the screen (and
/// across monitors, one after the other). Grouping gives one walk per app; the
/// per-app threads make the whole batch land in the time of the slowest *app*,
/// not the sum of all writes. AX is just Mach IPC and safe off the main thread —
/// each thread builds its own app element rather than sharing one.
pub fn move_windows(moves: &[(Pid, WindowId, Point)]) {
    let mut by_app: HashMap<i32, Vec<(WindowId, Point)>> = HashMap::new();
    for (pid, w, p) in moves {
        by_app.entry(pid.0).or_default().push((*w, *p));
    }
    std::thread::scope(|scope| {
        for (pid, wins) in by_app {
            scope.spawn(move || move_app_windows(pid, &wins));
        }
    });
}

fn move_app_windows(pid: i32, wins: &[(WindowId, Point)]) {
    let el = unsafe { AXUIElement::new_application(pid) };
    unsafe { el.set_messaging_timeout(MESSAGING_TIMEOUT_SECS) };
    let Some(raw) = (unsafe { copy_attr(&el, "AXWindows") }) else {
        return;
    };
    // One EUI bracket around the whole app's batch, not one per window.
    let restore_eui = unsafe { disable_enhanced_ui(&el) };
    unsafe {
        for i in 0..super::cf::array_len(raw) {
            let win = super::cf::array_get(raw, i) as *const AXUIElement;
            let Some(id) = window_id(win) else { continue };
            let Some((_, at)) = wins.iter().find(|(w, _)| *w == id) else {
                continue;
            };
            let _ = set_point(&*win, "AXPosition", at.x, at.y);
        }
        if restore_eui {
            set_bool(&el, "AXEnhancedUserInterface", true);
        }
        sys::CFRelease(raw);
    }
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
