//! The monitors diagram in the menu bar item's menu: a tile per virtual
//! monitor, left to right, and over them a frame for the physical displays —
//! the part of the row that is actually on screen. When the view changes
//! while the menu is open, the frame slides.
//!
//! The frame is its own subview, so a view change animates as one thing
//! moving rather than a redraw.
//!
//! While some monitor is spare (more monitors than displays), a tile can be
//! dragged onto another to merge the two; the drop asks first, in place of
//! the tiles, because a merge renumbers monitors on every workspace. The plus
//! after the last tile adds a monitor without asking: it is empty and moves
//! nothing, so there is nothing to confirm.

use std::cell::{Cell, OnceCell, RefCell};

use objc2::rc::Retained;
use objc2::{define_class, msg_send, AnyThread, DefinedClass, MainThreadOnly};
use objc2_app_kit::{
    NSAccessibility, NSAnimatablePropertyContainer, NSAnimationContext,
    NSAttributedStringNSStringDrawing, NSBezierPath, NSColor, NSFont, NSFontAttributeName,
    NSFontWeightSemibold, NSForegroundColorAttributeName, NSLineBreakMode, NSMutableParagraphStyle,
    NSAutoresizingMaskOptions, NSEvent, NSParagraphStyleAttributeName, NSTextAlignment, NSView,
};
use objc2_foundation::{
    MainThreadMarker, NSAttributedString, NSDictionary, NSPoint, NSRect, NSSize, NSString,
};

use ordo_core::HotkeyAction;

use crate::menubar::{MonitorEntry, MonitorsView};

const MARGIN_X: f64 = 20.0;
const TOP: f64 = 4.0;
const TILE_W: f64 = 72.0;
const TILE_H: f64 = 42.0;
const TILE_GAP: f64 = 10.0;
const TILE_R: f64 = 5.0;
const FRAME_PAD: f64 = 5.0;
const FRAME_R: f64 = 9.0;
const FRAME_LINE: f64 = 2.0;
const BOTTOM: f64 = 6.0;
const DOT: f64 = 4.0;
const DOT_GAP: f64 = 3.0;
/// Windows shown as dots before the row stops growing.
const MAX_DOTS: usize = 6;
const SLIDE_SECS: f64 = 0.15;
/// How far a tile must travel before a press becomes a drag.
const DRAG_SLOP: f64 = 3.0;
const BUTTON_W: f64 = 72.0;
const BUTTON_H: f64 = 20.0;
const BUTTON_GAP: f64 = 8.0;
const BUTTON_R: f64 = 6.0;
const ADD_W: f64 = 28.0;

/// A tile in the hand: which one, where the pointer holds it, and where the
/// pointer is now.
#[derive(Clone, Copy)]
struct Drag {
    from: usize,
    grab: NSPoint,
    at: NSPoint,
    moved: bool,
}

pub struct MapIvars {
    view: RefCell<Option<MonitorsView>>,
    frame: OnceCell<Retained<DisplayFrame>>,
    engaged: Cell<bool>,
    mergeable: Cell<bool>,
    drag: Cell<Option<Drag>>,
    /// The plus is held down.
    adding: Cell<bool>,
    /// A drop awaiting its answer, as tile indices (from, into).
    confirm: Cell<Option<(usize, usize)>>,
    on_command: Box<dyn Fn(HotkeyAction)>,
}

