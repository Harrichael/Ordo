//! The global hotkey tap — and the intent channel.
//!
//! A `CGEventTap` on its own thread watches key-downs, matches them against the
//! (pure) [`crate::keys`] table, and for a hit posts a [`Msg`] to the engine and
//! swallows the key. Two hard rules from the research shape this:
//!   - the callback must never block — it does no AX work, only a channel send
//!     (and, for a mouse-down, a lookup well under a millisecond);
//!   - the tap gets disabled by the OS on timeout or heavy user input, and must
//!     re-enable itself, or hotkeys silently die after a while.
//!
//! The same tap WITNESSES the user's focus gestures Ordo does not own — every
//! mouse-down (telling menu bar and Dock clicks apart), and macOS's Cmd+Tab /
//! Cmd+` — and reports them as [`Msg::Gesture`] while passing the event
//! through untouched. Without that trace, a click and a notification stealing
//! focus look identical to the core. Every other key that reaches an app is
//! reported as a bare [`Gesture::Key`] (no key, at most one per interval, and
//! nothing while Ordo is paused or rescued; see [`keys::KeyWitness`]): the
//! core lets it explain only the focus change right after it within the app
//! typed into, and never a follow onto a hidden workspace.
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

use objc2_app_kit::NSRunningApplication;
use objc2_foundation::NSString;
use ordo_core::{Gesture, Point, Rect, WindowId};

use crate::engine::Msg;
use crate::keys::{self, Chord, Mods, Witness};
use crate::platform::display::{self, MenuBars};
use crate::platform::status_item::OwnMenu;
use crate::platform::zorder;

/// Two presses of the rescue chord within this window engage rescue.
const RESCUE_WINDOW: Duration = Duration::from_secs(2);

struct TapContext {
    tx: Sender<Msg>,
    intercepting: Arc<AtomicBool>,
    menu_bars: MenuBars,
    own_menu: OwnMenu,
    /// Set after the tap is created, so the callback can re-enable it.
    tap: RefCell<Option<CFRetained<CFMachPort>>>,
    last_rescue: Cell<Option<Instant>>,
    /// Cmd+Tab was pressed and Cmd is still held: the app switcher acts on the
    /// release, which is when the gesture is reported.
    app_switcher_armed: Cell<bool>,
    keys: RefCell<keys::KeyWitness>,
    dock: DockWitness,
}

