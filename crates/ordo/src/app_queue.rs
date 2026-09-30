//! One queue per app, and the record of writes still on their way.
//!
//! Every write Ordo makes to an app's windows is a round trip to that app's
//! main thread, and a switch writes to several apps. Waiting for each step
//! across all apps before the next (moves, then un-hides, then focus) made a
//! switch cost the sum of each step's slowest app. Here each app has its own
//! thread working through its own queue, so a switch costs its slowest app's
//! chain, and the caller returns as soon as the jobs are queued.
//!
//! Because the caller moves on before the writes land, anything that looks
//! at the screen can catch a window mid-flight. [`AppQueues::in_flight`] says
//! which windows have a write still on its way, so a decision that reads a
//! frame can tell a stale observation from a real one. Frames themselves are
//! never rewritten: what the screen showed stays what the screen showed.
//!
//! A job still on the queue can be dropped, which is the only cancelling
//! there is: a message that has reached an app is past recall. A newer move
//! for a window replaces its unsent older one, and a newer focus anywhere
//! turns an unsent older one into a no-op. An unsent un-hide follows the
//! newest decisions too: a park adds its window to the hold, a restore takes
//! it out. [`AppQueues::abandon`] drops everything.
//!
//! ```no_run
//! # use ordo::app_queue::{AppQueues, AppSession};
//! # use ordo_core::{Pid, Point, WindowId};
//! # use ordo_emulated::Move;
//! # fn open(pid: Pid) -> Box<dyn AppSession> { unimplemented!() }
//! let queues = AppQueues::new(open);
//! let to = Point { x: 0.0, y: 0.0 };
//! queues.move_windows(&[Move { pid: Pid(7), window: WindowId(1), to, parks: false }]);
//! queues.focus(Pid(7), WindowId(1));
//! let landed = queues.marker(&[Pid(7)]);
//! ```

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use ordo_core::{Pid, Point, Rect, WindowId};
use ordo_emulated::{ChainStat, FocusStat, HoldStat, Move, WriteStat};

/// How long a write that returned still counts as on its way. The window
/// server's copy of a frame trails the app's by a few ms; this is generous
/// against that, and short enough that a write that didn't stick is judged
/// on the next scan or two.
pub const LAND_WINDOW: Duration = Duration::from_millis(500);

/// An app's thread exits after this long with nothing to do: pids come and
/// go as apps quit and relaunch, and an idle thread per pid ever seen would
/// only grow.
const IDLE_EXIT: Duration = Duration::from_secs(60);

/// What one app's thread does to that app. Made on the app's own thread and
/// kept there, so it may hold per-app handles that can't cross threads.
pub trait AppSession {
    /// Position writes, in order, stopping early once `cancel` says so. Per
    /// write made: whether the app took it, and how long the call took, in ms.
    fn move_windows(
        &mut self,
        moves: &[(WindowId, Point)],
        cancel: &dyn Fn() -> bool,
    ) -> Vec<(WindowId, bool, f64)>;
    /// A move that may also resize; whether the window was found.
    fn set_frame(&mut self, window: WindowId, to: Rect) -> bool;
    /// Un-hide the app, holding the listed windows at their spots until the
    /// window server shows them there, or until `cancel` says to stop.
    fn show(&mut self, hold: &[(WindowId, Point)], cancel: &dyn Fn() -> bool) -> HoldStat;
    fn hide(&mut self);
    /// Front the app with this window key; whether the window was found.
    fn focus(&mut self, window: WindowId) -> bool;
}

type Opener = dyn Fn(Pid) -> Box<dyn AppSession> + Send + Sync;

#[derive(Clone)]
pub struct AppQueues {
    inner: Arc<Inner>,
}

struct Inner {
    open: Box<Opener>,
    lanes: Mutex<HashMap<Pid, Arc<Lane>>>,
    record: Mutex<Record>,
    /// Bumped by every focus queued; a focus whose number is behind it when
    /// its turn comes has been overtaken and is skipped.
    focus_gen: AtomicU64,
    /// Bumped by [`AppQueues::abandon`]; a job that started before the bump
    /// stops as soon as it can.
    epoch: AtomicU64,
    chains: Mutex<Vec<ChainStat>>,
}

struct Lane {
    state: Mutex<LaneState>,
    wake: Condvar,
}

