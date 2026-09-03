//! Probe: two independent questions about parking, in one run.
//!
//! Q1 — can a park be a pure MOVE, off the LEFT edge of the main display?
//!   Ordo parks at the bottom-right of the RIGHTMOST display with the
//!   pos->size->pos idiom, which always writes AXSize; on a display shorter
//!   than the window the size is clamped and the clamped frame is then saved as
//!   the window's real frame. Measured here: whether a POSITION-ONLY write off
//!   the left edge is accepted, how far it is pulled back, whether the size
//!   survives, and what (if anything) clamps a size written from an off-left
//!   origin. The ratchet itself is measured directly for comparison: a shipped
//!   `set_frame` at each candidate park, asking what height comes back.
//!
//! Q2 — can the un-hide re-home be PRE-EMPTED?
//!   An un-hidden app yanks its parked windows back on screen. Hypothesis: AX
//!   requests to one app are ordered on that app's main thread, so issuing
//!   `AXHidden := false` and the re-park position writes back to back — no
//!   sleep, no intervening round trip, window elements resolved in advance —
//!   may land the re-park before any frame is painted. A `control` arm that
//!   un-hides WITHOUT re-parking is run interleaved: if the control's re-home
//!   is seen and the pre-empted arm's is not, the method has demonstrated its
//!   own sensitivity.
//!
//!   Q2 watches the window server (`CGWindowListCopyWindowInfo`), not AX, from
//!   a separate thread: a CG read costs no round trip to the app under test, so
//!   polling cannot itself serialize behind the writes being timed, and the
//!   bounds it reports are the ones the compositor uses. It still samples; it
//!   cannot prove nothing was painted, only that the window server was never
//!   *observed* holding the window on screen off-park.
//!
//! Only ever run this against scratch apps you launched yourself:
//!   open -g -a TextEdit /tmp/a.txt ; open -g -a TextEdit /tmp/b.txt
//!   open -g -a Safari
//!   cargo run --example left_park_probe -- <textedit pid> <safari pid> [q1] [q2]

use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::sleep;
use std::time::{Duration, Instant};

use objc2_app_kit::NSRunningApplication;
use objc2_application_services::{AXError, AXUIElement, AXValue, AXValueType};
use objc2_core_foundation::{CFBoolean, CFString, CFType, CGPoint, CGSize};
use objc2_core_graphics::{CGWindowListCopyWindowInfo, CGWindowListOption};
use ordo::platform::{ax, cf, display};
use ordo_core::{Rect, WindowId};
use ordo_skylight_sys as sys;

/// Core Foundation's double selector for `CFNumberGetValue` (CFNumber.h); the
/// shared binding only carries the SInt64 one and CG bounds are doubles.
const CF_NUMBER_DOUBLE: i32 = 13;

// --- AX plumbing (the probe writes position ALONE, which ax.rs never does) ---

fn app_el(pid: i32) -> objc2_core_foundation::CFRetained<AXUIElement> {
    let el = unsafe { AXUIElement::new_application(pid) };
    unsafe { el.set_messaging_timeout(0.25) };
    el
}

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

unsafe fn set_position(el: &AXUIElement, x: f64, y: f64) -> AXError {
    let mut p = CGPoint { x, y };
    let Some(val) = AXValue::new(
        AXValueType::CGPoint,
        NonNull::new(&mut p as *mut _ as *mut c_void).unwrap(),
    ) else {
        return AXError::Failure;
    };
    let attr = CFString::from_str("AXPosition");
    el.set_attribute_value(&attr, val.as_ref())
}