define_class!(
    // SAFETY: NSView has no subclassing requirements beyond these overrides,
    // and MonitorMap does not implement Drop.
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "OrdoMonitorMap"]
    #[ivars = MapIvars]
    pub struct MonitorMap;

    impl MonitorMap {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty: NSRect) {
            let Some(view) = &*self.ivars().view.borrow() else {
                return;
            };
            let n = view.monitors.len();
            if let Some((from, into)) = self.ivars().confirm.get() {
                draw_confirm(self.bounds(), &view.monitors[from], &view.monitors[into]);
                return;
            }
            let drag = self.ivars().drag.get().filter(|d| d.moved);
            let target = drag.and_then(|d| tile_at(n, d.at).filter(|t| *t != d.from));
            for (i, m) in view.monitors.iter().enumerate() {
                let r = tile_rect(i);
                if drag.is_some_and(|d| d.from == i) {
                    draw_slot(r);
                    continue;
                }
                if target == Some(i) {
                    draw_target(r);
                }
                draw_tile(r, m.id.0, m.windows, m.display.is_some(), m.id == view.viewed);
            }
            if self.ivars().engaged.get() && drag.is_none() {
                draw_add(add_rect(n), self.ivars().adding.get());
            }
            if let Some(d) = drag {
                let m = &view.monitors[d.from];
                let r = NSRect::new(
                    NSPoint::new(d.at.x - d.grab.x, d.at.y - d.grab.y),
                    NSSize::new(TILE_W, TILE_H),
                );
                draw_lifted(r);
                draw_tile(r, m.id.0, m.windows, true, m.id == view.viewed);
            }
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            let ivars = self.ivars();
            if ivars.confirm.get().is_some() {
                return;
            }
            let p = self.local(event);
            let n = ivars.view.borrow().as_ref().map_or(0, |v| v.monitors.len());
            if ivars.engaged.get() && contains(add_rect(n), p) {
                ivars.adding.set(true);
                self.redraw();
                return;
            }
            if !ivars.mergeable.get() {
                return;
            }
            if let Some(i) = tile_at(n, p) {
                let o = tile_rect(i).origin;
                ivars.drag.set(Some(Drag {
                    from: i,
                    grab: NSPoint::new(p.x - o.x, p.y - o.y),
                    at: p,
                    moved: false,
                }));
            }
        }

        #[unsafe(method(mouseDragged:))]
        fn mouse_dragged(&self, event: &NSEvent) {
            let Some(mut d) = self.ivars().drag.get() else {
                return;
            };
            d.at = self.local(event);
            let o = tile_rect(d.from).origin;
            let (dx, dy) = (d.at.x - (o.x + d.grab.x), d.at.y - (o.y + d.grab.y));
            d.moved |= dx.hypot(dy) > DRAG_SLOP;
            self.ivars().drag.set(Some(d));
            self.redraw();
        }

        #[unsafe(method(mouseUp:))]
        fn mouse_up(&self, event: &NSEvent) {
            let p = self.local(event);
            if let Some((from, into)) = self.ivars().confirm.get() {
                let (cancel, merge) = buttons(self.bounds());
                if contains(merge, p) {
                    self.merge(from, into);
                } else if contains(cancel, p) {
                    self.answer();
                }
                return;
            }
            let n = self.ivars().view.borrow().as_ref().map_or(0, |v| v.monitors.len());
            if self.ivars().adding.take() {
                self.redraw();
                if contains(add_rect(n), p) {
                    (self.ivars().on_command)(HotkeyAction::AddMonitor);
                    self.close_menu();
                }
                return;
            }
            let Some(d) = self.ivars().drag.take() else {
                return;
            };
            match tile_at(n, p).filter(|t| d.moved && *t != d.from) {
                Some(into) => self.ask(d.from, into),
                None => self.redraw(),
            }
        }
    }
);

define_class!(
    // SAFETY: as for MonitorMap.
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "OrdoDisplayFrame"]
    struct DisplayFrame;

    impl DisplayFrame {
        // Transparent to the mouse: the tiles under it are what a drag grabs.
        #[unsafe(method(hitTest:))]
        fn hit_test(&self, _point: NSPoint) -> *mut NSView {
            std::ptr::null_mut()
        }

        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty: NSRect) {
            let size = self.frame().size;
            let outline = NSRect::new(
                NSPoint::new(FRAME_LINE / 2.0, FRAME_LINE / 2.0),
                NSSize::new(size.width - FRAME_LINE, size.height - FRAME_LINE),
            );
            let path =
                NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(outline, FRAME_R, FRAME_R);
            let accent = NSColor::controlAccentColor();
            accent.colorWithAlphaComponent(0.12).setFill();
            path.fill();
            accent.setStroke();
            path.setLineWidth(FRAME_LINE);
            path.stroke();
        }
    }
);

