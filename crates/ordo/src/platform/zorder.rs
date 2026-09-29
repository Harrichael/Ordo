//! Global window z-order: read it, and impose a desired order on it.
//!
//! Reading is public CoreGraphics (`CGWindowListCopyWindowInfo` returns
//! on-screen windows front-to-back). Writing is not: WindowServer refuses
//! `SLSOrderWindow` on a foreign window from a daemon connection (error 1000,
//! probed in examples/order_probe.rs — yabai needs its SIP-off Dock payload
//! for the direct route). What *is* honored from a daemon is `AXRaise`, which
//! moves a window in the global stack (examples/raise_probe.rs) — so an
//! arbitrary order is spelled "raise each one, back-to-front".
//!
//! It is also where the cheap per-window window-server reads live — existence
//! and bounds, neither of them z-order, but both the same list read and the
//! same "no round trip to the app" property that makes the ordering gates
//! affordable.
//!
//! One Tahoe quirk is a feature here: raises land BELOW the active app's key
//! window, so callers should hand focus to the intended top window *before*
//! restacking — the raises then slot in under it.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use ordo_core::{Rect, WindowId};
use ordo_skylight_sys as sys;

use crate::ports::RestackStats;
use crate::restack::{self, Seen, WindowServer};

use super::ws_events::{RaiseSignals, WaitOutcome};
use super::{ax, cf};

const ON_SCREEN_ONLY: u32 = 1 << 0;
const EXCLUDE_DESKTOP: u32 = 1 << 4;

#[cfg_attr(target_os = "macos", link(name = "CoreGraphics", kind = "framework"))]
extern "C" {
    fn CGWindowListCopyWindowInfo(option: u32, relative_to: u32) -> sys::CFArrayRef;
}

/// On-screen normal (layer-0) windows with their owning pids, front to back.
/// Pids come from the same CG read (`kCGWindowOwnerPID`) — no AX involved.
pub fn stack_with_pids() -> Vec<(u32, i32)> {
    let mut out = Vec::new();
    unsafe {
        let arr = CGWindowListCopyWindowInfo(ON_SCREEN_ONLY | EXCLUDE_DESKTOP, 0);
        if arr.is_null() {
            return out;
        }
        for i in 0..cf::array_len(arr) {
            let d = cf::array_get(arr, i) as sys::CFDictionaryRef;
            if cf::number_i64(cf::dict_get(d, "kCGWindowLayer")) != Some(0) {
                continue;
            }
            let wid = cf::number_i64(cf::dict_get(d, "kCGWindowNumber"));
            let pid = cf::number_i64(cf::dict_get(d, "kCGWindowOwnerPID"));
            if let (Some(w), Some(p)) = (wid, pid) {
                out.push((w as u32, p as i32));
            }
        }
        sys::CFRelease(arr);
    }
    out
}