#[derive(Default)]
struct LaneState {
    jobs: VecDeque<Queued>,
    /// The thread is carrying out a job it has taken off the queue.
    busy: bool,
    /// Moves dropped since the chain began, replaced by newer ones.
    replaced: usize,
}

struct Queued {
    job: Job,
    at: Instant,
}

enum Job {
    Move {
        window: WindowId,
        to: Point,
        parks: bool,
        seq: u64,
    },
    Frame { window: WindowId, to: Rect, seq: u64 },
    Show { hold: Vec<(WindowId, Point)>, seqs: Vec<u64> },
    Hide,
    Focus { window: WindowId, gen: u64 },
    Marker(Arc<Latch>),
}

impl AppQueues {
    pub fn new(open: impl Fn(Pid) -> Box<dyn AppSession> + Send + Sync + 'static) -> Self {
        AppQueues {
            inner: Arc::new(Inner {
                open: Box::new(open),
                lanes: Mutex::new(HashMap::new()),
                record: Mutex::new(Record::default()),
                focus_gen: AtomicU64::new(0),
                epoch: AtomicU64::new(0),
                chains: Mutex::new(Vec::new()),
            }),
        }
    }

    /// Each write goes to the queue of the pid given with it, so every caller
    /// must name a window's owner the same way — the window server's owner
    /// pid, as every caller does today. Two queues writing one window would
    /// land its writes in either order, and the record would track only the
    /// newer.
    pub fn move_windows(&self, moves: &[Move]) {
        let mut by_app: Vec<(Pid, Vec<Job>)> = Vec::new();
        {
            let mut record = self.inner.record.lock().unwrap();
            for m in moves {
                let seq = record.queued(m.window, Instant::now());
                let job = Job::Move {
                    window: m.window,
                    to: m.to,
                    parks: m.parks,
                    seq,
                };
                match by_app.iter_mut().find(|(p, _)| *p == m.pid) {
                    Some((_, jobs)) => jobs.push(job),
                    None => by_app.push((m.pid, vec![job])),
                }
            }
        }
        for (pid, jobs) in by_app {
            self.enqueue(pid, jobs);
        }
    }

    pub fn set_frame(&self, pid: Pid, window: WindowId, to: Rect) {
        let seq = self.inner.record.lock().unwrap().queued(window, Instant::now());
        self.enqueue(pid, vec![Job::Frame { window, to, seq }]);
    }

    pub fn show(&self, pid: Pid, hold: Vec<(WindowId, Point)>) {
        let seqs = {
            let mut record = self.inner.record.lock().unwrap();
            let now = Instant::now();
            hold.iter().map(|(w, _)| record.queued(*w, now)).collect()
        };
        self.enqueue(pid, vec![Job::Show { hold, seqs }]);
    }

    pub fn hide(&self, pid: Pid) {
        self.enqueue(pid, vec![Job::Hide]);
    }

    pub fn focus(&self, pid: Pid, window: WindowId) {
        let gen = self.inner.focus_gen.fetch_add(1, Ordering::SeqCst) + 1;
        self.enqueue(pid, vec![Job::Focus { window, gen }]);
    }

    /// Completes once these apps' queues have carried out everything queued
    /// on them before this call.
    pub fn marker(&self, apps: &[Pid]) -> Landing {
        let lanes = self.inner.lanes.lock().unwrap();
        let mut waiting: Vec<std::sync::MutexGuard<LaneState>> = lanes
            .iter()
            .filter(|(pid, _)| apps.contains(pid))
            .map(|(_, l)| l.state.lock().unwrap())
            .filter(|s| s.busy || !s.jobs.is_empty())
            .collect();
        let latch = Arc::new(Latch {
            left: Mutex::new(waiting.len()),
            done: Condvar::new(),
        });
        let now = Instant::now();
        for s in &mut waiting {
            s.jobs.push_back(Queued {
                job: Job::Marker(latch.clone()),
                at: now,
            });
        }
        drop(waiting);
        for lane in lanes.values() {
            lane.wake.notify_one();
        }
        Landing(latch)
    }

    /// Whether a write to this window is queued, being made, or returned
    /// less than [`LAND_WINDOW`] ago. An observation of the window made now
    /// may not show that write yet.
    pub fn in_flight(&self, window: WindowId, now: Instant) -> bool {
        self.inner.record.lock().unwrap().in_flight(window, now)
    }

