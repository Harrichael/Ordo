//! What the menu bar shows of Ordo: every workspace, which one is current,
//! and what is on each.
//!
//! Read from the core's belief, never from intent, so the menu bar cannot
//! show a switch that has not landed. Pure, so the whole account is testable
//! without AppKit; drawing it is `platform::status_item`'s job.

use std::collections::HashMap;

use ordo_core::{Mode, Pid, State, WorkspaceId};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MenuBarView {
    /// One per workspace, in order.
    pub workspaces: Vec<WorkspaceEntry>,
    pub current: Option<WorkspaceId>,
    /// False once rescued (or started paused): the core ignores every
    /// command then, so a pick from the menu would do nothing.
    pub engaged: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkspaceEntry {
    pub id: WorkspaceId,
    /// Apps with a window here, each once, most recently used first — the
    /// order that answers "which workspace was I doing that on".
    pub apps: Vec<Pid>,
}

impl MenuBarView {
    pub fn of(s: &State) -> Self {
        let mut workspaces: Vec<WorkspaceEntry> = (1..=s.workspace_count)
            .map(|n| WorkspaceEntry {
                id: WorkspaceId(n),
                apps: Vec::new(),
            })
            .collect();

        let rank: HashMap<_, _> = s
            .focus_history
            .iter()
            .enumerate()
            .map(|(i, w)| (w, i))
            .collect();
        let mut windows: Vec<_> = s.windows.values().collect();
        windows.sort_by_key(|r| rank.get(&r.id).copied().unwrap_or(usize::MAX));

        for r in windows {
            let Some(entry) = workspaces.iter_mut().find(|e| e.id == r.workspace) else {
                continue;
            };
            if !entry.apps.contains(&r.app) {
                entry.apps.push(r.app);
            }
        }

        MenuBarView {
            workspaces,
            current: s.current_workspace(),
            engaged: s.mode == Mode::Active,
        }
    }
}
