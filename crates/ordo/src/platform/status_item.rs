//! Ordo's menu bar item: one mark per workspace — the current one a pill
//! with its number, the rest dots, solid where windows live and hollow where
//! none do — and a menu that switches workspace on a pick.
//!
//! The engine thread owns the model; AppKit owns the main thread. They meet
//! in a mailbox: [`MenuBar::show`] (any thread) leaves the newest view there
//! and wakes the main queue to redraw the icon. The menu is built only as it
//! opens (`menuNeedsUpdate:`), so a rescan landing while it is open never
//! reshuffles the rows under the pointer.
//!
//! A pick reaches the engine as the same `WorkspaceSwitchTo` that Cmd+Alt+digit
//! mints: the menu is a second keyboard, not a second decision path.

use std::cell::OnceCell;
use std::sync::{Arc, Mutex};

use block2::RcBlock;
use crossbeam_channel::Sender;
use dispatch2::DispatchQueue;
use objc2::rc::Retained;
use objc2::runtime::{Bool, ProtocolObject};
use objc2::{define_class, msg_send, sel, AnyThread, DefinedClass, MainThreadOnly};
use objc2_app_kit::{
    NSAccessibility, NSAttributedStringNSStringDrawing, NSBezierPath, NSColor,
    NSCompositingOperation, NSControlStateValueOff, NSControlStateValueOn, NSEventModifierFlags,
    NSFont, NSFontAttributeName, NSFontWeightBold, NSForegroundColorAttributeName,
    NSGraphicsContext, NSImage, NSMenu, NSMenuDelegate, NSMenuItem, NSRunningApplication,
    NSStatusBar, NSStatusItem, NSVariableStatusItemLength,
};
use objc2_foundation::{
    ns_string, MainThreadMarker, NSAttributedString, NSDictionary, NSObject, NSObjectProtocol,
    NSPoint, NSRect, NSSize, NSString,
};
use ordo_core::{HotkeyAction, WorkspaceId};

use crate::engine::Msg;
use crate::menubar::MenuBarView;

/// Image height; the status bar centers it vertically.
const HEIGHT: f64 = 16.0;
const DOT: f64 = 6.0;
const RING_LINE: f64 = 1.0;
const PILL_H: f64 = 14.0;
/// Wider than tall even for one digit, so the pill never reads as a big dot.
const PILL_MIN_W: f64 = 17.0;
const PILL_PAD: f64 = 5.0;
const GAP: f64 = 4.0;
/// App names a menu row spells out before summarizing the rest as "+N".
const NAMED_APPS: usize = 3;

struct Mailbox {
    latest: Mutex<Option<MenuBarView>>,
    tx: Sender<Msg>,
}

/// The engine's handle to the menu bar. Cheap to clone, safe to call from
/// any thread.
#[derive(Clone)]
pub struct MenuBar {
    mailbox: Arc<Mailbox>,
}

impl MenuBar {
    /// Redraws only when the view actually changed, which at rescan cadence is
    /// almost never.
    pub fn show(&self, view: MenuBarView) {
        {
            let mut latest = self.mailbox.latest.lock().unwrap();
            if latest.as_ref() == Some(&view) {
                return;
            }
            *latest = Some(view);
        }
        let mailbox = self.mailbox.clone();
        DispatchQueue::main().exec_async(move || redraw(&mailbox));
    }
}

/// Nothing appears until the first view arrives: an item that showed a
/// guess before the first snapshot would be the menu bar lying.
pub fn install(tx: Sender<Msg>) -> MenuBar {
    MenuBar {
        mailbox: Arc::new(Mailbox {
            latest: Mutex::new(None),
            tx,
        }),
    }
}

struct Ui {
    item: Retained<NSStatusItem>,
    /// Owned here because the menu holds its delegate weakly.
    _controller: Retained<Controller>,
}

thread_local! {
    // Main thread only: created by the first redraw, alive for the process.
    static UI: OnceCell<Ui> = const { OnceCell::new() };
}

fn redraw(mailbox: &Arc<Mailbox>) {
    let mtm = MainThreadMarker::new().expect("redraws run on the main queue");
    let Some(view) = mailbox.latest.lock().unwrap().clone() else {
        return;
    };
    UI.with(|ui| {
        let ui = ui.get_or_init(|| Ui::new(mailbox.clone(), mtm));
        let Some(button) = ui.item.button(mtm) else {
            return;
        };
        button.setImage(Some(&icon(&view)));
        // Dimmed, the way macOS marks an item that is present but inert.
        button.setAppearsDisabled(!view.engaged);
        let summary = NSString::from_str(&summary(&view));
        button.setToolTip(Some(&summary));
        button.setAccessibilityLabel(Some(&summary));
    });
}

