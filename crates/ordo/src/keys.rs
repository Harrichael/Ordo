//! The keybinding table — pure, so the whole policy is testable without a live
//! event tap.
//!
//! v0's bindings, and only these (no inventing keys the user hasn't asked for):
//!   - Cmd+Left / Cmd+Right      → previous / next workspace
//!   - Cmd+Shift+Left / Right    → move the focused window to the other
//!     monitor, dragging focus and mouse along with it
//!   - Alt+Tab                   → MRU window in the workspace
//!   - Alt+Shift+Tab             → MRU window in the workspace + monitor
//!   - Alt+Backtick              → MRU window in the workspace + app
//!   - Alt+End                   → demote the focused window to the back of
//!     the MRU history and focus the next one
//!   - Ctrl+Alt+Cmd+Escape       → rescue candidate (engages on a double-press;
//!     the timing lives in the tap, not here)
//!   - Ctrl+Alt+Cmd+O            → engage: undo a rescue, or bring a --paused
//!     run alive. Deliberately a different key than rescue so mashing the panic
//!     chord can never re-engage the thing being escaped.
//!
//! Modifier matching is deliberately strict so Ordo only swallows the exact
//! chord: Cmd+Shift+arrows are claimed (the user gave up select-to-line for
//! them, same trade as Cmd+arrows), but Cmd+Alt+arrows etc. pass through.

use ordo_core::HotkeyAction;

/// macOS virtual key codes for the keys we bind.
mod code {
    pub const LEFT: u16 = 0x7B;
    pub const RIGHT: u16 = 0x7C;
    pub const TAB: u16 = 0x30;
    pub const GRAVE: u16 = 0x32; // backtick
    pub const ESCAPE: u16 = 0x35;
    pub const O: u16 = 0x1F;
    pub const END: u16 = 0x77;
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
    /// (Re)engage Ordo: single press, honored even while disengaged.
    Engage,
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

    match keycode {
        code::LEFT if only_cmd(m) => Some(Chord::Hotkey(WorkspacePrev)),
        code::RIGHT if only_cmd(m) => Some(Chord::Hotkey(WorkspaceNext)),
        code::LEFT | code::RIGHT if cmd_shift(m) => Some(Chord::Hotkey(MoveFocusedToOtherMonitor)),
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

/// Cmd+Shift exactly.
fn cmd_shift(m: Mods) -> bool {
    m.cmd && m.shift && !m.alt && !m.ctrl
}

#[cfg(test)]
mod tests {
    use super::*;
    use ordo_core::HotkeyAction::*;

    fn mods(cmd: bool, alt: bool, shift: bool, ctrl: bool) -> Mods {
        Mods {
            cmd,
            alt,
            shift,
            ctrl,
        }
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
    }

    #[test]
    fn cmd_shift_arrow_moves_window_and_other_decorated_arrows_pass_through() {
        // Cmd+Shift+arrows are claimed (window to other monitor)…
        assert_eq!(
            match_chord(code::LEFT, mods(true, false, true, false)),
            Some(Chord::Hotkey(MoveFocusedToOtherMonitor))
        );
        assert_eq!(
            match_chord(code::RIGHT, mods(true, false, true, false)),
            Some(Chord::Hotkey(MoveFocusedToOtherMonitor))
        );
        // …but any other decoration still belongs to the apps.
        assert_eq!(
            match_chord(code::RIGHT, mods(true, true, false, false)),
            None
        );
        assert_eq!(match_chord(code::LEFT, mods(true, true, true, false)), None);
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
    fn unbound_keys_pass_through() {
        assert_eq!(
            match_chord(code::TAB, mods(true, false, false, false)),
            None
        );
        assert_eq!(match_chord(0x00, mods(false, false, false, false)), None);
    }
}
