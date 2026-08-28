//! The WindowServer's push stream: raise/order landings and Space changes,
//! delivered instead of polled.
//!
//! Probed live (examples/raise_notify_probe.rs, Tahoe 26.6): WindowServer
//! pushes 808 for every focused-or-raised window — including a pure
//! background AXRaise, exactly the reassert's operation — 815/816 per
//! order-in/out, and 1329/1401 on Space changes. Three parts are mandatory
//! and each was measured to be load-bearing: the per-code registration, the
//! per-window opt-in (which REPLACES the connection's watch list, so the full
//! set is resent whenever it changes), and a running NSApplication event loop
//! on the main thread — a bare CFRunLoop receives nothing.
//!
//! Events here are HINTS that wake a read-back, never the authority — the
//! same contract as every other notification in Ordo. The reassert's landing
//! gates still confirm by CG read; an event only ends the sleep between
//! reads. That is what makes every loss mode harmless: a dropped ring entry,
//! a window not yet opted in, or a dead stream all degrade to the pre-event
//! polling cadence, never to a missed landing.

use std::collections::VecDeque;
use std::ffi::c_void;
use std::os::raw::c_int;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use crossbeam_channel::Sender;
use ordo_core::RescanTrigger;
use ordo_skylight_sys as sys;

use crate::engine::Msg;
use crate::ports::WorldSource;

/// Focused or raised — fires per window, even for background raises.
const EV_FOCUSED_OR_RAISED: u32 = 808;
/// Ordered in (pixels joined the stack) — an un-hide's resurfacing signal.
const EV_ORDERED_IN: u32 = 815;
/// Space changes, both flavors observed live; either alone is incomplete.
const EV_SPACE_CURRENT_CHANGED: u32 = 1329;
const EV_ACTIVE_SPACE_CHANGED: u32 = 1401;

/// How long a waiter sleeps between forced re-checks. This is the cancel
/// latency when no events flow and the poll fallback cadence when the stream
/// is dead — 4x the old 5ms tick, acceptable because the common case (an
/// event arrives) wakes instantly instead of on the next tick.
const WAIT_SLICE: Duration = Duration::from_millis(20);

/// Entries kept for late readers; past this, old entries drop and a stale
/// cursor degrades to "something happened, go read" (see [`RaiseSignals::wait`]).
const RING_CAP: usize = 512;

pub enum WaitOutcome {
    /// An entry past the cursor matched (or the ring outran the cursor):
    /// worth a read-back now.
    Hint,
    Timeout,
    Cancelled,
}

/// The landing-hint mailbox between the notify proc (AppKit's event thread)
/// and the gates that today sleep-poll (restack worker). A grow-only sequence
/// plus a bounded ring: waiters remember where they've read to, so one shared
/// log serves any number of concurrent gates without consuming each other's
/// events.
pub struct RaiseSignals {
    state: Mutex<Ring>,
    wake: Condvar,
}

struct Ring {
    next_seq: u64,
    entries: VecDeque<(u64, u32)>, // (seq, window id)
}

impl RaiseSignals {
    pub fn new() -> Self {
        RaiseSignals {
            state: Mutex::new(Ring {
                next_seq: 0,
                entries: VecDeque::new(),
            }),
            wake: Condvar::new(),
        }
    }

    /// Where the log currently ends; a waiter starts reading from here.
    pub fn cursor(&self) -> u64 {
        self.state.lock().unwrap().next_seq
    }

    pub fn push(&self, wid: u32) {
        let mut ring = self.state.lock().unwrap();
        let seq = ring.next_seq;
        ring.next_seq += 1;
        ring.entries.push_back((seq, wid));
        if ring.entries.len() > RING_CAP {
            ring.entries.pop_front();
        }
        drop(ring);
        self.wake.notify_all();
    }

