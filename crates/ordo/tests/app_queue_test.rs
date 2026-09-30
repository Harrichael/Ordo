//! The apps' queues, against fake apps that record what reached them.
//!
//! A test can hold an app mid-job with its gate: whatever is queued for that
//! app meanwhile is still ours to replace or drop, exactly the window a real
//! app's slow main thread opens.

use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use ordo::app_queue::{AppQueues, AppSession, LAND_WINDOW};
use ordo_core::{Pid, Point, Rect, WindowId};
use ordo_emulated::{HoldStat, Move};

const A: Pid = Pid(1);
const B: Pid = Pid(2);

fn w(n: u32) -> WindowId {
    WindowId(n)
}

fn at(x: f64) -> Point {
    Point { x, y: 0.0 }
}

fn restores(list: &[(Pid, WindowId, Point)]) -> Vec<Move> {
    list.iter()
        .map(|(pid, window, to)| Move {
            pid: *pid,
            window: *window,
            to: *to,
            parks: false,
        })
        .collect()
}

fn park(pid: Pid, window: WindowId, x: f64) -> Move {
    Move {
        pid,
        window,
        to: at(x),
        parks: true,
    }
}

#[derive(Default)]
struct World {
    /// What reached each app, in the order it did.
    log: Mutex<Vec<(Pid, String)>>,
    closed: Mutex<Vec<Pid>>,
    opened: Condvar,
    /// Windows whose writes the app refuses, as for a window that is gone.
    gone: Mutex<Vec<WindowId>>,
    /// Each write to this app takes this long.
    slow: Mutex<Vec<(Pid, Duration)>>,
    /// A write to this window panics, as a bug in a session would.
    panics_on: Mutex<Option<WindowId>>,
    opened_sessions: Mutex<Vec<Pid>>,
    /// While set, only this many more calls get through the gate, to any
    /// closed app.
    passes: Mutex<Option<usize>>,
}

impl World {
    fn close(&self, pid: Pid) {
        self.closed.lock().unwrap().push(pid);
    }

    fn open(&self, pid: Pid) {
        self.closed.lock().unwrap().retain(|p| *p != pid);
        self.opened.notify_all();
    }

    /// Let `n` more calls through to closed apps, then hold them again.
    fn let_through(&self, n: usize) {
        *self.passes.lock().unwrap() = Some(n);
        self.opened.notify_all();
    }

    fn reached(&self, pid: Pid) -> Vec<String> {
        let log = self.log.lock().unwrap();
        log.iter()
            .filter(|(p, s)| *p == pid && !s.starts_with("fronted"))
            .map(|(_, s)| s.clone())
            .collect()
    }

    /// Where in the whole log, across apps, this entry of this app first is.
    fn when(&self, pid: Pid, what: &str) -> usize {
        let log = self.log.lock().unwrap();
        log.iter()
            .position(|(p, s)| *p == pid && s == what)
            .unwrap_or_else(|| panic!("{pid:?} never saw {what}"))
    }

    /// Wait until the app has started on something, so what is queued next
    /// sits behind it.
    fn busy(&self, pid: Pid) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while self.reached(pid).is_empty() {
            assert!(Instant::now() < deadline, "app {pid:?} never started");
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}

struct FakeApp {
    pid: Pid,
    world: Arc<World>,
}

impl FakeApp {
    fn arrive(&self, what: String) {
        self.world.log.lock().unwrap().push((self.pid, what));
        let mut closed = self.world.closed.lock().unwrap();
        loop {
            if !closed.contains(&self.pid) {
                break;
            }
            let mut passes = self.world.passes.lock().unwrap();
            if let Some(n) = passes.as_mut().filter(|n| **n > 0) {
                *n -= 1;
                break;
            }
            drop(passes);
            closed = self.world.opened.wait(closed).unwrap();
        }
        drop(closed);
        let slow = self.world.slow.lock().unwrap().iter().find(|(p, _)| *p == self.pid).map(|(_, d)| *d);
        if let Some(d) = slow {
            std::thread::sleep(d);
        }
    }
}

impl AppSession for FakeApp {
    fn move_windows(
        &mut self,
        moves: &[(WindowId, Point)],
        cancel: &dyn Fn() -> bool,
    ) -> Vec<(WindowId, bool, f64)> {
        let mut out = Vec::new();
        for (window, to) in moves {
            if cancel() {
                break;
            }
            self.arrive(format!("move {} to {}", window.0, to.x));
            if *self.world.panics_on.lock().unwrap() == Some(*window) {
                panic!("a session bug");
            }
            let took = !self.world.gone.lock().unwrap().contains(window);
            out.push((*window, took, 0.0));
        }
        out
    }

