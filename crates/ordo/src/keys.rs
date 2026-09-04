//! The keybinding table — pure, so the whole policy is testable without a live
//! event tap.
//!
//! v0's bindings, and only these (no inventing keys the user hasn't asked for):
//!   - Cmd+Left / Cmd+Right      → previous / next workspace
//!   - Cmd+Alt+1 … Cmd+Alt+9     → jump straight to that workspace (a
//!     workspace that doesn't exist is a no-op). Alt-qualified so apps keep
//!     their Cmd-digit tab switching.
//!   - Cmd+Shift+Left / Right    → move the focused window to the previous /
//!     next virtual monitor, dragging focus and mouse along with it
//!   - Cmd+Alt+J / Cmd+Alt+K     → view the previous / next virtual monitor
//!     (focus jumps to its MRU window; where displays are short, its windows
//!     are revealed and the current monitor's hidden)
//!   - Ctrl+Alt+Cmd+V            → virtualization on/off: off collapses every
//!     virtual monitor onto the displays present. A mode switch, not a
//!     navigation key, so it sits with the other full-triple chords
//!   - Ctrl+Cmd+Left / Right     → carry the focused window to the adjacent
//!     workspace and switch there with it
//!   - Alt+Tab                   → MRU window in the workspace
//!   - Alt+Shift+Tab             → MRU window in the workspace + monitor
//!   - Alt+Backtick              → MRU window in the workspace + app
//!   - Ctrl+Alt+Tab              → MRU window on the OTHER monitor
//!   - Alt+End                   → demote the focused window to the back of
//!     the MRU history and focus the next one
//!   - Ctrl+Alt+Cmd+Escape       → rescue candidate (engages on a double-press;
//!     the timing lives in the tap, not here)
//!   - Ctrl+Alt+Cmd+O            → engage: bring Ordo up WITH the state file
//!     (undo a rescue, or bring a --paused run alive). Deliberately a
//!     different key than rescue so mashing the panic chord can never
//!     re-engage the thing being escaped.
//!   - Ctrl+Alt+Cmd+R            → engage fresh: O's corollary — bring Ordo up
//!     WITHOUT the state file. Blank workspace model, and the file is neither
//!     read nor written until S or O says otherwise. Single press, like O.
//!   - Ctrl+Alt+Cmd+S            → save state: turn the state file back on
//!     (after an R bring-up) and persist the current arrangement as the new
//!     durable truth. Idempotent when persistence is already on.
//!
//! Modifier matching is deliberately strict so Ordo only swallows the exact
//! chord: Cmd+Shift+arrows are claimed (the user gave up select-to-line for
//! them, same trade as Cmd+arrows), but Cmd+Alt+arrows etc. pass through.
//!
//! A second, separate table ([`witness`]) names macOS's OWN focus-moving
//! chords — Cmd+Tab and Cmd+` — which Ordo never swallows but must see: they
//! are the user's intent about focus, and without a trace of them the focus
//! change they cause is indistinguishable from an app flinging focus around.

use ordo_core::{HotkeyAction, WorkspaceId};

/// macOS virtual key codes for the keys we bind.
mod code {
    pub const LEFT: u16 = 0x7B;
    pub const RIGHT: u16 = 0x7C;
    pub const TAB: u16 = 0x30;
    pub const GRAVE: u16 = 0x32; // backtick
    pub const ESCAPE: u16 = 0x35;
    pub const O: u16 = 0x1F;
    pub const R: u16 = 0x0F;
    pub const S: u16 = 0x01;
    pub const END: u16 = 0x77;
    pub const J: u16 = 0x26;
    pub const K: u16 = 0x28;
    pub const V: u16 = 0x09;

