//! The global hotkey tap — and the intent channel.
//!
//! A `CGEventTap` on its own thread watches key-downs, matches them against the
//! (pure) [`crate::keys`] table, and for a hit posts a [`Msg`] to the engine and
//! swallows the key. Two hard rules from the research shape this:
//!   - the callback must never block — it does no AX work, only a channel send;
//!   - the tap gets disabled by the OS on timeout or heavy user input, and must
//!     re-enable itself, or hotkeys silently die after a while.
//!
//! The same tap WITNESSES the user's focus gestures Ordo does not own — every
//! mouse-down, and macOS's Cmd+Tab / Cmd+` — and reports them as
//! [`Msg::Gesture`] while passing the event through untouched. Without that
//! trace, a click and a notification stealing focus look identical to the
//! core. Ordinary keystrokes are deliberately NOT reported: they say nothing
//! about focus, and treating "recent typing" as intent blessed a fling once.
//!
//! The rescue chord is handled here, ahead of everything, so the kill switch
//! works even if the engine thread is wedged: on the second press within the
//! window it flips interception off (freeing the keyboard immediately),
//! re-associates the mouse (freeing the pointer), and signals the engine.

use std::cell::{Cell, RefCell};
use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::Sender;
use objc2_core_foundation::{kCFRunLoopCommonModes, CFMachPort, CFRetained, CFRunLoop};
use objc2_core_graphics::{
    CGEvent, CGEventField, CGEventFlags, CGEventMask, CGEventTapLocation, CGEventTapOptions,
    CGEventTapPlacement, CGEventType,
};

use ordo_core::{Gesture, Point};

use crate::engine::Msg;
use crate::keys::{self, Chord, Mods, Witness};

/// Two presses of the rescue chord within this window engage rescue.
const RESCUE_WINDOW: Duration = Duration::from_secs(2);

struct TapContext {
    tx: Sender<Msg>,
    intercepting: Arc<AtomicBool>,
    /// Set after the tap is created, so the callback can re-enable it.
    tap: RefCell<Option<CFRetained<CFMachPort>>>,
    last_rescue: Cell<Option<Instant>>,
    /// Cmd+Tab was pressed and Cmd is still held: the app switcher acts on the
    /// release, which is when the gesture is reported.
    app_switcher_armed: Cell<bool>,
}

/// Spawn the tap on its own thread with its own run loop. The thread runs until
/// the process exits. Returns immediately; if the tap can't be created (no
/// Accessibility permission), logs and the thread ends — Ordo still observes,
/// just without hotkeys.
pub fn spawn(tx: Sender<Msg>, intercepting: Arc<AtomicBool>) {
    std::thread::spawn(move || {
        let ctx = Box::into_raw(Box::new(TapContext {
            tx,
            intercepting,
            tap: RefCell::new(None),
            last_rescue: Cell::new(None),
            app_switcher_armed: Cell::new(false),
        }));

        // Mouse-downs and modifier changes are listened to, never altered;
        // one tap serves both because a second would double every event's
        // trip through this process.
        let mask: CGEventMask = (1 << CGEventType::KeyDown.0)
            | (1 << CGEventType::FlagsChanged.0)
            | (1 << CGEventType::LeftMouseDown.0)
            | (1 << CGEventType::RightMouseDown.0)
            | (1 << CGEventType::OtherMouseDown.0);
        let tap = unsafe {
            CGEvent::tap_create(
                CGEventTapLocation::SessionEventTap,
                CGEventTapPlacement::HeadInsertEventTap,
                CGEventTapOptions::Default,
                mask,
                Some(callback),
                ctx as *mut c_void,
            )
        };
        let Some(tap) = tap else {
            eprintln!("ordo: could not create event tap (grant Accessibility permission).");
            return;
        };

        let Some(source) = CFMachPort::new_run_loop_source(None, Some(&tap), 0) else {
            eprintln!("ordo: could not create run loop source for tap.");
            return;
        };
        if let Some(rl) = CFRunLoop::current() {
            unsafe { rl.add_source(Some(&source), kCFRunLoopCommonModes) };
        }
        CGEvent::tap_enable(&tap, true);
        // Hand the tap to the callback for self re-enable, then keep it alive.
        unsafe { (*ctx).tap.replace(Some(tap)) };

        CFRunLoop::run();
    });
}

