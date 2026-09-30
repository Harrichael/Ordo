//! Restacking: making the on-screen z-order agree with the core's priority
//! order wherever the difference can be seen.
//!
//! The core hands over one total order, its MRU history. Only windows that
//! overlap have a visible order, so only their pairs are enforced: `i` must be
//! above `j` exactly when they overlap and `i` comes first. The windows that
//! must be raised are then a unique minimum set found in one pass
//! ([`Layout::raise_set`]), and groups of windows with no overlap between
//! them (typically one per display) are independent lanes whose raises can be
//! in flight at once, so a slow app on one display doesn't hold up another.
//!
//! Everything about overlap is rebuilt from each window-list read and never
//! kept. The plan has to agree with where the windows are at the moment the
//! stack is read, the core's frames can be a tick old, and the rebuild costs
//! microseconds against a read of a quarter millisecond.
//!
//! Raise physics, probed (see `platform/zorder.rs`): a raise lands just below
//! the key window; a raise of another window of the key window's app (a
//! sibling) lands above it; raising the key window puts it back on top, with
//! the siblings beneath it.

use std::time::{Duration, Instant};

use ordo_core::{Rect, WindowId};

use crate::ports::{RaiseKind, RaiseStat, RestackStats};

/// Overlap, and visibility on a display, both need more than this on each
/// axis. A parked window shows a 1pt sliver, and tiled neighbours touch or
/// overlap by AX rounding; neither is an order anyone can see.
pub const MIN_OVERLAP: f64 = 2.0;

/// Windows past this many in priority order are left unordered. The largest
/// restack logged had 14.
pub const MAX_WINDOWS: usize = 64;

const MISSING: u16 = u16::MAX;
const PRESENCE_TIMEOUT: Duration = Duration::from_millis(600);
/// Generous on purpose: a landed raise confirms in single-digit ms, so this is
/// paid only by an app genuinely slower than it (Chrome has been measured past
/// 400ms), and a timeout is what the second pass exists to absorb.
const LANDING_TIMEOUT: Duration = Duration::from_millis(1000);
const SETTLE: Duration = Duration::from_millis(150);

/// One on-screen, layer-0 window as the window server lists it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Seen {
    pub id: WindowId,
    pub pid: i32,
    pub frame: Rect,
}

type Bits = u64;

fn bit(i: usize) -> Bits {
    1 << i
}

/// Indices strictly before `i`: the windows that outrank it.
fn before(i: usize) -> Bits {
    bit(i) - 1
}

/// Indices strictly after `i`: the windows it outranks.
fn after(i: usize) -> Bits {
    if i + 1 >= MAX_WINDOWS {
        0
    } else {
        !0 << (i + 1)
    }
}

fn ones(mut m: Bits) -> impl Iterator<Item = usize> {
    std::iter::from_fn(move || {
        (m != 0).then(|| {
            let i = m.trailing_zeros() as usize;
            m &= m - 1;
            i
        })
    })
}

fn overlap(a: &Rect, b: &Rect) -> bool {
    let ix = (a.x + a.w).min(b.x + b.w) - a.x.max(b.x);
    let iy = (a.y + a.h).min(b.y + b.h) - a.y.max(b.y);
    ix > MIN_OVERLAP && iy > MIN_OVERLAP
}

/// The priority order restricted to the windows actually on a display, with
/// who overlaps whom and where each sits in one stack read. Index 0 is the
/// highest priority present; fixed arrays, so building one allocates nothing.
pub struct Layout {
    n: usize,
    ids: [WindowId; MAX_WINDOWS],
    pids: [i32; MAX_WINDOWS],
    frames: [Rect; MAX_WINDOWS],
    /// Position in the stack read, front = 0.
    depth: [u16; MAX_WINDOWS],
    adj: [Bits; MAX_WINDOWS],
    /// Index 0 is the designated top, not just the highest present.
    has_top: bool,
    /// Frames of windows attached to a root here, with the root's index:
    /// part of the root's footprint.
    attached: [(u8, Rect); MAX_WINDOWS],
    n_attached: usize,
}