impl MonitorMap {
    /// `on_command` is handed what the diagram asks for: a confirmed merge,
    /// or a monitor added.
    pub fn new(mtm: MainThreadMarker, on_command: Box<dyn Fn(HotkeyAction)>) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(MapIvars {
            view: RefCell::new(None),
            frame: OnceCell::new(),
            engaged: Cell::new(false),
            mergeable: Cell::new(false),
            drag: Cell::new(None),
            adding: Cell::new(false),
            confirm: Cell::new(None),
            on_command,
        });
        let this: Retained<Self> = unsafe { msg_send![super(this), initWithFrame: NSRect::ZERO] };
        // Stretched to the menu's width, so the merge prompt has room even
        // when two tiles alone would not give it any.
        this.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable);
        // Layer-backed so the slide is Core Animation's, which keeps running
        // while the menu holds the run loop in its tracking mode.
        this.setWantsLayer(true);
        let frame: Retained<DisplayFrame> =
            unsafe { msg_send![DisplayFrame::alloc(mtm), initWithFrame: NSRect::ZERO] };
        this.addSubview(&frame);
        let _ = this.ivars().frame.set(frame);
        this
    }

    /// Show `view`, sliding the display frame from where it stood when
    /// `animate` — the menu is open and the user just watched it change.
    /// `engaged`: Ordo acts on commands, so a merge or an add would be
    /// carried out.
    pub fn show(&self, view: &MonitorsView, engaged: bool, animate: bool) {
        let n = view.monitors.len();
        let ivars = self.ivars();
        // A fresh menu starts clean, and a drop's tile indices mean nothing
        // once the monitors themselves change under it.
        let same = ivars.view.borrow().as_ref().is_some_and(|v| v.monitors.len() == n);
        if !animate || !same {
            ivars.drag.set(None);
            ivars.confirm.set(None);
        }
        ivars.adding.set(false);
        ivars.engaged.set(engaged);
        let mergeable = engaged && n > view.displays.len().max(1);
        ivars.mergeable.set(mergeable);
        let tip = match (engaged, mergeable) {
            (_, true) => Some("Drag a monitor onto another to merge them; + adds a monitor"),
            (true, false) => Some("+ adds a monitor"),
            (false, _) => None,
        };
        self.setToolTip(tip.map(NSString::from_str).as_deref());
        let frame_h = TILE_H + 2.0 * FRAME_PAD;
        let plus = if engaged { TILE_GAP + ADD_W } else { 0.0 };
        self.setFrameSize(NSSize::new(
            2.0 * MARGIN_X
                + 2.0 * FRAME_PAD
                + n as f64 * TILE_W
                + n.saturating_sub(1) as f64 * TILE_GAP
                + plus,
            TOP + frame_h + BOTTOM,
        ));

        let hosted: Vec<usize> = view
            .monitors
            .iter()
            .enumerate()
            .filter(|(_, m)| m.display.is_some())
            .map(|(i, _)| i)
            .collect();
        let frame = self.ivars().frame.get().expect("built in new");
        match (hosted.first(), hosted.last()) {
            // The prompt takes the tiles' place, frame and all.
            _ if ivars.confirm.get().is_some() => frame.setHidden(true),
            (Some(&first), Some(&last)) => {
                let x = tile_rect(first).origin.x - FRAME_PAD;
                let w = tile_rect(last).origin.x + TILE_W + FRAME_PAD - x;
                let rect = NSRect::new(NSPoint::new(x, TOP), NSSize::new(w, frame_h));
                frame.setHidden(false);
                if animate {
                    NSAnimationContext::beginGrouping();
                    let ctx = NSAnimationContext::currentContext();
                    ctx.setDuration(SLIDE_SECS);
                    ctx.setAllowsImplicitAnimation(true);
                    frame.animator().setFrame(rect);
                    NSAnimationContext::endGrouping();
                } else {
                    frame.setFrame(rect);
                }
            }
            // No display hosts anything: displays asleep, nothing on screen.
            _ => frame.setHidden(true),
        }

        self.setAccessibilityLabel(Some(&NSString::from_str(&describe(view))));
        *ivars.view.borrow_mut() = Some(view.clone());
        self.redraw();
    }

    fn local(&self, event: &NSEvent) -> NSPoint {
        self.convertPoint_fromView(event.locationInWindow(), None)
    }

    /// Now, not at the end of the run loop pass: a menu tracking the mouse
    /// may not get to one before the next drag event.
    fn redraw(&self) {
        self.setNeedsDisplay(true);
        self.displayIfNeeded();
    }

    fn ask(&self, from: usize, into: usize) {
        self.ivars().confirm.set(Some((from, into)));
        self.ivars().frame.get().expect("built in new").setHidden(true);
        self.redraw();
    }

    /// Back to the tiles, as they were.
    fn answer(&self) {
        self.ivars().confirm.set(None);
        let view = self.ivars().view.borrow().clone();
        if let Some(view) = view {
            self.show(&view, self.ivars().mergeable.get(), true);
        }
    }

    /// Closes the menu too: the merge renumbers the very tiles it drew.
    fn merge(&self, from: usize, into: usize) {
        let ids = self
            .ivars()
            .view
            .borrow()
            .as_ref()
            .map(|v| (v.monitors[from].id, v.monitors[into].id));
        self.ivars().confirm.set(None);
        if let Some((from, into)) = ids {
            (self.ivars().on_command)(HotkeyAction::MergeMonitors { from, into });
        }
        self.close_menu();
    }

    /// After a merge or an add the tiles no longer match the monitors, and an
    /// open menu doesn't take a new width from its item's view.
    fn close_menu(&self) {
        // SAFETY: the item is in the menu that is showing this view.
        if let Some(menu) = self.enclosingMenuItem().and_then(|item| unsafe { item.menu() }) {
            menu.cancelTracking();
        }
    }
}

