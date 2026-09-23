//! Ordo's menu bar item: one mark per workspace — the current one a pill
//! with its number, the rest dots, solid where windows live and hollow where
//! none do — then the virtual monitors, solid where a display shows them,
//! under a frame that slides as the view moves. Its menu switches workspace
//! on a pick.
//!
//! The engine thread owns the model; AppKit owns the main thread. They meet
//! in a mailbox: [`MenuBar::show`] (any thread) leaves the newest view there
//! and wakes the main queue to redraw the icon. The menu is built only as it
//! opens (`menuNeedsUpdate:`), so a rescan landing while it is open never
//! reshuffles the rows under the pointer.
//!
//! A pick reaches the engine as the same `WorkspaceSwitchTo` that Cmd+Alt+digit
//! mints: the menu is a second keyboard, not a second decision path.

use std::cell::{Cell, OnceCell};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use block2::RcBlock;
use crossbeam_channel::Sender;
use dispatch2::{DispatchQueue, DispatchTime};
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
use crate::menubar::{MenuBarView, MonitorsView};
use crate::platform::monitor_map::MonitorMap;

/// Image height; the status bar centers it vertically.
const HEIGHT: f64 = 16.0;
const DOT: f64 = 6.0;
const RING_LINE: f64 = 1.0;
const PILL_H: f64 = 14.0;
/// Wider than tall even for one digit, so the pill never reads as a big dot.
const PILL_MIN_W: f64 = 17.0;
const PILL_PAD: f64 = 5.0;
const GAP: f64 = 4.0;
/// Wider than GAP, so workspaces and monitors read as two groups.
const GROUP_GAP: f64 = 8.0;
const SCREEN_W: f64 = 10.0;
const SCREEN_H: f64 = 7.0;
const SCREEN_R: f64 = 1.5;
/// Wider than the frame's reach past a screen (pad plus line), or the frame's
/// edge would cut into the hidden neighbour.
const SCREEN_GAP: f64 = 4.0;
const FRAME_PAD: f64 = 1.5;
const FRAME_LINE: f64 = 1.0;
const FRAME_R: f64 = 3.0;
const SLIDE_SECS: f64 = 0.12;
const SLIDE_FRAMES: u32 = 8;
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
    controller: Retained<Controller>,
    /// The monitors glyph as last drawn, mid-slide included: where the next
    /// slide starts from.
    strip: Cell<Option<Strip>>,
    /// Bumped by every repaint that is not a step of the running slide, which
    /// is how that slide learns it has been overtaken.
    slide: Cell<u64>,
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
        match (ui.strip.get(), Strip::of(&view)) {
            (Some(from), Some(to)) if from.screens == to.screens && from != to => {
                slide(mailbox, ui, from, to)
            }
            (_, to) => {
                ui.slide.set(ui.slide.get() + 1);
                ui.paint(&view, to);
            }
        }
        // Dimmed, the way macOS marks an item that is present but inert.
        button.setAppearsDisabled(!view.engaged);
        let summary = NSString::from_str(&summary(&view));
        button.setToolTip(Some(&summary));
        button.setAccessibilityLabel(Some(&summary));
        ui.controller.follow(&view);
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
            controller,
            strip: Cell::new(None),
            slide: Cell::new(0),
        }
    }

    fn paint(&self, view: &MenuBarView, strip: Option<Strip>) {
        let Some(button) = self.item.button(self.controller.mtm()) else {
            return;
        };
        button.setImage(Some(&icon(view, strip)));
        self.strip.set(strip);
    }
}

/// Steps the icon's frame from `from` to `to`, as the menu's frame slides.
/// Dispatched frame by frame rather than animated by AppKit, because a status
/// item shows an image, and only a template image follows the menu bar's
/// appearance.
fn slide(mailbox: &Arc<Mailbox>, ui: &Ui, from: Strip, to: Strip) {
    let run = ui.slide.get() + 1;
    ui.slide.set(run);
    for k in 1..=SLIDE_FRAMES {
        let t = k as f64 / SLIDE_FRAMES as f64;
        let at = DispatchTime::try_from(Duration::from_secs_f64(SLIDE_SECS * t))
            .unwrap_or(DispatchTime::NOW);
        let mailbox = mailbox.clone();
        let _ = DispatchQueue::main().after(at, move || {
            UI.with(|ui| {
                let Some(ui) = ui.get().filter(|ui| ui.slide.get() == run) else {
                    return;
                };
                let Some(view) = mailbox.latest.lock().unwrap().clone() else {
                    return;
                };
                ui.paint(&view, Some(from.toward(to, t)));
            })
        });
    }
}

