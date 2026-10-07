//! Ordo's settings: the panel the gear in its menu opens, and the choices
//! that outlast a launch.
//!
//! Hiding is kept in the user defaults and sent to the engine at launch and
//! on every change. Debug mode is not kept: it is off at every launch (see
//! [`crate::debug`]).

use std::cell::{Cell, OnceCell, RefCell};
use std::ptr::NonNull;

use block2::RcBlock;
use crossbeam_channel::Sender;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{define_class, msg_send, sel, DefinedClass, MainThreadOnly};
use objc2_app_kit::{
    NSButton, NSControlStateValueOff, NSControlStateValueOn, NSEvent, NSEventMask, NSFont,
    NSPopover,
    NSPopoverBehavior, NSPopoverDelegate, NSScreen, NSTextField, NSView, NSViewController,
};
use objc2_foundation::{
    MainThreadMarker, NSNotification, NSObject, NSObjectProtocol, NSPoint, NSRect, NSRectEdge,
    NSSize, NSString, NSUserDefaults,
};
use ordo_core::Rect;
use ordo_emulated::{HideWhen, Hiding, Idle};

use crate::engine::Msg;

use super::status_item::OwnMenu;

const HIDE_WHEN: &str = "HideApps";
const HIDE_IDLE: &str = "HideAppsWithNothingOn";

const WHEN: [(HideWhen, &str, &str); 3] = [
    (HideWhen::Never, "Never", "never"),
    (HideWhen::Settled, "Right after a switch", "settled"),
    (HideWhen::Delayed, "After 5 seconds on a workspace", "delayed"),
];

const IDLE: [(Idle, &str, &str); 2] = [
    (Idle::OffScreen, "None of its windows is on screen", "screen"),
    (Idle::OffWorkspace, "None is on this workspace, on any monitor", "workspace"),
];

/// The hiding setting as last saved, or today's behavior if never set.
pub fn saved_hiding() -> Hiding {
    let defaults = NSUserDefaults::standardUserDefaults();
    let read = |key: &str| {
        defaults
            .stringForKey(&NSString::from_str(key))
            .map(|s| s.to_string())
    };
    let saved = |key, names: &[&str]| read(key).and_then(|s| names.iter().position(|n| *n == s));
    let when_names = WHEN.map(|(_, _, name)| name);
    let idle_names = IDLE.map(|(_, _, name)| name);
    Hiding {
        when: saved(HIDE_WHEN, &when_names).map_or(HideWhen::default(), |i| WHEN[i].0),
        idle: saved(HIDE_IDLE, &idle_names).map_or(Idle::default(), |i| IDLE[i].0),
    }
}

fn save_hiding(hiding: Hiding) {
    let defaults = NSUserDefaults::standardUserDefaults();
    let when = WHEN.iter().find(|w| w.0 == hiding.when).map(|w| w.2);
    let idle = IDLE.iter().find(|i| i.0 == hiding.idle).map(|i| i.2);
    for (key, value) in [(HIDE_WHEN, when), (HIDE_IDLE, idle)] {
        let Some(value) = value else { continue };
        let value = NSString::from_str(value);
        let value: &AnyObject = &value;
        unsafe { defaults.setObject_forKey(Some(value), &NSString::from_str(key)) };
    }
}

pub struct PanelIvars {
    tx: Sender<Msg>,
    own: OwnMenu,
    hiding: Cell<Hiding>,
    popover: OnceCell<Retained<NSPopover>>,
    debug: OnceCell<Retained<NSButton>>,
    /// Watches for clicks in other apps while the panel is shown. A transient
    /// popover of an app that isn't active can miss them, and Ordo is never
    /// made active: that would take focus from the user's window.
    outside: RefCell<Option<Retained<AnyObject>>>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements, and SettingsPanel
    // does not implement Drop.
    #[unsafe(super = NSObject)]
    #[thread_kind = MainThreadOnly]
    #[name = "OrdoSettingsPanel"]
    #[ivars = PanelIvars]
    pub struct SettingsPanel;

    impl SettingsPanel {
        // SAFETY: each signature matches its action selector's.
        #[unsafe(method(toggleDebug:))]
        fn toggle_debug(&self, sender: &NSButton) {
            crate::debug::set(sender.state() == NSControlStateValueOn);
        }

        #[unsafe(method(pickWhen:))]
        fn pick_when(&self, sender: &NSButton) {
            let when = WHEN[sender.tag() as usize].0;
            self.change(Hiding { when, ..self.ivars().hiding.get() });
        }

        #[unsafe(method(pickIdle:))]
        fn pick_idle(&self, sender: &NSButton) {
            let idle = IDLE[sender.tag() as usize].0;
            self.change(Hiding { idle, ..self.ivars().hiding.get() });
        }
    }

    unsafe impl NSObjectProtocol for SettingsPanel {}

    unsafe impl NSPopoverDelegate for SettingsPanel {
        #[unsafe(method(popoverDidClose:))]
        fn popover_did_close(&self, _notification: &NSNotification) {
            if let Some(monitor) = self.ivars().outside.borrow_mut().take() {
                unsafe { NSEvent::removeMonitor(&monitor) };
            }
            self.ivars().own.set_panel(&self.ivars().tx, None);
        }
    }
);

const WIDTH: f64 = 320.0;
const PAD: f64 = 14.0;
const ROW: f64 = 22.0;
const GAP: f64 = 10.0;

impl SettingsPanel {
    pub fn new(tx: Sender<Msg>, own: OwnMenu, mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(PanelIvars {
            tx,
            own,
            hiding: Cell::new(saved_hiding()),
            popover: OnceCell::new(),
            debug: OnceCell::new(),
            outside: RefCell::new(None),
        });
        unsafe { msg_send![super(this), init] }
    }

