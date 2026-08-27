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

use std::collections::HashMap;

use ordo_core::{Rect, WindowId, WorkspaceId};

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
}

impl EmulatedBackend {
    pub fn new(count: u8) -> Self {
        EmulatedBackend {
            ledger: Ledger::new(count),
            saved: HashMap::new(),
        }
    }

    fn current_frames() -> HashMap<WindowId, Rect> {
        ax::scan()
            .windows
            .into_iter()
            .map(|w| (w.id, w.frame))
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

    fn park(&mut self, window: WindowId, frames: &HashMap<WindowId, Rect>) {
        if let Some(f) = frames.get(&window) {
            self.saved.insert(window, *f);
            ax::set_frame(window, Self::park_frame(*f));
        }
    }

    fn restore(&mut self, window: WindowId) {
        if let Some(f) = self.saved.get(&window) {
            ax::set_frame(window, *f);
        }
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
        for w in plan.park {
            self.park(w, &frames);
        }
        for w in plan.restore {
            self.restore(w);
        }
        Ok(())
    }

    fn move_window_to_workspace(&mut self, window: WindowId, target: WorkspaceId) -> Result<()> {
        match self.ledger.assign_window(window, target) {
            Some(MoveAction::Park) => {
                let frames = Self::current_frames();
                self.park(window, &frames);
                Ok(())
            }
            Some(MoveAction::Restore) => {
                self.restore(window);
                Ok(())
            }
            None => Err(BackendError(format!("workspace {} out of range", target.0))),
        }
    }

    fn rescue_window(&mut self, window: WindowId) -> Result<()> {
        // Claim it for the visible workspace and bring it back on-screen.
        self.ledger.assign_window(window, self.ledger.current());
        self.restore(window);
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
