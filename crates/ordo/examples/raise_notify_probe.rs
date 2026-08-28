//! Probe: does WindowServer push an event when a window is RAISED — a pure
//! z-reorder among already-onscreen windows? Nobody upstream answers this:
//! alt-tab-macos consumes only order-in/out style events (its 804/1325/1326)
//! and tiling WMs never overlap windows, so no one has needed a raise event.
//! If SOME code fires per raise, the reassert's landing gate and the
//! ghost-raise tail (a cancelled/timed-out raise landing after the final
//! read-back) both become event-driven instead of polled.
//!
//! Method: discovery mode. Register ONE handler for every SkyLight connection
//! event code 0..=2000, subscribe every onscreen window via
//! SLSRequestNotificationsForWindows, then choreograph known z-order changes
//! (background raise, second raise, focus handoff, and a frame nudge as a
//! does-the-plumbing-work-at-all control) and see which codes fire in which
//! phase. High-frequency codes are printed a few times then only counted.
//!
//! The two symbols are resolved with dlsym rather than link-time externs, so
//! an OS that dropped them fails THIS probe honestly instead of breaking the
//! link of every binary in the workspace. Promote them to ordo-skylight-sys
//! only if this probe proves them useful.
//!
//! DISENGAGE THE DAEMON FIRST (rescue chord or don't have it engaged): the
//! choreography's raises/focuses/moves are external events the engine reacts
//! to, and its corrections interleave with the phases — the 2026-08-27 run
//! captured a full daemon fight in the middle of its own measurements.
//!
//! Run: cargo run --example raise_notify_probe
//!
//! ANSWERED (2026-08-27, Tahoe 26.6): YES. A pure background AXRaise of an
//! already-onscreen window emits 808 + 815 for exactly that window id,
//! instantly, in both raise phases and the focus phase. Full notes in
//! issues.txt (zero-poll upgrade section).

use std::collections::BTreeMap;
use std::ffi::c_void;
use std::os::raw::{c_char, c_int};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use objc2_app_kit::NSApplication;
use ordo::platform::{ax, display, zorder};
use ordo_core::{Rect, WindowId};
use ordo_skylight_sys as sys;

extern "C" {
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
}
/// dlfcn.h's pseudo-handle "search every loaded image" — SkyLight is already
/// in the process image because ordo links it.
const RTLD_DEFAULT: *mut c_void = -2isize as *mut c_void;

/// Per alt-tab's verified decl: (event id, payload, payload length, context,
/// connection id) — length is a 64-bit Int, not the classic CGS unsigned int.
type NotifyProc = unsafe extern "C" fn(u32, *mut c_void, usize, *mut c_void, c_int);
type RegisterFn = unsafe extern "C" fn(c_int, NotifyProc, u32, *mut c_void) -> c_int;
type RequestWindowsFn = unsafe extern "C" fn(c_int, *const u32, c_int) -> c_int;

fn resolve(name: &str) -> Option<*mut c_void> {
    let c = std::ffi::CString::new(name).unwrap();
    let p = unsafe { dlsym(RTLD_DEFAULT, c.as_ptr()) };
    println!(
        "{name}: {}",
        if p.is_null() { "NOT EXPORTED" } else { "found" }
    );
    (!p.is_null()).then_some(p)
}

const PHASES: [&str; 6] = [
    "quiet-before",
    "raise-bottom",
    "raise-mid",
    "focus",
    "move-control",
    "quiet-after",
];
/// How many hits of one (phase, code) pair are printed in full before the
/// probe falls back to counting — some codes are mouse-move-frequency.
const PRINT_CAP: u64 = 6;

static START: OnceLock<Instant> = OnceLock::new();
static PHASE: AtomicUsize = AtomicUsize::new(0);
static SEEN: OnceLock<Mutex<BTreeMap<(usize, u32), u64>>> = OnceLock::new();

unsafe extern "C" fn on_event(
    event: u32,
    data: *mut c_void,
    len: usize,
    _ctx: *mut c_void,
    _cid: c_int,
) {
    let phase = PHASE.load(Ordering::Relaxed);
    let n = {
        let mut seen = SEEN.get().unwrap().lock().unwrap();
        let n = seen.entry((phase, event)).or_insert(0);
        *n += 1;
        *n
    };
    if n > PRINT_CAP {
        return;
    }
    // Payloads are undocumented; u32 words is the shape that has decoded
    // best historically (window ids, space ids).
    let words: Vec<u32> = if data.is_null() {
        Vec::new()
    } else {
        let bytes = unsafe { std::slice::from_raw_parts(data as *const u8, len.min(64)) };
        bytes
            .as_chunks::<4>()
            .0
            .iter()
            .map(|c| u32::from_le_bytes(*c))
            .collect()
    };
    let ms = START.get().map(|s| s.elapsed().as_millis()).unwrap_or(0);
    println!(
        "[{ms:>6}ms] {:>13}: event {event:>4} len {len:>3} words {words:?}",
        PHASES[phase]
    );
}

fn nudged(f: &Rect) -> Rect {
    Rect {
        x: f.x + 10.0,
        ..*f
    }
}

