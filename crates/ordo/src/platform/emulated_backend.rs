//! The emulated workspace backend: Ordo owns workspaces outright.
//!
//! Everything lives in one native Space. A window on a hidden workspace is
//! parked off-screen (bottom-right corner, a sliver left visible because macOS
//! forcibly re-homes fully-off-screen windows). Switching parks the outgoing
//! workspace's windows and restores the incoming one's to their saved frames.
//! The decisions are the pure [`Ledger`]; this type just applies them with AX.
//!
//! Chosen via `--backend emulated`. Its tradeoffs vs. native (Mission Control
//! clutter, Cmd-Tab showing every app, the visible sliver) are the price of
//! unlimited instant workspaces with no private Space APIs — see the research.
//! Best paired with "Displays have separate Spaces" off.

use std::collections::{HashMap, HashSet};

use ordo_core::{Pid, Rect, WindowId, WorkspaceId};

use crate::backend::{
    BackendError, BackendTopology, Capabilities, MonitorWorkspace, Result, WorkspaceBackend,
};
use crate::ledger::{Ledger, MoveAction};

use super::{ax, display};

/// How much of a parked window stays on-screen. macOS refuses to keep a fully
/// off-screen window where you put it, so we leave a 1px handle — also the
/// manual escape hatch if Ordo dies mid-park.
const SLIVER: f64 = 1.0;

pub struct EmulatedBackend {
    ledger: Ledger,
    /// On-screen frame to restore each parked window to.
    saved: HashMap<WindowId, Rect>,
    /// Windows currently parked off-screen. Tracked so re-parking an
    /// already-parked window (a hidden->hidden move) doesn't overwrite its real
    /// saved frame with the sliver position.
    parked: HashSet<WindowId>,
}

impl EmulatedBackend {
    pub fn new(count: u8) -> Self {
        EmulatedBackend {
            ledger: Ledger::new(count),
            saved: HashMap::new(),
            parked: HashSet::new(),
        }
    }

    fn current_frames() -> HashMap<WindowId, (Pid, Rect)> {
        ax::windows()
            .into_iter()
            .map(|w| (w.id, (w.app, w.frame)))
            .collect()
    }

    /// Bottom-right corner of the main display, keeping the window's size — so
    /// only `SLIVER` points remain visible.
    fn park_frame(size: Rect) -> Rect {
        let main = main_frame();
        Rect {
            x: main.x + main.w - SLIVER,
            y: main.y + main.h - SLIVER,
            w: size.w,
            h: size.h,
        }
    }

    /// Bookkeep a park and return the frame write it requires, so a switch can
    /// batch every write into one parallel pass instead of moving windows one
    /// by one (which made multi-monitor switches visibly ripple).
    fn park(
        &mut self,
        window: WindowId,
        frames: &HashMap<WindowId, (Pid, Rect)>,
    ) -> Option<(Pid, WindowId, Rect)> {
        // Save the real frame only on the transition onto-screen -> parked; a
        // window already parked keeps its original saved frame rather than
        // recording the sliver position.
        if self.parked.contains(&window) {
            return None;
        }
        let (pid, f) = frames.get(&window)?;
        self.saved.insert(window, *f);
        self.parked.insert(window);
        Some((*pid, window, Self::park_frame(*f)))
    }

    fn restore(
        &mut self,
        window: WindowId,
        frames: &HashMap<WindowId, (Pid, Rect)>,
    ) -> Option<(Pid, WindowId, Rect)> {
        self.parked.remove(&window);
        let (pid, _) = frames.get(&window)?;
        let f = self.saved.get(&window)?;
        Some((*pid, window, *f))
    }
}

impl WorkspaceBackend for EmulatedBackend {
    fn topology(
        &mut self,
        windows: &[WindowId],
        monitors: &[(ordo_core::MonitorId, bool)],
    ) -> Result<BackendTopology> {
        self.ledger.forget_missing(windows);
        self.ledger.note_seen(windows);

        let mons = monitors
            .iter()
            .map(|(id, _)| MonitorWorkspace {
                monitor: *id,
                active: self.ledger.current(),
                count: self.ledger.count(),
            })
            .collect();
        let window_ws = self.ledger.window_ws().into_iter().collect();

        Ok(BackendTopology {
            monitors: mons,
            window_ws,
        })
    }

    fn switch_workspace(&mut self, target: WorkspaceId) -> Result<()> {
        let plan = self.ledger.switch(target);
        if plan.park.is_empty() && plan.restore.is_empty() {
            return Ok(());
        }
        let frames = Self::current_frames();
        let mut writes = Vec::new();
        for w in plan.park {
            writes.extend(self.park(w, &frames));
        }
        for w in plan.restore {
            writes.extend(self.restore(w, &frames));
        }
        ax::set_frames(&writes);
        Ok(())
    }

    fn move_window_to_workspace(&mut self, window: WindowId, target: WorkspaceId) -> Result<()> {
        let frames = Self::current_frames();
        let write = match self.ledger.assign_window(window, target) {
            Some(MoveAction::Park) => self.park(window, &frames),
            Some(MoveAction::Restore) => self.restore(window, &frames),
            None => return Err(BackendError(format!("workspace {} out of range", target.0))),
        };
        ax::set_frames(write.as_slice());
        Ok(())
    }

    fn rescue_window(&mut self, window: WindowId) -> Result<()> {
        // Claim it for the visible workspace and bring it back on-screen.
        let frames = Self::current_frames();
        self.ledger.assign_window(window, self.ledger.current());
        let write = self.restore(window, &frames);
        ax::set_frames(write.as_slice());
        Ok(())
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            // The whole point of emulation: we can mint workspaces freely.
            fixed_workspace_count: false,
            max_workspaces: self.ledger.count(),
        }
    }
}

fn main_frame() -> Rect {
    let displays = display::active_displays();
    displays
        .iter()
        .find(|d| d.is_main)
        .or_else(|| displays.first())
        .map(|d| d.frame)
        .unwrap_or(Rect {
            x: 0.0,
            y: 0.0,
            w: 1920.0,
            h: 1080.0,
        })
}
