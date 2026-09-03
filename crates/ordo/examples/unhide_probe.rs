//! Probe: does ANY un-hide route avoid the two harms of Dock dimming?
//!
//!   1. Focus theft — the un-hidden app's window becomes key, yanking focus
//!      from the window Ordo just granted it to.
//!   2. Re-homing — the un-hidden app's parked window (1pt on screen past
//!      the rightmost display) is pulled fully back onto a display.
//!
//! Candidates, interleaved trial by trial so drift in the desktop can't
//! masquerade as a difference between routes:
//!   - control: park, no hide, no un-hide (does a parked window stay parked
//!     on its own?)
//!   - ax:      `AXHidden := false` on the app element (what ships)
//!   - nsra:    `NSRunningApplication.unhide()` (AppKit; the WRITE, not the
//!              stale `isHidden` read that bit us before)
//!   - shp:     Carbon `ShowHideProcess(psn, true)`, dlsym'd — the Process
//!              Manager primitive under both of the above, if still exported
//!
//! The probe parks the scratch window itself and polls its frame through
//! the HIDDEN phase as well as after the un-hide, so a jump can be pinned to
//! the hide or to the un-hide. It needs nothing from the daemon.
//!
//! Only ever run this against scratch apps you launched yourself, e.g.
//!   open -g -a TextEdit /tmp/scratch.txt ; open -g -a Calculator
//!   cargo run --example unhide_probe -- <scratch pid> <victim pid> [trials] [active]
//!
//! `active`: make the scratch app the ACTIVE app at hide time (Ordo never
//! hides the focused app, but an app hidden while active is the one macOS
//! is most tempted to re-activate on un-hide — the pessimistic variant).
//!
//! `SLIVER=<pt>` (env, default 1): how much of the parked window stays on
//! screen. Lets you ask whether the re-home is a "too little visible"
//! threshold Ordo could park around, or unconditional.
//!
//! `grant`: a different question — Ordo grants focus to the destination
//! window BEFORE un-hiding its app, so the grant targets a HIDDEN app. This
//! mode times how long such a grant takes to land (scratch frontmost AND its
//! focused window == target) for each un-hide route, against a never-hidden
//! control. If hidden grants are slow, that alone would explain a focus
//! latency gap without any theft.

use std::ffi::c_void;
use std::ptr::NonNull;
use std::thread::sleep;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use objc2_app_kit::NSRunningApplication;
use objc2_application_services::{AXError, AXUIElement, AXValueType};
use objc2_core_foundation::{CFBoolean, CFString, CFType, CGPoint, CGSize};
use ordo::platform::{ax, cf, display};
use ordo_core::{Pid, Rect, WindowId};
use ordo_skylight_sys as sys;

const POLL: Duration = Duration::from_millis(15);
const WATCH: Duration = Duration::from_millis(2000);
const HIDDEN_WATCH: Duration = Duration::from_millis(500);

#[derive(Clone, Copy, PartialEq, Debug)]
enum Route {
    Control,
    Ax,
    Nsra,
    Shp,
}

impl Route {
    fn name(self) -> &'static str {
        match self {
            Route::Control => "control",
            Route::Ax => "ax",
            Route::Nsra => "nsra",
            Route::Shp => "shp",
        }
    }
}

type ShowHideProcessFn = unsafe extern "C" fn(*const sys::ProcessSerialNumber, u8) -> i16;

fn show_hide_process() -> Option<ShowHideProcessFn> {
    let sym = unsafe { libc::dlsym(libc::RTLD_DEFAULT, c"ShowHideProcess".as_ptr()) };
    if sym.is_null() {
        None
    } else {
        Some(unsafe { std::mem::transmute::<*mut c_void, ShowHideProcessFn>(sym) })
    }
}

fn unhide(route: Route, pid: i32, shp: Option<ShowHideProcessFn>) -> String {
    match route {
        Route::Control => "n/a".into(),
        Route::Ax => {
            ax::set_app_hidden(Pid(pid), false);
            "ok".into()
        }
        Route::Nsra => {
            let Some(app) = NSRunningApplication::runningApplicationWithProcessIdentifier(pid)
            else {
                return "no NSRunningApplication".into();
            };
            format!("returned {}", app.unhide())
        }
        Route::Shp => {
            let Some(f) = shp else {
                return "symbol missing".into();
            };
            let mut psn = sys::ProcessSerialNumber::default();
            if unsafe { sys::GetProcessForPID(pid, &mut psn) } != 0 {
                return "no PSN".into();
            }
            format!("OSErr {}", unsafe { f(&psn, 1) })
        }
    }
}

