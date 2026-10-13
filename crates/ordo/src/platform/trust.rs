//! Accessibility trust: whether macOS lets Ordo watch the keyboard and read
//! and move other apps' windows.
//!
//! Trust is checked, not cached: macOS judges it at tap creation and on each
//! Accessibility call, so a grant made while Ordo runs takes effect in this
//! process. Until then Ordo waits with the tap and observers down, and the
//! menu bar says what it needs.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crossbeam_channel::Sender;
use objc2_application_services::{
    kAXTrustedCheckOptionPrompt, AXIsProcessTrusted, AXIsProcessTrustedWithOptions,
};
use objc2_core_foundation::{CFBoolean, CFDictionary, CFString};

use ordo_core::RescanTrigger;

use crate::engine::Msg;

/// A local check, cheap enough to poll.
const POLL: Duration = Duration::from_secs(2);

const PANE: &str = "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility";

pub fn trusted() -> bool {
    unsafe { AXIsProcessTrusted() }
}

/// macOS's own prompt, and the pane where the grant is made.
pub fn ask() {
    let key: &CFString = unsafe { kAXTrustedCheckOptionPrompt };
    let options = CFDictionary::<CFString, CFBoolean>::from_slices(&[key], &[CFBoolean::new(true)]);
    unsafe { AXIsProcessTrustedWithOptions(Some(options.as_ref())) };
    let _ = std::process::Command::new("/usr/bin/open").arg(PANE).spawn();
}

/// Run `bring_up` (true once what it raised is up) now if Ordo is trusted,
/// else as soon as it is. `needs` holds while it waits: untrusted, or trusted
/// with the tap refused, the other sign of a missing grant. A late bring-up
/// asks for a look at once, rather than at the next periodic one.
pub fn when_trusted(
    needs: Arc<AtomicBool>,
    tx: Sender<Msg>,
    mut bring_up: impl FnMut() -> bool + Send + 'static,
) {
    if trusted() && bring_up() {
        return;
    }
    needs.store(true, Ordering::Relaxed);
    eprintln!("ordo: needs Accessibility (System Settings > Privacy & Security > Accessibility); waiting for the grant.");
    std::thread::spawn(move || loop {
        std::thread::sleep(POLL);
        if !(trusted() && bring_up()) {
            continue;
        }
        needs.store(false, Ordering::Relaxed);
        eprintln!("ordo: Accessibility granted; hotkeys and window tracking are up.");
        let _ = tx.send(Msg::Rescan(RescanTrigger::BackendHint {
            kind: "accessibility_granted".into(),
        }));
        return;
    });
}
