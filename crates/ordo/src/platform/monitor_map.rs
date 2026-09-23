//! The monitors diagram in the menu bar item's menu: a tile per virtual
//! monitor, left to right, and over them a frame for the physical displays —
//! the part of the row that is actually on screen, named display by display.
//! When the view changes while the menu is open, the frame slides.
//!
//! The frame is its own subview, carrying the display names beneath it, so a
//! view change animates as one thing moving rather than a redraw.

use std::cell::{OnceCell, RefCell};

use objc2::rc::Retained;
use objc2::{define_class, msg_send, AnyThread, DefinedClass, MainThreadOnly};
use objc2_app_kit::{
    NSAccessibility, NSAnimatablePropertyContainer, NSAnimationContext,
    NSAttributedStringNSStringDrawing, NSBezierPath, NSColor, NSFont, NSFontAttributeName,
    NSFontWeightSemibold, NSForegroundColorAttributeName, NSLineBreakMode, NSMutableParagraphStyle,
    NSParagraphStyleAttributeName, NSTextAlignment, NSView,
};
use objc2_foundation::{
    MainThreadMarker, NSAttributedString, NSDictionary, NSPoint, NSRect, NSSize, NSString,
};

use crate::menubar::MonitorsView;
use crate::platform::display::{DisplayLabel, MirrorPanel};

const MARGIN_X: f64 = 20.0;
const TOP: f64 = 4.0;
const TILE_W: f64 = 72.0;
const TILE_H: f64 = 42.0;
const TILE_GAP: f64 = 10.0;
const TILE_R: f64 = 5.0;
const FRAME_PAD: f64 = 5.0;
const FRAME_R: f64 = 9.0;
const FRAME_LINE: f64 = 2.0;
const LABEL_GAP: f64 = 3.0;
const LINE_H: f64 = 13.0;
const BOTTOM: f64 = 6.0;
const DOT: f64 = 4.0;
const DOT_GAP: f64 = 3.0;
/// Windows shown as dots before the row stops growing.
const MAX_DOTS: usize = 6;
const SLIDE_SECS: f64 = 0.3;

/// A display's name under the frame, centered over the tiles it shows.
struct Caption {
    /// In the frame's own coordinates, so the name travels with it.
    center_x: f64,
    width: f64,
    name: String,
    mirror: Option<&'static str>,
}

#[derive(Default)]
pub struct MapIvars {
    view: RefCell<Option<MonitorsView>>,
    frame: OnceCell<Retained<DisplayFrame>>,
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
            for (i, m) in view.monitors.iter().enumerate() {
                draw_tile(tile_rect(i), m.id.0, m.windows, m.display.is_some(), m.id == view.viewed);
            }
        }
    }
);

#[derive(Default)]
struct FrameIvars {
    captions: RefCell<Vec<Caption>>,
}

define_class!(
    // SAFETY: as for MonitorMap.
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "OrdoDisplayFrame"]
    #[ivars = FrameIvars]
    struct DisplayFrame;

    impl DisplayFrame {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty: NSRect) {
            let w = self.frame().size.width;
            let outline = NSRect::new(
                NSPoint::new(FRAME_LINE / 2.0, FRAME_LINE / 2.0),
                NSSize::new(w - FRAME_LINE, TILE_H + 2.0 * FRAME_PAD - FRAME_LINE),
            );
            let path =
                NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(outline, FRAME_R, FRAME_R);
            let accent = NSColor::controlAccentColor();
            accent.colorWithAlphaComponent(0.12).setFill();
            path.fill();
            accent.setStroke();
            path.setLineWidth(FRAME_LINE);
            path.stroke();

            let name_y = TILE_H + 2.0 * FRAME_PAD + LABEL_GAP;
            for c in self.ivars().captions.borrow().iter() {
                let x = c.center_x - c.width / 2.0;
                let name = text(&c.name, &NSFont::systemFontOfSize(10.0), &NSColor::secondaryLabelColor());
                name.drawInRect(NSRect::new(NSPoint::new(x, name_y), NSSize::new(c.width, LINE_H)));
                if let Some(mirror) = c.mirror {
                    let m = text(mirror, &NSFont::systemFontOfSize(9.0), &NSColor::tertiaryLabelColor());
                    m.drawInRect(NSRect::new(
                        NSPoint::new(x, name_y + LINE_H),
                        NSSize::new(c.width, LINE_H),
                    ));
                }
            }
        }
    }
);

