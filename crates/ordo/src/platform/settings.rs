//! Ordo's settings: the submenu and the choices that outlast a launch.
//!
//! Hiding is kept in the user defaults and sent to the engine at launch and
//! on every change. Debug mode is not kept: it is off at every launch (see
//! [`crate::debug`]).

use std::cell::Cell;

use crossbeam_channel::Sender;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Sel};
use objc2::{define_class, msg_send, sel, DefinedClass, MainThreadOnly};
use objc2_app_kit::{
    NSControlStateValueOff, NSControlStateValueOn, NSImage, NSMenu, NSMenuItem,
};
use objc2_foundation::{ns_string, MainThreadMarker, NSObject, NSObjectProtocol, NSString, NSUserDefaults};
use ordo_emulated::{HideWhen, Hiding, Idle};

use crate::engine::Msg;

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

pub struct Ivars {
    tx: Sender<Msg>,
    hiding: Cell<Hiding>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements, and Settings does
    // not implement Drop.
    #[unsafe(super = NSObject)]
    #[thread_kind = MainThreadOnly]
    #[name = "OrdoSettings"]
    #[ivars = Ivars]
    pub struct Settings;

    impl Settings {
        // SAFETY: each signature matches its action selector's.
        #[unsafe(method(toggleDebug:))]
        fn toggle_debug(&self, _sender: &NSMenuItem) {
            crate::debug::set(!crate::debug::enabled());
        }

        #[unsafe(method(pickWhen:))]
        fn pick_when(&self, sender: &NSMenuItem) {
            let when = WHEN[sender.tag() as usize].0;
            self.change(Hiding { when, ..self.ivars().hiding.get() });
        }

        #[unsafe(method(pickIdle:))]
        fn pick_idle(&self, sender: &NSMenuItem) {
            let idle = IDLE[sender.tag() as usize].0;
            self.change(Hiding { idle, ..self.ivars().hiding.get() });
        }
    }

    unsafe impl NSObjectProtocol for Settings {}
);

impl Settings {
    pub fn new(tx: Sender<Msg>, mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(Ivars {
            tx,
            hiding: Cell::new(saved_hiding()),
        });
        unsafe { msg_send![super(this), init] }
    }

    /// The "Settings" item and its submenu, as things stand now. Built each
    /// time Ordo's menu opens, so the checkmarks are current.
    pub fn item(&self) -> Retained<NSMenuItem> {
        let mtm = self.mtm();
        let hiding = self.ivars().hiding.get();
        let menu = NSMenu::new(mtm);
        menu.setAutoenablesItems(false);

        let debug = self.choice("Debug mode", sel!(toggleDebug:), 0, crate::debug::enabled());
        debug.setToolTip(Some(ns_string!(
            "Logs the stacking order at each step of a switch. Costs a few ms per switch; off at every launch."
        )));
        menu.addItem(&debug);

        menu.addItem(&NSMenuItem::separatorItem(mtm));
        menu.addItem(&NSMenuItem::sectionHeaderWithTitle(
            ns_string!("Hide apps with nothing to show"),
            mtm,
        ));
        for (i, (when, title, _)) in WHEN.iter().enumerate() {
            menu.addItem(&self.choice(title, sel!(pickWhen:), i, *when == hiding.when));
        }

        menu.addItem(&NSMenuItem::separatorItem(mtm));
        menu.addItem(&NSMenuItem::sectionHeaderWithTitle(
            ns_string!("An app has nothing to show when"),
            mtm,
        ));
        for (i, (idle, title, _)) in IDLE.iter().enumerate() {
            menu.addItem(&self.choice(title, sel!(pickIdle:), i, *idle == hiding.idle));
        }

        let item = NSMenuItem::new(mtm);
        item.setTitle(ns_string!("Settings"));
        item.setImage(
            NSImage::imageWithSystemSymbolName_accessibilityDescription(
                ns_string!("gearshape"),
                Some(ns_string!("Settings")),
            )
            .as_deref(),
        );
        item.setSubmenu(Some(&menu));
        item
    }

    fn choice(&self, title: &str, action: Sel, tag: usize, on: bool) -> Retained<NSMenuItem> {
        let item = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(self.mtm()),
                &NSString::from_str(title),
                Some(action),
                ns_string!(""),
            )
        };
        unsafe { item.setTarget(Some(self)) };
        item.setTag(tag as isize);
        item.setState(if on { NSControlStateValueOn } else { NSControlStateValueOff });
        item
    }

    fn change(&self, hiding: Hiding) {
        self.ivars().hiding.set(hiding);
        save_hiding(hiding);
        let _ = self.ivars().tx.send(Msg::Hiding(hiding));
    }
}
