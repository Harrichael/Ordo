//! The projection: how the virtual monitors of the control plane land on the
//! displays that are physically present right now.
//!
//! Pure and shared. The core needs it to know which windows are on screen (a
//! window is visible when its workspace is current AND its virtual monitor is
//! hosted); the emulated backend needs the very same answer to park and restore
//! so the screen matches. One function, two callers, so the two can never
//! disagree about what "hosted" means.
//!
//! Displays are addressed by POSITION — their index in the left-to-right
//! ordering — never by hardware identity, which is what lets an unplugged
//! monitor's windows come back onto whatever stands in that position next.
//!
//! Three regimes:
//! - enough displays: virtual monitor `v` sits on display `v`, one to one;
//! - fewer displays, virtualization ON: a viewport of `physical` consecutive
//!   virtual monitors is hosted, positioned so the `viewed` one (the anchor) is
//!   always inside it; the rest are hidden;
//! - fewer displays, virtualization OFF: everything collapses onto the displays
//!   present, the rightmost absorbing the overflow.
//!
//! `viewed` is an ANCHOR, not "the one monitor you see": with one display they
//! coincide, but with two displays and three virtual monitors stepping the
//! anchor from 3 to 2 changes nothing on screen — it is a focus jump, exactly
//! as J/K are on a full rig.

use crate::ids::VirtualMonitorId;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Projection {
    /// Per virtual monitor (ordinal - 1): the index of its display in the
    /// left-to-right ordering, or `None` while it is hidden.
    hosts: Vec<Option<usize>>,
}

pub fn project(count: u8, viewed: VirtualMonitorId, enabled: bool, physical: usize) -> Projection {
    let count = count.max(1) as usize;
    let viewed = (viewed.0.max(1) as usize).min(count);
    let hosts = if physical == 0 {
        vec![None; count]
    } else if physical >= count {
        (0..count).map(Some).collect()
    } else if enabled {
        // The viewport slides only as far as it must to keep the anchor
        // inside; with one display that is "exactly the viewed one".
        let start = viewed.min(count - physical + 1);
        (1..=count)
            .map(|v| (v >= start && v < start + physical).then(|| v - start))
            .collect()
    } else {
        (1..=count).map(|v| Some(v.min(physical) - 1)).collect()
    };
    Projection { hosts }
}

impl Projection {
    pub fn count(&self) -> u8 {
        self.hosts.len() as u8
    }

    /// The display (by position) hosting this virtual monitor, if any.
    pub fn host(&self, vm: VirtualMonitorId) -> Option<usize> {
        self.hosts.get(vm.0.checked_sub(1)? as usize).copied().flatten()
    }

    /// The virtual monitor a display STANDS FOR: the lowest ordinal it hosts.
    /// Unique while virtualization is on; under collapse the overflow display
    /// hosts several, and the first it absorbs is the one a window dragged onto
    /// it is taken to mean. Any rule that demanded uniqueness would refuse the
    /// adoption there and let the re-host check fling the window back on every
    /// drag.
    pub fn canonical_vm(&self, display: usize) -> Option<VirtualMonitorId> {
        self.hosts
            .iter()
            .position(|h| *h == Some(display))
            .map(|i| VirtualMonitorId(i as u8 + 1))
    }

    pub fn is_hosted(&self, vm: VirtualMonitorId) -> bool {
        self.host(vm).is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vm(n: u8) -> VirtualMonitorId {
        VirtualMonitorId(n)
    }

    #[test]
    fn a_full_rig_is_one_to_one_whatever_the_anchor_or_switch() {
        for enabled in [true, false] {
            for viewed in 1..=3 {
                let p = project(3, vm(viewed), enabled, 3);
                assert_eq!((p.host(vm(1)), p.host(vm(2)), p.host(vm(3))), (Some(0), Some(1), Some(2)));
                assert_eq!(p.canonical_vm(2), Some(vm(3)));
            }
        }
        // More displays than virtual monitors still maps by position.
        assert_eq!(project(2, vm(1), true, 3).host(vm(2)), Some(1));
    }

    #[test]
    fn one_display_shows_exactly_the_anchor_when_on_and_everything_when_off() {
        let on = project(2, vm(2), true, 1);
        assert_eq!((on.host(vm(1)), on.host(vm(2))), (None, Some(0)));
        assert_eq!(on.canonical_vm(0), Some(vm(2)));
        let off = project(2, vm(2), false, 1);
        assert_eq!((off.host(vm(1)), off.host(vm(2))), (Some(0), Some(0)));
        assert_eq!(off.canonical_vm(0), Some(vm(1)), "the first it absorbs");
    }

    #[test]
    fn the_viewport_slides_only_to_keep_the_anchor_inside() {
        // Three virtual monitors, two displays: anchors 2 and 3 project alike.
        let a1 = project(3, vm(1), true, 2);
        assert_eq!((a1.host(vm(1)), a1.host(vm(2)), a1.host(vm(3))), (Some(0), Some(1), None));
        let a2 = project(3, vm(2), true, 2);
        let a3 = project(3, vm(3), true, 2);
        assert_eq!(a2, a3);
        assert_eq!((a3.host(vm(1)), a3.host(vm(2)), a3.host(vm(3))), (None, Some(0), Some(1)));
        // Collapsed: the rightmost display absorbs the overflow.
        let off = project(3, vm(1), false, 2);
        assert_eq!((off.host(vm(2)), off.host(vm(3))), (Some(1), Some(1)));
        assert_eq!(off.canonical_vm(1), Some(vm(2)));
    }

    #[test]
    fn no_displays_hosts_nothing_and_bad_ordinals_are_unhosted() {
        let p = project(2, vm(1), true, 0);
        assert_eq!(p.host(vm(1)), None);
        let p = project(2, vm(9), true, 1);
        assert_eq!(p.host(vm(2)), Some(0), "an out-of-range anchor clamps");
        assert_eq!(p.host(vm(0)), None);
        assert_eq!(p.host(vm(7)), None);
    }
}