unsafe fn set_size(el: &AXUIElement, w: f64, h: f64) {
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

unsafe fn set_bool(el: &AXUIElement, name: &str, value: bool) {
    let b = if value {
        sys::kCFBooleanTrue
    } else {
        sys::kCFBooleanFalse
    };
    if b.is_null() {
        return;
    }
    let attr = CFString::from_str(name);
    let _ = el.set_attribute_value(&attr, &*(b as *const CFType));
}

fn window_id(el: *const AXUIElement) -> Option<WindowId> {
    if el.is_null() {
        return None;
    }
    let mut wid: u32 = 0;
    let err = unsafe { sys::_AXUIElementGetWindow(el as *const c_void, &mut wid) };
    (err == 0 && wid != 0).then_some(WindowId(wid))
}

/// Resolve `pid`'s window elements once and hand them to `f`. The AXWindows
/// array owns the element pointers, so everything must happen inside.
fn with_app_windows<T>(
    pid: i32,
    f: impl FnOnce(&AXUIElement, &[(WindowId, *const AXUIElement)]) -> T,
) -> Option<T> {
    let el = app_el(pid);
    let raw = unsafe { copy_attr(&el, "AXWindows") }?;
    let mut wins = Vec::new();
    unsafe {
        for i in 0..cf::array_len(raw) {
            let w = cf::array_get(raw, i) as *const AXUIElement;
            if let Some(id) = window_id(w) {
                wins.push((id, w));
            }
        }
    }
    let out = f(&el, &wins);
    unsafe { sys::CFRelease(raw) };
    Some(out)
}

/// Position-only write: never touches AXSize. This is the candidate idiom.
fn move_only(pid: i32, target: WindowId, x: f64, y: f64) -> bool {
    with_app_windows(pid, |_app, wins| {
        for (id, w) in wins {
            if *id == target {
                return unsafe { set_position(&**w, x, y) } == AXError::Success;
            }
        }
        false
    })
    .unwrap_or(false)
}

fn resize_only(pid: i32, target: WindowId, w: f64, h: f64) -> bool {
    with_app_windows(pid, |_app, wins| {
        for (id, el) in wins {
            if *id == target {
                unsafe { set_size(&**el, w, h) };
                return true;
            }
        }
        false
    })
    .unwrap_or(false)
}

fn frame_of(pid: i32, target: WindowId) -> Option<Rect> {
    with_app_windows(pid, |_app, wins| {
        let (_, w) = wins.iter().find(|(id, _)| *id == target)?;
        let w = unsafe { &**w };
        unsafe {
            let (Some(praw), Some(sraw)) = (copy_attr(w, "AXPosition"), copy_attr(w, "AXSize"))
            else {
                return None;
            };
            let mut p = CGPoint { x: 0.0, y: 0.0 };
            let mut s = CGSize {
                width: 0.0,
                height: 0.0,
            };
            let okp = (*(praw as *const AXValue)).value(
                AXValueType::CGPoint,
                NonNull::new(&mut p as *mut _ as *mut c_void).unwrap(),
            );
            let oks = (*(sraw as *const AXValue)).value(
                AXValueType::CGSize,
                NonNull::new(&mut s as *mut _ as *mut c_void).unwrap(),
            );
            sys::CFRelease(praw);
            sys::CFRelease(sraw);
            (okp && oks).then_some(Rect {
                x: p.x,
                y: p.y,
                w: s.width,
                h: s.height,
            })
        }
    })
    .flatten()
}

fn app_bool(pid: i32, name: &str) -> Option<bool> {
    let el = app_el(pid);
    let raw = unsafe { copy_attr(&el, name) }?;
    let v = unsafe { &*(raw as *const CFBoolean) }.value();
    unsafe { sys::CFRelease(raw) };
    Some(v)
}

// --- window-server reads (no IPC to the app under test) ---------------------

#[derive(Clone, Copy, Debug, PartialEq)]
struct Cg {
    bounds: Rect,
    onscreen: bool,
}

unsafe fn num_f64(n: *const c_void) -> Option<f64> {
    if n.is_null() || sys::CFGetTypeID(n) != sys::CFNumberGetTypeID() {
        return None;
    }
    let mut v: f64 = 0.0;
    let ok = sys::CFNumberGetValue(n, CF_NUMBER_DOUBLE, &mut v as *mut f64 as *mut c_void);
    (ok != 0).then_some(v)
}

fn cg_of(wid: u32) -> Option<Cg> {
    // kCGWindowListOptionIncludingWindow alone: just this window, on screen or
    // not, straight from the window server.
    let arr = CGWindowListCopyWindowInfo(CGWindowListOption(1 << 3), wid)?;
    let raw = (&*arr) as *const _ as *const c_void;
    unsafe {
        if cf::array_len(raw) < 1 {
            return None;
        }
        let d = cf::array_get(raw, 0);
        let b = cf::dict_get(d, "kCGWindowBounds");
        if b.is_null() {
            return None;
        }
        let bounds = Rect {
            x: num_f64(cf::dict_get(b, "X"))?,
            y: num_f64(cf::dict_get(b, "Y"))?,
            w: num_f64(cf::dict_get(b, "Width"))?,
            h: num_f64(cf::dict_get(b, "Height"))?,
        };
        let on = cf::dict_get(d, "kCGWindowIsOnscreen");
        let onscreen = !on.is_null()
            && !sys::kCFBooleanTrue.is_null()
            && sys::CFEqual(on, sys::kCFBooleanTrue) != 0;
        Some(Cg { bounds, onscreen })
    }
}

// --- geometry ---------------------------------------------------------------

fn same_pos(a: &Rect, b: &Rect) -> bool {
    (a.x - b.x).abs() < 1.5 && (a.y - b.y).abs() < 1.5
}

fn same(a: &Rect, b: &Rect) -> bool {
    same_pos(a, b) && (a.w - b.w).abs() < 1.5 && (a.h - b.h).abs() < 1.5
}

fn fmt(r: &Rect) -> String {
    format!("({:.0},{:.0} {:.0}x{:.0})", r.x, r.y, r.w, r.h)
}

/// Area of `r` that falls inside any display, and the widest on-screen column.
fn visible_on(r: &Rect, displays: &[Rect]) -> (f64, f64, f64) {
    let (mut area, mut wide, mut tall) = (0.0, 0.0f64, 0.0f64);
    for d in displays {
        let w = (r.x + r.w).min(d.x + d.w) - r.x.max(d.x);
        let h = (r.y + r.h).min(d.y + d.h) - r.y.max(d.y);
        if w > 0.0 && h > 0.0 {
            area += w * h;
            wide = wide.max(w);
            tall = tall.max(h);
        }
    }
    (area, wide, tall)
}

fn settle<T: PartialEq + Clone>(mut f: impl FnMut() -> T, n: usize, budget: Duration) -> T {
    let start = Instant::now();
    let mut last = f();
    let mut streak = 1;
    while start.elapsed() < budget {
        sleep(Duration::from_millis(30));
        let cur = f();
        if cur == last {
            streak += 1;
            if streak >= n {
                break;
            }
        } else {
            streak = 1;
            last = cur;
        }
    }
    last
}

fn app_name(pid: i32) -> String {
    NSRunningApplication::runningApplicationWithProcessIdentifier(pid)
        .and_then(|a| a.localizedName().map(|n| n.to_string()))
        .unwrap_or_else(|| format!("pid {pid}"))
}

fn med(v: &mut Vec<f64>) -> f64 {
    if v.is_empty() {
        return f64::NAN;
    }
    v.sort_by(|a, b| a.total_cmp(b));
    v[v.len() / 2]
}

fn spread(v: &[f64]) -> String {
    if v.is_empty() {
        return "-".into();
    }
    let mut s = v.to_vec();
    let m = med(&mut s);
    format!(
        "median {:.1} min {:.1} max {:.1}",
        m,
        s.first().unwrap(),
        s.last().unwrap()
    )
}

// === Q1 =====================================================================

#[derive(Default)]
struct Q1 {
    /// (a) requested x vs landed x, off the left edge, y unchanged.
    a_dx: Vec<f64>,
    a_dy: Vec<f64>,
    a_dw: Vec<f64>,
    a_dh: Vec<f64>,
    a_drift: Vec<f64>,
    a_vis_w: Vec<f64>,
    a_vis_area: Vec<f64>,
    /// (b) same, plus y at the bottom edge.
    b_dx: Vec<f64>,
    b_dy: Vec<f64>,
    b_dh: Vec<f64>,
    b_vis_w: Vec<f64>,
    /// (c) height a size write can reach from an off-left origin.
    c_left_h: Vec<f64>,
    /// (d) position-only restore: offset from home, and size drift.
    d_dx: Vec<f64>,
    d_dy: Vec<f64>,
    d_dh: Vec<f64>,
    /// (e) shipped set_frame at each candidate park: what height comes back.
    ratchet_right_h: Vec<f64>,
    ratchet_left_h: Vec<f64>,
}

fn q1_trial(
    pid: i32,
    wid: WindowId,
    home: Rect,
    main: Rect,
    right_host: Rect,
    displays: &[Rect],
    out: &mut Q1,
) {
    let budget = Duration::from_millis(1200);
    let put_home = || {
        ax::set_frame(wid, home);
        settle(|| frame_of(pid, wid), 2, budget)
    };
    let Some(start) = put_home() else { return };

    // (a) position-only, off the left edge, y untouched.
    let want_x = main.x - start.w + 1.0;
    move_only(pid, wid, want_x, start.y);
    let Some(a) = settle(|| frame_of(pid, wid), 3, budget) else {
        return;
    };
    let a_cg = cg_of(wid.0);
    sleep(Duration::from_millis(2000));
    let a2 = frame_of(pid, wid).unwrap_or(a);
    out.a_dx.push(a.x - want_x);
    out.a_dy.push(a.y - start.y);
    out.a_dw.push(a.w - start.w);
    out.a_dh.push(a.h - start.h);
    out.a_drift.push((a2.x - a.x).abs().max((a2.y - a.y).abs()));
    let vis =
        a_cg.map(|c| visible_on(&c.bounds, displays))
            .unwrap_or((f64::NAN, f64::NAN, f64::NAN));
    out.a_vis_area.push(vis.0);
    out.a_vis_w.push(vis.1);
    println!(
        "    a  want x={:.0} -> AX {}  CG {}  visible {:.0}pt wide x {:.0}pt tall (area {:.0})  drift@2s {:.1}",
        want_x,
        fmt(&a),
        a_cg.map(|c| format!("{} onscreen={}", fmt(&c.bounds), c.onscreen)).unwrap_or("?".into()),
        vis.1,
        vis.2,
        vis.0,
        out.a_drift.last().unwrap(),
    );

    // (b) bottom-left corner.
    let want_y = main.y + main.h - 1.0;
    move_only(pid, wid, want_x, want_y);
    let Some(b) = settle(|| frame_of(pid, wid), 3, budget) else {
        return;
    };
    let b_cg = cg_of(wid.0);
    out.b_dx.push(b.x - want_x);
    out.b_dy.push(b.y - want_y);
    out.b_dh.push(b.h - start.h);
    let bvis =
        b_cg.map(|c| visible_on(&c.bounds, displays))
            .unwrap_or((f64::NAN, f64::NAN, f64::NAN));
    out.b_vis_w.push(bvis.1);
    println!(
        "    b  want ({:.0},{:.0}) -> AX {}  CG {}  visible {:.0}x{:.0}",
        want_x,
        want_y,
        fmt(&b),
        b_cg.map(|c| format!("{} onscreen={}", fmt(&c.bounds), c.onscreen))
            .unwrap_or("?".into()),
        bvis.1,
        bvis.2,
    );

    // (c) from an off-left origin, ask for far more height than any display has.
    resize_only(pid, wid, b.w, b.h + 500.0);
    let c = settle(|| frame_of(pid, wid), 3, budget).unwrap_or(b);
    out.c_left_h.push(c.h);
    println!(
        "    c  size write h={:.0} from off-left origin -> {}",
        b.h + 500.0,
        fmt(&c)
    );
    ax::set_frame(wid, home);
    settle(|| frame_of(pid, wid), 2, budget);

    // (d) position-only restore from the left park.
    move_only(pid, wid, want_x, want_y);
    settle(|| frame_of(pid, wid), 2, budget);
    move_only(pid, wid, home.x, home.y);
    let d = settle(|| frame_of(pid, wid), 3, budget).unwrap_or(home);
    out.d_dx.push(d.x - home.x);
    out.d_dy.push(d.y - home.y);
    out.d_dh.push(d.h - start.h);
    println!(
        "    d  pos-only restore -> {} (home {})",
        fmt(&d),
        fmt(&home)
    );

    // (e) the ratchet itself: the shipped pos->size->pos write at each park.
    let Some(base) = put_home() else { return };
    ax::set_frame(
        wid,
        Rect {
            x: right_host.x + right_host.w - 1.0,
            y: right_host.y + right_host.h - 1.0,
            w: base.w,
            h: base.h,
        },
    );
    let r = settle(|| frame_of(pid, wid), 3, budget).unwrap_or(base);
    out.ratchet_right_h.push(r.h);
    let Some(base) = put_home() else { return };
    ax::set_frame(
        wid,
        Rect {
            x: main.x - base.w + 1.0,
            y: main.y + main.h - 1.0,
            w: base.w,
            h: base.h,
        },
    );
    let l = settle(|| frame_of(pid, wid), 3, budget).unwrap_or(base);
    out.ratchet_left_h.push(l.h);
    println!(
        "    e  shipped set_frame (h={:.0} asked): right park -> h={:.0} {}   left park -> h={:.0} {}",
        base.h,
        r.h,
        fmt(&r),
        l.h,
        fmt(&l),
    );
    put_home();
}

// === Q2 =====================================================================

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Arm {
    /// Un-hide only — yesterday's behaviour, and this run's sensitivity check.
    Control,
    /// Un-hide then immediately re-issue the park position writes, once.
    Preempt,
    /// Un-hide then re-issue them in a tight loop until the window server
    /// agrees the window is back at its park (or a deadline passes).
    Chase,
}

impl Arm {
    fn name(self) -> &'static str {
        match self {
            Arm::Control => "control",
            Arm::Preempt => "preempt",
            Arm::Chase => "chase",
        }
    }
}

