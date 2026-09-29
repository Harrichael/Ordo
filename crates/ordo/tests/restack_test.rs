//! The restack against a fake window server with the raise physics probed on
//! Tahoe: a raise lands just below the key window, a raise of another window
//! of the key window's app lands above it, and focusing a window puts it on
//! top. Every raise and focus lands after its app's own latency, on a fake
//! clock that only moves when the restack waits.
//!
//! Tests judge the final stack by geometry alone: every pair of windows that
//! overlap on screen must be in priority order.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use ordo::restack::{reassert, Seen, WindowServer};
use ordo_core::{Rect, WindowId};

enum Act {
    Raise(WindowId),
    Focus(WindowId),
}

struct Fake {
    start: Instant,
    t: Duration,
    /// Front to back.
    stack: Vec<Seen>,
    key: Option<WindowId>,
    displays: Vec<Rect>,
    latency: HashMap<i32, Duration>,
    due: Vec<(Duration, Act)>,
    raised: Vec<WindowId>,
    landed: HashMap<WindowId, Duration>,
}

fn rect(x: f64, y: f64, w: f64, h: f64) -> Rect {
    Rect { x, y, w, h }
}

fn w(n: u32) -> WindowId {
    WindowId(n)
}

const MAIN: Rect = Rect {
    x: 0.0,
    y: 0.0,
    w: 1920.0,
    h: 1080.0,
};
const EXTERNAL: Rect = Rect {
    x: 1920.0,
    y: 0.0,
    w: 1920.0,
    h: 1080.0,
};

impl Fake {
    /// `stack` front to back, as (window, app, frame).
    fn new(stack: &[(WindowId, i32, Rect)], key: Option<WindowId>) -> Fake {
        Fake {
            start: Instant::now(),
            t: Duration::ZERO,
            stack: stack
                .iter()
                .map(|&(id, pid, frame)| Seen { id, pid, frame })
                .collect(),
            key,
            displays: vec![MAIN, EXTERNAL],
            latency: HashMap::new(),
            due: Vec::new(),
            raised: Vec::new(),
            landed: HashMap::new(),
        }
    }

    fn latency(mut self, pid: i32, ms: u64) -> Fake {
        self.latency.insert(pid, Duration::from_millis(ms));
        self
    }

    fn pid(&self, w: WindowId) -> Option<i32> {
        self.stack.iter().find(|s| s.id == w).map(|s| s.pid)
    }

    fn pos(&self, w: WindowId) -> Option<usize> {
        self.stack.iter().position(|s| s.id == w)
    }

    fn schedule(&mut self, w: WindowId, act: Act) {
        let lag = self.pid(w).and_then(|p| self.latency.get(&p)).copied();
        self.due.push((self.t + lag.unwrap_or_default(), act));
    }

    fn apply(&mut self, act: Act) {
        match act {
            Act::Raise(w) => {
                let key_pid = self.key.and_then(|k| self.pid(k));
                let Some(i) = self.pos(w) else { return };
                let s = self.stack.remove(i);
                let at = if self.key == Some(w) || key_pid == Some(s.pid) {
                    0
                } else {
                    self.key.and_then(|k| self.pos(k)).map_or(0, |k| k + 1)
                };
                self.stack.insert(at, s);
                self.landed.insert(w, self.t);
            }
            Act::Focus(w) => {
                let Some(i) = self.pos(w) else { return };
                let s = self.stack.remove(i);
                self.stack.insert(0, s);
                self.key = Some(w);
            }
        }
    }

    /// Advance to `to`, landing everything due by then, in order.
    fn advance(&mut self, to: Duration) -> bool {
        let mut any = false;
        loop {
            let next = self
                .due
                .iter()
                .enumerate()
                .filter(|(_, (t, _))| *t <= to)
                .min_by_key(|(_, (t, _))| *t)
                .map(|(i, _)| i);
            let Some(i) = next else { break };
            let (t, act) = self.due.remove(i);
            self.t = self.t.max(t);
            self.apply(act);
            any = true;
        }
        self.t = self.t.max(to);
        any
    }

    /// Land whatever is still in flight once the restack has returned.
    fn drain(&mut self) {
        self.advance(Duration::from_secs(3600));
    }

    /// Pairs that overlap on screen and are out of priority order.
    fn out_of_order(&self, priority: &[WindowId]) -> Vec<(WindowId, WindowId)> {
        let on_screen = |f: &Rect| self.displays.iter().any(|d| overlaps(f, d));
        let mut out = Vec::new();
        for (a, &hi) in priority.iter().enumerate() {
            for &lo in &priority[a + 1..] {
                let (Some(i), Some(j)) = (self.pos(hi), self.pos(lo)) else {
                    continue;
                };
                let (fi, fj) = (&self.stack[i].frame, &self.stack[j].frame);
                if on_screen(fi) && on_screen(fj) && overlaps(fi, fj) && i > j {
                    out.push((hi, lo));
                }
            }
        }
        out
    }
}