    /// Virtual keycodes for the top-row digits 1..9, in order. GOTCHA: these
    /// are not sequential and 5/6 are transposed — the layout is physical, not
    /// numeric, so indexing this table is the only safe way to map a digit.
    pub const DIGITS: [u16; 9] = [0x12, 0x13, 0x14, 0x15, 0x17, 0x16, 0x1A, 0x1C, 0x19];
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mods {
    pub cmd: bool,
    pub alt: bool,
    pub shift: bool,
    pub ctrl: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Chord {
    Hotkey(HotkeyAction),
    /// One press of the rescue combo. The tap decides whether it's the second
    /// within the window and thus actually engages rescue.
    RescueCandidate,
    /// (Re)engage Ordo, loading the state file: single press, honored even
    /// while disengaged.
    Engage,
    /// (Re)engage Ordo with a blank workspace model, leaving the state file
    /// untouched and unused: O's corollary, same handling.
    EngageFresh,
    /// Turn persistence back on and save the current model as the new durable
    /// state. Only meaningful while engaged, so it sits behind the
    /// interception gate like the ordinary hotkeys.
    SaveState,
}

/// A system focus gesture seen on a key-down. Passed through untouched; the
/// tap only reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Witness {
    /// Cmd+Tab (or Cmd+Shift+Tab): the app switcher is now up. It acts when
    /// Cmd is RELEASED, possibly seconds later, so the tap arms on this and
    /// reports the gesture on the release.
    AppSwitcherArmed,
    /// Cmd+` (or Cmd+Shift+`): the in-app window cycle, which acts at once.
    WindowCycle,
}

/// macOS's own focus-moving chords, checked on keys `match_chord` let through.
/// Alt is excluded because Ordo's own Alt+Tab / Alt+` never reach here, and
/// Ctrl because Ctrl+Tab is every browser's tab switch, not a focus move.
pub fn witness(keycode: u16, m: Mods) -> Option<Witness> {
    if !(m.cmd && !m.alt && !m.ctrl) {
        return None;
    }
    match keycode {
        code::TAB => Some(Witness::AppSwitcherArmed),
        code::GRAVE => Some(Witness::WindowCycle),
        _ => None,
    }
}

/// Which chord, if any, a key-down maps to. `None` means "not ours — pass it
/// through to the app".
pub fn match_chord(keycode: u16, m: Mods) -> Option<Chord> {
    use HotkeyAction::*;

    // Rescue first: it's the one combo that must be recognized even amid other
    // held modifiers, because it's the escape hatch. Engage matches the same
    // way — it's the other side of the same switch and must work while inert.
    if m.ctrl && m.alt && m.cmd && keycode == code::ESCAPE {
        return Some(Chord::RescueCandidate);
    }
    if m.ctrl && m.alt && m.cmd && keycode == code::O {
        return Some(Chord::Engage);
    }
    if m.ctrl && m.alt && m.cmd && keycode == code::R {
        return Some(Chord::EngageFresh);
    }
    if m.ctrl && m.alt && m.cmd && keycode == code::S {
        return Some(Chord::SaveState);
    }
    // A mode switch rather than navigation, so it lives with the full-triple
    // family — but unlike rescue/engage it is only ours while intercepting.
    if m.ctrl && m.alt && m.cmd && keycode == code::V {
        return Some(Chord::Hotkey(ToggleVirtualMonitors));
    }

    // Cmd+Alt+1..9: jump straight to that workspace. Alt is what keeps the
    // apps' Cmd-digit shortcuts — browser and terminal tab switching — which
    // are used far too often to swallow, unlike the Cmd+arrows Ordo already
    // takes. A workspace that does not exist is a no-op.
    if cmd_alt(m) {
        if let Some(i) = code::DIGITS.iter().position(|c| *c == keycode) {
            return Some(Chord::Hotkey(WorkspaceSwitchTo(WorkspaceId(i as u8 + 1))));
        }
        // Virtual monitors live under the same Cmd+Alt prefix as the direct
        // workspace jumps: J/K step the view (vim's down/up, here left/right).
        match keycode {
            code::J => return Some(Chord::Hotkey(ViewMonitorPrev)),
            code::K => return Some(Chord::Hotkey(ViewMonitorNext)),
            _ => {}
        }
    }

    match keycode {
        code::LEFT if only_cmd(m) => Some(Chord::Hotkey(WorkspacePrev)),
        code::RIGHT if only_cmd(m) => Some(Chord::Hotkey(WorkspaceNext)),
        code::LEFT if cmd_shift(m) => Some(Chord::Hotkey(MoveFocusedToMonitorPrev)),
        code::RIGHT if cmd_shift(m) => Some(Chord::Hotkey(MoveFocusedToMonitorNext)),
        code::LEFT if cmd_ctrl(m) => Some(Chord::Hotkey(CarryFocusedToWorkspacePrev)),
        code::RIGHT if cmd_ctrl(m) => Some(Chord::Hotkey(CarryFocusedToWorkspaceNext)),
        code::TAB if m.ctrl && m.alt && !m.cmd && !m.shift => Some(Chord::Hotkey(MruOtherMonitor)),
        code::TAB if m.alt && m.shift && !m.cmd && !m.ctrl => Some(Chord::Hotkey(MruMonitor)),
        code::TAB if m.alt && !m.shift && !m.cmd && !m.ctrl => Some(Chord::Hotkey(MruWorkspace)),
        code::GRAVE if m.alt && !m.cmd && !m.ctrl => Some(Chord::Hotkey(MruApp)),
        code::END if m.alt && !m.shift && !m.cmd && !m.ctrl => Some(Chord::Hotkey(MruDemote)),
        _ => None,
    }
}

/// Cmd, and none of the others — so Cmd+Alt/Ctrl+arrow keeps working.
fn only_cmd(m: Mods) -> bool {
    m.cmd && !m.alt && !m.shift && !m.ctrl
}

/// Cmd+Alt exactly.
fn cmd_alt(m: Mods) -> bool {
    m.cmd && m.alt && !m.shift && !m.ctrl
}

/// Cmd+Shift exactly.
fn cmd_shift(m: Mods) -> bool {
    m.cmd && m.shift && !m.alt && !m.ctrl
}

/// Ctrl+Cmd exactly. `!alt` is load-bearing beyond strictness: the native
/// backend synthesizes Ctrl+Alt+Cmd+arrows (the Mission Control lever), and
/// those must sail through our own tap untouched.
fn cmd_ctrl(m: Mods) -> bool {
    m.cmd && m.ctrl && !m.alt && !m.shift
}

#[cfg(test)]
mod tests {
    use super::*;
    use ordo_core::HotkeyAction::*;
    use ordo_core::WorkspaceId;