impl Layout {
    /// A window counts only if it is listed and more than a sliver of it is on
    /// some display: a parked window shows 1pt, and has no order anyone can
    /// see. `attached` pairs each attached window with its root: the root
    /// overlaps whatever it or they overlap.
    pub fn new(
        priority: &[WindowId],
        attached: &[(WindowId, WindowId)],
        stack: &[Seen],
        displays: &[Rect],
    ) -> Layout {
        const NO_FRAME: Rect = Rect {
            x: 0.0,
            y: 0.0,
            w: 0.0,
            h: 0.0,
        };
        let mut l = Layout {
            n: 0,
            ids: [WindowId(0); MAX_WINDOWS],
            pids: [0; MAX_WINDOWS],
            frames: [NO_FRAME; MAX_WINDOWS],
            depth: [MISSING; MAX_WINDOWS],
            adj: [0; MAX_WINDOWS],
            has_top: false,
            attached: [(0, NO_FRAME); MAX_WINDOWS],
            n_attached: 0,
        };
        for &w in priority {
            if l.n == MAX_WINDOWS {
                break;
            }
            let Some((d, s)) = stack.iter().enumerate().find(|(_, s)| s.id == w) else {
                continue;
            };
            if !displays.is_empty() && !displays.iter().any(|r| overlap(&s.frame, r)) {
                continue;
            }
            l.ids[l.n] = w;
            l.pids[l.n] = s.pid;
            l.frames[l.n] = s.frame;
            l.depth[l.n] = d as u16;
            l.n += 1;
        }
        l.has_top = l.n > 0 && priority.first() == Some(&l.ids[0]);
        for &(w, root) in attached {
            if l.n_attached == MAX_WINDOWS {
                break;
            }
            let (Some(i), Some(s)) = (l.index_of(root), stack.iter().find(|s| s.id == w)) else {
                continue;
            };
            l.attached[l.n_attached] = (i as u8, s.frame);
            l.n_attached += 1;
        }
        for i in 0..l.n {
            for j in i + 1..l.n {
                let o = l.footprints_overlap(i, j) as Bits;
                l.adj[i] |= o << j;
                l.adj[j] |= o << i;
            }
        }
        l
    }

    fn footprint(&self, i: usize) -> impl Iterator<Item = &Rect> {
        std::iter::once(&self.frames[i]).chain(
            self.attached[..self.n_attached]
                .iter()
                .filter(move |(r, _)| *r as usize == i)
                .map(|(_, f)| f),
        )
    }

    fn footprints_overlap(&self, i: usize, j: usize) -> bool {
        self.footprint(i)
            .any(|a| self.footprint(j).any(|b| overlap(a, b)))
    }

    pub fn len(&self) -> usize {
        self.n
    }

    pub fn is_empty(&self) -> bool {
        self.n == 0
    }

    fn all(&self) -> Bits {
        if self.n == MAX_WINDOWS {
            !0
        } else {
            bit(self.n) - 1
        }
    }

    fn index_of(&self, w: WindowId) -> Option<usize> {
        self.ids[..self.n].iter().position(|&x| x == w)
    }

    /// The same windows' positions in a newer stack read.
    fn depths(&self, stack: &[Seen]) -> [u16; MAX_WINDOWS] {
        let mut out = [MISSING; MAX_WINDOWS];
        for (d, s) in stack.iter().enumerate() {
            if let Some(i) = self.index_of(s.id) {
                out[i] = d as u16;
            }
        }
        out
    }

    /// Pairs that must be ordered: overlapping, one outranking the other.
    pub fn edges(&self) -> u32 {
        (0..self.n)
            .map(|i| (self.adj[i] & after(i)).count_ones())
            .sum()
    }

    fn violators_at(&self, depth: &[u16; MAX_WINDOWS]) -> Bits {
        let mut v = 0;
        for i in 0..self.n {
            if ones(self.adj[i] & after(i)).any(|j| depth[j] < depth[i]) {
                v |= bit(i);
            }
        }
        v
    }

    /// Windows sitting below one they must be above.
    pub fn violators(&self) -> Bits {
        self.violators_at(&self.depth)
    }