fn overlaps(a: &Rect, b: &Rect) -> bool {
    (a.x + a.w).min(b.x + b.w) - a.x.max(b.x) > 2.0
        && (a.y + a.h).min(b.y + b.h) - a.y.max(b.y) > 2.0
}

impl WindowServer for Fake {
    fn now(&self) -> Instant {
        self.start + self.t
    }

    fn read_stack(&mut self, out: &mut Vec<Seen>) {
        out.clear();
        out.extend_from_slice(&self.stack);
    }

    fn displays(&mut self) -> Vec<Rect> {
        self.displays.clone()
    }

    fn is_key(&mut self, w: WindowId, _pid: i32) -> bool {
        self.key == Some(w)
    }

    fn focused_window(&mut self) -> Option<WindowId> {
        self.key
    }

    fn focus(&mut self, w: WindowId) -> bool {
        self.schedule(w, Act::Focus(w));
        true
    }

    fn raise(&mut self, w: WindowId, _pid: i32) -> bool {
        if self.pos(w).is_none() {
            return false;
        }
        self.raised.push(w);
        self.schedule(w, Act::Raise(w));
        true
    }

    fn wait(&mut self, until: Instant, _cancel: &dyn Fn() -> bool) -> bool {
        let until = until.saturating_duration_since(self.start);
        let next = self.due.iter().map(|(t, _)| *t).min();
        match next {
            Some(t) if t <= until => self.advance(t),
            _ => self.advance(until),
        }
    }
}

fn never() -> bool {
    false
}

/// Two displays, the priority interleaving them as MRU does. The main display
/// is already in order; on the external one, the window that should be in
/// front is behind. Only that one window is raised.
#[test]
fn a_display_already_in_order_is_left_alone() {
    let on_main = |n: f64| rect(100.0 + 40.0 * n, 100.0 + 40.0 * n, 800.0, 600.0);
    let on_ext = |n: f64| rect(2100.0 + 40.0 * n, 100.0 + 40.0 * n, 800.0, 600.0);
    let mut ws = Fake::new(
        &[
            (w(1), 1, on_main(0.0)),
            (w(5), 5, on_ext(1.0)),
            (w(2), 2, on_main(1.0)),
            (w(4), 4, on_ext(0.0)),
            (w(3), 3, on_main(2.0)),
        ],
        Some(w(1)),
    );
    let priority = [w(1), w(4), w(2), w(5), w(3)];

    let stats = reassert(&mut ws, &priority, true, &never).unwrap();
    ws.drain();

    assert_eq!(ws.out_of_order(&priority), vec![]);
    assert_eq!(ws.raised, vec![w(4)]);
    assert!(stats.converged);
}

/// The key window's own app's windows land above it when raised. Ordering
/// them under it, around another app's window, still ends with the key
/// window on top and every window in order.
#[test]
fn siblings_ordered_under_the_key_window_leave_it_on_top() {
    let f = |n: f64| rect(100.0 + 30.0 * n, 100.0, 900.0, 700.0);
    let (top, s1, other, s2) = (w(1), w(2), w(3), w(4));
    let mut ws = Fake::new(
        &[
            (top, 1, f(0.0)),
            (s2, 1, f(1.0)),
            (other, 2, f(2.0)),
            (s1, 1, f(3.0)),
        ],
        Some(top),
    )
    .latency(1, 8)
    .latency(2, 20);
    let priority = [top, s1, other, s2];

    reassert(&mut ws, &priority, true, &never).unwrap();
    ws.drain();

    assert_eq!(ws.out_of_order(&priority), vec![]);
    assert_eq!(ws.stack[0].id, top);
    assert_eq!(ws.key, Some(top));
}

/// A window from a slow app needs raising on the external display, and one
/// from a fast app on the main display. The two displays share no overlap, so
/// the fast one lands in its own time instead of waiting behind the slow one.
#[test]
fn a_slow_app_on_one_display_does_not_hold_up_the_other() {
    let on_main = |n: f64| rect(100.0 + 40.0 * n, 100.0, 800.0, 600.0);
    let on_ext = |n: f64| rect(2100.0 + 40.0 * n, 100.0, 800.0, 600.0);
    let (top, fast, below_fast, slow, below_slow) = (w(1), w(2), w(3), w(4), w(5));
    let mut ws = Fake::new(
        &[
            (top, 1, on_main(0.0)),
            (below_slow, 5, on_ext(1.0)),
            (below_fast, 3, on_main(2.0)),
            (fast, 2, on_main(1.0)),
            (slow, 4, on_ext(0.0)),
        ],
        Some(top),
    )
    .latency(2, 10)
    .latency(4, 900);
    let priority = [top, slow, fast, below_slow, below_fast];

    let stats = reassert(&mut ws, &priority, true, &never).unwrap();
    ws.drain();

    assert_eq!(ws.out_of_order(&priority), vec![]);
    assert!(ws.landed[&fast] < Duration::from_millis(50));
    assert!(ws.landed[&slow] >= Duration::from_millis(900));
    assert_eq!(stats.lanes, 2);
}