fn summary(view: &MenuBarView) -> String {
    let count = view.workspaces.len();
    let mut s = match view.current {
        Some(ws) => format!("Ordo — workspace {} of {count}", ws.0),
        None => format!("Ordo — {count} workspaces"),
    };
    if let Some(m) = slidable(view) {
        let shown: Vec<String> = m
            .monitors
            .iter()
            .filter(|e| e.display.is_some())
            .map(|e| e.id.0.to_string())
            .collect();
        s.push_str(&format!(
            ", showing monitors {} of {}",
            shown.join(" and "),
            m.monitors.len()
        ));
    }
    if !view.engaged {
        s.push_str(" (paused)");
    }
    s
}

/// The monitors, when any can be out of sight. With virtualization off, or
/// no more monitors than displays, every one is always shown, and a glyph
/// that never changes would only take up menu bar.
fn slidable(view: &MenuBarView) -> Option<&MonitorsView> {
    view.monitors
        .as_ref()
        .filter(|m| m.enabled && m.monitors.len() > m.displays.len())
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

/// The monitors glyph: how many screens, and where the display frame stands
/// over them, its outer edges in the glyph's own x. A screen is solid wherever
/// the frame covers it, so mid-slide the screens light up as it passes.
#[derive(Clone, Copy, PartialEq)]
struct Strip {
    screens: usize,
    left: f64,
    right: f64,
}

impl Strip {
    fn of(view: &MenuBarView) -> Option<Strip> {
        let m = slidable(view)?;
        let first = m.monitors.iter().position(|e| e.display.is_some())?;
        let last = m.monitors.iter().rposition(|e| e.display.is_some())?;
        Some(Strip {
            screens: m.monitors.len(),
            left: screen_x(first) - FRAME_PAD - FRAME_LINE,
            right: screen_x(last) + SCREEN_W + FRAME_PAD + FRAME_LINE,
        })
    }

    fn width(&self) -> f64 {
        let n = self.screens as f64;
        2.0 * (FRAME_LINE + FRAME_PAD) + n * SCREEN_W + (n - 1.0) * SCREEN_GAP
    }

    /// Eased out: the frame moves the instant the view does, and only its
    /// landing is soft.
    fn toward(self, to: Strip, t: f64) -> Strip {
        let e = 1.0 - (1.0 - t).powi(3);
        Strip {
            screens: to.screens,
            left: self.left + (to.left - self.left) * e,
            right: self.right + (to.right - self.right) * e,
        }
    }

    fn draw(&self, x0: f64) {
        NSColor::blackColor().setFill();
        NSColor::blackColor().setStroke();
        let y = (HEIGHT - SCREEN_H) / 2.0;
        for i in 0..self.screens {
            let x = screen_x(i);
            if (self.left..self.right).contains(&(x + SCREEN_W / 2.0)) {
                let r = NSRect::new(NSPoint::new(x0 + x, y), NSSize::new(SCREEN_W, SCREEN_H));
                NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(r, SCREEN_R, SCREEN_R)
                    .fill();
            } else {
                let r = NSRect::new(
                    NSPoint::new(x0 + x + RING_LINE / 2.0, y + RING_LINE / 2.0),
                    NSSize::new(SCREEN_W - RING_LINE, SCREEN_H - RING_LINE),
                );
                let path =
                    NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(r, SCREEN_R, SCREEN_R);
                path.setLineWidth(RING_LINE);
                path.stroke();
            }
        }
        let h = SCREEN_H + 2.0 * (FRAME_PAD + FRAME_LINE);
        let r = NSRect::new(
            NSPoint::new(
                x0 + self.left + FRAME_LINE / 2.0,
                (HEIGHT - h) / 2.0 + FRAME_LINE / 2.0,
            ),
            NSSize::new(self.right - self.left - FRAME_LINE, h - FRAME_LINE),
        );
        let frame = NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(r, FRAME_R, FRAME_R);
        frame.setLineWidth(FRAME_LINE);
        frame.stroke();
    }
}

fn screen_x(i: usize) -> f64 {
    FRAME_LINE + FRAME_PAD + i as f64 * (SCREEN_W + SCREEN_GAP)
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

fn icon(view: &MenuBarView, strip: Option<Strip>) -> Retained<NSImage> {
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
    let marks_w =
        marks.iter().map(Mark::width).sum::<f64>() + GAP * marks.len().saturating_sub(1) as f64;
    let width = marks_w + strip.map_or(0.0, |s| GROUP_GAP + s.width());
    let draw = RcBlock::new(move |_: NSRect| -> Bool {
        let mut x = 0.0;
        for m in &marks {
            m.draw(x);
            x += m.width() + GAP;
        }
        if let Some(s) = strip {
            s.draw(marks_w + GROUP_GAP);
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
    /// Kept across rebuilds so a view change can slide the frame it drew.
    map: OnceCell<Retained<MonitorMap>>,
    open: Cell<bool>,
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
        #[unsafe(method(menuWillOpen:))]
        fn menu_will_open(&self, _menu: &NSMenu) {
            self.ivars().open.set(true);
        }

        #[unsafe(method(menuDidClose:))]
        fn menu_did_close(&self, _menu: &NSMenu) {
            self.ivars().open.set(false);
        }

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
        let this = Self::alloc(mtm).set_ivars(Ivars {
            mailbox,
            map: OnceCell::new(),
            open: Cell::new(false),
        });
        unsafe { msg_send![super(this), init] }
    }

    fn map(&self) -> &MonitorMap {
        self.ivars().map.get_or_init(|| {
            let tx = self.ivars().mailbox.tx.clone();
            MonitorMap::new(
                self.mtm(),
                Box::new(move |from, into| {
                    let _ = tx.send(Msg::Hotkey(HotkeyAction::MergeMonitors { from, into }));
                }),
            )
        })
    }

    /// A view that changed under an open menu: only the diagram follows it,
    /// sliding, while the rows stay put under the pointer.
    fn follow(&self, view: &MenuBarView) {
        if !self.ivars().open.get() {
            return;
        }
        if let (Some(monitors), Some(map)) = (&view.monitors, self.ivars().map.get()) {
            map.show(monitors, view.engaged, true);
        }
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
        if let Some(monitors) = &view.monitors {
            menu.addItem(&NSMenuItem::separatorItem(mtm));
            fill_monitors(menu, self.map(), monitors, view.engaged, mtm);
        }
        if !view.engaged {
            menu.addItem(&NSMenuItem::separatorItem(mtm));
            menu.addItem(&info_item("Paused — press ⌃⌥⌘O to resume", mtm));
        }
    }
}

/// The monitors diagram plus the one mode line the picture can't show.
fn fill_monitors(
    menu: &NSMenu,
    map: &MonitorMap,
    view: &MonitorsView,
    engaged: bool,
    mtm: MainThreadMarker,
) {
    menu.addItem(&NSMenuItem::sectionHeaderWithTitle(
        ns_string!("Monitors"),
        mtm,
    ));
    map.show(view, engaged, false);
    let item = NSMenuItem::new(mtm);
    item.setView(Some(map));
    menu.addItem(&item);

    let toggle = info_item(
        if view.enabled {
            "Virtualization on"
        } else {
            "Virtualization off — all monitors shown"
        },
        mtm,
    );
    toggle.setKeyEquivalent(ns_string!("v"));
    toggle.setKeyEquivalentModifierMask(
        NSEventModifierFlags::Control
            | NSEventModifierFlags::Option
            | NSEventModifierFlags::Command,
    );
    menu.addItem(&toggle);
}

fn info_item(title: &str, mtm: MainThreadMarker) -> Retained<NSMenuItem> {
    let item = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            &NSString::from_str(title),
            None,
            ns_string!(""),
        )
    };
    item.setEnabled(false);
    item
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
