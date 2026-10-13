//! Ordo's settings: the submenu and the choices that outlast a launch.
//!
//! Hiding is kept in the user defaults and sent to the engine at launch and
//! on every change. Debug mode is not kept: it is off at every launch (see
//! [`crate::debug`]).

use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};

use crossbeam_channel::Sender;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{define_class, msg_send, AnyThread, DefinedClass, MainThreadOnly};
use objc2_app_kit::{
    NSAttributedStringNSStringDrawing, NSAutoresizingMaskOptions, NSBezierPath, NSColor, NSEvent,
    NSFont, NSFontAttributeName, NSForegroundColorAttributeName, NSImage, NSMenu, NSMenuItem,
    NSView,
};
use objc2_foundation::{
    ns_string, MainThreadMarker, NSAttributedString, NSDictionary, NSPoint, NSRect, NSSize,
    NSString, NSUserDefaults,
};
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

/// One of the submenu's choices.
#[derive(Clone, Copy, PartialEq)]
enum Choice {
    Debug,
    When(HideWhen),
    Idle(Idle),
}

/// The choices as they stand, and the rows of the submenu last built.
pub struct Settings {
    tx: Sender<Msg>,
    hiding: Cell<Hiding>,
    rows: RefCell<Vec<Retained<ChoiceRow>>>,
}

impl Settings {
    pub fn new(tx: Sender<Msg>) -> Rc<Self> {
        Rc::new(Settings {
            tx,
            hiding: Cell::new(saved_hiding()),
            rows: RefCell::new(Vec::new()),
        })
    }

    /// The "Settings" item and its submenu, as things stand now. Built each
    /// time Ordo's menu opens, so the checkmarks are current.
    pub fn item(self: &Rc<Self>, mtm: MainThreadMarker) -> Retained<NSMenuItem> {
        let menu = NSMenu::new(mtm);
        menu.setAutoenablesItems(false);
        let mut rows = Vec::new();
        let mut add = |choice, title: &str| {
            let row = ChoiceRow::new(Rc::downgrade(self), choice, title, mtm);
            let item = NSMenuItem::new(mtm);
            item.setView(Some(&row));
            menu.addItem(&item);
            rows.push(row.clone());
            row
        };

        add(Choice::Debug, "Debug mode").setToolTip(Some(ns_string!(
            "Logs the stacking order at each step of a switch. Costs a few ms per switch; off at every launch."
        )));
        menu.addItem(&NSMenuItem::separatorItem(mtm));
        menu.addItem(&NSMenuItem::sectionHeaderWithTitle(
            ns_string!("Hide apps with nothing to show"),
            mtm,
        ));
        for (when, title, _) in WHEN {
            add(Choice::When(when), title);
        }
        menu.addItem(&NSMenuItem::separatorItem(mtm));
        menu.addItem(&NSMenuItem::sectionHeaderWithTitle(
            ns_string!("An app has nothing to show when"),
            mtm,
        ));
        for (idle, title, _) in IDLE {
            add(Choice::Idle(idle), title);
        }
        *self.rows.borrow_mut() = rows;
        self.refresh();

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

    fn is_on(&self, choice: Choice) -> bool {
        let hiding = self.hiding.get();
        match choice {
            Choice::Debug => crate::debug::enabled(),
            Choice::When(when) => when == hiding.when,
            Choice::Idle(idle) => idle == hiding.idle,
        }
    }

    fn pick(&self, choice: Choice) {
        let hiding = self.hiding.get();
        match choice {
            Choice::Debug => crate::debug::set(!crate::debug::enabled()),
            Choice::When(when) => self.change(Hiding { when, ..hiding }),
            Choice::Idle(idle) => self.change(Hiding { idle, ..hiding }),
        }
        self.refresh();
    }

    fn change(&self, hiding: Hiding) {
        self.hiding.set(hiding);
        save_hiding(hiding);
        let _ = self.tx.send(Msg::Hiding(hiding));
    }

    /// Every row's check, so a pick moves its group's at once.
    fn refresh(&self) {
        for row in self.rows.borrow().iter() {
            let on = self.is_on(row.ivars().choice);
            if row.ivars().on.replace(on) != on {
                // Now: a menu tracking the mouse may not reach the end of a
                // run loop pass, where a deferred redraw would happen.
                row.setNeedsDisplay(true);
                row.displayIfNeeded();
            }
        }
    }
}

const ROW_H: f64 = 22.0;
/// The highlight's inset from the menu's edges, as macOS draws its own.
const INSET_X: f64 = 5.0;
const HIGHLIGHT_R: f64 = 4.0;
const CHECK_X: f64 = 10.0;
const TITLE_X: f64 = 24.0;
const PAD_RIGHT: f64 = 20.0;

pub struct RowIvars {
    settings: Weak<Settings>,
    choice: Choice,
    title: String,
    on: Cell<bool>,
}

define_class!(
    // SAFETY: NSView has no subclassing requirements beyond these overrides,
    // and ChoiceRow does not implement Drop.
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "OrdoSettingsChoiceRow"]
    #[ivars = RowIvars]
    pub struct ChoiceRow;