fn contains(r: NSRect, p: NSPoint) -> bool {
    p.x >= r.origin.x
        && p.x < r.origin.x + r.size.width
        && p.y >= r.origin.y
        && p.y < r.origin.y + r.size.height
}

fn tile_at(n: usize, p: NSPoint) -> Option<usize> {
    (0..n).find(|i| contains(tile_rect(*i), p))
}

fn inset(r: NSRect, d: f64) -> NSRect {
    NSRect::new(
        NSPoint::new(r.origin.x + d, r.origin.y + d),
        NSSize::new(r.size.width - 2.0 * d, r.size.height - 2.0 * d),
    )
}

fn rounded(r: NSRect, radius: f64) -> Retained<NSBezierPath> {
    NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(r, radius, radius)
}

/// Where a lifted tile came from: an outline, so the row keeps its shape.
fn draw_slot(r: NSRect) {
    let path = rounded(inset(r, 0.5), TILE_R);
    let dash: [f64; 2] = [3.0, 2.0];
    unsafe { path.setLineDash_count_phase(dash.as_ptr(), 2, 0.0) };
    NSColor::tertiaryLabelColor().setStroke();
    path.setLineWidth(1.0);
    path.stroke();
}

/// The tile a drop would merge into.
fn draw_target(r: NSRect) {
    let path = rounded(inset(r, 1.0), TILE_R);
    let accent = NSColor::controlAccentColor();
    accent.colorWithAlphaComponent(0.25).setFill();
    path.fill();
    accent.setStroke();
    path.setLineWidth(2.0);
    path.stroke();
}

