//! Gathering the freeze diagnostics ([`crate::freeze`]) from macOS, on a
//! worker of their own.
//!
//! The worker exists because of where a freeze leaves the rest of the
//! process. The tap thread must answer in microseconds; the engine may be
//! mid-snapshot inside an app that has stopped answering; the main thread
//! belongs to AppKit, whose delivery is the very thing that stalled. So the
//! tap only pokes the worker, the worker reads the window server and the
//! kernel directly (no run loop, no UI), says what it found on stderr at
//! once, and hands the record to the engine for filing. If the engine is
//! wedged, stderr still has it.
//!
//! The same worker reads the tap list every few seconds and files it when
//! it has changed — a tap appearing, going, or being disabled — so the
//! pipeline's shape before a freeze is on record without a chord.

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crossbeam_channel::{bounded, RecvTimeoutError, Sender};
use objc2_core_graphics::{
    CGError, CGEventFlags, CGEventSource, CGEventSourceStateID, CGEventTapInformation,
    CGEventTapLocation, CGEventTapOptions, CGEventType, CGGetEventTapList, CGMouseButton,
    CGSessionCopyCurrentDictionary,
};
use objc2_core_foundation::CFDictionary;
use ordo_skylight_sys as sys;

use crate::clock::wall_now_ms;
use crate::engine::Msg;
use crate::freeze::{
    is_suspect, Diag, FreezeSnapshot, Front, InputState, SessionInfo, SourceState, TapInfo,
    TapPoint,
};

use super::{ax, cf, drained, zorder};

/// How often the tap list is read. One kernel call; the read is cheap, the
/// row is only written on change.
const SAMPLE_EVERY: Duration = Duration::from_secs(5);

/// The longest `ps` gets. A machine in trouble may be slow to list its
/// processes; the rest of the snapshot must not wait on it forever.
const PS_TIMEOUT: Duration = Duration::from_secs(3);

/// Header plus the top ten.
const PS_LINES: usize = 11;

#[link(name = "Carbon", kind = "framework")]
extern "C" {
    fn IsSecureEventInputEnabled() -> bool;
}

/// The tap's handle on the worker. The channel holds one request: a chord
/// mashed while a gather is under way queues one more gather, not one per
/// press.
#[derive(Clone)]
pub struct FreezeTrigger(Sender<()>);

impl FreezeTrigger {
    pub fn request(&self) {
        let _ = self.0.try_send(());
    }
}

/// What a tap sample is compared on: the pipeline's shape, not the latency
/// numbers, which drift every read.
type TapShape = Vec<(i32, TapPoint, bool, bool, u64)>;

fn shape(taps: &[TapInfo]) -> TapShape {
    taps.iter()
        .map(|t| (t.tapping_pid, t.point, t.listen_only, t.enabled, t.mask))
        .collect()
}