    /// Drop every job not yet started, stop a running batch of moves at its
    /// next write and a running un-hide's hold (the un-hide itself, once
    /// sent, still lands), and forget every write on its way. For when Ordo
    /// lets go of the screen: a write landing after that would undo whatever
    /// took over.
    pub fn abandon(&self) {
        self.inner.epoch.fetch_add(1, Ordering::SeqCst);
        let lanes = self.inner.lanes.lock().unwrap();
        for lane in lanes.values() {
            let mut state = lane.state.lock().unwrap();
            state.replaced = 0;
            for q in state.jobs.drain(..) {
                if let Job::Marker(latch) = q.job {
                    latch.count_down();
                }
            }
        }
        self.inner.record.lock().unwrap().windows.clear();
    }

    /// Each app's finished chains since the last call.
    pub fn take_chains(&self) -> Vec<ChainStat> {
        std::mem::take(&mut *self.inner.chains.lock().unwrap())
    }

    fn enqueue(&self, pid: Pid, jobs: Vec<Job>) {
        let mut lanes = self.inner.lanes.lock().unwrap();
        let lane = lanes
            .entry(pid)
            .or_insert_with(|| {
                let lane = Arc::new(Lane {
                    state: Mutex::new(LaneState::default()),
                    wake: Condvar::new(),
                });
                let inner = self.inner.clone();
                let l = lane.clone();
                std::thread::spawn(move || run_lane(inner, pid, l));
                lane
            })
            .clone();
        let now = Instant::now();
        let mut state = lane.state.lock().unwrap();
        for job in jobs {
            let decided = match &job {
                Job::Move {
                    window,
                    to,
                    parks,
                    seq,
                } => Some((*window, parks.then_some((*to, *seq)))),
                Job::Frame { window, .. } => Some((*window, None)),
                _ => None,
            };
            if let Some((window, park)) = decided {
                // The newest decision about a window is the only one left
                // standing: an older move would carry it somewhere it no
                // longer belongs before this one lands.
                let before = state.jobs.len();
                state.jobs.retain(|q| {
                    !matches!(&q.job, Job::Move { window: w, .. } | Job::Frame { window: w, .. } if *w == window)
                });
                state.replaced += before - state.jobs.len();
                // And an un-hide still to come must hold what is parked now,
                // and nothing that is coming back on screen: revealing the
                // app drags every window it does not hold onto a display.
                for q in state.jobs.iter_mut() {
                    if let Job::Show { hold, seqs } = &mut q.job {
                        let at = hold.iter().position(|(w, _)| *w == window);
                        match (at, park) {
                            (Some(i), Some((to, seq))) => {
                                hold[i].1 = to;
                                seqs[i] = seq;
                            }
                            (None, Some((to, seq))) => {
                                hold.push((window, to));
                                seqs.push(seq);
                            }
                            (Some(i), None) => {
                                hold.remove(i);
                                seqs.remove(i);
                            }
                            (None, None) => {}
                        }
                    }
                }
            }
            state.jobs.push_back(Queued { job, at: now });
        }
        drop(state);
        drop(lanes);
        lane.wake.notify_one();
    }
}

/// What a lane's thread took off its queue in one go: consecutive moves
/// travel together, so an app's batch of moves shares one bracket of
/// whatever the session wraps its writes in.
enum Work {
    Moves(Vec<(WindowId, Point, u64)>),
    One(Job),
}