struct Q2Row {
    arm: Arm,
    n_windows: usize,
    left: bool,
    /// First sample where the window server had the window on screen somewhere
    /// other than its parked bounds.
    leak_ms: Option<u128>,
    leak_at: Option<Rect>,
    /// ms from that first off-park on-screen sample until it read parked again.
    leak_span_ms: Option<u128>,
    /// ms until the window re-entered the window server's list at all.
    reappear_ms: Option<u128>,
    /// Whether the very first sample after reappearance was already off-park.
    reappeared_offpark: bool,
    /// ms until the window server first called it on screen.
    onscreen_ms: Option<u128>,
    final_parked: bool,
    samples: usize,
    /// Position writes issued in the critical section, and how many succeeded.
    writes: usize,
    ok_writes: usize,
    /// Windows the app exposed while hidden, and how many matched a target.
    resolved: usize,
    matched: usize,
}

/// One window-server observation, or its absence from the list entirely (which
/// is what a hidden app's windows look like).
type Sample = (u128, Option<Cg>);

/// How long `chase` keeps re-issuing the park.
const CHASE: Duration = Duration::from_millis(250);

/// (arm, parked windows, park off the LEFT edge). Interleaved per trial so
/// desktop drift can't masquerade as a difference between arms.
const Q2_ARMS: [(Arm, usize, bool); 8] = [
    (Arm::Control, 1, false),
    (Arm::Preempt, 1, false),
    (Arm::Chase, 1, false),
    (Arm::Preempt, 2, false),
    (Arm::Chase, 2, false),
    (Arm::Preempt, 1, true),
    (Arm::Chase, 1, true),
    (Arm::Chase, 2, true),
];