impl Ui {
    fn new(mailbox: Arc<Mailbox>, mtm: MainThreadMarker) -> Self {
        let item = NSStatusBar::systemStatusBar().statusItemWithLength(NSVariableStatusItemLength);
        // Keeps a Cmd-drag rearrangement of the menu bar across restarts.
        item.setAutosaveName(Some(ns_string!("ordo.workspaces")));
        let controller = Controller::new(mailbox, mtm);
        let menu = NSMenu::new(mtm);
        menu.setAutoenablesItems(false);
        menu.setDelegate(Some(ProtocolObject::from_ref(&*controller)));
        item.setMenu(Some(&menu));
        Ui {
            item,
            _controller: controller,
        }
    }
}

fn summary(view: &MenuBarView) -> String {
    let count = view.workspaces.len();
    let mut s = match view.current {
        Some(ws) => format!("Ordo — workspace {} of {count}", ws.0),
        None => format!("Ordo — {count} workspaces"),
    };
    if !view.engaged {
        s.push_str(" (paused)");
    }
    s
}

// --- the icon ----------------------------------------------------------------

enum Mark {
    Current(Retained<NSAttributedString>),
    Occupied,
    Empty,
}

impl Mark {
    fn width(&self) -> f64 {
        match self {
            // Whole points, so every mark after the pill still lands on the
            // pixel grid of a 1x display.
            Mark::Current(label) => (label.size().width + 2.0 * PILL_PAD).max(PILL_MIN_W).ceil(),
            Mark::Occupied | Mark::Empty => DOT,
        }
    }

    fn draw(&self, x: f64) {
        NSColor::blackColor().setFill();
        NSColor::blackColor().setStroke();
        match self {
            Mark::Current(label) => {
                let w = self.width();
                let pill = NSRect::new(
                    NSPoint::new(x, (HEIGHT - PILL_H) / 2.0),
                    NSSize::new(w, PILL_H),
                );
                NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(
                    pill,
                    PILL_H / 2.0,
                    PILL_H / 2.0,
                )
                .fill();
                // Punched out rather than drawn in white: the icon is a
                // template, so only alpha survives, and the hole is what lets
                // the menu bar's own color show through as the digit.
                let Some(ctx) = NSGraphicsContext::currentContext() else {
                    return;
                };
                ctx.setCompositingOperation(NSCompositingOperation::DestinationOut);
                let font = label_font();
                let size = label.size();
                // Centered on cap height, not line height: digits have no
                // descenders, and line-box centering sits them visibly high.
                let baseline = HEIGHT / 2.0 - font.capHeight() / 2.0;
                label.drawAtPoint(NSPoint::new(
                    x + (w - size.width) / 2.0,
                    baseline + font.descender(),
                ));
                ctx.setCompositingOperation(NSCompositingOperation::SourceOver);
            }
            Mark::Occupied => {
                NSBezierPath::bezierPathWithOvalInRect(dot_rect(x, 0.0)).fill();
            }
            Mark::Empty => {
                let ring = NSBezierPath::bezierPathWithOvalInRect(dot_rect(x, RING_LINE / 2.0));
                ring.setLineWidth(RING_LINE);
                ring.stroke();
            }
        }
    }
}

fn dot_rect(x: f64, inset: f64) -> NSRect {
    NSRect::new(
        NSPoint::new(x + inset, (HEIGHT - DOT) / 2.0 + inset),
        NSSize::new(DOT - 2.0 * inset, DOT - 2.0 * inset),
    )
}

fn label_font() -> Retained<NSFont> {
    NSFont::monospacedDigitSystemFontOfSize_weight(11.0, unsafe { NSFontWeightBold })
}

fn attributed(text: &str, font: &NSFont, color: &NSColor) -> Retained<NSAttributedString> {
    let attrs = NSDictionary::from_slices(
        unsafe { &[NSFontAttributeName, NSForegroundColorAttributeName] },
        &[&**font as &objc2::runtime::AnyObject, &**color],
    );
    unsafe {
        NSAttributedString::initWithString_attributes(
            NSAttributedString::alloc(),
            &NSString::from_str(text),
            Some(&attrs),
        )
    }
}

fn icon(view: &MenuBarView) -> Retained<NSImage> {
    let marks: Vec<Mark> = view
        .workspaces
        .iter()
        .map(|e| {
            if Some(e.id) == view.current {
                Mark::Current(attributed(
                    &e.id.0.to_string(),
                    &label_font(),
                    &NSColor::blackColor(),
                ))
            } else if e.apps.is_empty() {
                Mark::Empty
            } else {
                Mark::Occupied
            }
        })
        .collect();
    let width =
        marks.iter().map(Mark::width).sum::<f64>() + GAP * marks.len().saturating_sub(1) as f64;
    let draw = RcBlock::new(move |_: NSRect| -> Bool {
        let mut x = 0.0;
        for m in &marks {
            m.draw(x);
            x += m.width() + GAP;
        }
        Bool::YES
    });
    let image = NSImage::imageWithSize_flipped_drawingHandler(
        NSSize::new(width.max(DOT), HEIGHT),
        false,
        &draw,
    );
    image.setTemplate(true);
    image
}