    /// Sleep until an entry past `*cursor` satisfies `matches`, the deadline
    /// passes, or `cancel` turns true. Advances the cursor past everything
    /// examined. A cursor the ring has outrun returns `Hint` unconditionally:
    /// the caller's response to a hint is a read-back, so a lost entry costs
    /// one extra read, never a missed landing.
    pub fn wait(
        &self,
        cursor: &mut u64,
        matches: &dyn Fn(u32) -> bool,
        deadline: Instant,
        cancel: &dyn Fn() -> bool,
    ) -> WaitOutcome {
        let mut ring = self.state.lock().unwrap();
        loop {
            if ring.next_seq > *cursor {
                let base = ring.entries.front().map_or(ring.next_seq, |e| e.0);
                let outrun = *cursor < base;
                let matched = ring
                    .entries
                    .iter()
                    .any(|&(seq, w)| seq >= *cursor && matches(w));
                *cursor = ring.next_seq;
                if outrun || matched {
                    return WaitOutcome::Hint;
                }
            }
            if cancel() {
                return WaitOutcome::Cancelled;
            }
            let now = Instant::now();
            if now >= deadline {
                return WaitOutcome::Timeout;
            }
            let slice = deadline.duration_since(now).min(WAIT_SLICE);
            ring = self.wake.wait_timeout(ring, slice).unwrap().0;
        }
    }
}

impl Default for RaiseSignals {
    fn default() -> Self {
        Self::new()
    }
}

struct Inner {
    signals: Arc<RaiseSignals>,
    tx: Sender<Msg>,
    /// The full watch list as last sent — the opt-in call replaces rather
    /// than adds, so this is both dedup and the set the next resend carries.
    watched: Mutex<Vec<u32>>,
    /// Space changes arrive in bursts of several 1329/1401 per switch; one
    /// rescan hint per burst is plenty (the 150ms space watcher remains the
    /// polling backstop underneath).
    last_space_hint: Mutex<Option<Instant>>,
}

#[derive(Clone)]
pub struct WsEvents {
    inner: Arc<Inner>,
}

impl WsEvents {
    /// The mailbox the restack worker's gates wait on.
    pub fn signals(&self) -> Arc<RaiseSignals> {
        self.inner.signals.clone()
    }

    /// Keep WindowServer streaming about exactly these windows. Idempotent
    /// and cheap when nothing changed; the full set goes out otherwise.
    pub fn subscribe(&self, mut ids: Vec<u32>) {
        ids.sort_unstable();
        ids.dedup();
        let mut watched = self.inner.watched.lock().unwrap();
        if *watched == ids {
            return;
        }
        unsafe {
            sys::SLSRequestNotificationsForWindows(
                super::skylight::connection(),
                ids.as_ptr(),
                ids.len() as c_int,
            );
        }
        *watched = ids;
    }
}

/// Register the notify procs on this process's WindowServer connection.
/// Call once, before [`run_app_loop`]; events start flowing when that loop
/// runs AND at least one window has been opted in via [`WsEvents::subscribe`].
pub fn install(tx: Sender<Msg>) -> WsEvents {
    let inner = Arc::new(Inner {
        signals: Arc::new(RaiseSignals::new()),
        tx,
        watched: Mutex::new(Vec::new()),
        last_space_hint: Mutex::new(None),
    });
    // The context pointer handed to WindowServer must outlive the process's
    // registrations, which are irrevocable — leak one clone (same lifetime
    // contract as the tap thread).
    let ctx = Arc::into_raw(inner.clone()) as *mut c_void;
    let cid = super::skylight::connection();
    for code in [
        EV_FOCUSED_OR_RAISED,
        EV_ORDERED_IN,
        EV_SPACE_CURRENT_CHANGED,
        EV_ACTIVE_SPACE_CHANGED,
    ] {
        unsafe {
            sys::SLSRegisterConnectionNotifyProc(cid, on_event, code, ctx);
        }
    }
    WsEvents { inner }
}