    /// The fewest windows to raise, raised lowest priority first, that leave
    /// every overlapping pair in order: the violators, plus every window that
    /// must end above one being raised. Unique, so there is nothing to search.
    /// Keeping a violator in place would keep what it is wrongly below in place
    /// too; raising a window without what must be above it would put it on top
    /// of them.
    pub fn raise_set(&self) -> Bits {
        let mut r = self.violators();
        for j in (0..self.n).rev() {
            if r & bit(j) != 0 {
                r |= self.adj[j] & before(j);
            }
        }
        r
    }

    /// Groups with no overlap between them. Their relative order is invisible,
    /// so each is a lane that can raise independently.
    pub fn lanes(&self) -> Vec<Bits> {
        let mut left = self.all();
        let mut out = Vec::new();
        while left != 0 {
            let mut lane = left.isolate_lowest_one();
            let mut frontier = lane;
            while frontier != 0 {
                let j = frontier.trailing_zeros() as usize;
                frontier &= frontier - 1;
                let new = self.adj[j] & !lane;
                lane |= new;
                frontier |= new;
            }
            out.push(lane);
            left &= !lane;
        }
        out
    }

    #[cfg(test)]
    fn overlaps(&self, i: usize, j: usize) -> bool {
        self.adj[i] & bit(j) != 0
    }
}

/// What the restack needs from the window server and the apps. The live one is
/// in `platform/zorder.rs`; tests use a fake with the same raise physics.
pub trait WindowServer {
    fn now(&self) -> Instant;
    /// On-screen layer-0 windows, front to back, into `out`.
    fn read_stack(&mut self, out: &mut Vec<Seen>);
    fn displays(&mut self) -> Vec<Rect>;
    /// Whether `w`, of app `pid`, is the key window: asks that one app only.
    fn is_key(&mut self, w: WindowId, pid: i32) -> bool;
    /// The key window, whichever app holds it: asks every app.
    fn focused_window(&mut self) -> Option<WindowId>;
    fn focus(&mut self, w: WindowId) -> bool;
    /// Issue a raise. Its landing is only ever confirmed by a later read.
    fn raise(&mut self, w: WindowId, pid: i32) -> bool;
    /// Sleep until a landing hint, `until`, or `cancel`. True on a hint.
    fn wait(&mut self, until: Instant, cancel: &dyn Fn() -> bool) -> bool;
}

fn ms(d: Duration) -> u64 {
    d.as_millis() as u64
}

