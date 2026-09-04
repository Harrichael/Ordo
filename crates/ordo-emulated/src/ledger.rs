//! The emulated backend's bookkeeping, kept pure so the whole workspace model
//! is testable without moving a single real window.
//!
//! When Ordo emulates workspaces (rather than driving native Spaces), it owns
//! the concept entirely: every window lives in one native Space, and a window
//! that is not on screen is parked off-screen. On screen means two things at
//! once — the window's workspace is the visible one AND its virtual monitor is
//! hosted by a display under the current [`Projection`] — and this ledger holds
//! both declarations per window plus the layout ones (visible workspace,
//! anchor monitor, virtualization on/off). Every change to any of them, and
//! every change to the display count, is planned the same way: diff the set
//! of on-screen windows before and after, park what left it, restore what
//! entered it. The platform wrapper applies that plan through the Desktop
//! port; the decisions live here.
//!
//! Assignments are DECLARATIONS: only commands (and the named adoption policy
//! for genuinely new windows) write them. Observation can never rewrite one —
//! removal requires explicit death evidence via [`Ledger::forget`], never mere
//! absence from a scan.

use std::collections::{BTreeMap, BTreeSet};

use ordo_core::{project, Pid, Projection, VirtualMonitorId, VirtualMonitors, WindowId, WorkspaceId};

/// What a change requires: hide the windows that left the screen, reveal the
/// ones that entered it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SwitchPlan {
    pub park: Vec<WindowId>,
    pub restore: Vec<WindowId>,
}

impl SwitchPlan {
    pub fn is_empty(&self) -> bool {
        self.park.is_empty() && self.restore.is_empty()
    }
}

/// A window's declared place plus the identity that declaration is about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Claim {
    pub ws: WorkspaceId,
    pub monitor: VirtualMonitorId,
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
    monitors: VirtualMonitors,
    /// Displays present at the last observation. Bookkeeping, not a
    /// declaration — but the projection depends on it, so a change to it is
    /// planned like any other change.
    physical: usize,
    assign: BTreeMap<WindowId, Claim>,
}

impl Ledger {
    pub fn new(count: u8) -> Self {
        Ledger {
            current: WorkspaceId(1),
            count: count.max(1),
            monitors: VirtualMonitors {
                count: 1,
                viewed: VirtualMonitorId(1),
                enabled: true,
            },
            physical: 0,
            assign: BTreeMap::new(),
        }
    }

    /// Rebuild from persisted claims (see [`crate::statefile`]). Out-of-range
    /// workspaces and monitors are CLAMPED into range rather than dropped: a
    /// config that shrank the workspace count must not silently delete
    /// declarations — the windows land on the nearest surviving one instead.
    pub fn restore(
        count: u8,
        current: WorkspaceId,
        monitors: VirtualMonitors,
        physical: usize,
        assign: BTreeMap<WindowId, Claim>,
    ) -> Self {
        let count = count.max(1);
        let mcount = monitors.count.max(1);
        let clamp = |ws: WorkspaceId| WorkspaceId(ws.0.clamp(1, count));
        let clamp_m = |m: VirtualMonitorId| VirtualMonitorId(m.0.clamp(1, mcount));
        let mut l = Ledger {
            current: clamp(current),
            count,
            monitors: VirtualMonitors {
                count: mcount,
                viewed: clamp_m(monitors.viewed),
                enabled: monitors.enabled,
            },
            physical,
            assign,
        };
        for claim in l.assign.values_mut() {
            claim.ws = clamp(claim.ws);
            claim.monitor = clamp_m(claim.monitor);
        }
        l
    }

    pub fn current(&self) -> WorkspaceId {
        self.current
    }

    pub fn count(&self) -> u8 {
        self.count
    }

    pub fn monitors(&self) -> VirtualMonitors {
        self.monitors
    }

    pub fn physical(&self) -> usize {
        self.physical
    }

    pub fn window_ws(&self) -> BTreeMap<WindowId, WorkspaceId> {
        self.assign.iter().map(|(id, c)| (*id, c.ws)).collect()
    }

    pub fn window_monitors(&self) -> BTreeMap<WindowId, VirtualMonitorId> {
        self.assign.iter().map(|(id, c)| (*id, c.monitor)).collect()
    }

    pub fn window_claims(&self) -> BTreeMap<WindowId, Claim> {
        self.assign.clone()
    }

    pub fn claim(&self, id: WindowId) -> Option<Claim> {
        self.assign.get(&id).copied()
    }

