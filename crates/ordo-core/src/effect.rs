use serde::{Deserialize, Serialize};

use crate::event::RescanTrigger;
use crate::ids::{OpId, Point, Rect, WindowId, WorkspaceId};

/// Instructions to the shell. Executors perform them and report back via
/// `Event::EffectResult`; the world's actual reaction arrives via the next
/// `WorldObserved`. Logging is deliberately NOT an effect — the engine logs
/// every event and effect ambiently, so making it one would double the stream.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Effect {
    /// Bring EVERY display to its `target`-th workspace. Workspaces span all
    /// monitors by definition, so there is no per-monitor switch effect.
    SwitchWorkspace {
        op: OpId,
        target: WorkspaceId,
    },
    /// Move one window to a workspace, including whatever visible change
    /// that implies (the emulated backend parks or restores it). The
    /// corrective for a window standing where it shouldn't.
    MoveWindowToWorkspace {
        op: OpId,
        window: WindowId,
        target: WorkspaceId,
    },
    /// Rewrite one window's workspace DECLARATION and nothing else — no
    /// frame is touched. A carry wants exactly this: the window stays put
    /// and the scenery changes around it. Issuing a full move there raced
    /// two frame writes per carry (the move's park against the following
    /// switch's restore), a coin flip against any slow app.
    AssignWindowToWorkspace {
        op: OpId,
        window: WindowId,
        target: WorkspaceId,
    },
    /// The core computes target geometry itself (pure math over monitor
    /// frames); the shell only performs the AX write.
    SetWindowFrame {
        op: OpId,
        window: WindowId,
        frame: Rect,
    },
    /// Raise + focus. v0's MRU predicates only ever pick targets on the
    /// current workspace, so this never implies a workspace switch. The first
    /// cross-workspace focus feature will force this into the backend trait —
    /// a known cliff, flagged in the plan.
    FocusWindow {
        op: OpId,
        window: WindowId,
    },
    WarpMouse {
        to: Point,
    },
    /// Impose this relative z-order (front-to-back) on the listed windows.
    /// The order is always derived from the MRU focus history — raising a
    /// window on macOS IS focusing it, so visual stacking and MRU are the
    /// same fact, and keeping a separate stacking memory would just be a
    /// second copy that drifts. Fire-and-forget like `WarpMouse`: z-order is
    /// not part of the core's belief, so there is no op and no expectation
    /// (the shell self-verifies against async apps).
    RestackWindows {
        order: Vec<WindowId>,
    },
    /// Ask the shell to enumerate and deliver a fresh `WorldObserved`. Emitted
    /// after every mutation so ops get confirmed promptly instead of waiting
    /// for the periodic tick.
    RequestRescan {
        reason: RescanTrigger,
    },
    SetIntercepting {
        enabled: bool,
    },
}

/// The observable post-condition of an issued effect. Snapshot deltas that
/// match a pending expectation are our own echo (macOS reports our moves back
/// to us); everything else is genuinely external. Matching on *content* rather
/// than time windows keeps the classification deterministic and replayable.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Expectation {
    AllMonitorsOn(WorkspaceId),
    WindowOn {
        window: WindowId,
        workspace: WorkspaceId,
    },
    WindowFramed {
        window: WindowId,
        frame: Rect,
    },
    Focused(WindowId),
}

/// Which independent placement fight a correction belongs to. Workspace
/// assignment and on-screen frame are corrected separately and damped
/// separately, so a window wrong on both doesn't burn one shared budget.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CorrectionAxis {
    Workspace,
    Frame,
}

impl Expectation {
    /// The window whose placement this expectation guards, if any — used to
    /// reset that window's damping counter once the world accepts a correction.
    pub fn window(&self) -> Option<WindowId> {
        match self {
            Expectation::WindowOn { window, .. } | Expectation::WindowFramed { window, .. } => {
                Some(*window)
            }
            _ => None,
        }
    }

    /// The damping axis this expectation's correction is damped on.
    pub fn axis(&self) -> Option<CorrectionAxis> {
        match self {
            Expectation::WindowOn { .. } => Some(CorrectionAxis::Workspace),
            Expectation::WindowFramed { .. } => Some(CorrectionAxis::Frame),
            _ => None,
        }
    }
}