fn run_lane(inner: Arc<Inner>, pid: Pid, lane: Arc<Lane>) {
    let mut session = (inner.open)(pid);
    let mut chain: Option<(Instant, ChainStat)> = None;
    loop {
        let work = {
            let mut state = lane.state.lock().unwrap();
            while state.jobs.is_empty() {
                let (s, timeout) = lane.wake.wait_timeout(state, IDLE_EXIT).unwrap();
                state = s;
                if timeout.timed_out() && state.jobs.is_empty() {
                    // Leaving means taking the lane out of the map, which
                    // is locked before a lane, as enqueuing locks them.
                    drop(state);
                    let mut lanes = inner.lanes.lock().unwrap();
                    let s = lane.state.lock().unwrap();
                    if s.jobs.is_empty() {
                        lanes.remove(&pid);
                        return;
                    }
                    drop(lanes);
                    state = s;
                }
            }
            state.busy = true;
            let first = state.jobs.pop_front().unwrap();
            if chain.is_none() {
                let wait = first.at.elapsed();
                let queued_wall_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.saturating_sub(wait).as_millis() as i64);
                chain = Some((first.at, ChainStat::new(pid, queued_wall_ms, wait)));
            }
            match first.job {
                Job::Move { window, to, seq, .. } => {
                    let mut moves = vec![(window, to, seq)];
                    while let Some(Queued {
                        job: Job::Move { .. },
                        ..
                    }) = state.jobs.front()
                    {
                        if let Some(Queued {
                            job: Job::Move { window, to, seq, .. },
                            ..
                        }) = state.jobs.pop_front()
                        {
                            moves.push((window, to, seq));
                        }
                    }
                    Work::Moves(moves)
                }
                job => Work::One(job),
            }
        };
        let touched: Vec<(WindowId, u64)> = match &work {
            Work::Moves(moves) => moves.iter().map(|(w, _, seq)| (*w, *seq)).collect(),
            Work::One(Job::Frame { window, seq, .. }) => vec![(*window, *seq)],
            Work::One(Job::Show { hold, seqs }) => {
                hold.iter().map(|(w, _)| *w).zip(seqs.iter().copied()).collect()
            }
            Work::One(_) => Vec::new(),
        };
        let (started, stat) = chain.as_mut().unwrap();
        let since = |t: Instant| (t - *started).as_secs_f64() * 1000.0;
        // A job that panics must not take the lane with it: a dead thread
        // would leave its app's queue growing forever, its windows in flight
        // forever, and every marker naming it waiting out its bound.
        let carried = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match work {
            Work::Moves(moves) => {
                {
                    let mut record = inner.record.lock().unwrap();
                    for (w, _, seq) in &moves {
                        record.sent(*w, *seq);
                    }
                }
                let asked: Vec<(WindowId, Point)> = moves.iter().map(|(w, to, _)| (*w, *to)).collect();
                let epoch = inner.epoch.load(Ordering::SeqCst);
                let cancel = || inner.epoch.load(Ordering::SeqCst) != epoch;
                let results = session.move_windows(&asked, &cancel);
                let now = Instant::now();
                let mut record = inner.record.lock().unwrap();
                for (w, took, ax_ms) in results {
                    let Some((_, _, seq)) = moves.iter().find(|(m, _, _)| *m == w) else {
                        continue;
                    };
                    if took {
                        record.written(w, *seq, now);
                    } else {
                        record.refused(w, *seq);
                    }
                    stat.moves += 1;
                    stat.moves_ms += ax_ms;
                    stat.writes.push((
                        w,
                        WriteStat {
                            ax_ms,
                            done_ms: since(now),
                        },
                    ));
                }
            }
            Work::One(Job::Frame { window, to, seq }) => {
                inner.record.lock().unwrap().sent(window, seq);
                let found = session.set_frame(window, to);
                let mut record = inner.record.lock().unwrap();
                if found {
                    record.written(window, seq, Instant::now());
                } else {
                    record.refused(window, seq);
                }
            }
            Work::One(Job::Show { hold, seqs }) => {
                {
                    let mut record = inner.record.lock().unwrap();
                    for ((w, _), seq) in hold.iter().zip(&seqs) {
                        record.sent(*w, *seq);
                    }
                }
                let epoch = inner.epoch.load(Ordering::SeqCst);
                let cancel = || inner.epoch.load(Ordering::SeqCst) != epoch;
                let held = session.show(&hold, &cancel);
                let now = Instant::now();
                let mut record = inner.record.lock().unwrap();
                for ((w, _), seq) in hold.iter().zip(&seqs) {
                    // An escaped window is not on its way anywhere: the
                    // model's own checks should see it and put it back.
                    if held.escaped.contains(w) {
                        record.refused(*w, *seq);
                    } else {
                        record.written(*w, *seq, now);
                    }
                }
                stat.show_done_ms = Some(since(now));
                stat.show = Some(held);
            }
            Work::One(Job::Hide) => session.hide(),
            Work::One(Job::Focus { window, gen }) => {
                let overtaken = inner.focus_gen.load(Ordering::SeqCst) != gen;
                let found = !overtaken && session.focus(window);
                stat.focus = Some(FocusStat {
                    window,
                    skipped: overtaken,
                    found,
                    done_ms: since(Instant::now()),
                });
            }
            Work::One(Job::Marker(latch)) => latch.count_down(),
            Work::One(Job::Move { .. }) => unreachable!("moves travel as Work::Moves"),
        }));
        if carried.is_err() {
            // Where its writes went is unknown, and the session's handles
            // are suspect: the windows are judged by what the screen shows,
            // and the app gets a fresh session.
            let mut record = inner.record.lock().unwrap_or_else(|e| e.into_inner());
            for (w, seq) in &touched {
                record.refused(*w, *seq);
            }
            drop(record);
            session = (inner.open)(pid);
        }
        let mut state = lane.state.lock().unwrap();
        state.busy = false;
        if state.jobs.is_empty() {
            if let Some((started, mut stat)) = chain.take() {
                stat.replaced = std::mem::take(&mut state.replaced);
                stat.done_ms = started.elapsed().as_secs_f64() * 1000.0;
                if !stat.is_empty() {
                    inner.chains.lock().unwrap().push(stat);
                }
            }
        }
    }
}