// --- minimal AX reads scoped to ONE app: cheap enough to poll at 15ms -------

fn app_el(pid: i32) -> objc2_core_foundation::CFRetained<AXUIElement> {
    let el = unsafe { AXUIElement::new_application(pid) };
    unsafe { el.set_messaging_timeout(0.2) };
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

fn app_bool(pid: i32, name: &str) -> Option<bool> {
    let el = app_el(pid);
    let raw = unsafe { copy_attr(&el, name) }?;
    let v = unsafe { &*(raw as *const CFBoolean) }.value();
    unsafe { sys::CFRelease(raw) };
    Some(v)
}

fn window_id(el: *const AXUIElement) -> Option<WindowId> {
    let mut wid: u32 = 0;
    let err = unsafe { sys::_AXUIElementGetWindow(el as *const c_void, &mut wid) };
    (err == 0 && wid != 0).then_some(WindowId(wid))
}

/// Frame of `target` by walking only `pid`'s windows.
fn frame_of(pid: i32, target: WindowId) -> Option<Rect> {
    let el = app_el(pid);
    let raw = unsafe { copy_attr(&el, "AXWindows") }?;
    let mut out = None;
    unsafe {
        for i in 0..cf::array_len(raw) {
            let win = cf::array_get(raw, i) as *const AXUIElement;
            if win.is_null() || window_id(win) != Some(target) {
                continue;
            }
            let win = &*win;
            let (Some(praw), Some(sraw)) = (copy_attr(win, "AXPosition"), copy_attr(win, "AXSize"))
            else {
                break;
            };
            let mut p = CGPoint { x: 0.0, y: 0.0 };
            let mut s = CGSize {
                width: 0.0,
                height: 0.0,
            };
            let okp = (*(praw as *const objc2_application_services::AXValue)).value(
                AXValueType::CGPoint,
                NonNull::new(&mut p as *mut _ as *mut c_void).unwrap(),
            );
            let oks = (*(sraw as *const objc2_application_services::AXValue)).value(
                AXValueType::CGSize,
                NonNull::new(&mut s as *mut _ as *mut c_void).unwrap(),
            );
            sys::CFRelease(praw);
            sys::CFRelease(sraw);
            if okp && oks {
                out = Some(Rect {
                    x: p.x,
                    y: p.y,
                    w: s.width,
                    h: s.height,
                });
            }
            break;
        }
        sys::CFRelease(raw);
    }
    out
}

/// Prefer a standard window; fall back to anything with a window id (Electron
/// apps have been seen reporting no subrole at all).
fn first_window(pid: i32) -> Option<(WindowId, Rect)> {
    let mine: Vec<_> = ax::windows()
        .into_iter()
        .filter(|w| w.app.0 == pid)
        .collect();
    for w in &mine {
        println!(
            "  candidate window {} subrole {:?} title {:?} {}",
            w.id.0,
            w.subrole,
            w.title,
            fmt(&w.frame)
        );
    }
    mine.iter()
        .find(|w| w.subrole.as_deref() == Some("AXStandardWindow"))
        .or(mine.first())
        .map(|w| (w.id, w.frame))
}

fn same(a: &Rect, b: &Rect) -> bool {
    (a.x - b.x).abs() < 0.5
        && (a.y - b.y).abs() < 0.5
        && (a.w - b.w).abs() < 0.5
        && (a.h - b.h).abs() < 0.5
}

fn fmt(r: &Rect) -> String {
    format!("({:.0},{:.0} {:.0}x{:.0})", r.x, r.y, r.w, r.h)
}

/// Wait until `f()` reads the same thing `n` times in a row, or give up.
fn settle<T: PartialEq + Clone>(mut f: impl FnMut() -> T, n: usize, budget: Duration) -> T {
    let start = Instant::now();
    let mut last = f();
    let mut streak = 1;
    while start.elapsed() < budget {
        sleep(Duration::from_millis(40));
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

fn wall_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis()
}

fn app_name(pid: i32) -> String {
    NSRunningApplication::runningApplicationWithProcessIdentifier(pid)
        .and_then(|a| a.localizedName().map(|n| n.to_string()))
        .unwrap_or_else(|| format!("pid {pid}"))
}

struct Trial {
    route: Route,
    /// Frame change while HIDDEN (before any un-hide).
    moved_at_hide: Option<Rect>,
    /// ms after un-hide until AXHidden read false.
    visible_ms: Option<u128>,
    /// ms after un-hide until the scratch app read AXFrontmost.
    stolen_ms: Option<u128>,
    /// ms after un-hide until the parked frame first changed, and where it ended.
    moved_ms: Option<u128>,
    final_frame: Rect,
    /// Who held focus at the end of the watch window (pid).
    focus_end: Option<i32>,
}

#[allow(clippy::too_many_arguments)]
fn run_trial(
    route: Route,
    scratch: i32,
    wid: WindowId,
    home: Rect,
    park: Rect,
    victim: i32,
    victim_wid: WindowId,
    shp: Option<ShowHideProcessFn>,
    active_hide: bool,
) -> Option<Trial> {
    // 1. Park the scratch window ourselves and let the WindowServer clamp it.
    ax::set_frame(wid, park);
    let parked = settle(|| frame_of(scratch, wid), 3, Duration::from_millis(1500))?;

    // 2. Hand focus to the victim, as Ordo grants focus before switching.
    //    In the pessimistic variant the scratch app is active when hidden and
    //    the victim takes focus only afterwards.
    let focus_victim = || {
        ax::focus(victim_wid);
        let focused = settle(|| ax::focused_window(), 2, Duration::from_millis(1500));
        if focused != Some(victim_wid) {
            println!("    ! victim did not take focus (got {focused:?}); trial still runs");
        }
    };
    if active_hide {
        ax::focus(wid);
        settle(|| ax::focused_window(), 2, Duration::from_millis(1500));
    } else {
        focus_victim();
    }

    // 3. Hide (the shipped way; the hide half is not under test).
    let mut moved_at_hide = None;
    if route != Route::Control {
        ax::set_app_hidden(Pid(scratch), true);
        if active_hide {
            sleep(Duration::from_millis(300));
            focus_victim();
        }
        let t = Instant::now();
        while t.elapsed() < HIDDEN_WATCH {
            if let Some(f) = frame_of(scratch, wid) {
                if !same(&f, &parked) && moved_at_hide.is_none() {
                    moved_at_hide = Some(f);
                }
            }
            sleep(POLL);
        }
        if app_bool(scratch, "AXHidden") != Some(true) {
            println!("    ! scratch never read AXHidden=true; skipping trial");
            ax::set_frame(wid, home);
            return None;
        }
    }
    let baseline = moved_at_hide.unwrap_or(parked);

    // 4. Un-hide by the route under test and watch.
    let t0 = Instant::now();
    let note = unhide(route, scratch, shp);
    let (mut visible_ms, mut stolen_ms, mut moved_ms) = (None, None, None);
    let mut last = baseline;
    while t0.elapsed() < WATCH {
        let now = t0.elapsed().as_millis();
        if visible_ms.is_none() && app_bool(scratch, "AXHidden") == Some(false) {
            visible_ms = Some(now);
        }
        if stolen_ms.is_none() && app_bool(scratch, "AXFrontmost") == Some(true) {
            stolen_ms = Some(now);
        }
        if let Some(f) = frame_of(scratch, wid) {
            if moved_ms.is_none() && !same(&f, &baseline) {
                moved_ms = Some(now);
            }
            last = f;
        }
        sleep(POLL);
    }
    let focus_end = ax::focused_window().and_then(|w| {
        if w == wid {
            Some(scratch)
        } else if w == victim_wid {
            Some(victim)
        } else {
            ax::windows()
                .into_iter()
                .find(|x| x.id == w)
                .map(|x| x.app.0)
        }
    });

    println!(
        "    {:<8} parked {} unhide:{:<12} visible@{:>5} front@{:>5} moved@{:>5} -> {}{}  focus_end={}",
        route.name(),
        fmt(&parked),
        note,
        visible_ms.map(|m| m.to_string()).unwrap_or("-".into()),
        stolen_ms.map(|m| m.to_string()).unwrap_or("-".into()),
        moved_ms.map(|m| m.to_string()).unwrap_or("-".into()),
        fmt(&last),
        moved_at_hide.map(|f| format!("  [moved AT HIDE to {}]", fmt(&f))).unwrap_or_default(),
        focus_end.map(app_name).unwrap_or("?".into()),
    );

    // 5. Leave the scratch app visible and its window home.
    if route == Route::Control || visible_ms.is_none() {
        ax::set_app_hidden(Pid(scratch), false);
    }
    ax::set_frame(wid, home);
    settle(|| frame_of(scratch, wid), 2, Duration::from_millis(1000));

    Some(Trial {
        route,
        moved_at_hide,
        visible_ms,
        stolen_ms,
        moved_ms,
        final_frame: last,
        focus_end,
    })
}

fn focused_window_of(pid: i32) -> Option<WindowId> {
    let el = app_el(pid);
    let raw = unsafe { copy_attr(&el, "AXFocusedWindow") }?;
    let id = window_id(raw as *const AXUIElement);
    unsafe { sys::CFRelease(raw) };
    id
}

/// `grant` mode: focus the scratch window while its app is hidden, un-hide by
/// `route`, and time until the grant has landed.
fn run_grant_trial(
    route: Route,
    scratch: i32,
    wid: WindowId,
    victim_wid: WindowId,
    shp: Option<ShowHideProcessFn>,
) -> Option<(Route, Option<u128>, Option<u128>)> {
    ax::focus(victim_wid);
    let focused = settle(|| ax::focused_window(), 2, Duration::from_millis(1500));
    if focused != Some(victim_wid) {
        println!("    ! victim did not take focus (got {focused:?}); trial still runs");
    }
    if route != Route::Control {
        ax::set_app_hidden(Pid(scratch), true);
        settle(
            || app_bool(scratch, "AXHidden"),
            3,
            Duration::from_millis(1000),
        );
        if app_bool(scratch, "AXHidden") != Some(true) {
            println!("    ! scratch never read AXHidden=true; skipping trial");
            return None;
        }
    }
    let t0 = Instant::now();
    ax::focus(wid);
    let grant_ms = t0.elapsed().as_millis();
    let note = unhide(route, scratch, shp);
    let (mut visible_ms, mut landed_ms) = (None, None);
    while t0.elapsed() < WATCH {
        let now = t0.elapsed().as_millis();
        if visible_ms.is_none() && app_bool(scratch, "AXHidden") == Some(false) {
            visible_ms = Some(now);
        }
        if landed_ms.is_none()
            && app_bool(scratch, "AXFrontmost") == Some(true)
            && focused_window_of(scratch) == Some(wid)
        {
            landed_ms = Some(now);
            break;
        }
        sleep(POLL);
    }
    println!(
        "    {:<8} unhide:{:<15} grant call took {grant_ms}ms  visible@{:>5}  landed@{:>5}",
        route.name(),
        note,
        visible_ms.map(|m| m.to_string()).unwrap_or("-".into()),
        landed_ms.map(|m| m.to_string()).unwrap_or("-".into()),
    );
    if visible_ms.is_none() {
        ax::set_app_hidden(Pid(scratch), false);
    }
    Some((route, visible_ms, landed_ms))
}

fn median(v: &mut Vec<u128>) -> String {
    if v.is_empty() {
        return "-".into();
    }
    v.sort();
    format!("{}", v[v.len() / 2])
}

fn main() {
    let mut args = std::env::args().skip(1);
    let scratch: i32 = args
        .next()
        .and_then(|a| a.parse().ok())
        .expect("scratch pid");
    let victim: i32 = args
        .next()
        .and_then(|a| a.parse().ok())
        .expect("victim pid");
    let trials: usize = args.next().and_then(|a| a.parse().ok()).unwrap_or(5);
    let mode = args.next().unwrap_or_default();
    let active_hide = mode == "active";
    let grant_mode = mode == "grant";

    let displays = display::active_displays();
    let main = displays
        .iter()
        .find(|d| d.is_main)
        .map(|d| d.frame)
        .expect("main display");
    let host = displays
        .iter()
        .map(|d| d.frame)
        .max_by(|a, b| (a.x + a.w).total_cmp(&(b.x + b.w)))
        .expect("a display");
    println!("main {}  park host {}", fmt(&main), fmt(&host));

    let (wid, home) = first_window(scratch).expect("scratch app has no standard window");
    let (victim_wid, _) = first_window(victim).expect("victim app has no standard window");
    let sliver: f64 = std::env::var("SLIVER")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1.0);
    let park = Rect {
        x: host.x + host.w - sliver,
        y: host.y + host.h - sliver,
        w: home.w,
        h: home.h,
    };
    let original_focus = ax::focused_window();
    let shp = show_hide_process();
    println!(
        "scratch {} ({}) window {} home {} -> park {}\nvictim {} ({}) window {}\nShowHideProcess symbol: {}\nstart wall_ms {}",
        scratch,
        app_name(scratch),
        wid.0,
        fmt(&home),
        fmt(&park),
        victim,
        app_name(victim),
        victim_wid.0,
        if shp.is_some() { "present" } else { "MISSING" },
        wall_ms()
    );
    if active_hide {
        println!("variant: scratch app is ACTIVE when hidden");
    }

    let routes = [Route::Ax, Route::Nsra, Route::Shp];
    if grant_mode {
        println!("mode: GRANT — focus the scratch window while its app is hidden, then un-hide");
        let mut out = Vec::new();
        for i in 0..trials {
            println!("  trial {}", i + 1);
            out.extend(run_grant_trial(
                Route::Control,
                scratch,
                wid,
                victim_wid,
                shp,
            ));
            for r in routes {
                if r == Route::Shp && shp.is_none() {
                    continue;
                }
                out.extend(run_grant_trial(r, scratch, wid, victim_wid, shp));
            }
        }
        println!("end wall_ms {}", wall_ms());
        ax::set_app_hidden(Pid(scratch), false);
        if let Some(f) = original_focus {
            ax::focus(f);
        }
        println!(
            "\n=== grant summary (landed = scratch frontmost AND AXFocusedWindow == target) ==="
        );
        for r in [Route::Control, Route::Ax, Route::Nsra, Route::Shp] {
            let rs: Vec<_> = out.iter().filter(|t| t.0 == r).collect();
            if rs.is_empty() {
                continue;
            }
            let n = rs.len();
            let mut landed: Vec<u128> = rs.iter().filter_map(|t| t.2).collect();
            let (min, max) = (landed.iter().min().copied(), landed.iter().max().copied());
            println!(
                "{:<8} N={n}  landed: {}/{n}  median {}ms  min {:?} max {:?}  all: {:?}",
                r.name(),
                landed.len(),
                median(&mut landed),
                min,
                max,
                rs.iter().map(|t| t.2).collect::<Vec<_>>()
            );
        }
        return;
    }
    let mut results: Vec<Trial> = Vec::new();
    for i in 0..trials {
        println!("  trial {}", i + 1);
        if i < 2 {
            results.extend(run_trial(
                Route::Control,
                scratch,
                wid,
                home,
                park,
                victim,
                victim_wid,
                shp,
                active_hide,
            ));
        }
        for r in routes {
            if r == Route::Shp && shp.is_none() {
                continue;
            }
            results.extend(run_trial(
                r,
                scratch,
                wid,
                home,
                park,
                victim,
                victim_wid,
                shp,
                active_hide,
            ));
        }
    }
    println!("end wall_ms {}", wall_ms());

    // Clean up: scratch visible and home; focus back where it was.
    ax::set_app_hidden(Pid(scratch), false);
    ax::set_frame(wid, home);
    if let Some(f) = original_focus {
        ax::focus(f);
    }
    let end_frame = settle(|| frame_of(scratch, wid), 2, Duration::from_millis(1000));
    println!(
        "scratch window now {} (home {}) {}",
        end_frame.map(|f| fmt(&f)).unwrap_or("?".into()),
        fmt(&home),
        if end_frame.is_some_and(|f| same(&f, &home)) {
            "restored"
        } else {
            "NOT RESTORED"
        }
    );

    println!("\n=== summary (N trials; theft = scratch read AXFrontmost within {}ms; re-home = parked frame changed) ===", WATCH.as_millis());
    for r in [Route::Control, Route::Ax, Route::Nsra, Route::Shp] {
        let rs: Vec<&Trial> = results.iter().filter(|t| t.route == r).collect();
        if rs.is_empty() {
            continue;
        }
        let n = rs.len();
        let mut vis: Vec<u128> = rs.iter().filter_map(|t| t.visible_ms).collect();
        let mut stolen: Vec<u128> = rs.iter().filter_map(|t| t.stolen_ms).collect();
        let mut moved: Vec<u128> = rs.iter().filter_map(|t| t.moved_ms).collect();
        let at_hide = rs.iter().filter(|t| t.moved_at_hide.is_some()).count();
        let focus_scratch = rs
            .iter()
            .filter(|t| t.focus_end.is_some_and(|p| p != victim))
            .count();
        let dests: Vec<String> = rs
            .iter()
            .filter(|t| t.moved_ms.is_some())
            .map(|t| fmt(&t.final_frame))
            .collect();
        println!(
            "{:<8} N={n}  visible: {}/{n} (median {}ms)  focus theft: {}/{n} (median {}ms; focus not on victim at end: {}/{n})  re-home: {}/{n} (median {}ms; moved at HIDE: {at_hide}/{n})",
            r.name(),
            vis.len(),
            median(&mut vis),
            stolen.len(),
            median(&mut stolen),
            focus_scratch,
            moved.len(),
            median(&mut moved),
        );
        if !dests.is_empty() {
            println!("         re-home destinations: {}", dests.join(" "));
        }
    }
}