    fn set_frame(&mut self, window: WindowId, to: Rect) -> bool {
        self.arrive(format!("frame {} to {}", window.0, to.x));
        true
    }

    fn show(&mut self, hold: &[(WindowId, Point)], _cancel: &dyn Fn() -> bool) -> HoldStat {
        let held: Vec<String> = hold.iter().map(|(w, _)| w.0.to_string()).collect();
        self.arrive(format!("show holding [{}]", held.join(" ")));
        HoldStat::new(self.pid, true, hold.len(), 0, 0, Vec::new())
    }

    fn hide(&mut self) {
        self.arrive("hide".into());
    }

    fn find(&mut self, _window: WindowId) -> bool {
        true
    }

    fn front(&mut self, window: WindowId) {
        self.arrive(format!("focus {}", window.0));
        self.world.log.lock().unwrap().push((self.pid, format!("fronted {}", window.0)));
    }

    fn make_key(&mut self, _window: WindowId) {}

    fn front_desktop(&mut self, window: u32) {
        self.arrive(format!("desktop {window}"));
    }
}

fn queues() -> (AppQueues, Arc<World>) {
    let world = Arc::new(World::default());
    let w2 = world.clone();
    let q = AppQueues::new(move |pid| {
        w2.opened_sessions.lock().unwrap().push(pid);
        Box::new(FakeApp {
            pid,
            world: w2.clone(),
        })
    });
    (q, world)
}

fn landed(q: &AppQueues, apps: &[Pid]) {
    assert!(
        q.marker(apps).wait(Instant::now() + Duration::from_secs(2), &|| false),
        "the queues never drained"
    );
}

/// A switch is one app's moves, then its un-hide, then its focus: the focus
/// must not front the app before its parked windows are held.
#[test]
fn an_apps_jobs_reach_it_in_the_order_queued() {
    let (q, world) = queues();
    q.move_windows(&restores(&[(A, w(1), at(10.0)), (A, w(2), at(20.0))]));
    q.show(A, vec![(w(3), at(-900.0))]);
    q.focus(A, w(1));
    landed(&q, &[A]);
    assert_eq!(
        world.reached(A),
        ["move 1 to 10", "move 2 to 20", "show holding [3]", "focus 1"]
    );
}

/// Chrome stuck on a write must not keep kitty's window from landing.
#[test]
fn a_slow_app_does_not_hold_up_another() {
    let (q, world) = queues();
    world.close(A);
    q.move_windows(&restores(&[(A, w(1), at(1.0)), (A, w(2), at(2.0)), (B, w(4), at(4.0))]));
    landed(&q, &[B]);
    world.busy(A);
    assert_eq!(world.reached(B), ["move 4 to 4"]);
    assert_eq!(world.reached(A), ["move 1 to 1"], "A is still on its first write");
    world.open(A);
    landed(&q, &[A]);
    assert_eq!(world.reached(A).len(), 2);
}

/// Going somewhere and straight back: the second decision about a window
/// is the only one left standing. Its older move never reaches the app,
/// and neither does an older un-hide's hold on it.
#[test]
fn a_newer_move_replaces_a_queued_one_and_frees_the_window_from_a_queued_hold() {
    let (q, world) = queues();
    world.close(A);
    q.hide(A);
    world.busy(A);
    q.move_windows(&restores(&[(A, w(1), at(-900.0))]));
    q.show(A, vec![(w(1), at(-900.0)), (w(2), at(-800.0))]);
    q.move_windows(&restores(&[(A, w(1), at(100.0))]));
    world.open(A);
    landed(&q, &[A]);
    assert_eq!(
        world.reached(A),
        ["hide", "show holding [2]", "move 1 to 100"]
    );
    let chain = q.take_chains().into_iter().find(|c| c.pid == A).unwrap();
    assert_eq!(chain.replaced, 1);
}

/// Only the latest focus means anything; an older one still waiting behind
/// a busy app is skipped rather than stealing focus back later.
#[test]
fn an_overtaken_focus_never_runs() {
    let (q, world) = queues();
    world.close(A);
    q.hide(A);
    world.busy(A);
    q.focus(A, w(1));
    q.focus(B, w(2));
    world.open(A);
    landed(&q, &[A, B]);
    assert_eq!(world.reached(A), ["hide"]);
    assert_eq!(world.reached(B), ["focus 2"]);
}

/// When Ordo lets go (rescue), nothing it queued may land afterwards on top
/// of whatever takes over.
#[test]
fn abandoning_drops_everything_not_yet_sent() {
    let (q, world) = queues();
    world.close(A);
    q.hide(A);
    world.busy(A);
    q.move_windows(&restores(&[(A, w(1), at(1.0)), (A, w(2), at(2.0))]));
    q.show(A, Vec::new());
    let before = q.marker(&[A]);
    q.abandon();
    world.open(A);
    assert!(before.wait(Instant::now() + Duration::from_secs(2), &|| false));
    landed(&q, &[A]);
    assert_eq!(world.reached(A), ["hide"]);
    assert!(!q.in_flight(w(1), Instant::now()));
}

/// A write counts as on its way while queued, while being made, and for a
/// while after it returns, since the window server's copy trails the app's.
/// A refused write was never going anywhere.
#[test]
fn a_write_is_in_flight_until_it_has_had_time_to_land() {
    let (q, world) = queues();
    world.gone.lock().unwrap().push(w(2));
    world.close(A);
    q.move_windows(&restores(&[(A, w(1), at(1.0)), (A, w(2), at(2.0))]));
    assert!(q.in_flight(w(1), Instant::now()), "queued");
    world.open(A);
    landed(&q, &[A]);
    let now = Instant::now();
    assert!(q.in_flight(w(1), now), "just written");
    assert!(!q.in_flight(w(2), now), "refused");
    assert!(!q.in_flight(w(1), now + LAND_WINDOW), "had its chance");
    assert!(!q.in_flight(w(3), now), "never written");
}

/// A write already being made can't be replaced; the newer one follows it.
/// When the older one reports back, it must not clear the newer one's
/// record, or the window would read as settled while its last write is
/// still to come.
#[test]
fn a_write_reporting_late_leaves_the_newer_one_in_flight() {
    let (q, world) = queues();
    world.close(A);
    world.gone.lock().unwrap().push(w(1));
    q.move_windows(&restores(&[(A, w(1), at(1.0))]));
    world.busy(A);
    q.move_windows(&restores(&[(A, w(1), at(2.0))]));
    world.let_through(1);
    // The first write comes back refused; the second is now being made.
    let deadline = Instant::now() + Duration::from_secs(2);
    while world.reached(A).len() < 2 {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(q.in_flight(w(1), Instant::now()), "the newer write is still on its way");
    world.open(A);
    landed(&q, &[A]);
    assert_eq!(world.reached(A), ["move 1 to 1", "move 1 to 2"]);
}

/// A resize-and-move and a plain move are the same decision about where a
/// window goes: either replaces the other, and either frees the window from
/// a queued hold.
#[test]
fn a_frame_and_a_move_replace_each_other() {
    let (q, world) = queues();
    world.close(A);
    q.hide(A);
    world.busy(A);
    q.move_windows(&restores(&[(A, w(1), at(1.0))]));
    q.set_frame(A, w(1), Rect { x: 5.0, y: 0.0, w: 10.0, h: 10.0 });
    q.show(A, vec![(w(2), at(-900.0))]);
    q.set_frame(A, w(2), Rect { x: 6.0, y: 0.0, w: 10.0, h: 10.0 });
    q.move_windows(&restores(&[(A, w(2), at(7.0))]));
    world.open(A);
    landed(&q, &[A]);
    assert_eq!(
        world.reached(A),
        ["hide", "frame 1 to 5", "show holding []", "move 2 to 7"]
    );
}

/// Rescue mid-batch: the write being made finishes, the rest never start.
#[test]
fn abandoning_stops_a_batch_of_moves_at_its_next_write() {
    let (q, world) = queues();
    world.close(A);
    q.move_windows(&restores(&[(A, w(1), at(1.0)), (A, w(2), at(2.0)), (A, w(3), at(3.0))]));
    world.busy(A);
    let before = q.marker(&[A]);
    q.abandon();
    world.open(A);
    assert!(before.wait(Instant::now() + Duration::from_secs(2), &|| false));
    landed(&q, &[A]);
    assert_eq!(world.reached(A), ["move 1 to 1"]);
}

/// A marker waits for the apps it names and nothing else, and for a job
/// already under way even when nothing is queued behind it.
#[test]
fn a_marker_waits_for_its_own_apps_and_their_running_job() {
    let (q, world) = queues();
    world.close(A);
    world.close(B);
    q.hide(A);
    q.hide(B);
    world.busy(A);
    world.busy(B);
    let a = q.marker(&[A]);
    assert!(!a.wait(Instant::now() + Duration::from_millis(30), &|| false), "A is mid-job");
    world.open(A);
    assert!(a.wait(Instant::now() + Duration::from_secs(2), &|| false), "B doesn't count");
    world.open(B);
}

/// A bug that panics inside one app's session costs that job, not the app:
/// its later jobs still run on a fresh session, its window isn't left in
/// flight, and other apps never notice.
#[test]
fn a_panicking_job_costs_that_job_only() {
    let (q, world) = queues();
    *world.panics_on.lock().unwrap() = Some(w(1));
    q.move_windows(&restores(&[(A, w(1), at(1.0)), (B, w(3), at(3.0))]));
    landed(&q, &[A, B]);
    *world.panics_on.lock().unwrap() = None;
    q.move_windows(&restores(&[(A, w(2), at(2.0))]));
    landed(&q, &[A]);
    assert_eq!(world.reached(A), ["move 1 to 1", "move 2 to 2"]);
    assert_eq!(world.reached(B), ["move 3 to 3"]);
    assert!(!q.in_flight(w(1), Instant::now()));
    let opened = world.opened_sessions.lock().unwrap();
    assert_eq!(opened.iter().filter(|p| **p == A).count(), 2, "a fresh session after the panic");
}

/// Two presses before a hidden app's queue gets going. The first switch
/// restores W1 and plans an un-hide holding W3; the second parks W1 again
/// and brings W3 back. The un-hide that finally runs must hold W1, which is
/// parked now, and not W3, which is coming on screen: revealing the app
/// drags every window it doesn't hold onto a display.
#[test]
fn an_unsent_unhide_holds_what_is_parked_by_the_time_it_runs() {
    let (q, world) = queues();
    world.close(A);
    q.hide(A);
    world.busy(A);
    q.move_windows(&restores(&[(A, w(1), at(100.0))]));
    q.show(A, vec![(w(3), at(-900.0))]);
    q.move_windows(&[park(A, w(1), -900.0)]);
    q.move_windows(&restores(&[(A, w(3), at(300.0))]));
    world.open(A);
    landed(&q, &[A]);
    assert_eq!(
        world.reached(A),
        ["hide", "show holding [1]", "move 1 to -900", "move 3 to 300"]
    );
}

/// Passing a workspace queues a restore of its window, and passing the next
/// queues its park. If the window's last write is known to have put it at
/// that park, neither needs making.
#[test]
fn a_restore_and_park_before_either_is_sent_cancel() {
    let (q, world) = queues();
    q.move_windows(&[park(A, w(1), -900.0)]);
    landed(&q, &[A]);
    world.close(A);
    q.hide(A);
    world.busy(A);
    q.move_windows(&restores(&[(A, w(1), at(100.0))]));
    q.move_windows(&[park(A, w(1), -900.0)]);
    world.open(A);
    landed(&q, &[A]);
    assert_eq!(world.reached(A), ["move 1 to -900", "hide"]);
    assert!(!q.in_flight(w(1), Instant::now()));
    let cancelled: usize = q.take_chains().iter().map(|c| c.cancelled).sum();
    assert_eq!(cancelled, 1);
}

/// An un-hide re-homes windows without telling us, so after one the queue no
/// longer knows where a window it didn't hold is, and a round trip back to
/// its old spot is written after all.
#[test]
fn after_an_unhide_a_round_trip_is_written() {
    let (q, world) = queues();
    q.move_windows(&[park(A, w(1), -900.0)]);
    q.show(A, Vec::new());
    landed(&q, &[A]);
    world.close(A);
    q.hide(A);
    world.busy(A);
    q.move_windows(&restores(&[(A, w(1), at(100.0))]));
    q.move_windows(&[park(A, w(1), -900.0)]);
    world.open(A);
    landed(&q, &[A]);
    assert_eq!(
        world.reached(A),
        ["move 1 to -900", "show holding []", "hide", "move 1 to -900"]
    );
}

/// A move to where the window already is, with nothing to replace, may be
/// putting back a window the app moved itself: it is always made.
#[test]
fn a_move_with_nothing_to_replace_is_always_made() {
    let (q, world) = queues();
    q.move_windows(&[park(A, w(1), -900.0)]);
    landed(&q, &[A]);
    q.move_windows(&[park(A, w(1), -900.0)]);
    landed(&q, &[A]);
    assert_eq!(world.reached(A), ["move 1 to -900", "move 1 to -900"]);
}

/// A focus goes after its app's un-hide still to come, and after its own
/// window's move, but ahead of the app's other moves.
#[test]
fn a_focus_goes_as_early_as_is_safe() {
    let (q, world) = queues();
    world.close(A);
    q.hide(A);
    world.busy(A);
    q.move_windows(&restores(&[(A, w(1), at(1.0)), (A, w(2), at(2.0))]));
    q.focus(A, w(1));
    world.open(A);
    landed(&q, &[A]);
    assert_eq!(world.reached(A), ["hide", "move 1 to 1", "focus 1", "move 2 to 2"]);

    let (q, world) = queues();
    world.close(A);
    q.hide(A);
    world.busy(A);
    q.move_windows(&restores(&[(A, w(2), at(3.0))]));
    q.show(A, Vec::new());
    q.move_windows(&restores(&[(A, w(3), at(4.0))]));
    q.focus(A, w(1));
    world.open(A);
    landed(&q, &[A]);
    assert_eq!(
        world.reached(A),
        ["hide", "move 2 to 3", "show holding []", "focus 1", "move 3 to 4"]
    );
}

/// Two apps, two focuses: the second is not fronted until the first is,
/// or the first could land last and win.
#[test]
fn focuses_on_two_apps_are_fronted_in_the_order_asked() {
    let (q, world) = queues();
    world.close(A);
    q.focus(A, w(1));
    world.busy(A);
    q.focus(B, w(2));
    std::thread::sleep(Duration::from_millis(20));
    world.open(A);
    landed(&q, &[A, B]);
    // B's focus, asked for later, overtook A's while A's was being made:
    // A's still finishes fronting first, and B's comes after it.
    assert!(world.when(A, "fronted 1") < world.when(B, "focus 2"));
}

/// A take-back decided before a newer focus was asked for is dropped.
#[test]
fn a_focus_decided_before_a_newer_one_is_dropped() {
    let (q, world) = queues();
    let gen = q.focus_generation();
    q.focus(B, w(2));
    assert!(!q.focus_if_current(A, w(1), gen));
    landed(&q, &[A, B]);
    assert!(world.reached(A).is_empty());
    let gen = q.focus_generation();
    assert!(q.focus_if_current(A, w(1), gen));
    landed(&q, &[A]);
    assert_eq!(world.reached(A), ["focus 1"]);
}

/// The desktop is a focus like any other: an older window focus still
/// queued doesn't land after it.
#[test]
fn a_desktop_focus_overtakes_an_older_window_focus() {
    let (q, world) = queues();
    world.close(A);
    q.hide(A);
    world.busy(A);
    q.focus(A, w(1));
    q.focus_desktop(B, 77);
    world.open(A);
    landed(&q, &[A, B]);
    assert_eq!(world.reached(A), ["hide"]);
    assert_eq!(world.reached(B), ["desktop 77"]);
}

#[test]
fn idle_is_every_queue_drained() {
    let (q, world) = queues();
    assert!(q.idle());
    world.close(A);
    q.move_windows(&restores(&[(A, w(1), at(1.0))]));
    assert!(!q.wait_idle(Instant::now() + Duration::from_millis(20)));
    world.open(A);
    assert!(q.wait_idle(Instant::now() + Duration::from_secs(2)));
}

/// A burst cancels round trips on the engine's thread while the app's own
/// thread is recording its writes landing. Neither may wait on the other.
#[test]
fn cancelling_while_writes_land_never_deadlocks() {
    let (q, _world) = queues();
    q.move_windows(&[park(A, w(1), -900.0)]);
    landed(&q, &[A]);
    let (tx, rx) = std::sync::mpsc::channel();
    let burst = q.clone();
    std::thread::spawn(move || {
        for i in 0..20_000 {
            // Writes to other windows keep the app's thread recording
            // landings, taking the record, while the round trip is cancelled.
            burst.move_windows(&restores(&[(A, w(2 + i % 3), at(i as f64))]));
            burst.move_windows(&restores(&[(A, w(1), at(100.0))]));
            burst.move_windows(&[park(A, w(1), -900.0)]);
        }
        let drained = burst
            .marker(&[A])
            .wait(Instant::now() + Duration::from_secs(5), &|| false);
        let _ = tx.send(drained);
    });
    assert_eq!(rx.recv_timeout(Duration::from_secs(10)), Ok(true), "deadlocked");
}

/// A hide decided before the app was wanted in front would fling the focus
/// away: a focus drops it.
#[test]
fn a_focus_drops_a_hide_of_its_app_still_to_come() {
    let (q, world) = queues();
    world.close(A);
    q.move_windows(&restores(&[(A, w(9), at(9.0))]));
    world.busy(A);
    q.hide(A);
    q.focus(A, w(1));
    world.open(A);
    landed(&q, &[A]);
    assert_eq!(world.reached(A), ["move 9 to 9", "focus 1"]);
}

/// A frame the core asked for is waited on by the core: a round trip made
/// of it and a move is not cancelled.
#[test]
fn a_frame_is_never_cancelled_as_half_of_a_round_trip() {
    let (q, world) = queues();
    q.move_windows(&[park(A, w(1), -900.0)]);
    landed(&q, &[A]);
    world.close(A);
    q.hide(A);
    world.busy(A);
    q.set_frame(A, w(1), Rect { x: 50.0, y: 0.0, w: 10.0, h: 10.0 });
    q.move_windows(&[park(A, w(1), -900.0)]);
    world.open(A);
    landed(&q, &[A]);
    assert_eq!(world.reached(A)[1..], ["hide", "move 1 to -900"]);
}

/// An app that shows itself has re-homed its windows: where Ordo last put
/// them is no longer known, and a round trip is written after all.
#[test]
fn after_an_app_shows_itself_a_round_trip_is_written() {
    let (q, world) = queues();
    q.move_windows(&[park(A, w(1), -900.0)]);
    landed(&q, &[A]);
    q.forget_landed(A);
    world.close(A);
    q.hide(A);
    world.busy(A);
    q.move_windows(&restores(&[(A, w(1), at(100.0))]));
    q.move_windows(&[park(A, w(1), -900.0)]);
    world.open(A);
    landed(&q, &[A]);
    assert_eq!(world.reached(A)[1..], ["hide", "move 1 to -900"]);
}
