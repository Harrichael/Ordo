//! The emulated backend's bookkeeping, kept pure so the whole workspace model
//! is testable without moving a single real window.
//!
//! When Ordo emulates workspaces (rather than driving native Spaces), it owns
//! the concept entirely: every window lives in one native Space, and a window
//! that belongs to a *hidden* workspace is parked off-screen. This ledger tracks
//! which workspace each window belongs to and which workspace is showing, and
//! turns a switch or a move into a plan of park/restore actions. The platform
//! wrapper applies that plan with AX; the decisions live here.

use std::collections::BTreeMap;

use ordo_core::{WindowId, WorkspaceId};

/// What a switch requires: hide the outgoing workspace's windows, reveal the
/// incoming one's.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SwitchPlan {
    pub park: Vec<WindowId>,
    pub restore: Vec<WindowId>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MoveAction {
    /// The window's new workspace is hidden — get it off-screen.
    Park,
    /// The window's new workspace is the visible one — bring it on-screen.
    Restore,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Ledger {
    current: WorkspaceId,
    count: u8,
    assign: BTreeMap<WindowId, WorkspaceId>,
}

impl Ledger {
    pub fn new(count: u8) -> Self {
        Ledger {
            current: WorkspaceId(1),
            count: count.max(1),
            assign: BTreeMap::new(),
        }
    }

    /// Rebuild from persisted claims (see [`crate::statefile`]). Out-of-range
    /// entries are clamped/dropped rather than trusted — the file is a claim.
    pub fn restore(
        count: u8,
        current: WorkspaceId,
        assign: BTreeMap<WindowId, WorkspaceId>,
    ) -> Self {
        let count = count.max(1);
        let mut l = Ledger {
            current: if current.0 >= 1 && current.0 <= count {
                current
            } else {
                WorkspaceId(1)
            },
            count,
            assign,
        };
        l.assign.retain(|_, ws| ws.0 >= 1 && ws.0 <= count);
        l
    }

    pub fn current(&self) -> WorkspaceId {
        self.current
    }

    pub fn count(&self) -> u8 {
        self.count
    }

    pub fn window_ws(&self) -> BTreeMap<WindowId, WorkspaceId> {
        self.assign.clone()
    }

    /// A window Ordo sees for the first time is on whatever workspace is
    /// currently showing (that's where new windows appear); record it there.
    pub fn note_seen(&mut self, ids: &[WindowId]) {
        for id in ids {
            self.assign.entry(*id).or_insert(self.current);
        }
    }

    /// Drop windows that no longer exist, so the ledger doesn't accrete ghosts.
    pub fn forget_missing(&mut self, live: &[WindowId]) {
        self.assign.retain(|id, _| live.contains(id));
    }

    /// Plan a switch to `target`. Empty (and a no-op) if target is the current
    /// workspace or out of range.
    pub fn switch(&mut self, target: WorkspaceId) -> SwitchPlan {
        if target == self.current || target.0 < 1 || target.0 > self.count {
            return SwitchPlan::default();
        }
        let park = self.windows_on(self.current);
        let restore = self.windows_on(target);
        self.current = target;
        SwitchPlan { park, restore }
    }

    /// Reassign a window and say whether that means hiding or revealing it.
    pub fn assign_window(&mut self, window: WindowId, target: WorkspaceId) -> Option<MoveAction> {
        if target.0 < 1 || target.0 > self.count {
            return None;
        }
        self.assign.insert(window, target);
        Some(if target == self.current {
            MoveAction::Restore
        } else {
            MoveAction::Park
        })
    }

    fn windows_on(&self, ws: WorkspaceId) -> Vec<WindowId> {
        self.assign
            .iter()
            .filter(|(_, w)| **w == ws)
            .map(|(id, _)| *id)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(n: u32) -> WindowId {
        WindowId(n)
    }
    fn ws(n: u8) -> WorkspaceId {
        WorkspaceId(n)
    }

    #[test]
    fn new_windows_land_on_the_visible_workspace() {
        let mut l = Ledger::new(3);
        l.note_seen(&[w(1), w(2)]);
        assert_eq!(l.window_ws()[&w(1)], ws(1));
        assert_eq!(l.window_ws()[&w(2)], ws(1));
    }

    #[test]
    fn switching_parks_the_old_and_restores_the_new() {
        let mut l = Ledger::new(3);
        l.note_seen(&[w(1), w(2)]); // both on ws 1
        l.assign_window(w(2), ws(2)); // move w2 to ws 2 (now parked)

        let plan = l.switch(ws(2));
        assert_eq!(l.current(), ws(2));
        assert_eq!(plan.park, vec![w(1)]); // ws1's window hides
        assert_eq!(plan.restore, vec![w(2)]); // ws2's window reveals
    }

    #[test]
    fn switching_to_current_or_out_of_range_is_a_noop() {
        let mut l = Ledger::new(3);
        l.note_seen(&[w(1)]);
        assert_eq!(l.switch(ws(1)), SwitchPlan::default());
        assert_eq!(l.switch(ws(9)), SwitchPlan::default());
        assert_eq!(l.current(), ws(1));
    }

    #[test]
    fn assigning_to_the_visible_workspace_restores_otherwise_parks() {
        let mut l = Ledger::new(3);
        l.note_seen(&[w(1)]);
        assert_eq!(l.assign_window(w(1), ws(2)), Some(MoveAction::Park));
        assert_eq!(l.assign_window(w(1), ws(1)), Some(MoveAction::Restore));
    }

    #[test]
    fn closed_windows_are_forgotten() {
        let mut l = Ledger::new(3);
        l.note_seen(&[w(1), w(2)]);
        l.forget_missing(&[w(1)]);
        assert!(l.window_ws().contains_key(&w(1)));
        assert!(!l.window_ws().contains_key(&w(2)));
    }
}
