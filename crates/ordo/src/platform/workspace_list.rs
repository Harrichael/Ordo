//! The workspaces in the menu bar item's menu: a row per workspace, which a
//! click switches to and a drag moves to another place in the order.
//!
//! One view for every row, not a menu item each, because a drag must cross
//! rows: AppKit hands the drag's events to the view the press began in.
//! Being one view, it draws the menu's own hover highlight itself.
//!
//! A drop renumbers at once, here, rather than waiting for the engine's next
//! view: the rows would otherwise jump back for a moment and then forward.

use std::cell::{Cell, RefCell};

use objc2::rc::Retained;
use objc2::{define_class, msg_send, AnyThread, DefinedClass, MainThreadOnly};
use objc2_app_kit::{
    NSAccessibility, NSAttributedStringNSStringDrawing, NSAutoresizingMaskOptions, NSBezierPath,
    NSColor, NSEvent, NSFont, NSFontAttributeName, NSFontWeightSemibold,
    NSForegroundColorAttributeName, NSLineBreakMode, NSMutableParagraphStyle,
    NSParagraphStyleAttributeName, NSTextAlignment, NSTrackingArea, NSTrackingAreaOptions, NSView,
};
use objc2_foundation::{
    MainThreadMarker, NSAttributedString, NSDictionary, NSPoint, NSRect, NSSize, NSString,
};

use ordo_core::{after_move, HotkeyAction, WorkspaceId};

use crate::keys;

const ROW_H: f64 = 22.0;
const PAD_Y: f64 = 1.0;
/// The highlight's inset from the menu's edges, as macOS draws its own.
const INSET_X: f64 = 5.0;
const HIGHLIGHT_R: f64 = 4.0;
const BADGE_X: f64 = 14.0;
const BADGE: f64 = 15.0;
const TITLE_X: f64 = 38.0;
const LEGEND_PAD: f64 = 14.0;
const LEGEND_GAP: f64 = 24.0;
const DRAG_SLOP: f64 = 3.0;

/// One workspace's row. Its number is its place in the list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    /// Its apps, most recently used first; empty for no windows.
    pub apps: String,
}

/// A row in the hand: which one, where the pointer took it, and where the
/// pointer is now.
#[derive(Clone, Copy)]
struct Drag {
    from: usize,
    grab_y: f64,
    at: NSPoint,
    moved: bool,
}

pub struct ListIvars {
    rows: RefCell<Vec<Row>>,
    current: Cell<Option<usize>>,
    engaged: Cell<bool>,
    hover: Cell<Option<usize>>,
    drag: Cell<Option<Drag>>,
    on_command: Box<dyn Fn(HotkeyAction)>,
}