/// ALL windows the window server knows — including windows of Cmd+H-hidden
/// apps, which is what makes this usable as death evidence for parked windows
/// (dock dimming hides exactly their apps; an on-screen-only read would
/// report them dead). No layer filter, unlike the stacking reads above: for
/// an existence question a filter can only manufacture false deaths, never
/// remove a false alive. `None` when the read fails or comes back empty: an
/// empty full list is a failed read, not a desktop with no windows.
pub fn all_windows() -> Option<Vec<WindowId>> {
    let mut out = Vec::new();
    unsafe {
        let arr = CGWindowListCopyWindowInfo(EXCLUDE_DESKTOP, 0);
        if arr.is_null() {
            return None;
        }
        for i in 0..cf::array_len(arr) {
            let d = cf::array_get(arr, i) as sys::CFDictionaryRef;
            if let Some(wid) = cf::number_i64(cf::dict_get(d, "kCGWindowNumber")) {
                out.push(WindowId(wid as u32));
            }
        }
        sys::CFRelease(arr);
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

/// What the window server knows about a window that the apps don't say: its
/// layer (0 for normal windows; panels and notification-style windows sit
/// higher) and the window it is a child of, if any.
pub struct ServerFacts {
    pub layer: Option<i32>,
    pub parent: Option<WindowId>,
}

/// [`ServerFacts`] for these windows: one list read for the layers, one
/// window query for the parents. A window the server doesn't know, or a query
/// that isn't available on this macOS, just leaves its fact unknown.
pub fn server_facts(windows: &[WindowId]) -> HashMap<WindowId, ServerFacts> {
    let mut out: HashMap<WindowId, ServerFacts> = HashMap::new();
    unsafe {
        let arr = CGWindowListCopyWindowInfo(EXCLUDE_DESKTOP, 0);
        if !arr.is_null() {
            for i in 0..cf::array_len(arr) {
                let d = cf::array_get(arr, i) as sys::CFDictionaryRef;
                let Some(wid) = cf::number_i64(cf::dict_get(d, "kCGWindowNumber")) else {
                    continue;
                };
                let w = WindowId(wid as u32);
                if windows.contains(&w) {
                    let layer = cf::number_i64(cf::dict_get(d, "kCGWindowLayer")).map(|l| l as i32);
                    out.insert(w, ServerFacts { layer, parent: None });
                }
            }
            sys::CFRelease(arr);
        }
    }
    static QUERY: std::sync::OnceLock<Option<sys::WindowQuery>> = std::sync::OnceLock::new();
    let (Some(q), false) = (*QUERY.get_or_init(sys::WindowQuery::resolve), windows.is_empty()) else {
        return out;
    };
    unsafe {
        let Some(ids) = super::skylight::make_number_array(windows) else {
            return out;
        };
        let query = (q.query_windows)(sys::SLSMainConnectionID(), ids, 0);
        sys::CFRelease(ids);
        if query.is_null() {
            return out;
        }
        let iter = (q.copy_windows)(query);
        if !iter.is_null() {
            while (q.advance)(iter) {
                let w = WindowId((q.window_id)(iter));
                let parent = (q.parent_id)(iter);
                if let Some(f) = out.get_mut(&w) {
                    f.parent = (parent != 0).then_some(WindowId(parent));
                }
            }
            sys::CFRelease(iter);
        }
        sys::CFRelease(query);
    }
    out
}

/// Where the WINDOW SERVER says one window is — `None` while it doesn't know
/// the window at all.
///
/// `kCGWindowListOptionIncludingWindow` alone, relative to the window itself:
/// one descriptor, on screen or not, with no round trip to the owning app.
/// That is what makes it safe to poll during an un-hide, when an AX read would
/// queue behind the very order-in work being watched for — and it is the
/// instrument the un-hide measurements were taken with. The `None` is
/// meaningful there too: an un-hiding app's windows are missing from the list
/// until they order back in.
pub fn window_bounds(w: WindowId) -> Option<Rect> {
    const INCLUDING_WINDOW: u32 = 1 << 3;
    unsafe {
        let arr = CGWindowListCopyWindowInfo(INCLUDING_WINDOW, w.0);
        if arr.is_null() {
            return None;
        }
        let out = (cf::array_len(arr) > 0)
            .then(|| {
                let d = cf::array_get(arr, 0) as sys::CFDictionaryRef;
                let b = cf::dict_get(d, "kCGWindowBounds");
                Some(Rect {
                    x: cf::number_f64(cf::dict_get(b, "X"))?,
                    y: cf::number_f64(cf::dict_get(b, "Y"))?,
                    w: cf::number_f64(cf::dict_get(b, "Width"))?,
                    h: cf::number_f64(cf::dict_get(b, "Height"))?,
                })
            })
            .flatten();
        sys::CFRelease(arr);
        out
    }
}

/// The window that draws a display's desktop — Finder's, at the desktop-icon
/// level, covering exactly the display — with its owner's pid. `None` when
/// there is none, e.g. with Finder's desktop turned off.
pub fn desktop_window_on(display: Rect) -> Option<(u32, i32)> {
    // kCGDesktopIconWindowLevel.
    const DESKTOP_ICON_LEVEL: i64 = -2147483603;
    unsafe {
        let arr = CGWindowListCopyWindowInfo(ON_SCREEN_ONLY, 0);
        if arr.is_null() {
            return None;
        }
        let mut out = None;
        for i in 0..cf::array_len(arr) {
            let d = cf::array_get(arr, i) as sys::CFDictionaryRef;
            if cf::number_i64(cf::dict_get(d, "kCGWindowLayer")) != Some(DESKTOP_ICON_LEVEL) {
                continue;
            }
            let b = cf::dict_get(d, "kCGWindowBounds");
            let bounds = (|| {
                Some(Rect {
                    x: cf::number_f64(cf::dict_get(b, "X"))?,
                    y: cf::number_f64(cf::dict_get(b, "Y"))?,
                    w: cf::number_f64(cf::dict_get(b, "Width"))?,
                    h: cf::number_f64(cf::dict_get(b, "Height"))?,
                })
            })();
            if !bounds.is_some_and(|r| r.approx_eq(&display, 1.0)) {
                continue;
            }
            let wid = cf::number_i64(cf::dict_get(d, "kCGWindowNumber"));
            let pid = cf::number_i64(cf::dict_get(d, "kCGWindowOwnerPID"));
            if let (Some(w), Some(p)) = (wid, pid) {
                out = Some((w as u32, p as i32));
                break;
            }
        }
        sys::CFRelease(arr);
        out
    }
}

/// On-screen normal (layer-0) windows, front to back.
pub fn stack_front_to_back() -> Vec<WindowId> {
    let mut out = Vec::new();
    unsafe {
        let arr = CGWindowListCopyWindowInfo(ON_SCREEN_ONLY | EXCLUDE_DESKTOP, 0);
        if arr.is_null() {
            return out;
        }
        for i in 0..cf::array_len(arr) {
            let d = cf::array_get(arr, i) as sys::CFDictionaryRef;
            if cf::number_i64(cf::dict_get(d, "kCGWindowLayer")) != Some(0) {
                continue;
            }
            if let Some(wid) = cf::number_i64(cf::dict_get(d, "kCGWindowNumber")) {
                out.push(WindowId(wid as u32));
            }
        }
        sys::CFRelease(arr);
    }
    out
}

/// On-screen layer-0 windows with their pids and frames, front to back, from
/// one list read: the frames ride in the same descriptors as the order.
pub fn read_stack(out: &mut Vec<Seen>) {
    out.clear();
    unsafe {
        let arr = CGWindowListCopyWindowInfo(ON_SCREEN_ONLY | EXCLUDE_DESKTOP, 0);
        if arr.is_null() {
            return;
        }
        for i in 0..cf::array_len(arr) {
            let d = cf::array_get(arr, i) as sys::CFDictionaryRef;
            if cf::number_i64(cf::dict_get(d, "kCGWindowLayer")) != Some(0) {
                continue;
            }
            let b = cf::dict_get(d, "kCGWindowBounds");
            let seen = (|| {
                Some(Seen {
                    id: WindowId(cf::number_i64(cf::dict_get(d, "kCGWindowNumber"))? as u32),
                    pid: cf::number_i64(cf::dict_get(d, "kCGWindowOwnerPID"))? as i32,
                    frame: Rect {
                        x: cf::number_f64(cf::dict_get(b, "X"))?,
                        y: cf::number_f64(cf::dict_get(b, "Y"))?,
                        w: cf::number_f64(cf::dict_get(b, "Width"))?,
                        h: cf::number_f64(cf::dict_get(b, "Height"))?,
                    },
                })
            })();
            out.extend(seen);
        }
        sys::CFRelease(arr);
    }
}

/// Impose `desired` (front to back) wherever those windows overlap; see
/// [`crate::restack`] for the plan and the lanes.
///
/// The only per-window lever a SIP-on daemon has is `AXRaise` (probed:
/// SLSOrderWindow is refused with error 1000; SLPS make-key records don't
/// reorder; a burst of SLPSSetFrontProcessWithOptions coalesces to the last
/// call — see examples/order_probe.rs and slps_order_probe.rs). AXRaise is
/// processed on the target APP's schedule (Chromium: 100ms+), so raises to
/// different apps land in arbitrary relative order: within a lane, each raise
/// is confirmed landed, read back from the window server, before the next.
///
/// Raise physics (examples/slps_sibling_probe.rs, refuting AeroSpace #395 on
/// Tahoe): a background app's window raises to just below the key window, but
/// a sibling of the key window raises ABOVE it, and raising the key window
/// again freezes the siblings beneath it.
///
/// `signals`, when present, is the WindowServer's push stream (808/815): a
/// gate sleeps until a hint instead of a fixed tick, and wakes read back
/// exactly as before — events cut the latency between a landing and our
/// seeing it to ~zero, they never replace the CG read as the authority. With
/// `None` (probes) every gate is the classic 5ms poll.
pub fn reassert_stack(
    desired: &[WindowId],
    focus_top: bool,
    cancel: &dyn Fn() -> bool,
    signals: Option<&RaiseSignals>,
) -> Option<RestackStats> {
    let mut live = Live {
        gate: Gate::new(signals),
        raiser: ax::Raiser::default(),
    };
    restack::reassert(&mut live, desired, focus_top, cancel)
}

struct Live<'a> {
    gate: Gate<'a>,
    raiser: ax::Raiser,
}

impl WindowServer for Live<'_> {
    fn now(&self) -> Instant {
        Instant::now()
    }

    fn read_stack(&mut self, out: &mut Vec<Seen>) {
        read_stack(out);
    }

    fn displays(&mut self) -> Vec<Rect> {
        super::display::active_displays()
            .into_iter()
            .map(|d| d.frame)
            .collect()
    }

    fn is_key(&mut self, w: WindowId, pid: i32) -> bool {
        ax::is_key(w, pid)
    }

    fn focused_window(&mut self) -> Option<WindowId> {
        ax::focused_window()
    }

    fn focus(&mut self, w: WindowId) -> bool {
        ax::focus(w)
    }

    fn raise(&mut self, w: WindowId, pid: i32) -> bool {
        self.raiser.raise(w, pid)
    }

    fn wait(&mut self, until: Instant, cancel: &dyn Fn() -> bool) -> bool {
        self.gate.wait(until, cancel)
    }
}