/// Impose `desired` (front to back) wherever windows overlap.
///
/// `desired[0]` is the designated top: the window the core wants key, not
/// whatever AX says is focused right now, which races the focus effect's own
/// landing. With `focus_top` it is made key here once the un-hides have
/// resurfaced, and again at the end if focus went elsewhere: an un-hide can
/// hand focus to the app it revealed.
///
/// Two passes: the first does the work; the second replans from a fresh read,
/// absorbing a raise that outlived its landing timeout, a late arrival, or a
/// window that moved meanwhile. `cancel` turning true (a newer order exists)
/// stops it between reads and raises.
pub fn reassert(
    ws: &mut dyn WindowServer,
    desired: &[WindowId],
    attached: &[(WindowId, WindowId)],
    focus_top: bool,
    cancel: &dyn Fn() -> bool,
) -> Option<RestackStats> {
    let t0 = ws.now();
    let (&top, rest) = desired.split_first()?;
    if rest.is_empty() && !focus_top {
        return None;
    }
    // Latched: a converged reassert whose successor arrived microseconds
    // later is still a valid latency sample.
    let aborted = std::cell::Cell::new(false);
    let cancel = || {
        let c = cancel();
        if c {
            aborted.set(true);
        }
        c
    };

    let displays = ws.displays();
    let mut stack = Vec::with_capacity(MAX_WINDOWS);

    // Windows still resurfacing from an un-hide are absent from the list; a
    // window that pops back mid-pass would land wherever it left off. Each
    // resurfacing sends a hint, so this mostly sleeps until one arrives.
    let presence_until = t0 + PRESENCE_TIMEOUT;
    let listed = |stack: &[Seen]| {
        desired
            .iter()
            .filter(|w| stack.iter().any(|s| s.id == **w))
            .count()
    };
    loop {
        ws.read_stack(&mut stack);
        if listed(&stack) == desired.len() || ws.now() >= presence_until || cancel() {
            break;
        }
        ws.wait(presence_until, &cancel);
    }
    let presence_wait_ms = ms(ws.now() - t0);
    let missing = (desired.len() - listed(&stack)) as u32;
    let mut layout = Layout::new(desired, attached, &stack, &displays);
    let start_order: Vec<WindowId> = stack
        .iter()
        .map(|s| s.id)
        .filter(|w| layout.index_of(*w).is_some())
        .collect();
    let frames: Vec<(WindowId, Rect)> = (0..layout.n)
        .map(|i| (layout.ids[i], layout.frames[i]))
        .chain(
            attached
                .iter()
                .filter_map(|&(w, _)| stack.iter().find(|s| s.id == w).map(|s| (w, s.frame))),
        )
        .collect();
    let top_pid = stack.iter().find(|s| s.id == top).map(|s| s.pid);

    let take_focus = |ws: &mut dyn WindowServer| -> u32 {
        (focus_top && !cancel() && !top_pid.is_some_and(|p| ws.is_key(top, p)) && ws.focus(top))
            as u32
    };
    let mut refocused = take_focus(ws);

    let mut scope: Vec<WindowId> = desired.to_vec();
    let mut key: Option<WindowId> = None;
    let mut handoff_wait_ms = 0;
    let mut handoff_checked = false;
    let mut raises: Vec<RaiseStat> = Vec::new();
    let mut second_pass = false;
    let (mut edges, mut lanes, mut raise_set, mut untouched) = (0, 0, 0, layout.len() as u32);
    // Taking focus raises the top, so the presence read is stale after it.
    let mut reread = refocused > 0;
    for pass in 0..2u8 {
        if pass > 0 && raises.iter().any(|r| r.timed_out) {
            // Only a raise that timed out can still be in flight, so only
            // then is it worth giving a straggler time to land.
            let until = ws.now() + SETTLE;
            while ws.now() < until && !cancel() {
                ws.wait(until, &cancel);
            }
        }
        if cancel() {
            break;
        }
        if reread {
            ws.read_stack(&mut stack);
            layout = Layout::new(&scope, attached, &stack, &displays);
        }
        reread = true;
        if pass == 0 {
            edges = layout.edges();
        }
        let mut r = layout.raise_set();
        if r == 0 {
            break;
        }

        // While a window of ours holds key status, raises land below it, and
        // nothing can be raised above it; wait out the focus handoff in
        // flight (the focus effect always precedes the restack). A key window
        // that never hands off, or has no handoff coming because this restack
        // doesn't take focus, is left where it is and the rest ordered
        // around it.
        if !handoff_checked {
            handoff_checked = true;
            let t = ws.now();
            let until = t + LANDING_TIMEOUT;
            key = if focus_top && top_pid.is_some_and(|p| ws.is_key(top, p)) {
                Some(top)
            } else {
                ws.focused_window()
            };
            let blocks = |layout: &Layout, r: Bits, key: Option<WindowId>| {
                key.filter(|&k| k != top)
                    .and_then(|k| layout.index_of(k))
                    .is_some_and(|k| {
                        focus_top || ones(r & before(k)).any(|i| layout.adj[i] & bit(k) != 0)
                    })
            };
            let mut waited = false;
            while focus_top && blocks(&layout, r, key) && ws.now() < until && !cancel() {
                ws.wait(until, &cancel);
                key = ws.focused_window();
                waited = true;
            }
            handoff_wait_ms = ms(ws.now() - t);
            let exempt = blocks(&layout, r, key);
            if exempt {
                scope.retain(|w| Some(*w) != key);
            }
            if waited || exempt {
                // The handoff raised the new key window, or the scope shrank:
                // plan again.
                ws.read_stack(&mut stack);
                layout = Layout::new(&scope, attached, &stack, &displays);
                r = layout.raise_set();
                if r == 0 {
                    break;
                }
            }
        } else if focus_top && top_pid.is_some_and(|p| ws.is_key(top, p)) {
            // The handoff may have landed since the first pass looked.
            key = Some(top);
        }

        if pass == 0 {
            lanes = layout.lanes().iter().filter(|m| **m & r != 0).count() as u32;
            raise_set = r.count_ones();
            untouched = (layout.len() as u32).saturating_sub(raise_set);
        } else {
            second_pass = true;
        }
        run_lanes(
            ws,
            &layout,
            r,
            key,
            top_pid,
            pass,
            &mut raises,
            &mut stack,
            &cancel,
        );
    }
    // An activation that landed late is taken back here rather than left for
    // the core's focus expectation to time out.
    refocused += take_focus(ws);

    if layout.len() <= 1 && refocused == 0 {
        return None;
    }
    if reread && (!raises.is_empty() || refocused > 0) {
        ws.read_stack(&mut stack);
        layout = Layout::new(&scope, attached, &stack, &displays);
    }
    let violated_end = layout.violators().count_ones();
    Some(RestackStats {
        total_ms: ms(ws.now() - t0),
        landing_wait_ms: 0,
        presence_wait_ms,
        handoff_wait_ms,
        desired: desired.len() as u32,
        missing,
        second_pass,
        converged: violated_end == 0,
        aborted: aborted.get(),
        ghost_pass: false, // the worker marks its ghost-watch reruns
        refocused,
        start_order,
        edges,
        lanes,
        raise_set,
        untouched,
        violated_end,
        frames,
        raises,
    })
}