/// Spawn the worker. Runs until the process exits.
pub fn spawn(engine: Sender<Msg>) -> FreezeTrigger {
    let (tx, rx) = bounded(1);
    std::thread::spawn(move || {
        let mut last: Option<TapShape> = None;
        loop {
            match rx.recv_timeout(SAMPLE_EVERY) {
                Ok(()) => {
                    let snap = drained(gather);
                    eprintln!("ordo: {}", snap.summary());
                    eprintln!(
                        "ordo: freeze snapshot json {}",
                        serde_json::to_string(&snap).unwrap_or_else(|_| "null".into())
                    );
                    // The freeze ends in a reboot; stderr is a file under
                    // launchd and must reach the disk before then.
                    unsafe { libc::fsync(2) };
                    if engine.send(Msg::FreezeDiag(Diag::Snapshot(Box::new(snap)))).is_err() {
                        return;
                    }
                }
                Err(RecvTimeoutError::Timeout) => {
                    let wall_ms = wall_now_ms();
                    let (taps, _) = drained(tap_list);
                    let now = shape(&taps);
                    if last.as_ref() == Some(&now) {
                        continue;
                    }
                    last = Some(now);
                    if engine.send(Msg::FreezeDiag(Diag::TapSample { wall_ms, taps })).is_err() {
                        return;
                    }
                }
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
    });
    FreezeTrigger(tx)
}

/// The whole picture. Cheap reads first; `ps` and the apps' AX answers,
/// which can be slow on a machine in trouble, last and bounded.
pub fn gather() -> FreezeSnapshot {
    let started = Instant::now();
    let wall_ms = wall_now_ms();
    let (taps, taps_error) = tap_list();
    let session = session_info();
    let input = InputState {
        hid: source_state(CGEventSourceStateID::HIDSystemState),
        session: source_state(CGEventSourceStateID::CombinedSessionState),
    };
    let front = front();
    let (processes, ps_error) = processes();
    let suspects = processes.iter().filter(|l| is_suspect(l)).cloned().collect();
    let top_processes = processes.into_iter().take(PS_LINES).collect();
    FreezeSnapshot {
        wall_ms,
        took_ms: started.elapsed().as_millis() as u64,
        taps,
        taps_error,
        session,
        input,
        front,
        top_processes,
        suspects,
        ps_error,
    }
}

/// Every event tap in the session, as `CGGetEventTapList` lists them.
pub fn tap_list() -> (Vec<TapInfo>, Option<String>) {
    // Plenty: a busy machine has a dozen or so.
    const MAX_TAPS: usize = 256;
    // A plain C struct the call fills in; zero is a valid value of every
    // field.
    let mut raw: Vec<CGEventTapInformation> = vec![unsafe { std::mem::zeroed() }; MAX_TAPS];
    let mut count: u32 = 0;
    let err = unsafe { CGGetEventTapList(MAX_TAPS as u32, raw.as_mut_ptr(), &mut count) };
    if err != CGError::Success {
        return (Vec::new(), Some(format!("CGGetEventTapList failed: {}", err.0)));
    }
    raw.truncate(count as usize);
    let taps = raw
        .iter()
        .map(|t| TapInfo {
            id: t.eventTapID,
            point: match t.tapPoint {
                CGEventTapLocation::HIDEventTap => TapPoint::Hid,
                CGEventTapLocation::SessionEventTap => TapPoint::Session,
                CGEventTapLocation::AnnotatedSessionEventTap => TapPoint::AnnotatedSession,
                other => TapPoint::Other(other.0),
            },
            listen_only: t.options == CGEventTapOptions::ListenOnly,
            enabled: t.enabled,
            mask: t.eventsOfInterest,
            mask_names: mask_names(t.eventsOfInterest),
            tapping_pid: t.tappingProcess,
            tapping_process: process_path(t.tappingProcess),
            tapped_pid: t.processBeingTapped,
            min_usec_latency: t.minUsecLatency,
            avg_usec_latency: t.avgUsecLatency,
            max_usec_latency: t.maxUsecLatency,
        })
        .collect();
    (taps, None)
}

/// The event types a tap's mask names, for the bits worth reading at a
/// glance. The raw mask is kept beside it.
fn mask_names(mask: u64) -> Vec<String> {
    const NAMED: &[(CGEventType, &str)] = &[
        (CGEventType::LeftMouseDown, "left_mouse_down"),
        (CGEventType::LeftMouseUp, "left_mouse_up"),
        (CGEventType::RightMouseDown, "right_mouse_down"),
        (CGEventType::RightMouseUp, "right_mouse_up"),
        (CGEventType::MouseMoved, "mouse_moved"),
        (CGEventType::LeftMouseDragged, "left_mouse_dragged"),
        (CGEventType::RightMouseDragged, "right_mouse_dragged"),
        (CGEventType::KeyDown, "key_down"),
        (CGEventType::KeyUp, "key_up"),
        (CGEventType::FlagsChanged, "flags_changed"),
        (CGEventType::ScrollWheel, "scroll_wheel"),
        (CGEventType::TabletPointer, "tablet_pointer"),
        (CGEventType::TabletProximity, "tablet_proximity"),
        (CGEventType::OtherMouseDown, "other_mouse_down"),
        (CGEventType::OtherMouseUp, "other_mouse_up"),
        (CGEventType::OtherMouseDragged, "other_mouse_dragged"),
    ];
    if mask == u64::MAX {
        return vec!["all".into()];
    }
    NAMED
        .iter()
        .filter(|(ty, _)| mask & (1u64 << ty.0) != 0)
        .map(|(_, name)| name.to_string())
        .collect()
}

fn process_path(pid: i32) -> Option<String> {
    if pid <= 0 {
        return None;
    }
    let mut path = [0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    let n = unsafe { libc::proc_pidpath(pid, path.as_mut_ptr().cast(), path.len() as u32) };
    (n > 0).then(|| String::from_utf8_lossy(&path[..n as usize]).into_owned())
}

/// The session dictionary's account of the login session and of secure
/// input, plus Carbon's own secure-input flag. Neither needs root; the
/// secure-input pid key is undocumented but has been there for years.
fn session_info() -> SessionInfo {
    let dict = CGSessionCopyCurrentDictionary();
    let d = dict
        .as_deref()
        .map_or(std::ptr::null(), |d| d as *const CFDictionary as sys::CFDictionaryRef);
    let secure_input_pid = unsafe { cf::number_i64(cf::dict_get(d, "kCGSSessionSecureInputPID")) };
    SessionInfo {
        secure_input_pid,
        secure_input_process: secure_input_pid.and_then(|p| process_path(p as i32)),
        secure_event_input_enabled: unsafe { IsSecureEventInputEnabled() },
        on_console: unsafe { cf::boolean(cf::dict_get(d, "kCGSSessionOnConsoleKey")) },
        login_done: unsafe { cf::boolean(cf::dict_get(d, "kCGSSessionLoginDoneKey")) },
        user: unsafe { cf::string_value(cf::dict_get(d, "kCGSSessionUserNameKey")) },
    }
}

fn source_state(id: CGEventSourceStateID) -> SourceState {
    let flags: CGEventFlags = CGEventSource::flags_state(id);
    let since = |ty| CGEventSource::seconds_since_last_event_type(id, ty);
    SourceState {
        flags: flags.0,
        left_button: CGEventSource::button_state(id, CGMouseButton::Left),
        right_button: CGEventSource::button_state(id, CGMouseButton::Right),
        secs_since_key_down: since(CGEventType::KeyDown),
        secs_since_mouse_down: since(CGEventType::LeftMouseDown),
        secs_since_mouse_moved: since(CGEventType::MouseMoved),
    }
}

/// The window server's front window and its owner (one list read, no app
/// asked), then the apps' own `AXFrontmost`, each bounded by the messaging
/// timeout, as Ordo's focus tracking reads it.
fn front() -> Front {
    let (server_window, server_pid) = zorder::stack_with_pids()
        .first()
        .map_or((None, None), |(w, p)| (Some(*w), Some(*p)));
    let ax_pid = ax::frontmost_app().map(|p| p.0);
    Front {
        server_window,
        server_pid,
        server_process: server_pid.and_then(process_path),
        ax_pid,
        ax_process: ax_pid.and_then(process_path),
    }
}

/// Every process, by CPU, header first, bounded by [`PS_TIMEOUT`]. Its
/// output is drained on a thread of its own so a long process list can
/// never fill the pipe and stall `ps` into the timeout.
fn processes() -> (Vec<String>, Option<String>) {
    let mut child = match Command::new("/bin/ps")
        .args(["-axro", "pid,ppid,pcpu,pmem,etime,user,comm"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return (Vec::new(), Some(format!("ps: {e}"))),
    };
    let Some(mut stdout) = child.stdout.take() else {
        return (Vec::new(), Some("ps: no stdout".into()));
    };
    let reader = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = stdout.read_to_string(&mut text);
        text
    });
    let deadline = Instant::now() + PS_TIMEOUT;
    let error = loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => break None,
            Ok(Some(status)) => break Some(format!("ps exited {status}")),
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                break Some(format!("ps timed out after {} s", PS_TIMEOUT.as_secs()));
            }
            Err(e) => break Some(format!("ps: {e}")),
        }
    };
    // The pipe is closed either way by now, so the reader is done.
    let text = reader.join().unwrap_or_default();
    let lines: Vec<String> = text.lines().map(|l| l.trim_end().to_string()).collect();
    (lines, error)
}