/// Spawn the tap on its own thread with its own run loop. The thread runs until
/// the process exits. Returns immediately; if the tap can't be created (no
/// Accessibility permission), logs and the thread ends — Ordo still observes,
/// just without hotkeys.
pub fn spawn(tx: Sender<Msg>, intercepting: Arc<AtomicBool>, menu_bars: MenuBars, own_menu: OwnMenu) {
    std::thread::spawn(move || {
        let ctx = Box::into_raw(Box::new(TapContext {
            tx,
            intercepting,
            menu_bars,
            own_menu,
            tap: RefCell::new(None),
            last_rescue: Cell::new(None),
            app_switcher_armed: Cell::new(false),
            keys: RefCell::new(keys::KeyWitness::default()),
            dock: DockWitness::new(),
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
            // A click in Ordo's own menu or settings is no gesture on the
            // world: read as one, it explained an app's re-key onto a hidden
            // workspace, and the core followed it there.
            let p = CGEvent::location(Some(ev));
            let at = Point { x: p.x, y: p.y };
            if ctx.own_menu.takes(at) {
                return pass;
            }
            let gesture = if ctx.menu_bars.contains(at) {
                Gesture::MenuBar { at }
            } else if ctx.dock.takes(ev, at) {
                Gesture::Dock { at }
            } else {
                Gesture::MouseDown { at }
            };
            let _ = ctx.tx.send(Msg::Gesture(gesture));
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
    let typed = ctx.keys.borrow_mut().report(
        keycode,
        mods,
        ctx.intercepting.load(Ordering::Relaxed),
        Instant::now(),
    );

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
                let _ = ctx.tx.send(Msg::hotkey(action));
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
                None if typed => {
                    let _ = ctx.tx.send(Msg::Gesture(Gesture::Key));
                }
                None => {}
            }
            pass
        }
    }
}

/// Tells the clicks the window server routes to the Dock (its icons, their
/// menus, Mission Control) from the rest. Asked of the event, not of a point:
/// the Dock hides, and sits on any edge of any display. About 0.1 ms per
/// click, and no AppKit: one window described by id, and at most a syscall
/// or two to check its owner.
struct DockWitness {
    pid: Cell<Option<i32>>,
    reported: Cell<u32>,
    refused: Cell<u32>,
}

/// Where the system keeps the Dock: what a process is checked against when
/// the Dock may have restarted, since that check runs on the event path.
const DOCK_PATH: &[u8] = b"/System/Library/CoreServices/Dock.app/Contents/MacOS/Dock";

/// kCGDockWindowLevel: the layer of the window the Dock draws its icons in.
const DOCK_LAYER: i32 = 20;

/// The icon strip's depth from its edge, at most: the largest tile the Dock's
/// settings offer (128 pt), with room for its margin.
const DOCK_STRIP: f64 = 160.0;

/// Dock clicks, and refusals, reported on stderr per run: enough to see on
/// one install whether the window server's routing behaves.
const DOCK_REPORTS: u32 = 20;

impl DockWitness {
    fn new() -> Self {
        // Once, at tap start, so no AppKit runs in the callback.
        let bundle = NSString::from_str("com.apple.dock");
        let pid = NSRunningApplication::runningApplicationsWithBundleIdentifier(&bundle)
            .iter()
            .next()
            .map(|a| a.processIdentifier());
        DockWitness {
            pid: Cell::new(pid),
            reported: Cell::new(0),
            refused: Cell::new(0),
        }
    }

    fn takes(&self, ev: &CGEvent, at: Point) -> bool {
        let field = CGEventField::MouseEventWindowUnderMousePointerThatCanHandleThisEvent;
        let w = CGEvent::integer_value_field(Some(ev), field);
        if w <= 0 {
            return false;
        }
        let Some(d) = zorder::describe_one(WindowId(w as u32)) else {
            return false;
        };
        if !self.is_dock(d.pid) {
            return false;
        }
        let displays: Vec<Rect> = display::active_displays().into_iter().map(|x| x.frame).collect();
        let takes = takes_click(d.layer, d.bounds, &displays, at);
        let (count, verdict) = if takes {
            (&self.reported, "dock click")
        } else {
            (&self.refused, "dock backdrop click refused")
        };
        if count.get() < DOCK_REPORTS {
            count.set(count.get() + 1);
            eprintln!(
                "ordo: {verdict} at ({:.0}, {:.0}) window {} layer {:?} bounds {:?}",
                at.x, at.y, d.id.0, d.layer, d.bounds
            );
        }
        takes
    }

    fn is_dock(&self, pid: i32) -> bool {
        let known = self.pid.get();
        if known == Some(pid) {
            return true;
        }
        if known.is_some_and(alive) {
            return false;
        }
        // The Dock restarted, or was not found at start.
        let dock = runs_dock(pid);
        if dock {
            self.pid.set(Some(pid));
        }
        dock
    }
}

/// Whether a click the window server routed to this Dock window is the
/// Dock's. Its icons are drawn in a window at `DOCK_LAYER` spanning its whole
/// display, see-through beyond them. Should the routing name that window for
/// a click that falls through to a window beneath, taking every such click
/// would arm every click on the display, so only one near an edge the Dock
/// can sit on is taken. Its menus are windows of their own, and whatever else
/// display-wide it shows (Mission Control) is taken to sit at another layer.
fn takes_click(layer: Option<i32>, bounds: Rect, displays: &[Rect], at: Point) -> bool {
    let backdrop = layer == Some(DOCK_LAYER) && displays.iter().any(|f| f.approx_eq(&bounds, 1.0));
    if !backdrop {
        return true;
    }
    at.y >= bounds.y + bounds.h - DOCK_STRIP
        || at.x < bounds.x + DOCK_STRIP
        || at.x >= bounds.x + bounds.w - DOCK_STRIP
}

fn alive(pid: i32) -> bool {
    unsafe { libc::kill(pid, 0) == 0 }
}

fn runs_dock(pid: i32) -> bool {
    let mut path = [0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    let n = unsafe { libc::proc_pidpath(pid, path.as_mut_ptr().cast(), path.len() as u32) };
    n > 0 && &path[..n as usize] == DOCK_PATH
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The guard on the Dock's display-wide icon window. Should the window
    /// server name it for every click over it, only clicks where its icons
    /// can be are taken; its menus, and display-wide windows at other layers
    /// (Mission Control), are taken anywhere.
    #[test]
    fn only_the_dock_backdrops_edges_are_taken_for_the_dock() {
        let display = Rect { x: 0.0, y: 0.0, w: 1920.0, h: 1080.0 };
        let displays = [display, Rect { x: 1920.0, ..display }];
        let backdrop = |x, y| takes_click(Some(DOCK_LAYER), display, &displays, Point { x, y });
        assert!(backdrop(1053.0, 1050.0), "an icon at the bottom");
        assert!(backdrop(30.0, 500.0), "an icon on a left-hand Dock");
        assert!(!backdrop(960.0, 500.0), "the middle of the display");

        let menu = Rect { x: 1000.0, y: 700.0, w: 240.0, h: 300.0 };
        assert!(takes_click(Some(101), menu, &displays, Point { x: 1120.0, y: 850.0 }));
        assert!(takes_click(Some(1000), display, &displays, Point { x: 960.0, y: 500.0 }));
    }
}