    fn mods(cmd: bool, alt: bool, shift: bool, ctrl: bool) -> Mods {
        Mods {
            cmd,
            alt,
            shift,
            ctrl,
        }
    }

    /// Digit keycodes are not sequential and 5/6 are transposed, so a mapping
    /// that looks right can silently send you to the wrong workspace. Pinned
    /// against the literal codes rather than the table it is derived from.
    #[test]
    fn cmd_alt_digit_jumps_to_that_workspace() {
        let chord = mods(true, true, false, false);
        for (keycode, want) in [
            (0x12, 1u8),
            (0x13, 2),
            (0x14, 3),
            (0x15, 4),
            (0x17, 5),
            (0x16, 6),
            (0x1A, 7),
            (0x1C, 8),
            (0x19, 9),
        ] {
            assert_eq!(
                match_chord(keycode, chord),
                Some(Chord::Hotkey(WorkspaceSwitchTo(WorkspaceId(want)))),
                "keycode {keycode:#x}"
            );
        }
        // Bare Cmd+digit is deliberately NOT ours: tab switching is used far
        // too often to swallow. Nor is any other combination.
        assert_eq!(match_chord(0x12, mods(true, false, false, false)), None);
        assert_eq!(match_chord(0x12, mods(true, false, true, false)), None);
        assert_eq!(match_chord(0x12, mods(false, true, false, false)), None);
        assert_eq!(match_chord(0x12, mods(false, false, false, false)), None);
        // 0 is not bound: workspace 10 would need it and nothing asks for one.
        assert_eq!(match_chord(0x1D, chord), None);
    }

