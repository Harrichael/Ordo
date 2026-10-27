//! Restacking as a switch's moves land: a pass each time they go quiet,
//! until the last one is in, against fake apps whose moves take set times.
//!
//! The passes here stand in for a reassert and record what had landed when
//! they ran; what matters is that a slow app's window is ordered once it
//! arrives, not left wherever it landed.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ordo::app_queue::{AppQueues, AppSession};
use ordo::ports::RestackStats;
use ordo::restack::{follow, QUIET};
use ordo_core::{Pid, Point, Rect, WindowId};
use ordo_emulated::{HoldStat, Move};

const FAST: Pid = Pid(1);
const SLOW: Pid = Pid(2);

/// Windows whose moves have returned, in order.
type Landed = Arc<Mutex<Vec<WindowId>>>;

struct App {
    each_move: Duration,
    landed: Landed,
}

impl AppSession for App {
    fn move_windows(
        &mut self,
        moves: &[(WindowId, Point)],
        _cancel: &dyn Fn() -> bool,
    ) -> Vec<(WindowId, bool, f64)> {
        std::thread::sleep(self.each_move * moves.len() as u32);
        self.landed
            .lock()
            .unwrap()
            .extend(moves.iter().map(|(w, _)| *w));
        moves.iter().map(|(w, _)| (*w, true, 0.0)).collect()
    }
    fn set_frame(&mut self, _: WindowId, _: Rect) -> bool {
        true
    }
    fn show(&mut self, _: &[(WindowId, Point)], _: &dyn Fn() -> bool) -> HoldStat {
        unreachable!("no un-hides here")
    }
    fn hide(&mut self) {}
    fn find(&mut self, _: WindowId) -> bool {
        true
    }
    fn front(&mut self, _: WindowId) {}
    fn make_key(&mut self, _: WindowId) {}
    fn front_desktop(&mut self, _: u32) {}
}

/// Queues for a fast app, whose moves take 10ms as a real app's might, and
/// one whose every move takes `slow`.
fn apps(slow: Duration) -> (AppQueues, Landed) {
    let landed: Landed = Arc::default();
    let l = landed.clone();
    let queues = AppQueues::new(move |pid| {
        Box::new(App {
            each_move: if pid == SLOW {
                slow
            } else {
                Duration::from_millis(10)
            },
            landed: l.clone(),
        }) as Box<dyn AppSession>
    });
    (queues, landed)
}

fn restore(pid: Pid, window: u32) -> Move {
    Move {
        pid,
        window: WindowId(window),
        to: Point { x: 0.0, y: 0.0 },
        parks: false,
    }
}

/// What had landed when each pass started, and when.
type Passes = Vec<(Duration, Vec<WindowId>)>;

fn run(
    queues: &AppQueues,
    landed: &Landed,
    apps: &[Pid],
    pass: impl Fn(&dyn Fn() -> bool),
) -> Passes {
    let landing = queues.marker(apps);
    let t0 = Instant::now();
    let mut passes = Vec::new();
    follow(
        queues,
        &landing,
        &|| false,
        &mut |cancel| -> Option<RestackStats> {
            passes.push((t0.elapsed(), landed.lock().unwrap().clone()));
            pass(cancel);
            None
        },
        &mut |_| {},
    );
    passes
}

/*
 * The case that left a terminal over Chrome: the fast app's window lands at
 * once, the slow app's 300ms later. A pass orders what's there once the fast
 * app goes quiet, without waiting on the slow one, and another orders the
 * slow app's window once it lands. Nothing runs after that.
 */
#[test]
fn a_slow_apps_window_is_ordered_once_it_lands() {
    let (queues, landed) = apps(Duration::from_millis(300));
    queues.move_windows(&[restore(FAST, 1), restore(SLOW, 2)]);
    let passes = run(&queues, &landed, &[FAST, SLOW], |_| {});

    let (first_at, first_saw) = &passes[0];
    assert_eq!(
        first_saw,
        &[WindowId(1)],
        "the first pass doesn't wait for the slow app"
    );
    assert!(
        *first_at >= QUIET && *first_at < Duration::from_millis(200),
        "{first_at:?}"
    );

    let (last_at, last_saw) = passes.last().unwrap();
    assert!(
        last_saw.contains(&WindowId(2)),
        "a pass after the slow app landed"
    );
    assert!(
        *last_at >= Duration::from_millis(300) + QUIET,
        "{last_at:?}"
    );
    assert_eq!(passes.len(), 2, "{passes:?}");
}

/*
 * With nothing on its way there's nothing to wait for: one pass, at once.
 */
#[test]
fn with_no_moves_on_their_way_one_pass_runs_at_once() {
    let (queues, landed) = apps(Duration::ZERO);
    let passes = run(&queues, &landed, &[FAST], |_| {});

    assert_eq!(passes.len(), 1);
    assert!(passes[0].0 < QUIET, "{:?}", passes[0].0);
}

/*
 * A move that lands while a pass is running cancels it: the pass planned
 * against a screen that has since changed. The next pass replans after
 * quiet, with the late window in.
 */
#[test]
fn a_move_landing_mid_pass_cancels_it_and_the_next_replans() {
    let (queues, landed) = apps(Duration::from_millis(150));
    queues.move_windows(&[restore(FAST, 1), restore(SLOW, 2)]);
    let cancelled = Mutex::new(Vec::new());
    let passes = run(&queues, &landed, &[FAST, SLOW], |cancel| {
        // A pass slow enough for the slow app to land during it.
        let until = Instant::now() + Duration::from_millis(400);
        while !cancel() && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(1));
        }
        cancelled.lock().unwrap().push(cancel());
    });

    assert_eq!(*cancelled.lock().unwrap(), [true, false]);
    assert_eq!(passes[1].1, [WindowId(1), WindowId(2)]);
}