struct Flight {
    i: usize,
    kind: RaiseKind,
    lane: u8,
    issued: Instant,
    ax_ms: u64,
}

/// Raise `r` (minus the top) lowest priority first, each lane with at most one
/// raise in flight and each raise confirmed landed before its lane moves on.
/// AXRaise is applied on the target app's own schedule, so two unconfirmed
/// raises in one lane land in arbitrary order; across lanes, order is
/// invisible.
///
/// A sibling lands above the key window, and a later background raise that
/// must go above it would land below it. So a landed sibling stays pending
/// until the top is re-raised, which freezes every pending sibling beneath it
/// at once, and that re-raise happens only when a background raise needs it,
/// or at the end if a pending sibling covers the top.
#[allow(clippy::too_many_arguments)]
fn run_lanes(
    ws: &mut dyn WindowServer,
    layout: &Layout,
    r: Bits,
    key: Option<WindowId>,
    top_pid: Option<i32>,
    pass: u8,
    raises: &mut Vec<RaiseStat>,
    stack: &mut Vec<Seen>,
    cancel: &dyn Fn() -> bool,
) {
    let top = layout.has_top.then_some(0usize);
    let top_bit = if top.is_some() { bit(0) } else { 0 };
    let key_bit = key.and_then(|k| layout.index_of(k)).map_or(0, bit);
    // The physics that make siblings special belong to the key window's app;
    // with the designated top key, that is the top's.
    let sibling = |i: usize| top_pid.is_some_and(|p| layout.pids[i] == p);
    let lane_masks = layout.lanes();
    let lane_of = |i: usize| lane_masks.iter().position(|m| m & bit(i) != 0).unwrap_or(0);
    let start = layout.depth;

    let mut queues: Vec<Bits> = lane_masks.iter().map(|m| m & r & !top_bit).collect();
    let mut flights: Vec<Option<Flight>> = queues.iter().map(|_| None).collect();
    let mut top_flight: Option<Flight> = None;
    let mut settled = layout.all() & !r;
    // Windows above the top that it hasn't been re-raised over yet: landed
    // siblings, and, with the top key, whatever was already above it (a
    // window an un-hide brought forward over it, say), since a background
    // raise can't pass them either. And those a re-raise in flight is
    // freezing.
    let mut pending: Bits = if top.is_some() && key_bit == top_bit {
        ones(layout.all() & !top_bit)
            .filter(|&j| start[j] < start[0])
            .fold(0, |m, j| m | bit(j))
    } else {
        0
    };
    let mut freezing: Bits = 0;
    let mut depth = start;

    let issue = |ws: &mut dyn WindowServer, i: usize, kind: RaiseKind, lane: u8| {
        let t = ws.now();
        let sent = ws.raise(layout.ids[i], layout.pids[i]);
        sent.then(|| Flight {
            i,
            kind,
            lane,
            issued: t,
            ax_ms: ms(ws.now() - t),
        })
    };
    // Landed is the raise's own observable: above every settled window of
    // its lane, save those that rightly sit above it: the top, the key
    // window, and, for a background raise, windows above the top that it
    // needn't pass (any it must pass had the top re-raised over them before
    // it was issued). A sibling lands above all of those.
    let landed = |depth: &[u16; MAX_WINDOWS], settled: Bits, above_top: Bits, i: usize| {
        let passes = if sibling(i) { 0 } else { above_top };
        depth[i] != MISSING
            && ones(settled & lane_masks[lane_of(i)] & !bit(i) & !top_bit & !key_bit & !passes)
                .all(|j| depth[i] < depth[j])
    };
    let top_landed = |depth: &[u16; MAX_WINDOWS]| {
        depth[0] != MISSING && ones(layout.all() & !bit(0) & !key_bit).all(|j| depth[0] < depth[j])
    };
    let stat = |f: &Flight, t_now: Instant, timed_out: bool, via_event: bool| {
        let d = start[f.i];
        RaiseStat {
            window: layout.ids[f.i],
            pid: layout.pids[f.i],
            kind: f.kind,
            pass,
            lane: f.lane,
            above_scope: ones(layout.all()).filter(|&j| start[j] < d).count() as u32,
            above_all: d as u32,
            ax_ms: f.ax_ms,
            wait_ms: ms(t_now - f.issued),
            timed_out,
            via_event,
        }
    };

    loop {
        if cancel() {
            // Raises still in the air are logged as sent: they can land after
            // the successor converged, and the worker's ghost watch keys on
            // whether anything was.
            let now = ws.now();
            for f in flights.iter().flatten().chain(top_flight.iter()) {
                raises.push(stat(f, now, false, false));
            }
            return;
        }
        let mut want_top = false;
        for (lane, queue) in queues.iter_mut().enumerate() {
            while flights[lane].is_none() && *queue != 0 {
                let i = 63 - queue.leading_zeros() as usize;
                let kind = if sibling(i) {
                    RaiseKind::Sibling
                } else {
                    RaiseKind::Background
                };
                if kind == RaiseKind::Background
                    && layout.adj[i] & after(i) & (pending | freezing) != 0
                {
                    want_top = true;
                    break;
                }
                *queue &= !bit(i);
                if landed(&depth, settled, pending | freezing, i) {
                    // Already where the raise would put it. Raising it anyway
                    // would leave nothing to confirm the landing by, and the
                    // lane's next raise could land first and be overtaken.
                    settled |= bit(i);
                    continue;
                }
                flights[lane] = issue(ws, i, kind, lane as u8);
                if flights[lane].is_none() {
                    // No element to raise (the window closed): nothing will land.
                    settled |= bit(i);
                }
            }
        }
        if want_top && top.is_some() && top_flight.is_none() {
            // Windows the top is already above are frozen already.
            if ones(pending).any(|j| depth[j] < depth[0]) {
                top_flight = issue(ws, 0, RaiseKind::Top, lane_of(0) as u8);
                freezing = pending;
            }
            pending = 0;
            if top_flight.is_none() {
                continue;
            }
        }
        let in_flight = flights.iter().flatten().chain(top_flight.iter());
        let Some(until) = in_flight.map(|f| f.issued + LANDING_TIMEOUT).min() else {
            if want_top {
                // Blocked on a top re-raise that couldn't be sent; order the
                // rest anyway and let the second pass judge.
                pending = 0;
                freezing = 0;
                continue;
            }
            break;
        };
        let via_event = ws.wait(until, cancel);
        ws.read_stack(stack);
        depth = layout.depths(stack);
        let now = ws.now();
        for slot in flights.iter_mut() {
            let Some(f) = slot else { continue };
            let ok = landed(&depth, settled, pending | freezing, f.i);
            let late = now >= f.issued + LANDING_TIMEOUT;
            if ok || late {
                raises.push(stat(f, now, !ok, via_event));
                settled |= bit(f.i);
                if f.kind == RaiseKind::Sibling {
                    pending |= bit(f.i);
                }
                *slot = None;
            }
        }
        if let Some(f) = &top_flight {
            let ok = top_landed(&depth);
            if ok || now >= f.issued + LANDING_TIMEOUT {
                raises.push(stat(f, now, !ok, via_event));
                top_flight = None;
                freezing = 0;
            }
        }
    }

    // The top goes back on top if anything it overlaps, a pending sibling
    // most likely, is above it.
    let Some(t) = top else { return };
    let covered = ones(layout.adj[t] & !key_bit).any(|j| depth[j] < depth[t]);
    if cancel() || !(covered || pending & layout.adj[t] != 0) {
        return;
    }
    let Some(f) = issue(ws, t, RaiseKind::Top, lane_of(t) as u8) else {
        return;
    };
    let until = f.issued + LANDING_TIMEOUT;
    loop {
        let via_event = ws.wait(until, cancel);
        ws.read_stack(stack);
        depth = layout.depths(stack);
        let now = ws.now();
        let ok = top_landed(&depth);
        if ok || now >= until || cancel() {
            raises.push(stat(&f, now, !ok, via_event));
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: f64, y: f64, w: f64, h: f64) -> Rect {
        Rect { x, y, w, h }
    }

    /// A seeded xorshift, so the layouts are random but every run is the same.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    /// A raise set is valid when it holds everything that must end above one
    /// of its members, and every overlapping pair left in place is in order.
    fn valid(l: &Layout, set: Bits) -> bool {
        let up_closed = ones(set).all(|j| l.adj[j] & before(j) & !set == 0);
        let rest_in_order = ones(l.all() & !set).all(|i| {
            ones(l.all() & !set)
                .filter(|&j| j > i && l.overlaps(i, j))
                .all(|j| l.depth[i] < l.depth[j])
        });
        up_closed && rest_in_order
    }

    /// The claim the planner rests on, checked against brute force: over
    /// random layouts, the raise set is valid and nothing smaller is.
    #[test]
    fn the_raise_set_is_the_smallest_that_works() {
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        for _ in 0..3000 {
            let n = 1 + rng.below(8) as usize;
            let priority: Vec<WindowId> = (1..=n as u32).map(WindowId).collect();
            let mut stack: Vec<Seen> = priority
                .iter()
                .map(|&id| Seen {
                    id,
                    pid: 1,
                    frame: rect(
                        rng.below(300) as f64,
                        rng.below(300) as f64,
                        20.0 + rng.below(200) as f64,
                        20.0 + rng.below(200) as f64,
                    ),
                })
                .collect();
            for i in (1..stack.len()).rev() {
                stack.swap(i, rng.below(i as u64 + 1) as usize);
            }
            let l = Layout::new(&priority, &[], &stack, &[]);
            let r = l.raise_set();
            assert!(valid(&l, r));
            let smallest = (0..1u64 << n)
                .filter(|&s| valid(&l, s))
                .map(|s| s.count_ones())
                .min()
                .unwrap();
            assert_eq!(r.count_ones(), smallest);
        }
    }

    /// A chain where the ends don't touch: A over B over C, observed C, A, B.
    /// Only B is wrong, and raising B means raising A after it; C stays put.
    #[test]
    fn a_chain_raises_only_the_links_that_must_move() {
        let (a, b, c) = (WindowId(1), WindowId(2), WindowId(3));
        let stack = [
            Seen {
                id: c,
                pid: 1,
                frame: rect(200.0, 0.0, 150.0, 100.0),
            },
            Seen {
                id: a,
                pid: 1,
                frame: rect(0.0, 0.0, 150.0, 100.0),
            },
            Seen {
                id: b,
                pid: 1,
                frame: rect(100.0, 0.0, 150.0, 100.0),
            },
        ];
        let l = Layout::new(&[a, b, c], &[], &stack, &[]);
        assert_eq!(l.raise_set(), bit(0) | bit(1));
        assert_eq!(l.lanes().len(), 1);
    }
}
