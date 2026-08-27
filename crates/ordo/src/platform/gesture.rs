//! Animation-free Space switching by synthesizing a dock-swipe gesture.
//!
//! macOS exposes no API to activate a Space. The community-reverse-engineered
//! trick (yabai, BetterTouchTool, InstantSpaceSwitcher) is to post a
//! high-velocity horizontal dock-swipe gesture, which Mission Control follows
//! without the slide animation. It needs only Accessibility, no SIP.
//!
//! CAVEAT: the CGEvent *field numbers* below are private and undocumented
//! (kCGSEventType=55, gesture HID type=110, motion axis=123, phase=132, and the
//! progress/velocity fields). They match the values yabai uses and want
//! on-device validation. Because switching is *relative* (N swipes from the
//! current index), the backend reads the current index immediately before
//! swiping, verifies afterward, and reports failure rather than trusting it —
//! so a wrong constant surfaces as an honest "switch failed", never a silent
//! desync.

use objc2_core_foundation::CGPoint;
use objc2_core_graphics::{CGEvent, CGEventField, CGEventTapLocation, CGEventType};

// Private CGEvent field selectors for dock-swipe gestures.
const FIELD_EVENT_TYPE: u32 = 55; // set to kCGSEventDockControl
const FIELD_GESTURE_HID_TYPE: u32 = 110; // set to kIOHIDEventTypeDockSwipe
const FIELD_GESTURE_MOTION: u32 = 123; // 1 = horizontal
const FIELD_GESTURE_PHASE: u32 = 132;
const FIELD_PROGRESS: u32 = 124;
const FIELD_VELOCITY_X: u32 = 129;

const CGS_EVENT_DOCK_CONTROL: i64 = 30;
const IOHID_EVENT_TYPE_DOCK_SWIPE: i64 = 23;
const MOTION_HORIZONTAL: i64 = 1;

// Gesture phases (IOKit gesture phase bitfield).
const PHASE_BEGAN: i64 = 1;
const PHASE_ENDED: i64 = 4;

/// One swipe to an adjacent space. `direction` is +1 for next (rightward) or -1
/// for previous. High velocity is what suppresses the animation.
pub fn swipe(direction: i64) {
    let sign = if direction >= 0 { -1.0 } else { 1.0 };
    // A begin/continue/end sequence; the large velocity is the animation skip.
    post(PHASE_BEGAN, 0.0, 0.0);
    post(0, sign, sign * 9999.0);
    post(PHASE_ENDED, sign, sign * 9999.0);
}

fn post(phase: i64, progress: f64, velocity: f64) {
    let Some(ev) = CGEvent::new(None) else {
        return;
    };
    CGEvent::set_integer_value_field(
        Some(&ev),
        CGEventField(FIELD_EVENT_TYPE),
        CGS_EVENT_DOCK_CONTROL,
    );
    CGEvent::set_integer_value_field(
        Some(&ev),
        CGEventField(FIELD_GESTURE_HID_TYPE),
        IOHID_EVENT_TYPE_DOCK_SWIPE,
    );
    CGEvent::set_integer_value_field(
        Some(&ev),
        CGEventField(FIELD_GESTURE_MOTION),
        MOTION_HORIZONTAL,
    );
    if phase != 0 {
        CGEvent::set_integer_value_field(Some(&ev), CGEventField(FIELD_GESTURE_PHASE), phase);
    }
    CGEvent::set_double_value_field(Some(&ev), CGEventField(FIELD_PROGRESS), progress);
    CGEvent::set_double_value_field(Some(&ev), CGEventField(FIELD_VELOCITY_X), velocity);
    // Belt-and-braces: some readers key off the event's declared type too.
    CGEvent::set_type(Some(&ev), CGEventType(29));
    CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&ev));
}

/// The current cursor position, so a caller can restore it after warping the
/// pointer around to aim gestures at specific displays.
pub fn cursor_position() -> Option<CGPoint> {
    CGEvent::new(None).map(|ev| CGEvent::location(Some(&ev)))
}