impl MonitorMap {
    pub fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(MapIvars::default());
        let this: Retained<Self> = unsafe { msg_send![super(this), initWithFrame: NSRect::ZERO] };
        // Layer-backed so the slide is Core Animation's, which keeps running
        // while the menu holds the run loop in its tracking mode.
        this.setWantsLayer(true);
        let frame = DisplayFrame::alloc(mtm).set_ivars(FrameIvars::default());
        let frame: Retained<DisplayFrame> =
            unsafe { msg_send![super(frame), initWithFrame: NSRect::ZERO] };
        this.addSubview(&frame);
        let _ = this.ivars().frame.set(frame);
        this
    }

    /// Show `view`, sliding the display frame from where it stood when
    /// `animate` — the menu is open and the user just watched it change.
    pub fn show(
        &self,
        view: &MonitorsView,
        labels: &std::collections::HashMap<ordo_core::MonitorId, DisplayLabel>,
        animate: bool,
    ) {
        let n = view.monitors.len();
        let captions = captions(view, labels);
        let mirrored = captions.iter().any(|c| c.mirror.is_some());
        let label_lines = if mirrored { 2.0 } else { 1.0 };
        let frame_h = TILE_H + 2.0 * FRAME_PAD + LABEL_GAP + label_lines * LINE_H;
        self.setFrameSize(NSSize::new(
            2.0 * MARGIN_X
                + 2.0 * FRAME_PAD
                + n as f64 * TILE_W
                + n.saturating_sub(1) as f64 * TILE_GAP,
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
            (Some(&first), Some(&last)) => {
                let x = tile_rect(first).origin.x - FRAME_PAD;
                let w = tile_rect(last).origin.x + TILE_W + FRAME_PAD - x;
                let rect = NSRect::new(NSPoint::new(x, TOP), NSSize::new(w, frame_h));
                *frame.ivars().captions.borrow_mut() = captions
                    .into_iter()
                    .map(|c| Caption {
                        center_x: c.center_x - x,
                        ..c
                    })
                    .collect();
                frame.setHidden(false);
                frame.setNeedsDisplay(true);
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

        self.setAccessibilityLabel(Some(&NSString::from_str(&describe(view, labels))));
        *self.ivars().view.borrow_mut() = Some(view.clone());
        self.setNeedsDisplay(true);
    }
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

fn draw_tile(r: NSRect, number: u8, windows: usize, shown: bool, viewed: bool) {
    let inset = NSRect::new(
        NSPoint::new(r.origin.x + 0.5, r.origin.y + 0.5),
        NSSize::new(r.size.width - 1.0, r.size.height - 1.0),
    );
    let path = NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(inset, TILE_R, TILE_R);
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

/// One caption per display, centered over the tiles it shows — under
/// collapse the rightmost display shows several.
fn captions(
    view: &MonitorsView,
    labels: &std::collections::HashMap<ordo_core::MonitorId, DisplayLabel>,
) -> Vec<Caption> {
    view.displays
        .iter()
        .enumerate()
        .filter_map(|(d, id)| {
            let tiles: Vec<usize> = view
                .monitors
                .iter()
                .enumerate()
                .filter(|(_, m)| m.display == Some(d))
                .map(|(i, _)| i)
                .collect();
            let (first, last) = (*tiles.first()?, *tiles.last()?);
            let left = tile_rect(first).origin.x;
            let right = tile_rect(last).origin.x + TILE_W;
            let label = labels.get(id);
            Some(Caption {
                center_x: (left + right) / 2.0,
                width: right - left + TILE_GAP,
                name: label.map_or_else(|| format!("Display {}", d + 1), |l| l.name.clone()),
                mirror: label.and_then(|l| l.mirrors.first()).map(|m| match m {
                    MirrorPanel::BuiltIn => "+ built-in mirror",
                    MirrorPanel::External => "+ mirror",
                }),
            })
        })
        .collect()
}

fn describe(
    view: &MonitorsView,
    labels: &std::collections::HashMap<ordo_core::MonitorId, DisplayLabel>,
) -> String {
    let parts: Vec<String> = view
        .monitors
        .iter()
        .map(|m| match m.display {
            Some(d) => {
                let name = view
                    .displays
                    .get(d)
                    .and_then(|id| labels.get(id))
                    .map_or_else(|| format!("display {}", d + 1), |l| l.name.clone());
                format!("monitor {} on {name}", m.id.0)
            }
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