/// Poll the window server for `wid` on its own thread, so the reads cannot
/// serialize behind the AX writes under test. Consecutive duplicates are
/// collapsed; the raw sample count comes back alongside.
fn spawn_poller(
    wid: u32,
    t0: Instant,
    dur: Duration,
    ready: Arc<AtomicBool>,
) -> std::thread::JoinHandle<(Vec<Sample>, usize)> {
    std::thread::spawn(move || {
        let mut trace: Vec<Sample> = Vec::with_capacity(512);
        let mut n = 0usize;
        let mut last: Option<Option<Cg>> = None;
        while t0.elapsed() < dur {
            let t = t0.elapsed().as_micros() as u128;
            let c = cg_of(wid);
            n += 1;
            if last.as_ref() != Some(&c) {
                trace.push((t / 1000, c));
                last = Some(c);
            }
            ready.store(true, Ordering::Relaxed);
            sleep(Duration::from_micros(1000));
        }
        (trace, n)
    })
}

#[allow(clippy::too_many_arguments)]
fn q2_trial(
    arm: Arm,
    pid: i32,
    targets: &[WindowId],
    homes: &[Rect],
    park_of: &dyn Fn(&Rect) -> (f64, f64),
    left: bool,
    watch: Duration,
) -> Option<Q2Row> {
    let budget = Duration::from_millis(1200);
    // 1. Home, then park. The left candidate parks by moving; the right park is
    //    the shipped write, so the control arm matches what actually ships.
    for (w, h) in targets.iter().zip(homes) {
        ax::set_frame(*w, *h);
    }
    settle(|| frame_of(pid, targets[0]), 2, budget);
    let mut parks = Vec::new();
    for (w, h) in targets.iter().zip(homes) {
        let (px, py) = park_of(h);
        parks.push((px, py));
        if left {
            move_only(pid, *w, px, py);
        } else {
            ax::set_frame(
                *w,
                Rect {
                    x: px,
                    y: py,
                    w: h.w,
                    h: h.h,
                },
            );
        }
    }
    settle(|| frame_of(pid, targets[0]), 3, budget)?;
    let parked = cg_of(targets[0].0)?;

    // 2. Hide. Ordo never hides the focused app; nothing here focuses the
    //    scratch, so it is a background app throughout.
    unsafe { set_bool(&app_el(pid), "AXHidden", true) };
    let hidden = settle(|| app_bool(pid, "AXHidden"), 3, Duration::from_millis(1500));
    if hidden != Some(true) {
        println!("    ! never read AXHidden=true; skipping");
        return None;
    }
    sleep(Duration::from_millis(150));
    let parked_hidden = cg_of(targets[0].0).unwrap_or(parked);

    // 3. Watch the window server across the un-hide.
    let t0 = Instant::now();
    let ready = Arc::new(AtomicBool::new(false));
    let poller = spawn_poller(targets[0].0, t0, watch, ready.clone());
    let spin = Instant::now();
    while !ready.load(Ordering::Relaxed) && spin.elapsed() < Duration::from_millis(200) {
        std::hint::spin_loop();
    }

    // 4. The critical section: window elements are already resolved, so the
    //    un-hide and the re-park go out back to back on one Mach port with no
    //    round trip between them. `chase` keeps re-issuing until the window
    //    server agrees; the CG check costs nothing from the app under test, so
    //    the loop never competes with its own writes for the app's main thread.
    let mut writes = 0usize;
    let mut ok_writes = 0usize;
    let mut resolved = 0usize;
    let mut matched = 0usize;
    with_app_windows(pid, |app, wins| unsafe {
        resolved = wins.len();
        let mine: Vec<(usize, *const AXUIElement)> = wins
            .iter()
            .filter_map(|(id, el)| targets.iter().position(|t| t == id).map(|i| (i, *el)))
            .collect();
        matched = mine.len();
        set_bool(app, "AXHidden", false);
        if arm == Arm::Control {
            return;
        }
        let deadline = Instant::now() + CHASE;
        loop {
            for (i, el) in &mine {
                writes += 1;
                if set_position(&**el, parks[*i].0, parks[*i].1) == AXError::Success {
                    ok_writes += 1;
                }
            }
            if arm == Arm::Preempt || Instant::now() >= deadline {
                return;
            }
            if cg_of(targets[0].0).is_some_and(|c| same_pos(&c.bounds, &parked_hidden.bounds)) {
                return;
            }
            sleep(Duration::from_micros(500));
        }
    });

    let (trace, samples) = poller.join().ok()?;

    // 5. A leak is the window server holding the window ON SCREEN at bounds
    //    other than its park. While the app is hidden its windows leave the
    //    window list entirely, so the first `Some` sample is the reappearance.
    let onscreen_ms = trace
        .iter()
        .find(|(_, c)| c.is_some_and(|c| c.onscreen))
        .map(|(t, _)| *t);
    let reappear = trace
        .iter()
        .skip_while(|(_, c)| c.is_none())
        .nth(0)
        .cloned();
    let reappear_ms = reappear.map(|(t, _)| t);
    let reappeared_offpark = reappear
        .and_then(|(_, c)| c)
        .is_some_and(|c| !same_pos(&c.bounds, &parked_hidden.bounds));
    let leak = trace
        .iter()
        .filter_map(|(t, c)| c.map(|c| (*t, c)))
        .find(|(_, c)| c.onscreen && !same_pos(&c.bounds, &parked_hidden.bounds));
    let (leak_ms, leak_at, leak_span_ms) = match leak {
        Some((t, c)) => {
            let back = trace
                .iter()
                .filter_map(|(t2, c2)| c2.map(|c2| (*t2, c2)))
                .find(|(t2, c2)| *t2 > t && same_pos(&c2.bounds, &parked_hidden.bounds))
                .map(|(t2, _)| t2 - t);
            (Some(t), Some(c.bounds), back)
        }
        None => (None, None, None),
    };
    let end = cg_of(targets[0].0);
    let final_parked = end.is_some_and(|c| same_pos(&c.bounds, &parked_hidden.bounds));

    println!(
        "    {:<7} {}win {:<5} parked {} | back@{:>4} onscreen@{:>4} leak@{:>4} {} {} | end {} {} | {}/{} writes ok ({}win resolved, {} matched) | {} samples, {} states",
        arm.name(),
        targets.len(),
        if left { "left" } else { "right" },
        fmt(&parked_hidden.bounds),
        reappear_ms.map(|m| m.to_string()).unwrap_or("-".into()),
        onscreen_ms.map(|m| m.to_string()).unwrap_or("-".into()),
        leak_ms.map(|m| m.to_string()).unwrap_or("-".into()),
        leak_at.map(|r| fmt(&r)).unwrap_or("".into()),
        leak_span_ms.map(|m| format!("for {m}ms")).unwrap_or_default(),
        end.map(|c| fmt(&c.bounds)).unwrap_or("?".into()),
        if final_parked { "parked" } else { "OFF-PARK" },
        ok_writes,
        writes,
        resolved,
        matched,
        samples,
        trace.len(),
    );

    // 6. Home again for the next trial.
    for (w, h) in targets.iter().zip(homes) {
        ax::set_frame(*w, *h);
    }
    settle(|| frame_of(pid, targets[0]), 2, budget);

    Some(Q2Row {
        arm,
        n_windows: targets.len(),
        left,
        leak_ms,
        leak_at,
        leak_span_ms,
        reappear_ms,
        reappeared_offpark,
        onscreen_ms,
        final_parked,
        samples,
        writes,
        ok_writes,
        resolved,
        matched,
    })
}

