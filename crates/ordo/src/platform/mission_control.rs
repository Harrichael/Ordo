//! Space switching by pulling Mission Control's own lever.
//!
//! Every SIP-free private mechanism half-works or no-ops on Tahoe (see
//! issues.txt for the graveyard), so the native backend switches Spaces the
//! one way the OS fully supports: synthesizing the "Move left/right a space"
//! keyboard shortcut. Two facts discovered on-device make this workable:
//!   - the shortcut acts on the display UNDER THE MOUSE POINTER, so warping
//!     the pointer steers which display switches (the caller restores it);
//!   - the user rebound the shortcut to Ctrl+Alt+Cmd+arrows so it can't fight
//!     their Hammerspoon Ctrl+arrow bindings — the chord below must match
//!     System Settings, and reading it from com.apple.symbolichotkeys instead
//!     of hardcoding is an open issue.
//!
//! Ordo's own event tap sees these synthesized presses and passes them
//! through (Ctrl+Alt+Cmd+arrow matches none of its chords).

use std::thread::sleep;
use std::time::Duration;

use objc2_core_graphics::{
    CGEvent, CGEventFlags, CGEventSource, CGEventSourceStateID, CGEventTapLocation,
};

const KEY_LEFT: u16 = 0x7B;
const KEY_RIGHT: u16 = 0x7C;

/// Between repeated presses: Mission Control animates each slide, and a press
/// landing mid-animation gets coalesced or dropped.
const PRESS_GAP: Duration = Duration::from_millis(350);

/// Press the lever `times` times toward `direction` (+1 right, -1 left) on
/// whichever display currently holds the pointer.
pub fn switch(direction: i64, times: u64) {
    let key = if direction >= 0 { KEY_RIGHT } else { KEY_LEFT };
    for i in 0..times {
        if i > 0 {
            sleep(PRESS_GAP);
        }
        press(key);
    }
}

fn press(key: u16) {
    // A HID-state source, and the exact flag set the hotkey is stored with
    // (Ctrl+Alt+Cmd plus the secondary-fn bit arrow keys carry) — symbolic
    // hotkeys ignore source-less or wrongly-flagged events.
    let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState);
    let flags = CGEventFlags::MaskControl
        | CGEventFlags::MaskAlternate
        | CGEventFlags::MaskCommand
        | CGEventFlags::MaskSecondaryFn;
    for down in [true, false] {
        let Some(ev) = CGEvent::new_keyboard_event(source.as_deref(), key, down) else {
            continue;
        };
        CGEvent::set_flags(Some(&ev), flags);
        CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&ev));
        sleep(Duration::from_millis(30));
    }
}