/// One sleep between condition re-checks. With a push stream, the sleep ends
/// the instant a hint arrives — capped by a poll fallback so a dead or deaf
/// stream (a window not yet opted in) degrades to polling, never to a stall.
/// Without one (probes), the classic 5ms tick. The cursor rides along so one
/// stream serves every gate of a reassert without re-reading old entries.
struct Gate<'a> {
    signals: Option<&'a RaiseSignals>,
    cursor: u64,
}

impl<'a> Gate<'a> {
    fn new(signals: Option<&'a RaiseSignals>) -> Self {
        Gate {
            signals,
            cursor: signals.map_or(0, |s| s.cursor()),
        }
    }

    /// Returns whether the wake was a hint (telemetry: the event path
    /// carried this gate, rather than the fallback tick).
    fn wait(&mut self, deadline: Instant, cancel: &dyn Fn() -> bool) -> bool {
        const POLL_FALLBACK: Duration = Duration::from_millis(50);
        match self.signals {
            Some(s) => {
                let slice_end = deadline.min(Instant::now() + POLL_FALLBACK);
                matches!(
                    s.wait(&mut self.cursor, &|_| true, slice_end, cancel),
                    WaitOutcome::Hint
                )
            }
            None => {
                std::thread::sleep(Duration::from_millis(5));
                false
            }
        }
    }
}