    /// The projection in force: virtual monitors onto the displays last seen.
    pub fn projection(&self) -> Projection {
        project(
            self.monitors.count,
            self.monitors.viewed,
            self.monitors.enabled,
            self.physical,
        )
    }

    /// On screen under `proj`: current workspace and a hosted monitor.
    pub fn visible(&self, id: WindowId, proj: &Projection) -> bool {
        self.assign
            .get(&id)
            .is_some_and(|c| c.ws == self.current && proj.is_hosted(c.monitor))
    }

    fn visible_set(&self, proj: &Projection) -> BTreeSet<WindowId> {
        self.assign
            .keys()
            .filter(|id| self.visible(**id, proj))
            .copied()
            .collect()
    }

    /// The one planner. Apply any change to the declarations or the display
    /// count, then say what left the screen and what entered it. Every
    /// mutation goes through here so no path can decide hiding differently.
    pub fn retarget(&mut self, f: impl FnOnce(&mut Ledger)) -> SwitchPlan {
        let before = self.visible_set(&self.projection());
        f(self);
        let after = self.visible_set(&self.projection());
        SwitchPlan {
            park: before.difference(&after).copied().collect(),
            restore: after.difference(&before).copied().collect(),
        }
    }

    /// The adoption policy — the ONE place observation may create a
    /// declaration, and only for genuinely new windows: a (id, owner) pair the
    /// ledger has never met lands on the visible workspace (where macOS puts
    /// new windows) and on the monitor `adopt_monitor` names for it (the one
    /// the display under it stands for). A known id under a NEW owner is a
    /// recycled CGWindowID — also genuinely new; the stale declaration is
    /// replaced and the id is returned so the caller can drop its park
    /// bookkeeping for it.
    pub fn note_seen(
        &mut self,
        seen: &[(WindowId, Pid)],
        adopt_monitor: impl Fn(WindowId) -> VirtualMonitorId,
    ) -> Vec<WindowId> {
        let mut recycled = Vec::new();
        for (id, pid) in seen {
            let fresh = |l: &Ledger| Claim {
                ws: l.current,
                monitor: adopt_monitor(*id),
                owner: *pid,
            };
            match self.assign.get(id) {
                None => {
                    let c = fresh(self);
                    self.assign.insert(*id, c);
                }
                Some(c) if c.owner.0 == 0 => {
                    self.assign.get_mut(id).expect("present").owner = *pid;
                }
                Some(c) if c.owner != *pid => {
                    recycled.push(*id);
                    let c = fresh(self);
                    self.assign.insert(*id, c);
                }
                Some(_) => {}
            }
        }
        recycled
    }

    /// The display count changed (or is first learned). The virtual monitor
    /// count only ever grows to meet it: a monitor once seen keeps existing,
    /// which is the whole of monitor memory. Returns the plan for whatever the
    /// new projection hides or reveals.
    pub fn note_displays(&mut self, physical: usize) -> SwitchPlan {
        self.retarget(|l| {
            l.physical = physical;
            l.monitors.count = l.monitors.count.max(physical as u8).max(1);
        })
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
        self.retarget(|l| l.current = target)
    }

    /// Plan moving the anchor to `target`. `None` if out of range.
    pub fn view_monitor(&mut self, target: VirtualMonitorId) -> Option<SwitchPlan> {
        if target.0 < 1 || target.0 > self.monitors.count {
            return None;
        }
        Some(self.retarget(|l| l.monitors.viewed = target))
    }

    pub fn set_enabled(&mut self, enabled: bool) -> SwitchPlan {
        self.retarget(|l| l.monitors.enabled = enabled)
    }

    /// Set the anchor without planning — for the topology policy, whose plan
    /// is taken by the `note_displays` that follows.
    pub fn set_viewed(&mut self, target: VirtualMonitorId) {
        if target.0 >= 1 && target.0 <= self.monitors.count {
            self.monitors.viewed = target;
        }
    }

    /// Reassign a window's workspace and plan the hide or reveal that implies.
    /// An unknown window gets a claim with an unknown owner (rescue can claim
    /// ids the scan hasn't resolved yet); the owner upgrades on first sight.
    pub fn assign_window(&mut self, window: WindowId, target: WorkspaceId) -> Option<SwitchPlan> {
        if target.0 < 1 || target.0 > self.count {
            return None;
        }
        Some(self.retarget(|l| {
            let viewed = l.monitors.viewed;
            l.assign
                .entry(window)
                .and_modify(|c| c.ws = target)
                .or_insert(Claim {
                    ws: target,
                    monitor: viewed,
                    owner: Pid(0),
                });
        }))
    }