struct Latch {
    left: Mutex<usize>,
    done: Condvar,
}

impl Latch {
    fn count_down(&self) {
        let mut left = self.left.lock().unwrap();
        *left = left.saturating_sub(1);
        if *left == 0 {
            self.done.notify_all();
        }
    }
}

/// See [`AppQueues::marker`].
pub struct Landing(Arc<Latch>);

impl Landing {
    /// Whether everything landed before `deadline`. Gives up early, with
    /// false, once `cancel` says the wait no longer matters.
    pub fn wait(&self, deadline: Instant, cancel: &dyn Fn() -> bool) -> bool {
        let slice = Duration::from_millis(5);
        let mut left = self.0.left.lock().unwrap();
        while *left > 0 {
            let now = Instant::now();
            if now >= deadline || cancel() {
                return false;
            }
            left = self
                .0
                .done
                .wait_timeout(left, slice.min(deadline - now))
                .unwrap()
                .0;
        }
        true
    }
}

#[derive(Default)]
struct Record {
    windows: HashMap<WindowId, InFlight>,
    next_seq: u64,
}

struct InFlight {
    /// Which write this is: a newer write to the same window replaces the
    /// entry, and reports about the older one must not touch it.
    seq: u64,
    stage: Stage,
}

enum Stage {
    Queued,
    Sent,
    Written(Instant),
}

impl Record {
    fn queued(&mut self, window: WindowId, now: Instant) -> u64 {
        // Landed writes are forgotten here as well as when asked about: a
        // window nobody asks about again must not stay on the books.
        self.windows.retain(|_, f| match f.stage {
            Stage::Written(at) => now.saturating_duration_since(at) < LAND_WINDOW,
            _ => true,
        });
        self.next_seq += 1;
        let seq = self.next_seq;
        self.windows.insert(
            window,
            InFlight {
                seq,
                stage: Stage::Queued,
            },
        );
        seq
    }

    fn sent(&mut self, window: WindowId, seq: u64) {
        if let Some(f) = self.windows.get_mut(&window).filter(|f| f.seq == seq) {
            f.stage = Stage::Sent;
        }
    }

    fn written(&mut self, window: WindowId, seq: u64, at: Instant) {
        if let Some(f) = self.windows.get_mut(&window).filter(|f| f.seq == seq) {
            f.stage = Stage::Written(at);
        }
    }

    fn refused(&mut self, window: WindowId, seq: u64) {
        if self.windows.get(&window).is_some_and(|f| f.seq == seq) {
            self.windows.remove(&window);
        }
    }

    fn in_flight(&mut self, window: WindowId, now: Instant) -> bool {
        let Some(f) = self.windows.get(&window) else {
            return false;
        };
        let on_its_way = match f.stage {
            Stage::Queued | Stage::Sent => true,
            Stage::Written(at) => now.saturating_duration_since(at) < LAND_WINDOW,
        };
        if !on_its_way {
            self.windows.remove(&window);
        }
        on_its_way
    }
}