    /// Shown under `anchor`, the menu bar item.
    pub fn show(&self, anchor: &NSView) {
        let popover = self.ivars().popover.get_or_init(|| self.build());
        if let Some(debug) = self.ivars().debug.get() {
            debug.setState(if crate::debug::enabled() {
                NSControlStateValueOn
            } else {
                NSControlStateValueOff
            });
        }
        popover.showRelativeToRect_ofView_preferredEdge(anchor.bounds(), anchor, NSRectEdge::MinY);
        let mut outside = self.ivars().outside.borrow_mut();
        if outside.is_none() {
            let target = popover.clone();
            let close = RcBlock::new(move |_event: NonNull<NSEvent>| target.close());
            *outside = NSEvent::addGlobalMonitorForEventsMatchingMask_handler(
                NSEventMask::LeftMouseDown | NSEventMask::RightMouseDown | NSEventMask::OtherMouseDown,
                &close,
            );
        }
        drop(outside);
        let frame = popover
            .contentViewController()
            .and_then(|c| c.view().window())
            .map(|w| w.frame());
        if let Some(frame) = frame {
            let mtm = self.mtm();
            // Cocoa's y runs up from the bottom of the first screen; the tap's
            // runs down from its top.
            let primary_h = NSScreen::screens(mtm)
                .iter()
                .next()
                .map_or(0.0, |s| s.frame().size.height);
            let cg = Rect {
                x: frame.origin.x,
                y: primary_h - (frame.origin.y + frame.size.height),
                w: frame.size.width,
                h: frame.size.height,
            };
            self.ivars().own.set_panel(&self.ivars().tx, Some(cg));
        }
    }

    fn change(&self, hiding: Hiding) {
        self.ivars().hiding.set(hiding);
        save_hiding(hiding);
        let _ = self.ivars().tx.send(Msg::Hiding(hiding));
    }

    fn build(&self) -> Retained<NSPopover> {
        let mtm = self.mtm();
        let target: &AnyObject = self;
        let hiding = self.ivars().hiding.get();
        // Laid out top down; Cocoa's y runs up, so each row's y is taken from
        // the height once every row is known.
        let mut rows: Vec<(Retained<NSView>, f64, f64)> = Vec::new();
        let mut y = PAD;
        let mut add = |view: Retained<NSView>, indent: f64, gap_before: f64, y: &mut f64| {
            *y += gap_before;
            rows.push((view, indent, *y));
            *y += ROW;
        };

        let debug = unsafe {
            NSButton::checkboxWithTitle_target_action(
                &NSString::from_str("Debug mode"),
                Some(target),
                Some(sel!(toggleDebug:)),
                mtm,
            )
        };
        debug.setToolTip(Some(&NSString::from_str(
            "Logs the stacking order at each step of a switch. Costs a few ms per switch; off at every launch.",
        )));
        add(Retained::into_super(Retained::into_super(debug.clone())), 0.0, 0.0, &mut y);
        let _ = self.ivars().debug.set(debug);

        add(heading("Hide apps with nothing to show", mtm), 0.0, GAP, &mut y);
        for (i, (when, title, _)) in WHEN.iter().enumerate() {
            let radio = radio(title, i, sel!(pickWhen:), target, *when == hiding.when, mtm);
            add(radio, 18.0, 0.0, &mut y);
        }
        add(heading("An app has nothing to show when", mtm), 0.0, GAP, &mut y);
        for (i, (idle, title, _)) in IDLE.iter().enumerate() {
            let radio = radio(title, i, sel!(pickIdle:), target, *idle == hiding.idle, mtm);
            add(radio, 18.0, 0.0, &mut y);
        }
        let height = y + PAD;

        let content = NSView::initWithFrame(
            NSView::alloc(mtm),
            NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(WIDTH, height)),
        );
        for (view, indent, top) in rows {
            view.setFrame(NSRect::new(
                NSPoint::new(PAD + indent, height - top - ROW),
                NSSize::new(WIDTH - 2.0 * PAD - indent, ROW),
            ));
            content.addSubview(&view);
        }
        let controller = NSViewController::new(mtm);
        controller.setView(&content);
        let popover = NSPopover::new(mtm);
        popover.setBehavior(NSPopoverBehavior::Transient);
        popover.setContentSize(NSSize::new(WIDTH, height));
        popover.setContentViewController(Some(&controller));
        popover.setDelegate(Some(ProtocolObject::from_ref(self)));
        popover
    }
}

fn heading(title: &str, mtm: MainThreadMarker) -> Retained<NSView> {
    let label = NSTextField::labelWithString(&NSString::from_str(title), mtm);
    label.setFont(Some(&NSFont::boldSystemFontOfSize(13.0)));
    Retained::into_super(Retained::into_super(label))
}

/// Radios sharing a superview and an action are one group, which is what
/// keeps the two settings' radios apart.
fn radio(
    title: &str,
    tag: usize,
    action: objc2::runtime::Sel,
    target: &AnyObject,
    on: bool,
    mtm: MainThreadMarker,
) -> Retained<NSView> {
    let radio = unsafe {
        NSButton::radioButtonWithTitle_target_action(
            &NSString::from_str(title),
            Some(target),
            Some(action),
            mtm,
        )
    };
    radio.setTag(tag as isize);
    radio.setState(if on { NSControlStateValueOn } else { NSControlStateValueOff });
    Retained::into_super(Retained::into_super(radio))
}