unsafe extern "C-unwind" fn callback(
    _proxy: objc2_core_graphics::CGEventTapProxy,
    ty: CGEventType,
    event: NonNull<CGEvent>,
    userinfo: *mut c_void,
) -> *mut CGEvent {
    let ctx = &*(userinfo as *const TapContext);

    // The OS disables the tap under load or on timeout; turn it back on or
    // hotkeys quietly stop working.
    if ty == CGEventType::TapDisabledByTimeout || ty == CGEventType::TapDisabledByUserInput {
        if let Some(tap) = ctx.tap.borrow().as_ref() {
            CGEvent::tap_enable(tap, true);
        }
        return event.as_ptr();
    }

    let pass = event.as_ptr();
    let ev = event.as_ref();
    let flags = CGEvent::flags(Some(ev));
    let mods = Mods {
        cmd: flags.contains(CGEventFlags::MaskCommand),
        alt: flags.contains(CGEventFlags::MaskAlternate),
        shift: flags.contains(CGEventFlags::MaskShift),
        ctrl: flags.contains(CGEventFlags::MaskControl),
    };

    match ty {
        CGEventType::LeftMouseDown | CGEventType::RightMouseDown | CGEventType::OtherMouseDown => {
            let p = CGEvent::location(Some(ev));
            let _ = ctx.tx.send(Msg::Gesture(Gesture::MouseDown {
                at: Point { x: p.x, y: p.y },
            }));
            return pass;
        }
        CGEventType::FlagsChanged => {
            if !mods.cmd && ctx.app_switcher_armed.replace(false) {
                let _ = ctx.tx.send(Msg::Gesture(Gesture::SystemSwitch));
            }
            return pass;
        }
        CGEventType::KeyDown => {}
        _ => return pass,
    }

    let keycode = CGEvent::integer_value_field(Some(ev), CGEventField::KeyboardEventKeycode) as u16;

    // The engage chord is checked before the interception gate — its whole job
    // is to work while Ordo is disengaged (post-rescue, or a --paused start).
    // Everything else is only ours when intercepting; while disengaged, all
    // other keys (including our own hotkeys) belong to the apps again.
    match keys::match_chord(keycode, mods) {
        Some(Chord::Engage) => {
            // Flip the flag here, symmetric with rescue's fast path, so
            // engagement doesn't depend on the engine being responsive.
            ctx.intercepting.store(true, Ordering::Relaxed);
            let _ = ctx.tx.send(Msg::Engage);
            std::ptr::null_mut() // swallow
        }
        // O's corollary: identical fast path, the difference (blank model,
        // state file unused) is the engine's business.
        Some(Chord::EngageFresh) => {
            ctx.intercepting.store(true, Ordering::Relaxed);
            let _ = ctx.tx.send(Msg::EngageFresh);
            std::ptr::null_mut() // swallow
        }
        Some(chord) if ctx.intercepting.load(Ordering::Relaxed) => match chord {
            Chord::Hotkey(action) => {
                let _ = ctx.tx.send(Msg::Hotkey(action));
                std::ptr::null_mut()
            }
            Chord::RescueCandidate => {
                handle_rescue(ctx);
                std::ptr::null_mut()
            }
            Chord::SaveState => {
                let _ = ctx.tx.send(Msg::SaveState);
                std::ptr::null_mut()
            }
            Chord::Engage | Chord::EngageFresh => unreachable!(),
        },
        _ => {
            match keys::witness(keycode, mods) {
                Some(Witness::AppSwitcherArmed) => ctx.app_switcher_armed.set(true),
                Some(Witness::WindowCycle) => {
                    let _ = ctx.tx.send(Msg::Gesture(Gesture::SystemSwitch));
                }
                None => {}
            }
            pass
        }
    }
}

fn handle_rescue(ctx: &TapContext) {
    // Instant::now is a shell-side clock read — permitted outside the core.
    let now = Instant::now();
    let armed = ctx
        .last_rescue
        .get()
        .is_some_and(|t| now.duration_since(t) <= RESCUE_WINDOW);
    if armed {
        // Engage, minimally and immediately — don't depend on the engine.
        ctx.intercepting.store(false, Ordering::Relaxed);
        let _ = objc2_core_graphics::CGAssociateMouseAndMouseCursorPosition(true);
        let _ = ctx.tx.send(Msg::Rescue);
        ctx.last_rescue.set(None);
    } else {
        ctx.last_rescue.set(Some(now));
    }
}