    #[test]
    fn workspace_and_mru_chords_map_as_specified() {
        assert_eq!(
            match_chord(code::LEFT, mods(true, false, false, false)),
            Some(Chord::Hotkey(WorkspacePrev))
        );
        assert_eq!(
            match_chord(code::RIGHT, mods(true, false, false, false)),
            Some(Chord::Hotkey(WorkspaceNext))
        );
        assert_eq!(
            match_chord(code::TAB, mods(false, true, false, false)),
            Some(Chord::Hotkey(MruWorkspace))
        );
        assert_eq!(
            match_chord(code::TAB, mods(false, true, true, false)),
            Some(Chord::Hotkey(MruMonitor))
        );
        assert_eq!(
            match_chord(code::GRAVE, mods(false, true, false, false)),
            Some(Chord::Hotkey(MruApp))
        );
        assert_eq!(
            match_chord(code::TAB, mods(false, true, false, true)),
            Some(Chord::Hotkey(MruOtherMonitor))
        );
        // Ctrl+Alt+Cmd+Tab is nobody's chord — the full-triple family is
        // reserved for rescue/engage keys only.
        assert_eq!(match_chord(code::TAB, mods(true, true, false, true)), None);
    }

    #[test]
    fn cmd_alt_j_k_view_monitors_and_the_triple_v_toggles_virtualization() {
        let chord = mods(true, true, false, false);
        assert_eq!(
            match_chord(code::J, chord),
            Some(Chord::Hotkey(ViewMonitorPrev))
        );
        assert_eq!(
            match_chord(code::K, chord),
            Some(Chord::Hotkey(ViewMonitorNext))
        );
        // The toggle is a full-triple chord, and survives an extra shift like
        // its neighbours; Cmd+Alt+V is NOT it.
        assert_eq!(
            match_chord(code::V, mods(true, true, false, true)),
            Some(Chord::Hotkey(ToggleVirtualMonitors))
        );
        assert_eq!(
            match_chord(code::V, mods(true, true, true, true)),
            Some(Chord::Hotkey(ToggleVirtualMonitors))
        );
        assert_eq!(match_chord(code::V, chord), None);
        // Cmd+V is Paste, Cmd+J/K are the apps' — only the exact chords are ours.
        for key in [code::J, code::K, code::V] {
            assert_eq!(match_chord(key, mods(true, false, false, false)), None);
            assert_eq!(match_chord(key, mods(false, true, false, false)), None);
            assert_eq!(match_chord(key, mods(true, true, true, false)), None);
            assert_eq!(match_chord(key, mods(false, false, false, false)), None);
        }
    }

    #[test]
    fn cmd_shift_arrow_moves_window_and_other_decorated_arrows_pass_through() {
        // Cmd+Shift+arrows are claimed (window to the adjacent monitor)…
        assert_eq!(
            match_chord(code::LEFT, mods(true, false, true, false)),
            Some(Chord::Hotkey(MoveFocusedToMonitorPrev))
        );
        assert_eq!(
            match_chord(code::RIGHT, mods(true, false, true, false)),
            Some(Chord::Hotkey(MoveFocusedToMonitorNext))
        );
        // …but any other decoration still belongs to the apps.
        assert_eq!(
            match_chord(code::RIGHT, mods(true, true, false, false)),
            None
        );
        assert_eq!(match_chord(code::LEFT, mods(true, true, true, false)), None);
    }