// --- the menu ----------------------------------------------------------------

struct Ivars {
    mailbox: Arc<Mailbox>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements, and Controller does
    // not implement Drop.
    #[unsafe(super = NSObject)]
    #[thread_kind = MainThreadOnly]
    #[name = "OrdoMenuBarController"]
    #[ivars = Ivars]
    struct Controller;

    impl Controller {
        // SAFETY: the signature matches the action selector's.
        #[unsafe(method(selectWorkspace:))]
        fn select_workspace(&self, sender: &NSMenuItem) {
            let Ok(n) = u8::try_from(sender.tag()) else {
                return;
            };
            let _ = self
                .ivars()
                .mailbox
                .tx
                .send(Msg::Hotkey(HotkeyAction::WorkspaceSwitchTo(WorkspaceId(n))));
        }
    }

    unsafe impl NSObjectProtocol for Controller {}

    unsafe impl NSMenuDelegate for Controller {
        #[unsafe(method(menuNeedsUpdate:))]
        fn menu_needs_update(&self, menu: &NSMenu) {
            let Some(view) = self.ivars().mailbox.latest.lock().unwrap().clone() else {
                return;
            };
            self.fill(menu, &view);
        }
    }
);

impl Controller {
    fn new(mailbox: Arc<Mailbox>, mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(Ivars { mailbox });
        unsafe { msg_send![super(this), init] }
    }

    fn fill(&self, menu: &NSMenu, view: &MenuBarView) {
        let mtm = self.mtm();
        menu.removeAllItems();
        menu.addItem(&NSMenuItem::sectionHeaderWithTitle(
            ns_string!("Workspaces"),
            mtm,
        ));
        for entry in &view.workspaces {
            let n = entry.id.0;
            let current = view.current == Some(entry.id);
            // The chord is shown, not bound — the event tap answers it before
            // any menu could — so the menu doubles as the shortcut's legend.
            let key = if (1..=9).contains(&n) {
                n.to_string()
            } else {
                String::new()
            };
            let item = unsafe {
                NSMenuItem::initWithTitle_action_keyEquivalent(
                    NSMenuItem::alloc(mtm),
                    &NSString::from_str(&apps_title(&entry.apps)),
                    Some(sel!(selectWorkspace:)),
                    &NSString::from_str(&key),
                )
            };
            item.setKeyEquivalentModifierMask(
                NSEventModifierFlags::Command | NSEventModifierFlags::Option,
            );
            unsafe { item.setTarget(Some(self)) };
            item.setTag(n as isize);
            item.setEnabled(view.engaged);
            item.setState(if current {
                NSControlStateValueOn
            } else {
                NSControlStateValueOff
            });
            // Filled for the current workspace, echoing its pill in the icon.
            let symbol = format!("{n}.square{}", if current { ".fill" } else { "" });
            item.setImage(
                NSImage::imageWithSystemSymbolName_accessibilityDescription(
                    &NSString::from_str(&symbol),
                    Some(&NSString::from_str(&format!("Workspace {n}"))),
                )
                .as_deref(),
            );
            if entry.apps.is_empty() {
                item.setAttributedTitle(Some(&attributed(
                    "Empty",
                    &NSFont::menuFontOfSize(0.0),
                    &NSColor::secondaryLabelColor(),
                )));
            }
            menu.addItem(&item);
        }
        if !view.engaged {
            menu.addItem(&NSMenuItem::separatorItem(mtm));
            let paused = unsafe {
                NSMenuItem::initWithTitle_action_keyEquivalent(
                    NSMenuItem::alloc(mtm),
                    ns_string!("Paused — press ⌃⌥⌘O to resume"),
                    None,
                    ns_string!(""),
                )
            };
            paused.setEnabled(false);
            menu.addItem(&paused);
        }
    }
}

/// "Safari, Slack, Terminal +2", most recently used first.
fn apps_title(apps: &[ordo_core::Pid]) -> String {
    let mut names: Vec<String> = Vec::new();
    for pid in apps {
        let name = NSRunningApplication::runningApplicationWithProcessIdentifier(pid.0)
            .and_then(|a| a.localizedName())
            .map(|n| n.to_string());
        if let Some(name) = name {
            if !names.contains(&name) {
                names.push(name);
            }
        }
    }
    let rest = names.len().saturating_sub(NAMED_APPS);
    names.truncate(NAMED_APPS);
    let mut title = names.join(", ");
    if rest > 0 {
        title.push_str(&format!(" +{rest}"));
    }
    title
}
