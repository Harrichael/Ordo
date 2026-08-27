use serde::{Deserialize, Serialize};

use crate::ids::WindowId;

/// Bounds memory and checkpoint size; hundreds of times more windows than any
/// real session holds, so the cap never changes behavior in practice.
const CAP: usize = 512;

/// One global focus history, most-recent-first, each window at most once.
///
/// The alternative — per-scope MRU stacks indexed by workspace x monitor x
/// app — would need lockstep edits on every window move, destroy, and monitor
/// hot-plug, exactly the class of bookkeeping bug this project exists to
/// avoid. Instead there is one mutation site (`touch`) and scoping happens at
/// read time: `most_recent` filters by a predicate over the caller's current
/// window records. Move-to-front dedupe gives the classic alt-tab toggle
/// between the top two windows of any scope for free.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct FocusHistory {
    order: Vec<WindowId>,
}

impl FocusHistory {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record that `w` is now the most recently used window.
    pub fn touch(&mut self, w: WindowId) {
        self.remove(w);
        self.order.insert(0, w);
        self.order.truncate(CAP);
    }

    /// Enter a never-focused window at the *back*: it is by definition the
    /// least recently used, but without this it would be unreachable via MRU
    /// switching until its first focus.
    pub fn note_created(&mut self, w: WindowId) {
        if !self.order.contains(&w) && self.order.len() < CAP {
            self.order.push(w);
        }
    }

    /// Send `w` to the back: "I'm done with this one, stop offering it".
    /// Callers must also move focus off `w` — a still-focused window gets
    /// touched straight back to the front by the next observation.
    pub fn demote(&mut self, w: WindowId) {
        if self.order.contains(&w) {
            self.remove(w);
            self.order.push(w);
        }
    }

    pub fn remove(&mut self, w: WindowId) {
        self.order.retain(|x| *x != w);
    }

    /// Most recent window matching `pred`, skipping `skip` (the currently
    /// focused window — "switch to MRU" never means "stay put").
    pub fn most_recent(
        &self,
        skip: Option<WindowId>,
        mut pred: impl FnMut(WindowId) -> bool,
    ) -> Option<WindowId> {
        self.order
            .iter()
            .copied()
            .filter(|w| Some(*w) != skip)
            .find(|w| pred(*w))
    }

    pub fn iter(&self) -> impl Iterator<Item = WindowId> + '_ {
        self.order.iter().copied()
    }
}