    #[test]
    fn ctrl_cmd_arrows_carry_and_the_mission_control_lever_passes_through() {
        assert_eq!(
            match_chord(code::LEFT, mods(true, false, false, true)),
            Some(Chord::Hotkey(CarryFocusedToWorkspacePrev))
        );
        assert_eq!(
            match_chord(code::RIGHT, mods(true, false, false, true)),
            Some(Chord::Hotkey(CarryFocusedToWorkspaceNext))
        );
        // Ctrl+Alt+Cmd+arrow is the synthesized Mission Control lever chord;
        // swallowing it would deadlock the native backend's own switch.
        assert_eq!(match_chord(code::LEFT, mods(true, true, false, true)), None);
        assert_eq!(
            match_chord(code::RIGHT, mods(true, true, false, true)),
            None
        );
        // Plain Ctrl+arrows belong to Hammerspoon.
        assert_eq!(
            match_chord(code::LEFT, mods(false, false, false, true)),
            None
        );
    }

    #[test]
    fn alt_end_demotes_and_plain_end_is_not_ours() {
        assert_eq!(
            match_chord(code::END, mods(false, true, false, false)),
            Some(Chord::Hotkey(MruDemote))
        );
        assert_eq!(
            match_chord(code::END, mods(false, false, false, false)),
            None
        );
    }

    #[test]
    fn rescue_chord_is_recognized_even_amid_extra_state() {
        assert_eq!(
            match_chord(code::ESCAPE, mods(true, true, false, true)),
            Some(Chord::RescueCandidate)
        );
        // Escape without the full modifier set is not ours.
        assert_eq!(
            match_chord(code::ESCAPE, mods(false, false, false, false)),
            None
        );
    }

    #[test]
    fn engage_chord_is_recognized_and_plain_o_is_not_ours() {
        assert_eq!(
            match_chord(code::O, mods(true, true, false, true)),
            Some(Chord::Engage)
        );
        // Like rescue, it survives an extra held shift.
        assert_eq!(
            match_chord(code::O, mods(true, true, true, true)),
            Some(Chord::Engage)
        );
        // Plain or partially-modified O belongs to the apps.
        assert_eq!(match_chord(code::O, mods(false, false, false, false)), None);
        assert_eq!(match_chord(code::O, mods(true, false, false, false)), None);
    }

    #[test]
    fn engage_fresh_and_save_state_chords_map_and_bare_keys_pass_through() {
        assert_eq!(
            match_chord(code::R, mods(true, true, false, true)),
            Some(Chord::EngageFresh)
        );
        assert_eq!(
            match_chord(code::S, mods(true, true, false, true)),
            Some(Chord::SaveState)
        );
        assert_eq!(match_chord(code::R, mods(false, false, false, false)), None);
        assert_eq!(match_chord(code::S, mods(true, false, false, false)), None);
        // Cmd+S is Save
    }

    #[test]
    fn system_switchers_are_witnessed_but_never_claimed() {
        // Cmd+Tab and Cmd+` are macOS's; Ordo sees them and lets them go.
        for shift in [false, true] {
            let m = mods(true, false, shift, false);
            assert_eq!(match_chord(code::TAB, m), None);
            assert_eq!(witness(code::TAB, m), Some(Witness::AppSwitcherArmed));
            assert_eq!(match_chord(code::GRAVE, m), None);
            assert_eq!(witness(code::GRAVE, m), Some(Witness::WindowCycle));
        }
        // Ordo's own Alt-chords and the browsers' Ctrl+Tab are not witnessed.
        assert_eq!(witness(code::TAB, mods(false, true, false, false)), None);
        assert_eq!(witness(code::TAB, mods(false, false, false, true)), None);
        assert_eq!(witness(code::TAB, mods(true, true, false, false)), None);
        assert_eq!(witness(code::LEFT, mods(true, false, false, false)), None);
    }

    #[test]
    fn unbound_keys_pass_through() {
        assert_eq!(
            match_chord(code::TAB, mods(true, false, false, false)),
            None
        );
        assert_eq!(match_chord(0x00, mods(false, false, false, false)), None);
    }
}