fn main() {
    START.set(Instant::now()).unwrap();
    SEEN.set(Mutex::new(BTreeMap::new())).unwrap();

    let Some(reg) = resolve("SLSRegisterConnectionNotifyProc") else {
        return;
    };
    let req = resolve("SLSRequestNotificationsForWindows");
    let register: RegisterFn = unsafe { std::mem::transmute(reg) };

    let cid = unsafe { sys::SLSMainConnectionID() };
    let mut registered = 0u32;
    for event in 0..=2000u32 {
        if unsafe { register(cid, on_event, event, std::ptr::null_mut()) } == 0 {
            registered += 1;
        }
    }
    println!("registered handler for {registered} of 2001 event codes");

    let stack = zorder::stack_with_pids();
    let ids: Vec<u32> = stack.iter().map(|(w, _)| *w).collect();
    if let Some(req) = req {
        let request: RequestWindowsFn = unsafe { std::mem::transmute(req) };
        let ret = unsafe { request(cid, ids.as_ptr(), ids.len() as c_int) };
        println!(
            "SLSRequestNotificationsForWindows({} windows) -> {ret}",
            ids.len()
        );
    }

    // Choreography targets must be GENUINELY visible. CG counts the emulated
    // backend's parked windows (1px slivers at a display corner) as onscreen,
    // and choreographing one is how this probe once taught the daemon that a
    // window's home was the park corner — nudge + restore ossified the sliver
    // frame. Majority-onscreen is the filter that can't make that mistake.
    let displays = display::active_displays();
    let visible_area = |f: &Rect| -> f64 {
        displays
            .iter()
            .map(|d| {
                let w = (f.x + f.w).min(d.frame.x + d.frame.w) - f.x.max(d.frame.x);
                let h = (f.y + f.h).min(d.frame.y + d.frame.h) - f.y.max(d.frame.y);
                w.max(0.0) * h.max(0.0)
            })
            .sum()
    };
    let frames: std::collections::HashMap<WindowId, Rect> =
        ax::windows().into_iter().map(|w| (w.id, w.frame)).collect();
    let visible: Vec<(WindowId, Rect)> = ids
        .iter()
        .filter_map(|id| frames.get(&WindowId(*id)).map(|f| (WindowId(*id), *f)))
        .filter(|(_, f)| visible_area(f) >= 0.5 * f.w * f.h)
        .collect();
    if visible.len() < 3 {
        println!(
            "need at least 3 truly-visible windows for the choreography (have {})",
            visible.len()
        );
        return;
    }
    println!("onscreen stack front->back: {ids:?}");
    println!(
        "visible targets front->back: {:?}\n",
        visible.iter().map(|(w, _)| w.0).collect::<Vec<_>>()
    );

    let (bottom, bottom_rect) = visible[visible.len() - 1];
    let mid = visible[visible.len() / 2].0;
    let original_focus = ax::focused_window();
    let bottom_frame = Some(bottom_rect);

    std::thread::spawn(move || {
        let phase = |i: usize| {
            PHASE.store(i, Ordering::Relaxed);
            println!("-- {} --", PHASES[i]);
        };
        let settle = || std::thread::sleep(Duration::from_millis(2000));
        settle();

        phase(1);
        println!("   AXRaise {bottom:?} (bottom-most, no focus change)");
        ax::raise(bottom);
        settle();

        phase(2);
        println!("   AXRaise {mid:?}");
        ax::raise(mid);
        settle();

        phase(3);
        println!("   focus {bottom:?} (SLPS front-process + make-key)");
        ax::focus(bottom);
        settle();

        // Control: a frame change almost certainly produces SOMETHING if the
        // registration plumbing works at all; silence here means the sweep
        // itself is broken, not that raises are eventless.
        phase(4);
        if let Some(f) = &bottom_frame {
            println!("   nudge {bottom:?} +10px and back");
            ax::set_frame(bottom, nudged(f));
            std::thread::sleep(Duration::from_millis(500));
            ax::set_frame(bottom, *f);
        } else {
            println!("   (no frame for {bottom:?}; skipping move control)");
        }
        settle();

        phase(5);
        settle();

        if let Some(w) = original_focus {
            ax::focus(w);
        }
        // Let events from the focus restore drain before the tally.
        std::thread::sleep(Duration::from_millis(500));

        println!("\n== per-phase event counts ==");
        let seen = SEEN.get().unwrap().lock().unwrap();
        let mut last_phase = usize::MAX;
        for ((phase, event), count) in seen.iter() {
            if *phase != last_phase {
                println!("{}:", PHASES[*phase]);
                last_phase = *phase;
            }
            println!("  event {event:>4} x{count}");
        }
        if seen.is_empty() {
            println!("(nothing fired at all — even under a running NSApplication loop)");
        }
        // [NSApp run] never returns; the probe ends here by design.
        std::process::exit(0);
    });

    // The delivery pump, and the probe's central finding about PLUMBING: the
    // notify stream arrives as datagrams on AppKit's WindowServer connection,
    // and only a running NSApplication event loop receives them. A bare
    // CFRunLoop — even after NSApplicationLoad() — got ZERO events across a
    // full choreography with identical registrations. yabai ends its main the
    // same way ([NSApp run], yabai.c); alt-tab gets it implicitly from being
    // an AppKit app (delivery on its _NSEventThread).
    let mtm = objc2::MainThreadMarker::new().expect("main thread");
    let app = NSApplication::sharedApplication(mtm);
    app.run();
}