define_class!(
    // SAFETY: NSView has no subclassing requirements beyond these overrides,
    // and WorkspaceList does not implement Drop.
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "OrdoWorkspaceList"]
    #[ivars = ListIvars]
    pub struct WorkspaceList;

    impl WorkspaceList {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty: NSRect) {
            let ivars = self.ivars();
            let rows = ivars.rows.borrow();
            let width = self.bounds().size.width;
            let engaged = ivars.engaged.get();
            let drag = ivars.drag.get().filter(|d| d.moved);
            for (i, row) in rows.iter().enumerate() {
                let r = row_rect(i, width);
                if drag.is_some_and(|d| d.from == i) {
                    draw_slot(r);
                    continue;
                }
                let lit = engaged && drag.is_none() && ivars.hover.get() == Some(i);
                draw_row(r, i, row, ivars.current.get() == Some(i), lit, engaged);
            }
            if let Some(d) = drag {
                let g = gap_at(rows.len(), d.at.y);
                if g != d.from && g != d.from + 1 {
                    draw_insertion(g, width);
                }
                let r = NSRect::new(
                    NSPoint::new(0.0, d.at.y - d.grab_y),
                    NSSize::new(width, ROW_H),
                );
                draw_lifted(r);
                draw_row(r, d.from, &rows[d.from], ivars.current.get() == Some(d.from), false, true);
            }
        }

        #[unsafe(method(mouseEntered:))]
        fn mouse_entered(&self, event: &NSEvent) {
            self.hover_at(self.local(event));
        }

        #[unsafe(method(mouseMoved:))]
        fn mouse_moved(&self, event: &NSEvent) {
            self.hover_at(self.local(event));
        }

        #[unsafe(method(mouseExited:))]
        fn mouse_exited(&self, _event: &NSEvent) {
            if self.ivars().hover.take().is_some() {
                self.redraw();
            }
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            let ivars = self.ivars();
            if !ivars.engaged.get() {
                return;
            }
            let p = self.local(event);
            if let Some(i) = row_at(ivars.rows.borrow().len(), p.y) {
                ivars.drag.set(Some(Drag {
                    from: i,
                    grab_y: p.y - row_top(i),
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
            d.moved |= (d.at.y - (row_top(d.from) + d.grab_y)).abs() > DRAG_SLOP;
            self.ivars().drag.set(Some(d));
            self.redraw();
        }

        #[unsafe(method(mouseUp:))]
        fn mouse_up(&self, event: &NSEvent) {
            let Some(d) = self.ivars().drag.take() else {
                return;
            };
            let p = self.local(event);
            let n = self.ivars().rows.borrow().len();
            if !d.moved {
                if row_at(n, p.y) == Some(d.from) {
                    let ws = WorkspaceId(d.from as u8 + 1);
                    (self.ivars().on_command)(HotkeyAction::WorkspaceSwitchTo(ws));
                    self.close_menu();
                }
                return;
            }
            let g = gap_at(n, p.y);
            let to = if g > d.from { g - 1 } else { g };
            if to != d.from {
                (self.ivars().on_command)(HotkeyAction::MoveWorkspace {
                    from: WorkspaceId(d.from as u8 + 1),
                    to: WorkspaceId(to as u8 + 1),
                });
                self.reorder(d.from, to);
            }
            self.hover_at(p);
        }
    }
);

impl WorkspaceList {
    /// `on_command` is handed what the rows ask for: a switch, or a move.
    pub fn new(mtm: MainThreadMarker, on_command: Box<dyn Fn(HotkeyAction)>) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(ListIvars {
            rows: RefCell::new(Vec::new()),
            current: Cell::new(None),
            engaged: Cell::new(false),
            hover: Cell::new(None),
            drag: Cell::new(None),
            on_command,
        });
        let this: Retained<Self> = unsafe { msg_send![super(this), initWithFrame: NSRect::ZERO] };
        this.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable);
        // InVisibleRect: the area follows the view's size by itself.
        let area = unsafe {
            NSTrackingArea::initWithRect_options_owner_userInfo(
                NSTrackingArea::alloc(),
                NSRect::ZERO,
                NSTrackingAreaOptions::MouseEnteredAndExited
                    | NSTrackingAreaOptions::MouseMoved
                    | NSTrackingAreaOptions::ActiveAlways
                    | NSTrackingAreaOptions::InVisibleRect,
                Some(&this),
                None,
            )
        };
        this.addTrackingArea(&area);
        this
    }

    /// Show `rows`, workspace 1 first. Left alone mid-drag: the rows are
    /// the ones the hand is moving among. `engaged`: Ordo acts on commands,
    /// so a switch or a move would be carried out.
    pub fn show(&self, rows: Vec<Row>, current: Option<usize>, engaged: bool, fresh: bool) {
        let ivars = self.ivars();
        if fresh {
            ivars.drag.set(None);
            ivars.hover.set(None);
        } else if ivars.drag.get().is_some() {
            return;
        }
        ivars.engaged.set(engaged);
        ivars.current.set(current);
        let font = NSFont::menuFontOfSize(0.0);
        let widest = rows
            .iter()
            .enumerate()
            .map(|(i, row)| {
                let title = text(title_of(row), &font, &NSColor::labelColor()).size().width;
                let legend = legend(i).map_or(0.0, |l| {
                    LEGEND_GAP + text(&l, &font, &NSColor::labelColor()).size().width
                });
                title + legend
            })
            .fold(0.0, f64::max);
        self.setFrameSize(NSSize::new(
            TITLE_X + widest + LEGEND_PAD,
            rows.len() as f64 * ROW_H + 2.0 * PAD_Y,
        ));
        self.setToolTip(
            engaged
                .then_some("Drag a workspace to reorder")
                .map(NSString::from_str)
                .as_deref(),
        );
        self.setAccessibilityLabel(Some(&NSString::from_str(&describe(&rows, current))));
        *ivars.rows.borrow_mut() = rows;
        self.redraw();
    }

    /// The rows as the move leaves them: each keeps its apps and takes the
    /// number of its new place, the current one too.
    fn reorder(&self, from: usize, to: usize) {
        let ivars = self.ivars();
        let mut rows = ivars.rows.borrow_mut();
        let row = rows.remove(from);
        rows.insert(to, row);
        let moved = |i: usize| after_move(i as u8, from as u8, to as u8) as usize;
        ivars.current.set(ivars.current.get().map(moved));
        drop(rows);
        self.redraw();
    }

    fn hover_at(&self, p: NSPoint) {
        let at = row_at(self.ivars().rows.borrow().len(), p.y);
        if self.ivars().hover.replace(at) != at {
            self.redraw();
        }
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

    fn close_menu(&self) {
        // SAFETY: the item is in the menu that is showing this view.
        if let Some(menu) = self.enclosingMenuItem().and_then(|item| unsafe { item.menu() }) {
            menu.cancelTracking();
        }
    }
}

fn row_top(i: usize) -> f64 {
    PAD_Y + i as f64 * ROW_H
}

fn row_rect(i: usize, width: f64) -> NSRect {
    NSRect::new(NSPoint::new(0.0, row_top(i)), NSSize::new(width, ROW_H))
}

fn row_at(n: usize, y: f64) -> Option<usize> {
    let i = ((y - PAD_Y) / ROW_H).floor();
    (i >= 0.0 && (i as usize) < n).then_some(i as usize)
}

/// The gap between rows nearest `y`: 0 before the first, `n` after the last.
fn gap_at(n: usize, y: f64) -> usize {
    (((y - PAD_Y) / ROW_H).round().max(0.0) as usize).min(n)
}

fn legend(i: usize) -> Option<String> {
    keys::key_for(&HotkeyAction::WorkspaceSwitchTo(WorkspaceId(i as u8 + 1)))
        .and_then(|k| k.legend())
}

fn title_of(row: &Row) -> &str {
    if row.apps.is_empty() {
        "Empty"
    } else {
        &row.apps
    }
}

fn draw_row(r: NSRect, i: usize, row: &Row, current: bool, lit: bool, engaged: bool) {
    let y = r.origin.y;
    if lit {
        let h = NSRect::new(
            NSPoint::new(INSET_X, y),
            NSSize::new(r.size.width - 2.0 * INSET_X, ROW_H),
        );
        NSColor::selectedContentBackgroundColor().setFill();
        NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(h, HIGHLIGHT_R, HIGHLIGHT_R).fill();
    }
    let (ink, faint) = match (lit, engaged) {
        (true, _) => (
            NSColor::selectedMenuItemTextColor(),
            NSColor::selectedMenuItemTextColor(),
        ),
        (false, true) => (NSColor::labelColor(), NSColor::secondaryLabelColor()),
        (false, false) => (NSColor::tertiaryLabelColor(), NSColor::tertiaryLabelColor()),
    };

    // The number in a square, filled for the current workspace as its pill
    // is in the icon.
    let badge = NSRect::new(
        NSPoint::new(BADGE_X, y + (ROW_H - BADGE) / 2.0),
        NSSize::new(BADGE, BADGE),
    );
    let path = NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(
        NSRect::new(
            NSPoint::new(badge.origin.x + 0.5, badge.origin.y + 0.5),
            NSSize::new(BADGE - 1.0, BADGE - 1.0),
        ),
        3.0,
        3.0,
    );
    let number_font = NSFont::systemFontOfSize_weight(10.0, unsafe { NSFontWeightSemibold });
    let number = (i + 1).to_string();
    if current {
        ink.setFill();
        path.fill();
        let knocked = if lit {
            NSColor::selectedContentBackgroundColor()
        } else {
            NSColor::windowBackgroundColor()
        };
        centered(&number, &number_font, &knocked, badge, 13.0);
    } else {
        ink.setStroke();
        path.setLineWidth(1.0);
        path.stroke();
        centered(&number, &number_font, &ink, badge, 13.0);
    }

    let font = NSFont::menuFontOfSize(0.0);
    let title_ink = if row.apps.is_empty() { &faint } else { &ink };
    let legend = legend(i);
    let legend_w = legend
        .as_deref()
        .map_or(0.0, |l| text(l, &font, &faint).size().width);
    let title_w = r.size.width - TITLE_X - LEGEND_PAD - legend_w - if legend.is_some() { 8.0 } else { 0.0 };
    text(title_of(row), &font, title_ink).drawInRect(NSRect::new(
        NSPoint::new(TITLE_X, y + 3.0),
        NSSize::new(title_w.max(0.0), 17.0),
    ));
    if let Some(l) = legend {
        text(&l, &font, &faint).drawAtPoint(NSPoint::new(
            r.size.width - LEGEND_PAD - legend_w,
            y + 3.0,
        ));
    }
}

/// Where the row in the hand came from.
fn draw_slot(r: NSRect) {
    let path = NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(
        NSRect::new(
            NSPoint::new(INSET_X + 0.5, r.origin.y + 0.5),
            NSSize::new(r.size.width - 2.0 * INSET_X - 1.0, ROW_H - 1.0),
        ),
        HIGHLIGHT_R,
        HIGHLIGHT_R,
    );
    let dash: [f64; 2] = [3.0, 2.0];
    unsafe { path.setLineDash_count_phase(dash.as_ptr(), 2, 0.0) };
    NSColor::tertiaryLabelColor().setStroke();
    path.setLineWidth(1.0);
    path.stroke();
}

/// Backing for the row in the hand, so the rows it passes over don't show
/// through.
fn draw_lifted(r: NSRect) {
    let path = NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(
        NSRect::new(
            NSPoint::new(INSET_X, r.origin.y),
            NSSize::new(r.size.width - 2.0 * INSET_X, ROW_H),
        ),
        HIGHLIGHT_R,
        HIGHLIGHT_R,
    );
    NSColor::windowBackgroundColor().setFill();
    path.fill();
    NSColor::controlAccentColor().setStroke();
    path.setLineWidth(1.5);
    path.stroke();
}

/// Where a drop would put the row: a line across the gap, a dot at its head.
fn draw_insertion(gap: usize, width: f64) {
    let y = row_top(gap);
    let accent = NSColor::controlAccentColor();
    accent.setFill();
    NSBezierPath::bezierPathWithOvalInRect(NSRect::new(
        NSPoint::new(INSET_X, y - 3.0),
        NSSize::new(6.0, 6.0),
    ))
    .fill();
    NSBezierPath::fillRect(NSRect::new(
        NSPoint::new(INSET_X + 5.0, y - 1.0),
        NSSize::new(width - 2.0 * INSET_X - 5.0, 2.0),
    ));
}

fn describe(rows: &[Row], current: Option<usize>) -> String {
    let parts: Vec<String> = rows
        .iter()
        .enumerate()
        .map(|(i, row)| {
            let mark = if current == Some(i) { " (current)" } else { "" };
            format!("workspace {}{mark}: {}", i + 1, title_of(row))
        })
        .collect();
    format!("Workspaces: {}", parts.join("; "))
}

fn centered(s: &str, font: &NSFont, color: &NSColor, r: NSRect, h: f64) {
    styled(s, font, color, NSTextAlignment::Center).drawInRect(NSRect::new(
        NSPoint::new(r.origin.x, r.origin.y + (r.size.height - h) / 2.0),
        NSSize::new(r.size.width, h),
    ));
}

fn text(s: &str, font: &NSFont, color: &NSColor) -> Retained<NSAttributedString> {
    styled(s, font, color, NSTextAlignment::Left)
}

fn styled(
    s: &str,
    font: &NSFont,
    color: &NSColor,
    align: NSTextAlignment,
) -> Retained<NSAttributedString> {
    let style = NSMutableParagraphStyle::new();
    style.setAlignment(align);
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
