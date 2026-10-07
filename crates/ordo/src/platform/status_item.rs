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
//! mints, and a drag as a `MoveWorkspace`: the menu is a second keyboard, not
//! a second decision path.

use std::cell::{Cell, OnceCell};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use block2::RcBlock;
use crossbeam_channel::Sender;
use dispatch2::{DispatchQueue, DispatchTime};
use objc2::rc::Retained;
use objc2::runtime::{Bool, ProtocolObject};
use objc2::{define_class, msg_send, sel, AnyThread, DefinedClass, MainThreadOnly};
use objc2_app_kit::{
    NSAccessibility, NSAttributedStringNSStringDrawing, NSBezierPath, NSColor,
    NSCompositingOperation, NSEventModifierFlags,
    NSFont, NSFontAttributeName, NSFontWeightBold, NSForegroundColorAttributeName,
    NSGraphicsContext, NSImage, NSMenu, NSMenuDelegate, NSMenuItem, NSRunningApplication,
    NSStatusBar, NSStatusItem, NSVariableStatusItemLength,
};
use objc2_foundation::{
    ns_string, MainThreadMarker, NSAttributedString, NSDictionary, NSObject, NSObjectProtocol,
    NSPoint, NSRect, NSSize, NSString,
};

use ordo_core::{Gesture, HotkeyAction, Point, Rect};

use crate::engine::Msg;
use crate::menubar::{MenuBarView, MonitorsView};
use crate::platform::monitor_map::MonitorMap;
use crate::platform::settings::{saved_hiding, SettingsPanel};
use crate::platform::workspace_list::{Row, WorkspaceList};

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
/// Long enough for the engine's view of a move that landed to arrive first.
const RESHOW_AFTER: Duration = Duration::from_millis(500);
/// App names a menu row spells out before summarizing the rest as "+N".
const NAMED_APPS: usize = 3;

struct Mailbox {
    latest: Mutex<Option<MenuBarView>>,
    tx: Sender<Msg>,
    own_menu: OwnMenu,
}

/// Whether Ordo's own menu or settings panel is open, for the threads that
/// must not read what happens in them as the user acting on the world: a
/// click in one is a pick of Ordo's, which reaches the engine as a command if
/// it asks for one.
#[derive(Clone, Default)]
pub struct OwnMenu(Arc<Surfaces>);

#[derive(Default)]
struct Surfaces {
    menu: AtomicBool,
    /// The panel's frame in CG coordinates, while it is shown.
    panel: Mutex<Option<Rect>>,
}

impl OwnMenu {
    pub fn is_open(&self) -> bool {
        self.0.menu.load(Ordering::Relaxed) || self.0.panel.lock().unwrap().is_some()
    }

    /// Whether a click at `at` is one of Ordo's own. The open menu takes
    /// every click, since macOS spends one outside it on closing it. The panel
    /// takes only its own: a click outside closes it and also lands on
    /// whatever is there, which is the user acting on the world.
    pub fn takes(&self, at: Point) -> bool {
        self.0.menu.load(Ordering::Relaxed)
            || self.0.panel.lock().unwrap().is_some_and(|f| f.contains(at))
    }

    fn set_menu(&self, tx: &Sender<Msg>, open: bool) {
        let before = self.is_open();
        self.0.menu.store(open, Ordering::Relaxed);
        self.changed(tx, before, "menu", open);
    }

    pub(crate) fn set_panel(&self, tx: &Sender<Msg>, frame: Option<Rect>) {
        let before = self.is_open();
        let open = frame.is_some();
        *self.0.panel.lock().unwrap() = frame;
        self.changed(tx, before, "settings", open);
    }