// === main ===================================================================

fn windows_of(pid: i32) -> Vec<(WindowId, Rect, Option<String>)> {
    ax::windows()
        .into_iter()
        .filter(|w| w.app.0 == pid)
        .map(|w| (w.id, w.frame, w.subrole))
        .collect()
}

fn main() {
    let mut args = std::env::args().skip(1);
    let te: i32 = args
        .next()
        .and_then(|a| a.parse().ok())
        .expect("TextEdit pid");
    let sf: i32 = args
        .next()
        .and_then(|a| a.parse().ok())
        .expect("Safari pid");
    let q1_trials: usize = args.next().and_then(|a| a.parse().ok()).unwrap_or(4);
    let q2_trials: usize = args.next().and_then(|a| a.parse().ok()).unwrap_or(12);

    let ds = display::active_displays();
    let displays: Vec<Rect> = ds.iter().map(|d| d.frame).collect();
    let main_d = ds
        .iter()
        .find(|d| d.is_main)
        .map(|d| d.frame)
        .expect("main display");
    let right = ds
        .iter()
        .map(|d| d.frame)
        .max_by(|a, b| (a.x + a.w).total_cmp(&(b.x + b.w)))
        .expect("a display");
    println!("displays:");
    for d in &ds {
        println!("  {} main={}", fmt(&d.frame), d.is_main);
    }
    println!("main {}  rightmost {}", fmt(&main_d), fmt(&right));

    let original_focus = ax::focused_window();
    println!("original focus: {original_focus:?}");

    let te_wins = windows_of(te);
    let sf_wins = windows_of(sf);
    println!("TextEdit pid {te} ({}) windows:", app_name(te));
    for (id, f, sr) in &te_wins {
        println!("  {} {} {:?}", id.0, fmt(f), sr);
    }
    println!("Safari pid {sf} ({}) windows:", app_name(sf));
    for (id, f, sr) in &sf_wins {
        println!("  {} {} {:?}", id.0, fmt(f), sr);
    }
    assert!(te_wins.len() >= 2, "need two TextEdit scratch windows");
    assert!(!sf_wins.is_empty(), "need a Safari window");

    // ---------------- Q1 ----------------
    println!("\n================ Q1: left-edge park as a pure MOVE ================");
    let mut q1s: Vec<(String, Q1)> = Vec::new();
    for (label, pid, wid) in [("TextEdit", te, te_wins[0].0), ("Safari", sf, sf_wins[0].0)] {
        let natural = frame_of(pid, wid).expect("frame");
        // Two homes: the window as it comes, and one taller than the second
        // display's 923pt of usable height — the case the ratchet bites.
        let tall = Rect {
            x: main_d.x + 100.0,
            y: main_d.y + 40.0,
            w: natural.w.max(600.0),
            h: 1000.0,
        };
        ax::set_frame(wid, tall);
        let tall = settle(|| frame_of(pid, wid), 3, Duration::from_millis(1500)).unwrap_or(tall);
        println!(
            "\n  {label}: natural {}  tall-config {}",
            fmt(&natural),
            fmt(&tall)
        );
        for (cfg, home) in [("tall", tall), ("natural", natural)] {
            let mut acc = Q1::default();
            println!("  -- {label} / {cfg} home {} --", fmt(&home));
            for i in 0..q1_trials {
                println!("   trial {}", i + 1);
                q1_trial(pid, wid, home, main_d, right, &displays, &mut acc);
            }
            q1s.push((format!("{label}/{cfg} h={:.0}", home.h), acc));
        }
        ax::set_frame(wid, natural);
        settle(|| frame_of(pid, wid), 2, Duration::from_millis(1200));
    }

    // ---------------- Q2 ----------------
    println!("\n================ Q2: pre-empting the un-hide re-home ================");
    let te_homes: Vec<Rect> = te_wins.iter().take(2).map(|(_, f, _)| *f).collect();
    let te_ids: Vec<WindowId> = te_wins.iter().take(2).map(|(id, _, _)| *id).collect();
    let right_park = |_h: &Rect| (right.x + right.w - 1.0, right.y + right.h - 1.0);
    let left_park = |h: &Rect| (main_d.x - h.w + 1.0, main_d.y + main_d.h - 1.0);
    let watch = Duration::from_millis(900);

    let mut rows: Vec<Q2Row> = Vec::new();
    let mut stolen = 0usize;
    for i in 0..q2_trials {
        println!("  trial {}", i + 1);
        for (arm, n, left) in Q2_ARMS {
            let park: &dyn Fn(&Rect) -> (f64, f64) = if left { &left_park } else { &right_park };
            rows.extend(q2_trial(
                arm,
                te,
                &te_ids[..n],
                &te_homes[..n],
                park,
                left,
                watch,
            ));
            // Un-hiding can front the scratch app; give the desk back each time.
            if app_bool(te, "AXFrontmost") == Some(true) {
                stolen += 1;
                if let Some(f) = original_focus {
                    ax::focus(f);
                }
            }
        }
    }

    // ---------------- cleanup ----------------
    unsafe { set_bool(&app_el(te), "AXHidden", false) };
    unsafe { set_bool(&app_el(sf), "AXHidden", false) };
    for (w, h) in te_ids.iter().zip(&te_homes) {
        ax::set_frame(*w, *h);
    }
    ax::set_frame(sf_wins[0].0, sf_wins[0].1);
    settle(|| frame_of(te, te_ids[0]), 2, Duration::from_millis(1500));
    if let Some(f) = original_focus {
        ax::focus(f);
    }
    println!("\nfinal frames:");
    for (id, home, _) in te_wins.iter().take(2) {
        println!(
            "  TextEdit {} {} (home {}) {}",
            id.0,
            frame_of(te, *id).map(|f| fmt(&f)).unwrap_or("?".into()),
            fmt(home),
            if frame_of(te, *id).is_some_and(|f| same(&f, home)) {
                "restored"
            } else {
                "NOT RESTORED"
            }
        );
    }
    println!(
        "  Safari {} {} (home {}) {}",
        sf_wins[0].0 .0,
        frame_of(sf, sf_wins[0].0)
            .map(|f| fmt(&f))
            .unwrap_or("?".into()),
        fmt(&sf_wins[0].1),
        if frame_of(sf, sf_wins[0].0).is_some_and(|f| same(&f, &sf_wins[0].1)) {
            "restored"
        } else {
            "NOT RESTORED"
        }
    );

    // ---------------- summaries ----------------
    println!("\n=== Q1 summary (N per row = trials; deltas in pt) ===");
    for (label, q) in &q1s {
        println!("{label}  N={}", q.a_dx.len());
        println!(
            "  a left park, y kept : x pull-back [{}]  y drift [{}]  w drift [{}]  h drift [{}]  2s drift [{}]",
            spread(&q.a_dx), spread(&q.a_dy), spread(&q.a_dw), spread(&q.a_dh), spread(&q.a_drift)
        );
        println!(
            "    visible column    : [{}] pt wide, area [{}] pt^2",
            spread(&q.a_vis_w),
            spread(&q.a_vis_area)
        );
        println!(
            "  b left+bottom       : x pull-back [{}]  y pull-back [{}]  h drift [{}]  visible w [{}]",
            spread(&q.b_dx), spread(&q.b_dy), spread(&q.b_dh), spread(&q.b_vis_w)
        );
        println!(
            "  c size write off-left: reached h [{}]",
            spread(&q.c_left_h)
        );
        println!(
            "  d pos-only restore  : dx [{}]  dy [{}]  h drift [{}]",
            spread(&q.d_dx),
            spread(&q.d_dy),
            spread(&q.d_dh)
        );
        println!(
            "  e shipped set_frame : right-park h [{}]   left-park h [{}]",
            spread(&q.ratchet_right_h),
            spread(&q.ratchet_left_h)
        );
    }

    println!("\n=== Q2 summary (leak = window server had it ON SCREEN off-park) ===");
    for (arm, n, left) in Q2_ARMS {
        let rs: Vec<&Q2Row> = rows
            .iter()
            .filter(|r| r.arm == arm && r.n_windows == n && r.left == left)
            .collect();
        if rs.is_empty() {
            continue;
        }
        let total = rs.len();
        let leaks: Vec<&&Q2Row> = rs.iter().filter(|r| r.leak_ms.is_some()).collect();
        let lat: Vec<f64> = leaks
            .iter()
            .filter_map(|r| r.leak_ms.map(|m| m as f64))
            .collect();
        let span: Vec<f64> = leaks
            .iter()
            .filter_map(|r| r.leak_span_ms.map(|m| m as f64))
            .collect();
        let on: Vec<f64> = rs
            .iter()
            .filter_map(|r| r.onscreen_ms.map(|m| m as f64))
            .collect();
        let back: Vec<f64> = rs
            .iter()
            .filter_map(|r| r.reappear_ms.map(|m| m as f64))
            .collect();
        let mut samp: Vec<f64> = rs.iter().map(|r| r.samples as f64).collect();
        println!(
            "{:<7} {}win {:<5}  N={total}  leaked {}/{total}  leak@[{}]ms  lasted [{}]ms  reappeared@[{}]ms already-off-park {}/{total}  onscreen@[{}]ms  ended parked {}/{total}  writes ok {}/{}  cg samples/trial median {:.0}",
            arm.name(),
            n,
            if left { "left" } else { "right" },
            leaks.len(),
            spread(&lat),
            spread(&span),
            spread(&back),
            rs.iter().filter(|r| r.reappeared_offpark).count(),
            spread(&on),
            rs.iter().filter(|r| r.final_parked).count(),
            rs.iter().map(|r| r.ok_writes).sum::<usize>(),
            rs.iter().map(|r| r.writes).sum::<usize>(),
            med(&mut samp),
        );
        println!(
            "         windows the hidden app exposed: {:?}  matched as targets: {:?}",
            rs.iter().map(|r| r.resolved).collect::<Vec<_>>(),
            rs.iter().map(|r| r.matched).collect::<Vec<_>>(),
        );
        let dests: Vec<String> = leaks
            .iter()
            .filter_map(|r| r.leak_at.map(|b| fmt(&b)))
            .collect();
        if !dests.is_empty() {
            println!("         leak bounds: {}", dests.join(" "));
        }
    }
    println!("\nfocus restored to the desk after {stolen} trial(s) where the scratch app went frontmost.");
}