/// Backing for the tile in the hand, so what it passes over doesn't show
/// through its translucent fill.
fn draw_lifted(r: NSRect) {
    let path = rounded(r, TILE_R);
    NSColor::windowBackgroundColor().setFill();
    path.fill();
    NSColor::controlAccentColor().setStroke();
    path.setLineWidth(1.5);
    path.stroke();
}

/// Cancel and Merge, side by side and centered at the foot of the prompt.
fn buttons(bounds: NSRect) -> (NSRect, NSRect) {
    let x = (bounds.size.width - 2.0 * BUTTON_W - BUTTON_GAP) / 2.0;
    let y = TOP + 33.0;
    let at = |x: f64| NSRect::new(NSPoint::new(x, y), NSSize::new(BUTTON_W, BUTTON_H));
    (at(x), at(x + BUTTON_W + BUTTON_GAP))
}

fn draw_confirm(bounds: NSRect, from: &MonitorEntry, into: &MonitorEntry) {
    let line = |s: &str, font: &NSFont, color: &NSColor, y: f64, h: f64| {
        text(s, font, color).drawInRect(NSRect::new(
            NSPoint::new(MARGIN_X / 2.0, y),
            NSSize::new(bounds.size.width - MARGIN_X, h),
        ));
    };
    line(
        &format!("Merge monitor {} into {}?", from.id.0, into.id.0),
        &NSFont::systemFontOfSize_weight(13.0, unsafe { NSFontWeightSemibold }),
        &NSColor::labelColor(),
        TOP - 1.0,
        17.0,
    );
    let detail = match from.all_windows {
        0 => "No windows to move".to_string(),
        1 => "1 window moves, on every workspace".to_string(),
        n => format!("{n} windows move, on every workspace"),
    };
    line(
        &detail,
        &NSFont::systemFontOfSize(11.0),
        &NSColor::secondaryLabelColor(),
        TOP + 16.0,
        14.0,
    );

    let (cancel, merge) = buttons(bounds);
    let label_font = NSFont::systemFontOfSize(12.0);
    let label = |r: NSRect, s: &str, font: &NSFont, color: &NSColor| {
        text(s, font, color).drawInRect(NSRect::new(
            NSPoint::new(r.origin.x, r.origin.y + 2.5),
            NSSize::new(r.size.width, 16.0),
        ));
    };
    let c = rounded(inset(cancel, 0.5), BUTTON_R);
    NSColor::labelColor().colorWithAlphaComponent(0.08).setFill();
    c.fill();
    NSColor::labelColor().colorWithAlphaComponent(0.2).setStroke();
    c.setLineWidth(1.0);
    c.stroke();
    label(cancel, "Cancel", &label_font, &NSColor::labelColor());
    NSColor::controlAccentColor().setFill();
    rounded(merge, BUTTON_R).fill();
    label(
        merge,
        "Merge",
        &NSFont::systemFontOfSize_weight(12.0, unsafe { NSFontWeightSemibold }),
        &NSColor::whiteColor(),
    );
}

fn tile_rect(i: usize) -> NSRect {
    NSRect::new(
        NSPoint::new(
            MARGIN_X + FRAME_PAD + i as f64 * (TILE_W + TILE_GAP),
            TOP + FRAME_PAD,
        ),
        NSSize::new(TILE_W, TILE_H),
    )
}

/// Where the plus sits: after the last tile, narrower than one, so it reads
/// as a control rather than a monitor.
fn add_rect(n: usize) -> NSRect {
    let after = tile_rect(n).origin.x;
    NSRect::new(NSPoint::new(after, TOP + FRAME_PAD), NSSize::new(ADD_W, TILE_H))
}