    /// The core hears only when Ordo's surfaces as a whole open or close: the
    /// panel opens from the menu, and the two may overlap.
    fn changed(&self, tx: &Sender<Msg>, before: bool, what: &str, open: bool) {
        let wall_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_millis());
        eprintln!(
            "ordo: own {what} {} at wall {wall_ms}",
            if open { "opened" } else { "closed" }
        );
        let after = self.is_open();
        if after != before {
            let _ = tx.send(Msg::Gesture(Gesture::OwnMenu { open: after }));
        }
    }
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
pub fn install(tx: Sender<Msg>, own_menu: OwnMenu) -> MenuBar {
    let _ = tx.send(Msg::Hiding(saved_hiding()));
    MenuBar {
        mailbox: Arc::new(Mailbox {
            latest: Mutex::new(None),
            tx,
            own_menu,
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
/// item shows an image, and an image drawn anew for each menu bar is what
/// takes that bar's colors.
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

/// What the menu's views ask for, sent to the engine. A drop shows its move
/// at once, before the engine's view; one the core refused (a switch was on
/// its way) would leave the rows moved, since a refusal changes no view and
/// so sends none. So the latest view is shown again a moment later: the same
/// rows if the move landed, the old ones if not.
fn commands(mailbox: &Arc<Mailbox>) -> Box<dyn Fn(HotkeyAction)> {
    let mailbox = mailbox.clone();
    Box::new(move |action| {
        let _ = mailbox.tx.send(Msg::hotkey(action));
        if matches!(action, HotkeyAction::MoveWorkspace { .. } | HotkeyAction::MoveMonitor { .. }) {
            let mailbox = mailbox.clone();
            let at = DispatchTime::try_from(RESHOW_AFTER).unwrap_or(DispatchTime::NOW);
            let _ = DispatchQueue::main().after(at, move || {
                let Some(view) = mailbox.latest.lock().unwrap().clone() else {
                    return;
                };
                UI.with(|ui| {
                    if let Some(ui) = ui.get() {
                        ui.controller.follow(&view);
                    }
                });
            });
        }
    })
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
        NSColor::labelColor().setFill();
        NSColor::labelColor().setStroke();
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
                // Punched out rather than drawn in a color: the hole lets the
                // menu bar itself show through as the digit, whatever is
                // behind it.
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
        NSColor::labelColor().setFill();
        NSColor::labelColor().setStroke();
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
    // Not a template, though that is the usual way to follow the menu bar:
    // on the unfocused display's bar macOS dims template images far past its
    // other items (measured at half their contrast, and gone entirely over a
    // light wallpaper). Drawing in the label color, resolved per bar at draw
    // time, tints it the same way without that dim.
    image.setTemplate(false);
    image
}

// --- the menu ----------------------------------------------------------------

struct Ivars {
    mailbox: Arc<Mailbox>,
    /// Kept across rebuilds so a view change can slide the frame it drew.
    map: OnceCell<Retained<MonitorMap>>,
    list: OnceCell<Retained<WorkspaceList>>,
    settings: OnceCell<Retained<SettingsPanel>>,
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
        #[unsafe(method(openSettings:))]
        fn open_settings(&self, _sender: &NSMenuItem) {
            // After the menu has finished closing: a popover shown while the
            // menu still tracks is closed along with it.
            DispatchQueue::main().exec_async(|| {
                let mtm = MainThreadMarker::new().expect("runs on the main queue");
                UI.with(|ui| {
                    let Some(ui) = ui.get() else { return };
                    let Some(button) = ui.item.button(mtm) else { return };
                    ui.controller.settings_panel().show(&button);
                });
            });
        }
    }

    unsafe impl NSObjectProtocol for Controller {}

    unsafe impl NSMenuDelegate for Controller {
        #[unsafe(method(menuWillOpen:))]
        fn menu_will_open(&self, _menu: &NSMenu) {
            self.ivars().open.set(true);
            let mailbox = &self.ivars().mailbox;
            mailbox.own_menu.set_menu(&mailbox.tx, true);
        }

        #[unsafe(method(menuDidClose:))]
        fn menu_did_close(&self, _menu: &NSMenu) {
            self.ivars().open.set(false);
            let mailbox = &self.ivars().mailbox;
            mailbox.own_menu.set_menu(&mailbox.tx, false);
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
            list: OnceCell::new(),
            settings: OnceCell::new(),
            open: Cell::new(false),
        });
        unsafe { msg_send![super(this), init] }
    }

    fn map(&self) -> &MonitorMap {
        self.ivars()
            .map
            .get_or_init(|| MonitorMap::new(self.mtm(), commands(&self.ivars().mailbox)))
    }

    fn settings_panel(&self) -> &SettingsPanel {
        self.ivars().settings.get_or_init(|| {
            let mailbox = &self.ivars().mailbox;
            SettingsPanel::new(mailbox.tx.clone(), mailbox.own_menu.clone(), self.mtm())
        })
    }

    fn list(&self) -> &WorkspaceList {
        self.ivars()
            .list
            .get_or_init(|| WorkspaceList::new(self.mtm(), commands(&self.ivars().mailbox)))
    }

    /// A view that changed under an open menu: the diagram follows it,
    /// sliding, and so do the workspace rows, which keep their places (a
    /// count that changed would need the menu's height to change too).
    fn follow(&self, view: &MenuBarView) {
        if !self.ivars().open.get() {
            return;
        }
        if let (Some(monitors), Some(map)) = (&view.monitors, self.ivars().map.get()) {
            map.show(monitors, view.engaged, true);
        }
        if let Some(list) = self.ivars().list.get() {
            list.show(rows(view), current_row(view), view.engaged, false);
        }
    }

    fn fill(&self, menu: &NSMenu, view: &MenuBarView) {
        let mtm = self.mtm();
        menu.removeAllItems();
        menu.addItem(&NSMenuItem::sectionHeaderWithTitle(
            ns_string!("Workspaces"),
            mtm,
        ));
        let list = self.list();
        list.show(rows(view), current_row(view), view.engaged, true);
        let item = NSMenuItem::new(mtm);
        item.setView(Some(list));
        menu.addItem(&item);
        if let Some(monitors) = &view.monitors {
            menu.addItem(&NSMenuItem::separatorItem(mtm));
            fill_monitors(menu, self.map(), monitors, view.engaged, mtm);
        }
        if !view.engaged {
            menu.addItem(&NSMenuItem::separatorItem(mtm));
            menu.addItem(&info_item("Paused — press ⌃⌥⌘O to resume", mtm));
        }
        if !view.unreachable.is_empty() {
            menu.addItem(&NSMenuItem::separatorItem(mtm));
            for pid in &view.unreachable {
                let name = app_name(*pid).unwrap_or_else(|| format!("pid {}", pid.0));
                menu.addItem(&info_item(
                    &format!("Can't reach {name}: quit and reopen it"),
                    mtm,
                ));
            }
        }
        menu.addItem(&NSMenuItem::separatorItem(mtm));
        menu.addItem(&self.settings(mtm));
    }

    fn settings(&self, mtm: MainThreadMarker) -> Retained<NSMenuItem> {
        let item = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                ns_string!("Settings…"),
                Some(sel!(openSettings:)),
                ns_string!(""),
            )
        };
        unsafe { item.setTarget(Some(self)) };
        item.setImage(
            NSImage::imageWithSystemSymbolName_accessibilityDescription(
                ns_string!("gearshape"),
                Some(ns_string!("Settings")),
            )
            .as_deref(),
        );
        item
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

fn app_name(pid: ordo_core::Pid) -> Option<String> {
    NSRunningApplication::runningApplicationWithProcessIdentifier(pid.0)
        .and_then(|a| a.localizedName())
        .map(|n| n.to_string())
}

fn rows(view: &MenuBarView) -> Vec<Row> {
    view.workspaces
        .iter()
        .map(|e| Row {
            apps: apps_title(&e.apps),
        })
        .collect()
}

fn current_row(view: &MenuBarView) -> Option<usize> {
    let current = view.current?;
    view.workspaces.iter().position(|e| e.id == current)
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
