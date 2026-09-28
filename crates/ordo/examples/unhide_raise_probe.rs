//! Probe: after a hide, where does an un-hidden window land, and which route
//! gets it back on top fastest?
//!
//! Measured in the daemon's log: a window hidden while it sat just above
//! another app's window comes back from the un-hide directly UNDER that
//! window (9 of 11 real hides), and the stacking worker's raise then has to
//! put it back, at the app's own pace. This asks whether any route avoids the
//! drop, or shortens it.
//!
//! Setup: two scratch apps you launched yourself, their windows overlapping.
//! `top` plays the app that gets hidden (Slack), `under` the app that stays
//! shown (Chrome). The terminal running this stays key, so both are
//! background windows, as they are during a switch. Each trial first puts
//! `top` above `under`, hides `top` and waits for the hide to land, then
//! brings it back by one route:
//!
//!   - none:  un-hide only, never raise. Where does it land by itself?
//!   - after: un-hide, wait until the window is back in the window list,
//!     then raise it (what the stacking worker does today).
//!   - with:  raise in the same breath as the un-hide, without waiting.
//!   - first: raise while still hidden, then un-hide.
//!   - front: bring the app frontmost with all its windows instead of an
//!     un-hide. Takes focus by design; measured to see what it costs.
//!
//! Routes are interleaved trial by trial so drift in the desktop can't pass
//! for a difference between them.
//!
//!   open -g -a TextEdit /tmp/a.txt ; open -g -a Calculator   # overlap them
//!   cargo run --example unhide_raise_probe -- <top> <under> [trials] [hidden_ms] [mode]
//!
//! `<top>` and `<under>` are a pid, or `pid:window` to pick one window.
//!
//! `RAISE_WHILE_HIDDEN=<window>` (env): raise that window while `top` is
//! hidden, as a switch's stacking does on the workspace in between.
//!
//! `hidden_ms` (default 300): how long `top` stays hidden before coming back.
//!
//! `park`: do what Ordo does around the hide — move `top` off the left edge
//! (a 1pt sliver left) before hiding it, and move it home while it is still
//! hidden, right before it comes back. `park-both`: park and restore `under`
//! alongside it (never hidden), as a switch does to every window it leaves.

use std::thread::sleep;
use std::time::{Duration, Instant};

use objc2_app_kit::NSRunningApplication;
use ordo::platform::{ax, zorder};
use ordo_core::{Pid, Point, Rect, WindowId};
use ordo_skylight_sys as sys;

const POLL: Duration = Duration::from_millis(2);
const WATCH: Duration = Duration::from_millis(1500);
const HIDE_BUDGET: Duration = Duration::from_millis(2000);
/// Brings every window of the process forward along with it.
const CPS_ALL_WINDOWS: u32 = 0x100;

#[derive(Clone, Copy, PartialEq)]
enum Route {
    None,
    After,
    With,
    First,
    Front,
}