fn draw_add(r: NSRect, pressed: bool) {
    let path = rounded(inset(r, 0.5), TILE_R);
    if pressed {
        NSColor::labelColor().colorWithAlphaComponent(0.12).setFill();
        path.fill();
    }
    let dash: [f64; 2] = [3.0, 2.0];
    unsafe { path.setLineDash_count_phase(dash.as_ptr(), 2, 0.0) };
    NSColor::secondaryLabelColor().setStroke();
    path.setLineWidth(1.0);
    path.stroke();
    text(
        "+",
        &NSFont::systemFontOfSize_weight(16.0, unsafe { NSFontWeightSemibold }),
        &NSColor::secondaryLabelColor(),
    )
    .drawInRect(NSRect::new(
        NSPoint::new(r.origin.x, r.origin.y + (r.size.height - 20.0) / 2.0),
        NSSize::new(r.size.width, 20.0),
    ));
}

fn draw_tile(r: NSRect, number: u8, windows: usize, shown: bool, viewed: bool) {
    let path = rounded(inset(r, 0.5), TILE_R);
    let ink = if shown {
        NSColor::labelColor()
    } else {
        NSColor::tertiaryLabelColor()
    };
    if shown {
        NSColor::labelColor()
            .colorWithAlphaComponent(0.08)
            .setFill();
        path.fill();
        NSColor::labelColor()
            .colorWithAlphaComponent(0.3)
            .setStroke();
    } else {
        // Dashed: there, but not on any display.
        let dash: [f64; 2] = [3.0, 2.0];
        unsafe { path.setLineDash_count_phase(dash.as_ptr(), 2, 0.0) };
        NSColor::tertiaryLabelColor().setStroke();
    }
    path.setLineWidth(1.0);
    path.stroke();

    // The anchor Cmd+Alt+J/K step from: its number in the accent, not its
    // outline, which would merge into the frame's edge.
    let number_color = if viewed {
        NSColor::controlAccentColor()
    } else {
        ink.clone()
    };
    let label = text(
        &number.to_string(),
        &NSFont::systemFontOfSize_weight(13.0, unsafe { NSFontWeightSemibold }),
        &number_color,
    );
    label.drawInRect(NSRect::new(
        NSPoint::new(r.origin.x, r.origin.y + 6.0),
        NSSize::new(r.size.width, 17.0),
    ));

    // One dot per window of the current workspace living here.
    let dots = windows.min(MAX_DOTS);
    if dots > 0 {
        let row = dots as f64 * DOT + (dots - 1) as f64 * DOT_GAP;
        let mut x = r.origin.x + (r.size.width - row) / 2.0;
        let y = r.origin.y + r.size.height - 10.0;
        ink.setFill();
        for _ in 0..dots {
            NSBezierPath::bezierPathWithOvalInRect(NSRect::new(
                NSPoint::new(x, y),
                NSSize::new(DOT, DOT),
            ))
            .fill();
            x += DOT + DOT_GAP;
        }
    }
}

fn describe(view: &MonitorsView) -> String {
    let parts: Vec<String> = view
        .monitors
        .iter()
        .map(|m| match m.display {
            Some(d) => format!("monitor {} on display {}", m.id.0, d + 1),
            None => format!("monitor {} hidden", m.id.0),
        })
        .collect();
    format!("Monitors: {}", parts.join(", "))
}

fn text(s: &str, font: &NSFont, color: &NSColor) -> Retained<NSAttributedString> {
    let style = NSMutableParagraphStyle::new();
    style.setAlignment(NSTextAlignment::Center);
    style.setLineBreakMode(NSLineBreakMode::ByTruncatingTail);
    let attrs = NSDictionary::from_slices(
        unsafe {
            &[
                NSFontAttributeName,
                NSForegroundColorAttributeName,
                NSParagraphStyleAttributeName,
            ]
        },
        &[&**font as &objc2::runtime::AnyObject, &**color, &**style],
    );
    unsafe {
        NSAttributedString::initWithString_attributes(
            NSAttributedString::alloc(),
            &NSString::from_str(s),
            Some(&attrs),
        )
    }
}