/// Runs on AppKit's event thread; the payload pointer is valid only for this
/// call. Extract the integers, forward, return — nothing heavier belongs here.
unsafe extern "C" fn on_event(
    event: u32,
    data: *mut c_void,
    len: usize,
    ctx: *mut c_void,
    _cid: c_int,
) {
    let inner = unsafe { &*(ctx as *const Inner) };
    match event {
        EV_FOCUSED_OR_RAISED | EV_ORDERED_IN => {
            if !data.is_null() && len >= 4 {
                let wid = u32::from_le_bytes(unsafe { *(data as *const [u8; 4]) });
                inner.signals.push(wid);
            }
        }
        EV_SPACE_CURRENT_CHANGED | EV_ACTIVE_SPACE_CHANGED => {
            let mut last = inner.last_space_hint.lock().unwrap();
            let now = Instant::now();
            if last.is_none_or(|t| now.duration_since(t) >= Duration::from_millis(150)) {
                *last = Some(now);
                let _ = inner.tx.send(Msg::Rescan(RescanTrigger::BackendHint {
                    kind: "space_event".into(),
                }));
            }
        }
        _ => {}
    }
}

/// Hand the main thread to AppKit forever. This IS the delivery mechanism —
/// the notify datagrams arrive through AppKit's connection event machinery
/// (yabai ends its main the same way). Never returns; shutdown paths must
/// exit the process from another thread.
pub fn run_app_loop() -> ! {
    use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};
    let mtm = objc2::MainThreadMarker::new().expect("run_app_loop must own the main thread");
    let app = NSApplication::sharedApplication(mtm);
    // A daemon, not an app: no Dock icon, no app switcher entry.
    app.setActivationPolicy(NSApplicationActivationPolicy::Prohibited);
    app.run();
    unreachable!("NSApp run returned without an [NSApp stop]");
}

/// Subscription upkeep, riding the observation path: every snapshot already
/// enumerates the world, so the watch list is corrected wherever it's stale —
/// self-healing at rescan cadence with zero extra IPC when nothing changed.
/// A window created between rescans is deaf until the next one; the gates'
/// poll fallback covers that gap by design.
pub struct SubscribingWorld<W: WorldSource> {
    inner: W,
    ws: WsEvents,
}

impl<W: WorldSource> SubscribingWorld<W> {
    pub fn new(inner: W, ws: WsEvents) -> Self {
        SubscribingWorld { inner, ws }
    }
}

impl<W: WorldSource> WorldSource for SubscribingWorld<W> {
    fn snapshot(&mut self) -> ordo_core::WorldSnapshot {
        let snap = self.inner.snapshot();
        self.ws
            .subscribe(snap.windows.iter().map(|w| w.id.0).collect());
        snap
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn never() -> impl Fn() -> bool {
        || false
    }

    #[test]
    fn a_push_wakes_the_waiter_and_wid_filters_apply() {
        let s = Arc::new(RaiseSignals::new());
        s.push(7); // before the cursor — must not satisfy the wait
        let mut cursor = s.cursor();

        let s2 = s.clone();
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(30));
            s2.push(9); // wrong window
            s2.push(7); // the one being waited for
        });
        let deadline = Instant::now() + Duration::from_millis(500);
        assert!(matches!(
            s.wait(&mut cursor, &|w| w == 7, deadline, &never()),
            WaitOutcome::Hint
        ));
        t.join().unwrap();

        // Cursor advanced past both pushes: nothing left to hint about.
        assert!(matches!(
            s.wait(
                &mut cursor,
                &|_| true,
                Instant::now() + Duration::from_millis(10),
                &never()
            ),
            WaitOutcome::Timeout
        ));
    }

    #[test]
    fn an_outrun_cursor_degrades_to_a_hint_not_a_lost_landing() {
        let s = RaiseSignals::new();
        let mut cursor = s.cursor();
        for _ in 0..(RING_CAP as u32 + 10) {
            s.push(1);
        }
        // The filter never matches, but the dropped entries force a Hint anyway.
        assert!(matches!(
            s.wait(
                &mut cursor,
                &|w| w == 999,
                Instant::now() + Duration::from_millis(50),
                &never()
            ),
            WaitOutcome::Hint
        ));
    }

    #[test]
    fn cancel_ends_the_wait_within_a_slice() {
        let s = RaiseSignals::new();
        let mut cursor = s.cursor();
        let t = Instant::now();
        assert!(matches!(
            s.wait(
                &mut cursor,
                &|_| true,
                Instant::now() + Duration::from_secs(5),
                &|| true
            ),
            WaitOutcome::Cancelled
        ));
        assert!(t.elapsed() < Duration::from_millis(100));
    }
}