    /// Reassign a window's virtual monitor; same contract as `assign_window`.
    pub fn assign_monitor(
        &mut self,
        window: WindowId,
        target: VirtualMonitorId,
    ) -> Option<SwitchPlan> {
        if target.0 < 1 || target.0 > self.monitors.count {
            return None;
        }
        Some(self.retarget(|l| {
            let current = l.current;
            l.assign
                .entry(window)
                .and_modify(|c| c.monitor = target)
                .or_insert(Claim {
                    ws: current,
                    monitor: target,
                    owner: Pid(0),
                });
        }))
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
    fn vm(n: u8) -> VirtualMonitorId {
        VirtualMonitorId(n)
    }
    fn seen(pairs: &[(u32, i32)]) -> Vec<(WindowId, Pid)> {
        pairs.iter().map(|(w, p)| (WindowId(*w), Pid(*p))).collect()
    }
    fn first(_: WindowId) -> VirtualMonitorId {
        vm(1)
    }
    /// A ledger that has seen `physical` displays.
    fn rig(physical: usize) -> Ledger {
        let mut l = Ledger::new(3);
        l.note_displays(physical);
        l
    }

    #[test]
    fn new_windows_land_on_the_visible_workspace_and_the_named_monitor() {
        let mut l = rig(2);
        l.note_seen(&seen(&[(1, 10), (2, 10)]), |id| if id == w(2) { vm(2) } else { vm(1) });
        assert_eq!(l.window_ws()[&w(1)], ws(1));
        assert_eq!(l.window_monitors()[&w(1)], vm(1));
        assert_eq!(l.window_monitors()[&w(2)], vm(2));
    }

    #[test]
    fn switching_parks_the_old_and_restores_the_new() {
        let mut l = rig(1);
        l.note_seen(&seen(&[(1, 10), (2, 10)]), first); // both on ws 1
        l.assign_window(w(2), ws(2)); // move w2 to ws 2 (now parked)

        let plan = l.switch(ws(2));
        assert_eq!(l.current(), ws(2));
        assert_eq!(plan.park, vec![w(1)]); // ws1's window hides
        assert_eq!(plan.restore, vec![w(2)]); // ws2's window reveals
    }

    #[test]
    fn switching_to_current_or_out_of_range_is_a_noop() {
        let mut l = rig(1);
        l.note_seen(&seen(&[(1, 10)]), first);
        assert_eq!(l.switch(ws(1)), SwitchPlan::default());
        assert_eq!(l.switch(ws(9)), SwitchPlan::default());
        assert_eq!(l.current(), ws(1));
    }

    #[test]
    fn assigning_to_the_visible_workspace_restores_otherwise_parks() {
        let mut l = rig(1);
        l.note_seen(&seen(&[(1, 10)]), first);
        let parked = l.assign_window(w(1), ws(2)).unwrap();
        assert_eq!((parked.park, parked.restore), (vec![w(1)], vec![]));
        let restored = l.assign_window(w(1), ws(1)).unwrap();
        assert_eq!((restored.park, restored.restore), (vec![], vec![w(1)]));
        // Hidden to hidden touches nothing on screen.
        l.assign_window(w(1), ws(2)).unwrap();
        assert!(l.assign_window(w(1), ws(3)).unwrap().is_empty());
        assert_eq!(l.assign_window(w(1), ws(9)), None);
    }

    /// The monitor axis hides exactly as the workspace axis does: with one
    /// display and virtualization on, only the anchor's windows are on
    /// screen, and viewing the other monitor swaps them.
    #[test]
    fn viewing_a_monitor_on_one_display_swaps_its_windows_for_the_anchors() {
        let mut l = rig(2);
        l.note_seen(&seen(&[(1, 10), (2, 10)]), |id| if id == w(2) { vm(2) } else { vm(1) });
        // The external display goes away: monitor 2 is hidden, w2 parks, and
        // the count does NOT shrink.
        let unplug = l.note_displays(1);
        assert_eq!((unplug.park, unplug.restore), (vec![w(2)], vec![]));
        assert_eq!(l.monitors().count, 2);

        let view = l.view_monitor(vm(2)).unwrap();
        assert_eq!((view.park, view.restore), (vec![w(1)], vec![w(2)]));
        assert_eq!(l.view_monitor(vm(3)), None, "out of range");

        // Collapsing shows everything; re-enabling hides the non-anchor again.
        let off = l.set_enabled(false);
        assert_eq!((off.park, off.restore), (vec![], vec![w(1)]));
        let on = l.set_enabled(true);
        assert_eq!((on.park, on.restore), (vec![w(1)], vec![]));

        // Plugging back in reveals w1 on its own display; nothing is parked.
        let replug = l.note_displays(2);
        assert_eq!((replug.park, replug.restore), (vec![], vec![w(1)]));
    }

    #[test]
    fn a_monitor_assignment_plans_like_a_workspace_one() {
        let mut l = rig(1);
        l.note_seen(&seen(&[(1, 10)]), first);
        let plan = l.assign_monitor(w(1), vm(2));
        assert_eq!(plan, None, "monitor 2 does not exist on a one-display rig");
        l.note_displays(2);
        l.note_displays(1);
        let hidden = l.assign_monitor(w(1), vm(2)).unwrap();
        assert_eq!((hidden.park, hidden.restore), (vec![w(1)], vec![]));
        let back = l.assign_monitor(w(1), vm(1)).unwrap();
        assert_eq!((back.park, back.restore), (vec![], vec![w(1)]));
    }

    #[test]
    fn absence_alone_never_forgets_a_declaration() {
        let mut l = rig(1);
        l.note_seen(&seen(&[(1, 10), (2, 10)]), first);
        l.assign_window(w(2), ws(2));
        // A scan misses w2 (slow app) and re-sights it: the declaration holds.
        l.note_seen(&seen(&[(1, 10)]), first);
        l.note_seen(&seen(&[(1, 10), (2, 10)]), first);
        assert_eq!(l.window_ws()[&w(2)], ws(2));
        // Only proof of death removes it.
        l.forget(&[w(2)]);
        assert!(!l.window_ws().contains_key(&w(2)));
    }

    #[test]
    fn a_recycled_id_is_a_new_window_not_the_old_declaration() {
        let mut l = rig(1);
        l.note_seen(&seen(&[(1, 10)]), first);
        l.assign_window(w(1), ws(3));
        l.switch(ws(2));
        // The id comes back under a different app: adopt here, don't inherit.
        let recycled = l.note_seen(&seen(&[(1, 99)]), first);
        assert_eq!(recycled, vec![w(1)]);
        assert_eq!(l.window_ws()[&w(1)], ws(2));
        // Same-owner sightings never count as recycling.
        assert!(l.note_seen(&seen(&[(1, 99)]), first).is_empty());
    }

    #[test]
    fn restore_clamps_out_of_range_claims_instead_of_deleting_them() {
        let claim = |wsn: u8, m: u8| Claim {
            ws: ws(wsn),
            monitor: vm(m),
            owner: Pid(10),
        };
        let claims: BTreeMap<WindowId, Claim> =
            [(w(1), claim(2, 1)), (w(2), claim(9, 7))].into();
        let monitors = VirtualMonitors {
            count: 2,
            viewed: vm(5),
            enabled: false,
        };
        let l = Ledger::restore(3, ws(7), monitors, 1, claims);
        assert_eq!(l.current(), ws(3));
        assert_eq!(l.monitors().viewed, vm(2), "anchor clamped");
        assert!(!l.monitors().enabled, "switch kept");
        assert_eq!(l.window_ws()[&w(1)], ws(2));
        assert_eq!(l.window_ws()[&w(2)], ws(3), "clamped, not dropped");
        assert_eq!(l.window_monitors()[&w(2)], vm(2), "monitor clamped too");
    }

    #[test]
    fn an_unknown_owner_matches_anyone_and_upgrades_in_place() {
        // Legacy state files carry no owner; the first sighting fills it in
        // without treating the window as recycled.
        let claims: BTreeMap<WindowId, Claim> = [(
            w(1),
            Claim {
                ws: ws(2),
                monitor: vm(1),
                owner: Pid(0),
            },
        )]
        .into();
        let mut l = Ledger::restore(3, ws(1), Ledger::new(1).monitors(), 1, claims);
        assert!(l.note_seen(&seen(&[(1, 42)]), first).is_empty());
        assert_eq!(l.window_ws()[&w(1)], ws(2));
        assert_eq!(l.window_claims()[&w(1)].owner, Pid(42));
    }
}