/// A parked window shows a 1pt sliver at the display's edge, and a window
/// flush against that edge overlaps it by that point. That is no order anyone
/// can see, so it raises nothing, even with the parked window ranked above.
#[test]
fn a_parked_sliver_constrains_nothing() {
    let (top, parked, flush) = (w(1), w(2), w(3));
    let mut ws = Fake::new(
        &[
            (top, 1, rect(900.0, 100.0, 800.0, 600.0)),
            (flush, 3, rect(0.0, 100.0, 800.0, 600.0)),
            (parked, 2, rect(-799.0, 100.0, 800.0, 600.0)),
        ],
        Some(top),
    );
    let priority = [top, parked, flush];

    let stats = reassert(&mut ws, &priority, true, &never).unwrap();

    assert_eq!(ws.raised, vec![]);
    assert_eq!(stats.presence_wait_ms, 0);
}

/// The window being demoted is still key when the restack starts, sitting on
/// top of the window that must now go above it. Nothing can be raised above
/// a key window, so the restack waits for the focus to hand over, then
/// orders the rest.
#[test]
fn the_focus_handoff_is_waited_out_before_raising_over_the_old_key_window() {
    let f = |n: f64| rect(100.0 + 30.0 * n, 100.0, 900.0, 700.0);
    let (top, middle, demoted) = (w(1), w(2), w(3));
    let mut ws = Fake::new(
        &[(demoted, 3, f(2.0)), (middle, 2, f(1.0)), (top, 1, f(0.0))],
        Some(demoted),
    )
    .latency(1, 80);
    let priority = [top, middle, demoted];

    reassert(&mut ws, &priority, true, &never).unwrap();
    ws.drain();

    assert_eq!(ws.out_of_order(&priority), vec![]);
    assert_eq!(ws.key, Some(top));
}

/// A seeded xorshift, so the layouts are random but every run is the same.
struct Rng(u64);

impl Rng {
    fn below(&mut self, n: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % n
    }
}

/// Random layouts over two displays: random frames, a few apps with random
/// latencies, a random starting order, and either a restack that takes focus
/// from whichever window holds it or one whose key window is off in some
/// other app. Every one ends with every overlapping pair in order, in one
/// pass, with no raise confirmed before its app applied it, and none
/// outliving its landing timeout: a raise wrongly taken as landed would show
/// up as a second pass, and one that can't land where it was sent as a
/// timeout.
#[test]
fn any_layout_ends_in_order() {
    let mut rng = Rng(0x2545_F491_4F6C_DD1D);
    for case in 0..5000 {
        let n = 2 + rng.below(7) as u32;
        let mut windows: Vec<(WindowId, i32, Rect)> = (1..=n)
            .map(|i| {
                let x = rng.below(3000) as f64;
                let y = rng.below(700) as f64;
                let size = (200 + rng.below(900)) as f64;
                (w(i), 1 + rng.below(3) as i32, rect(x, y, size, size * 0.7))
            })
            .collect();
        for i in (1..windows.len()).rev() {
            windows.swap(i, rng.below(i as u64 + 1) as usize);
        }
        let focus_top = rng.below(2) == 0;
        let key = if focus_top {
            Some(w(1 + rng.below(n as u64) as u32))
        } else {
            // Key status is off in another app, as on an empty display's
            // desktop.
            windows.insert(0, (w(99), 9, rect(0.0, 1000.0, 50.0, 50.0)));
            Some(w(99))
        };
        let mut ws = Fake::new(&windows, key);
        for pid in 1..=3 {
            ws = ws.latency(pid, rng.below(60));
        }
        let priority: Vec<WindowId> = (1..=n).map(w).collect();

        let stats = reassert(&mut ws, &priority, focus_top, &never);
        ws.drain();

        assert_eq!(ws.out_of_order(&priority), vec![], "case {case}");
        let stats = stats.unwrap_or_else(|| panic!("case {case}: nothing reported"));
        assert!(!stats.second_pass, "case {case}");
        assert!(stats.raises.iter().all(|r| !r.timed_out), "case {case}");
        // A raise can't be confirmed before its app has applied it.
        assert!(
            stats
                .raises
                .iter()
                .all(|r| Duration::from_millis(r.wait_ms) >= ws.latency[&r.pid]),
            "case {case}"
        );
    }
}
