//! The emulated backend's bookkeeping, kept pure so the whole workspace model
//! is testable without moving a single real window.
//!
//! When Ordo emulates workspaces (rather than driving native Spaces), it owns
//! the concept entirely: every window lives in one native Space, and a window
//! that belongs to a *hidden* workspace is parked off-screen. This ledger tracks
//! which workspace each window belongs to and which workspace is showing, and
//! turns a switch or a move into a plan of park/restore actions. The platform
//! wrapper applies that plan through the Desktop port; the decisions live here.
//!
//! Assignments are DECLARATIONS: only commands (and the named adoption policy
//! for genuinely new windows) write them. Observation can never rewrite one —
//! removal requires explicit death evidence via [`Ledger::forget`], never mere
//! absence from a scan.

use std::collections::BTreeMap;

use ordo_core::{Pid, WindowId, WorkspaceId};

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

/// A window's declared workspace plus the identity that declaration is about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Claim {
    pub ws: WorkspaceId,
    /// The owning app. Identity is (window id, owner): the window server
    /// recycles CGWindowIDs within a boot, so a known id reappearing under a
    /// different pid is a DIFFERENT window and must not inherit the old
    /// declaration. `Pid(0)` means unknown (legacy state files, rescue of an
    /// unscanned id); it matches any owner and upgrades in place when seen.
    /// Residual hole, accepted: SAME-app recycling (the app closes a window
    /// and a new one draws the same id before any scan sees the gap) is
    /// indistinguishable and inherits the declaration.
    pub owner: Pid,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Ledger {
    current: WorkspaceId,
    count: u8,
    assign: BTreeMap<WindowId, Claim>,
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
    /// workspaces are CLAMPED into range rather than dropped: a config that
    /// shrank the workspace count must not silently delete declarations —
    /// the windows land on the nearest surviving workspace instead.
    pub fn restore(count: u8, current: WorkspaceId, assign: BTreeMap<WindowId, Claim>) -> Self {
        let count = count.max(1);
        let clamp = |ws: WorkspaceId| WorkspaceId(ws.0.clamp(1, count));
        let mut l = Ledger {
            current: clamp(current),
            count,
            assign,
        };
        for claim in l.assign.values_mut() {
            claim.ws = clamp(claim.ws);
        }
        l
    }

    pub fn current(&self) -> WorkspaceId {
        self.current
    }

    pub fn count(&self) -> u8 {
        self.count
    }

    pub fn window_ws(&self) -> BTreeMap<WindowId, WorkspaceId> {
        self.assign.iter().map(|(id, c)| (*id, c.ws)).collect()
    }

    pub fn window_claims(&self) -> BTreeMap<WindowId, Claim> {
        self.assign.clone()
    }

    /// The adoption policy — the ONE place observation may create a
    /// declaration, and only for genuinely new windows: a (id, owner) pair the
    /// ledger has never met lands on the visible workspace (where macOS puts
    /// new windows). A known id under a NEW owner is a recycled CGWindowID —
    /// also genuinely new; the stale declaration is replaced and the id is
    /// returned so the caller can drop its park bookkeeping for it.
    pub fn note_seen(&mut self, seen: &[(WindowId, Pid)]) -> Vec<WindowId> {
        let mut recycled = Vec::new();
        for (id, pid) in seen {
            match self.assign.get_mut(id) {
                None => {
                    self.assign.insert(
                        *id,
                        Claim {
                            ws: self.current,
                            owner: *pid,
                        },
                    );
                }
                Some(c) if c.owner.0 == 0 => c.owner = *pid,
                Some(c) if c.owner != *pid => {
                    recycled.push(*id);
                    *c = Claim {
                        ws: self.current,
                        owner: *pid,
                    };
                }
                Some(_) => {}
            }
        }
        recycled
    }

    /// Remove declarations for windows PROVEN dead (absent from the window
    /// server itself, not merely from an AX scan — one slow app drops its
    /// whole window set from a scan, and forgetting on that re-adopted living
    /// windows onto the wrong workspace).
    pub fn forget(&mut self, dead: &[WindowId]) {
        for id in dead {
            self.assign.remove(id);
        }
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
    /// An unknown window gets a claim with an unknown owner (rescue can claim
    /// ids the scan hasn't resolved yet); the owner upgrades on first sight.
    pub fn assign_window(&mut self, window: WindowId, target: WorkspaceId) -> Option<MoveAction> {
        if target.0 < 1 || target.0 > self.count {
            return None;
        }
        self.assign
            .entry(window)
            .and_modify(|c| c.ws = target)
            .or_insert(Claim {
                ws: target,
                owner: Pid(0),
            });
        Some(if target == self.current {
            MoveAction::Restore
        } else {
            MoveAction::Park
        })
    }

    fn windows_on(&self, ws: WorkspaceId) -> Vec<WindowId> {
        self.assign
            .iter()
            .filter(|(_, c)| c.ws == ws)
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
    fn seen(pairs: &[(u32, i32)]) -> Vec<(WindowId, Pid)> {
        pairs.iter().map(|(w, p)| (WindowId(*w), Pid(*p))).collect()
    }

    #[test]
    fn new_windows_land_on_the_visible_workspace() {
        let mut l = Ledger::new(3);
        l.note_seen(&seen(&[(1, 10), (2, 10)]));
        assert_eq!(l.window_ws()[&w(1)], ws(1));
        assert_eq!(l.window_ws()[&w(2)], ws(1));
    }

    #[test]
    fn switching_parks_the_old_and_restores_the_new() {
        let mut l = Ledger::new(3);
        l.note_seen(&seen(&[(1, 10), (2, 10)])); // both on ws 1
        l.assign_window(w(2), ws(2)); // move w2 to ws 2 (now parked)

        let plan = l.switch(ws(2));
        assert_eq!(l.current(), ws(2));
        assert_eq!(plan.park, vec![w(1)]); // ws1's window hides
        assert_eq!(plan.restore, vec![w(2)]); // ws2's window reveals
    }

    #[test]
    fn switching_to_current_or_out_of_range_is_a_noop() {
        let mut l = Ledger::new(3);
        l.note_seen(&seen(&[(1, 10)]));
        assert_eq!(l.switch(ws(1)), SwitchPlan::default());
        assert_eq!(l.switch(ws(9)), SwitchPlan::default());
        assert_eq!(l.current(), ws(1));
    }

    #[test]
    fn assigning_to_the_visible_workspace_restores_otherwise_parks() {
        let mut l = Ledger::new(3);
        l.note_seen(&seen(&[(1, 10)]));
        assert_eq!(l.assign_window(w(1), ws(2)), Some(MoveAction::Park));
        assert_eq!(l.assign_window(w(1), ws(1)), Some(MoveAction::Restore));
    }

    #[test]
    fn absence_alone_never_forgets_a_declaration() {
        let mut l = Ledger::new(3);
        l.note_seen(&seen(&[(1, 10), (2, 10)]));
        l.assign_window(w(2), ws(2));
        // A scan misses w2 (slow app) and re-sights it: the declaration holds.
        l.note_seen(&seen(&[(1, 10)]));
        l.note_seen(&seen(&[(1, 10), (2, 10)]));
        assert_eq!(l.window_ws()[&w(2)], ws(2));
        // Only proof of death removes it.
        l.forget(&[w(2)]);
        assert!(!l.window_ws().contains_key(&w(2)));
    }

    #[test]
    fn a_recycled_id_is_a_new_window_not_the_old_declaration() {
        let mut l = Ledger::new(3);
        l.note_seen(&seen(&[(1, 10)]));
        l.assign_window(w(1), ws(3));
        l.switch(ws(2));
        // The id comes back under a different app: adopt here, don't inherit.
        let recycled = l.note_seen(&seen(&[(1, 99)]));
        assert_eq!(recycled, vec![w(1)]);
        assert_eq!(l.window_ws()[&w(1)], ws(2));
        // Same-owner sightings never count as recycling.
        assert!(l.note_seen(&seen(&[(1, 99)])).is_empty());
    }

    #[test]
    fn restore_clamps_out_of_range_claims_instead_of_deleting_them() {
        let claims: BTreeMap<WindowId, Claim> = [
            (
                w(1),
                Claim {
                    ws: ws(2),
                    owner: Pid(10),
                },
            ),
            (
                w(2),
                Claim {
                    ws: ws(9),
                    owner: Pid(10),
                },
            ),
        ]
        .into();
        let l = Ledger::restore(3, ws(7), claims);
        assert_eq!(l.current(), ws(3));
        assert_eq!(l.window_ws()[&w(1)], ws(2));
        assert_eq!(l.window_ws()[&w(2)], ws(3), "clamped, not dropped");
    }

    #[test]
    fn an_unknown_owner_matches_anyone_and_upgrades_in_place() {
        // Legacy state files carry no owner; the first sighting fills it in
        // without treating the window as recycled.
        let claims: BTreeMap<WindowId, Claim> = [(
            w(1),
            Claim {
                ws: ws(2),
                owner: Pid(0),
            },
        )]
        .into();
        let mut l = Ledger::restore(3, ws(1), claims);
        assert!(l.note_seen(&seen(&[(1, 42)])).is_empty());
        assert_eq!(l.window_ws()[&w(1)], ws(2));
        assert_eq!(l.window_claims()[&w(1)].owner, Pid(42));
    }
}