impl Route {
    fn name(self) -> &'static str {
        match self {
            Route::None => "none",
            Route::After => "after",
            Route::With => "with",
            Route::First => "first",
            Route::Front => "front",
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Place {
    Missing,
    Above,
    DirectlyUnder,
    FurtherUnder,
}

impl Place {
    fn name(self) -> &'static str {
        match self {
            Place::Missing => "missing",
            Place::Above => "above",
            Place::DirectlyUnder => "directly under",
            Place::FurtherUnder => "further under",
        }
    }
}

fn place(top: WindowId, under: WindowId) -> Place {
    let stack = zorder::stack_front_to_back();
    let (Some(t), Some(u)) = (
        stack.iter().position(|w| *w == top),
        stack.iter().position(|w| *w == under),
    ) else {
        return Place::Missing;
    };
    if t < u {
        Place::Above
    } else if t == u + 1 {
        Place::DirectlyUnder
    } else {
        Place::FurtherUnder
    }
}

struct Trial {
    route: Route,
    /// Milliseconds from the route's first call until the window is back in
    /// the window list, and where it was at that first sighting.
    back: Option<(u64, Place)>,
    /// Milliseconds until it first read above `under`.
    on_top: Option<u64>,
    end: Place,
    focus_stolen: bool,
    /// `first` only: whether the raise alone brought the hidden window back.
    raise_unhid: bool,
}

fn run_trial(
    route: Route,
    top: (Pid, WindowId),
    under: WindowId,
    hidden: Duration,
    park: &[(Pid, WindowId, Rect)],
    left_edge: f64,
    raise_while_hidden: Option<WindowId>,
) -> Trial {
    let (pid, win) = top;
    let key = ax::focused_window();

    if !park.is_empty() {
        let away: Vec<_> = park
            .iter()
            .map(|&(p, w, home)| {
                (
                    p,
                    w,
                    Point {
                        x: left_edge - home.w + 1.0,
                        y: home.y,
                    },
                )
            })
            .collect();
        ax::move_windows(&away);
        sleep(Duration::from_millis(50));
    }
    ax::set_app_hidden(pid, true);
    let t = Instant::now();
    while place(win, under) != Place::Missing && t.elapsed() < HIDE_BUDGET {
        sleep(POLL);
    }
    if let Some(w) = raise_while_hidden {
        ax::raise(w);
    }
    sleep(hidden);
    let homes: Vec<_> = park
        .iter()
        .map(|&(p, w, home)| (p, w, Point { x: home.x, y: home.y }))
        .collect();
    ax::move_windows(&homes);

    let mut raise_unhid = false;
    let start = Instant::now();
    match route {
        Route::None | Route::After => ax::set_app_hidden(pid, false),
        Route::With => {
            ax::set_app_hidden(pid, false);
            ax::raise(win);
        }
        Route::First => {
            ax::raise(win);
            let t = Instant::now();
            while t.elapsed() < Duration::from_millis(100) {
                if place(win, under) != Place::Missing {
                    raise_unhid = true;
                    break;
                }
                sleep(POLL);
            }
            ax::set_app_hidden(pid, false);
        }
        Route::Front => unsafe {
            let mut psn = sys::ProcessSerialNumber::default();
            if sys::GetProcessForPID(pid.0, &mut psn) == 0 {
                let _ = sys::SLPSSetFrontProcessWithOptions(&psn, 0, CPS_ALL_WINDOWS);
            }
        },
    }

    let mut back = None;
    let mut on_top = None;
    let mut raised = route != Route::After;
    while start.elapsed() < WATCH {
        let p = place(win, under);
        let ms = start.elapsed().as_millis() as u64;
        if back.is_none() && p != Place::Missing {
            back = Some((ms, p));
        }
        if back.is_some() && !raised {
            ax::raise(win);
            raised = true;
        }
        if on_top.is_none() && p == Place::Above {
            on_top = Some(ms);
        }
        sleep(POLL);
    }
    let end = place(win, under);
    let focus_stolen = ax::focused_window() != key;
    if focus_stolen {
        if let Some(k) = key {
            ax::focus(k);
        }
    }
    Trial {
        route,
        back,
        on_top,
        end,
        focus_stolen,
        raise_unhid,
    }
}

/// `top` above `under`, both below the key window, as a switch leaves them.
fn reset(top: WindowId, under: WindowId) -> bool {
    ax::raise(under);
    sleep(Duration::from_millis(150));
    ax::raise(top);
    let t = Instant::now();
    while t.elapsed() < Duration::from_millis(1000) {
        if place(top, under) == Place::Above {
            sleep(Duration::from_millis(200));
            return true;
        }
        sleep(POLL);
    }
    false
}

/// `<pid>` for the app's first on-screen window, or `<pid>:<window>` for an
/// exact one — an app with several windows, some of them parked, needs it.
fn target(arg: Option<String>, what: &str) -> (i32, WindowId) {
    let arg = arg.unwrap_or_else(|| panic!("{what} pid"));
    let (pid, win) = arg.split_once(':').unwrap_or((&arg, ""));
    let pid: i32 = pid.parse().unwrap_or_else(|_| panic!("{what} pid"));
    let win = match win.parse() {
        Ok(w) => WindowId(w),
        Err(_) => first_window(pid).unwrap_or_else(|| panic!("{what} app has no on-screen window")),
    };
    (pid, win)
}

fn first_window(pid: i32) -> Option<WindowId> {
    let on_screen = zorder::stack_front_to_back();
    ax::windows()
        .into_iter()
        .find(|w| w.app == Pid(pid) && on_screen.contains(&w.id))
        .map(|w| w.id)
}

fn app_name(pid: i32) -> String {
    NSRunningApplication::runningApplicationWithProcessIdentifier(pid)
        .and_then(|a| a.localizedName().map(|n| n.to_string()))
        .unwrap_or_else(|| format!("pid {pid}"))
}

fn median(mut v: Vec<u64>) -> String {
    if v.is_empty() {
        return "-".into();
    }
    v.sort_unstable();
    format!("{}ms", v[v.len() / 2])
}

fn main() {
    let mut args = std::env::args().skip(1);
    let (top_pid, top) = target(args.next(), "top");
    let (under_pid, under) = target(args.next(), "under");
    let trials: usize = args.next().and_then(|a| a.parse().ok()).unwrap_or(5);
    let hidden = Duration::from_millis(args.next().and_then(|a| a.parse().ok()).unwrap_or(300));
    let mode = args.next().unwrap_or_default();
    let raise_while_hidden = std::env::var("RAISE_WHILE_HIDDEN")
        .ok()
        .and_then(|v| v.parse().ok())
        .map(WindowId);
    if let Some(w) = raise_while_hidden {
        println!("raising window {} while top is hidden", w.0);
    }

    println!(
        "top {} ({}) window {}\nunder {} ({}) window {}\nkey window {:?}, hidden for {}ms",
        top_pid,
        app_name(top_pid),
        top.0,
        under_pid,
        app_name(under_pid),
        under.0,
        ax::focused_window().map(|w| w.0),
        hidden.as_millis()
    );

    let left_edge = ordo::platform::display::active_displays()
        .iter()
        .map(|d| d.frame.x)
        .fold(f64::INFINITY, f64::min);
    let home = |w: WindowId| zorder::window_bounds(w).expect("window frame");
    let park: Vec<(Pid, WindowId, Rect)> = match mode.as_str() {
        "park" => vec![(Pid(top_pid), top, home(top))],
        "park-both" => vec![
            (Pid(top_pid), top, home(top)),
            (Pid(under_pid), under, home(under)),
        ],
        _ => Vec::new(),
    };
    if !park.is_empty() {
        println!("mode {mode}: parking {} window(s) off x={left_edge:.0}", park.len());
    }

    let routes = [Route::None, Route::After, Route::With, Route::First, Route::Front];
    let mut out: Vec<Trial> = Vec::new();
    for i in 0..trials {
        for &route in &routes {
            if !reset(top, under) {
                println!("  trial {} {}: could not put top above under; skipped", i + 1, route.name());
                continue;
            }
            let r = run_trial(
                route,
                (Pid(top_pid), top),
                under,
                hidden,
                &park,
                left_edge,
                raise_while_hidden,
            );
            println!(
                "  trial {} {:5}: back {:>16}  on top {:>6}  end {:14}{}{}",
                i + 1,
                route.name(),
                r.back
                    .map(|(ms, p)| format!("{ms}ms {}", p.name()))
                    .unwrap_or_else(|| "never".into()),
                r.on_top.map(|ms| format!("{ms}ms")).unwrap_or_else(|| "never".into()),
                r.end.name(),
                if r.focus_stolen { "  FOCUS STOLEN" } else { "" },
                if r.raise_unhid { "  (raise un-hid it)" } else { "" },
            );
            out.push(r);
        }
    }
    ax::set_app_hidden(Pid(top_pid), false);

    println!("\nroute  n  back(med)  landed directly under  on top(med)  never on top  focus stolen");
    for &route in &routes {
        let rs: Vec<&Trial> = out.iter().filter(|t| t.route == route).collect();
        let under_n = rs
            .iter()
            .filter(|t| matches!(t.back, Some((_, Place::DirectlyUnder))))
            .count();
        println!(
            "{:5} {:2}  {:>9}  {:>21}  {:>11}  {:>12}  {:>12}",
            route.name(),
            rs.len(),
            median(rs.iter().filter_map(|t| t.back.map(|(ms, _)| ms)).collect()),
            under_n,
            median(rs.iter().filter_map(|t| t.on_top).collect()),
            rs.iter().filter(|t| t.on_top.is_none()).count(),
            rs.iter().filter(|t| t.focus_stolen).count(),
        );
    }
}