    impl ChoiceRow {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        // The menu redraws a view item as its highlight changes, the pointer
        // or the arrow keys moving over it.
        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty: NSRect) {
            let lit = self.enclosingMenuItem().is_some_and(|item| item.isHighlighted());
            let width = self.bounds().size.width;
            if lit {
                let h = NSRect::new(
                    NSPoint::new(INSET_X, 0.0),
                    NSSize::new(width - 2.0 * INSET_X, ROW_H),
                );
                NSColor::selectedContentBackgroundColor().setFill();
                NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(h, HIGHLIGHT_R, HIGHLIGHT_R)
                    .fill();
            }
            let ink = if lit {
                NSColor::selectedMenuItemTextColor()
            } else {
                NSColor::labelColor()
            };
            let font = NSFont::menuFontOfSize(0.0);
            let ivars = self.ivars();
            if ivars.on.get() {
                text("✓", &font, &ink).drawAtPoint(NSPoint::new(CHECK_X, 3.0));
            }
            text(&ivars.title, &font, &ink).drawAtPoint(NSPoint::new(TITLE_X, 3.0));
        }

        // Kept by the row, so the press stays with the view its release
        // comes to.
        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, _event: &NSEvent) {}

        // A view item's click is the view's own, and the menu stays open
        // through it: what lets several settings be changed in a row.
        #[unsafe(method(mouseUp:))]
        fn mouse_up(&self, _event: &NSEvent) {
            if let Some(settings) = self.ivars().settings.upgrade() {
                settings.pick(self.ivars().choice);
            }
        }
    }
);

impl ChoiceRow {
    fn new(settings: Weak<Settings>, choice: Choice, title: &str, mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(RowIvars {
            settings,
            choice,
            title: title.to_string(),
            on: Cell::new(false),
        });
        let width = TITLE_X
            + text(title, &NSFont::menuFontOfSize(0.0), &NSColor::labelColor())
                .size()
                .width
            + PAD_RIGHT;
        let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(width.ceil(), ROW_H));
        let this: Retained<Self> = unsafe { msg_send![super(this), initWithFrame: frame] };
        this.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable);
        this
    }
}

fn text(s: &str, font: &NSFont, color: &NSColor) -> Retained<NSAttributedString> {
    let attrs = NSDictionary::from_slices(
        unsafe { &[NSFontAttributeName, NSForegroundColorAttributeName] },
        &[&**font as &AnyObject, &**color],
    );
    unsafe {
        NSAttributedString::initWithString_attributes(
            NSAttributedString::alloc(),
            &NSString::from_str(s),
            Some(&attrs),
        )
    }
}
