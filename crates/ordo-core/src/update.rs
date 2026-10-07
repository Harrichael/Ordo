use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::effect::{CorrectionAxis, Effect, Expectation};
use crate::event::{
    AxHintKind, Event, Gesture, HotkeyAction, Input, OpOutcome, RescanTrigger, WorldSnapshot,
};
use crate::ids::{
    MonitorId, OpId, Pid, Rect, VirtualMonitorId, WindowId, WorkspaceId, FRAME_EPSILON,
};
use crate::project::{after_move, Projection};
use crate::reconcile::{self, Delta};
use crate::state::{Away, Awaited, FocusIntent, Landing, Mode, PendingOp, State, WindowRecord};

/// How long an expectation may go unmet before its op is declared lost.
///
/// Elapsed time, not a snapshot count. Snapshots are not a clock: they fire on
/// the periodic timer, on post-effect requests, AND on every accessibility
/// hint, and a workspace switch produces a burst of hints — three snapshots
/// went by in ~150ms while an app takes a median of 398ms to accept a focus
/// grant (415 measured grants: p50 398ms, p75 647ms, p90 975ms). So the grant
/// was declared lost and re-issued before the app could answer, on roughly one
/// switch in four, doubling the time a switch took to settle. One second is
/// the p90 of grants that land at all.
///
/// Deliberately one constant for every expectation kind. `AllMonitorsOn`
/// confirms on the post-effect snapshot under the emulated backend regardless;
/// `WindowOn`/`WindowFramed` merely retry a second later instead of ~150ms
/// later, which reduces write fights rather than regressing anything. The one
/// exposure is the native backend, whose multi-step switches run ~350ms per
/// step and could exceed this — native is not the daily driver, so that is
/// noted rather than special-cased.
///
/// The cost accepted: while an op is pending, its expectation explains
/// matching deltas as self-caused, so a dead op now suppresses external
/// attribution for up to a second instead of ~150ms. That narrowness was the
/// original reason the budget was small; a second of it is worth not fighting
/// every app that answers slowly.
const EXPECTATION_TTL_NS: u64 = 1_000_000_000;

/// Correctives per window (and per tear episode, and per focus declaration)
/// before we stop fighting and log instead. An app that re-places its own
/// window or keeps its own key window wins after this many rounds —
/// divergence becomes a loud log line, never an effect loop.
const DAMPING_LIMIT: u8 = 3;

/// The result of one pure step. `notes` are deterministic diagnostics — the
/// core's explanation of what it concluded (echo vs external, ops lost,
/// divergence). They exist for the log and for replay assertions; the shell
/// executes nothing from them.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Step {
    pub state: State,
    pub effects: Vec<Effect>,
    pub notes: Vec<Note>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Note {
    /// A snapshot confirmed the post-condition of our own op.
    SelfConfirmed { op: OpId },
    /// An op's expectation expired unconfirmed.
    OpLost { op: OpId },
    /// The executor reported failure; the pending expectation was dropped.
    OpFailed { op: OpId, detail: String },
    /// A newer op of the same kind made this one moot before it was seen to
    /// land: a switch replaced by the next press of a burst, a focus grant by
    /// the next grant (the app queues skip the older one), a window's move by
    /// its next move. Its expectation was dropped.
    OpSuperseded { op: OpId, by: OpId },
    /// A change we didn't cause. Belief absorbed it.
    External { delta: Delta },
    /// The user's input explained a focus landing, and it entered the MRU
    /// order (and, after a key press, the declaration). What makes the rule
    /// in `record_landing` measurable.
    LandingExplained { window: WindowId, by: Input },
    /// The verdict `handle_gesture` reached on a witnessed gesture: whether
    /// it armed a follow and, when a mouse-down did not, which visible window
    /// swallowed the point. This exists because an UNARMED follow is otherwise
    /// invisible — the gesture event is logged, the hidden landing is held,
    /// and nothing in between says why (the same absence-of-evidence hole
    /// `park_trace` closed for parking). It also proves whether `SystemSwitch`
    /// reaches the core at all, and a Dock click classified as a `MouseDown`
    /// inside a window is one the shell failed to see was the Dock's. The
    /// reverse, a click into a window taken for the Dock's, shows as a `Dock`
    /// row whose `at` lies inside a visible window's frame.
    GestureClassified {
        gesture: Gesture,
        armed: bool,
        within: Option<WindowId>,
    },
    /// A witnessed gesture (Cmd+Tab, a Dock click) landed focus on a hidden
    /// window; we switched workspace and/or viewed its monitor to follow the
    /// user. `monitor` is set when the landing needed a view change.
    FollowedFocus {
        window: WindowId,
        target: WorkspaceId,
        #[serde(default)]
        monitor: Option<VirtualMonitorId>,
    },
    /// A visible window stood on a live display other than its monitor's,
    /// with no display change to explain it — a drag, a resize across the
    /// seam, an app placing itself — and its monitor declaration followed.
    /// Placement within a workspace is observational; this is the one
    /// observation that writes a monitor declaration.
    MonitorAdopted {
        window: WindowId,
        monitor: VirtualMonitorId,
    },
    /// Focus fell onto a hidden workspace with no gesture to explain it while
    /// the OS owned the slot. Nobody can type into an invisible window, so
    /// Ordo declared the visible workspace's MRU window instead (and the
    /// re-assertion that follows pulls focus there). `from` is the hidden
    /// window the pull-back is pulling away from.
    HeldFocus {
        window: WindowId,
        from: WindowId,
        from_app: Pid,
    },
    /// The window holding focus closed, and focus went to the next window in
    /// the MRU order on its monitor (`to`), or that monitor's desktop (`None`).
    FocusHandedOn {
        closed: WindowId,
        to: Option<WindowId>,
    },
    /// The world contradicted the focus declaration; the grant was re-issued.
    FocusReasserted { window: WindowId },
    /// Focus re-assertion hit the damping limit: the app kept its own key
    /// window. The declaration is retired — the OS owns the slot until the
    /// next command. `winner` is the key window observed at that moment (its
    /// app is what the concession is keyed on); `None` is a focus vacuum.
    FocusDiverged {
        window: WindowId,
        winner: Option<WindowId>,
        winner_app: Option<Pid>,
    },
    /// A window took focus from the desktop the user is on; re-granted.
    DesktopReasserted { display: MonitorId, from: WindowId },
    /// The desktop could not hold focus within the damping budget; the slot
    /// is conceded to the app holding it, as for a window declaration.
    DesktopDiverged {
        winner: Option<WindowId>,
        winner_app: Option<Pid>,
    },
    /// Monitors disagreed on workspace without an in-flight switch of ours.
    TearDetected { target: WorkspaceId },
    /// Tear realignment hit the damping limit; we stopped re-aligning.
    TearPersisting,
    /// Placement of this window hit the damping limit; we stopped correcting.
    Diverged { window: WindowId },
}

pub fn update(state: &State, event: &Event) -> Step {
    let mut s = state.clone();
    let mut effects = Vec::new();
    let mut notes = Vec::new();

    match event {
        Event::Hotkey { at, action } => {
            if !s.own_menu {
                s.menu_open = false;
            }
            if s.mode == Mode::Active {
                handle_hotkey(&mut s, *action, at.mono_ns, &mut effects, &mut notes);
            }
        }
        Event::WorldObserved { at, trigger, snap } => {
            handle_snapshot(
                state,
                &mut s,
                at.mono_ns,
                trigger,
                snap,
                &mut effects,
                &mut notes,
            );
        }
        Event::EffectResult { op, outcome, .. } => {
            handle_effect_result(&mut s, *op, outcome, &mut notes);
        }
        Event::RescueEngaged { .. } => {
            s.mode = Mode::Rescued;
            // Ops in flight will never be verified or retried again; keeping
            // them would only misattribute their late echoes.
            s.pending.clear();
            s.workspace_intent = None;
            // The desktop is the user's again, focus included.
            s.declare_focus(FocusIntent::Deferred);
            // The tap already stopped intercepting on its own fast path; this
            // records the same intent from the core's side and is idempotent.
            effects.push(Effect::SetIntercepting { enabled: false });
        }
        Event::Engaged { .. } => {
            // The exact mirror of RescueEngaged. Belief needs no refresh:
            // snapshots kept applying while Rescued, only actions were withheld.
            s.mode = Mode::Active;
            effects.push(Effect::SetIntercepting { enabled: true });
        }
        // Not gated on mode: a gesture while rescued still means the OS owns
        // focus, which is exactly what a later Engaged must find.
        Event::Gesture { at, gesture } => handle_gesture(&mut s, *gesture, at.mono_ns, &mut notes),
    }

    Step {
        state: s,
        effects,
        notes,
    }
}

/// The windows of `ws` the projection in force puts on screen, most recently
/// used first: where a command looks for the window to hand focus to. It
/// reads belief, not declarations, on purpose: a window handed focus must be
/// on screen now. `desired_stack` reads declarations.
fn mru_stack(s: &State, ws: WorkspaceId) -> Vec<WindowId> {
    let proj = s.projection();
    s.focus_history
        .iter()
        .filter(|w| {
            s.windows
                .get(w)
                .is_some_and(|r| r.workspace == ws && proj.is_hosted(r.vmonitor))
        })
        .collect()
}

/// The stacking the declarations call for, front to back: the windows the
/// declared workspace and layout put on screen, in MRU order — and since
/// declaring a window's focus puts it at the head of that order, the declared
/// focus heads it. Read from declarations alone, so a command never builds a
/// stack of its own: it declares, and this says what the screen should show.
/// A window whose move or assignment is still pending stands where it is
/// going (run 51: a carry's restack, headed by a resident sibling, made
/// AppKit key that sibling instead of the carried window).
fn desired_stack(s: &State) -> Vec<WindowId> {
    let Some(here) = s.declared_workspace() else {
        return Vec::new();
    };
    let proj = s.declared_projection();
    // A window floating above the ordinary layer (the screenshot tool's
    // window is layer 3) is above every ordinary one whatever is raised, and
    // the worker, reading only layer 0, would wait for it on every restack.
    s.focus_history
        .iter()
        .filter(|w| {
            s.declared_workspace_of(*w) == Some(here)
                && s.declared_vmonitor_of(*w).is_some_and(|m| proj.is_hosted(m))
                && s.windows.get(w).is_some_and(|r| r.layer == 0)
        })
        .collect()
}

/// The effects that reveal windows. Each scrambles the real stack (parking
/// and app-hiding leave it in whatever order the un-hides land) without
/// changing the desired one.
fn reveals(e: &Effect) -> bool {
    matches!(
        e,
        Effect::SwitchWorkspace { .. }
            | Effect::ViewMonitor { .. }
            | Effect::SetVirtualMonitors { .. }
            | Effect::MergeMonitors { .. }
            | Effect::MoveMonitor { .. }
    )
}

/// Push the desired stack if it differs from the one the step began under, or
/// if something was revealed. Emitted last, because the shell takes the
/// landing it waits on when the restack is submitted; never for nothing,
/// because each one supersedes the restack in flight.
///
/// A lone window is sent only after a reveal: there is nothing to order it
/// against, but an un-hide in the same step may have stolen focus, and the
/// stacking worker takes it back.
fn restack_to_intent(s: &State, before: &[WindowId], revealed: bool, fx: &mut Vec<Effect>) {
    let order = desired_stack(s);
    let changed = order.len() >= 2 && order != before;
    if !(changed || (revealed && !order.is_empty())) {
        return;
    }
    // Under a desktop declaration, or with the slot the OS's, the top is
    // only the most recent window, and focus is not the worker's to take.
    let focus_top = s.focus_intent() == FocusIntent::Window(order[0]);
    fx.push(restack(s, order, focus_top));
}

/// The one way a restack is asked for: with each root in `order`, the visible
/// windows attached to it, whose frames the shell counts as the root's.
fn restack(s: &State, order: Vec<WindowId>, focus_top: bool) -> Effect {
    let attached = s
        .windows
        .values()
        .filter(|r| r.parent.is_some() && s.is_visible(r))
        .map(|r| (r.id, s.root_of(r.id)))
        .filter(|(w, root)| w != root && order.contains(root))
        .collect();
    Effect::RestackWindows {
        order,
        attached,
        focus_top,
    }
}

/// Issue a view change to `target`, with its expectation. The single emitter,
/// so every path that reveals a monitor books it the same way.
fn push_view(s: &mut State, target: VirtualMonitorId, now_ns: u64, fx: &mut Vec<Effect>) -> OpId {
    let op = s.mint_op();
    fx.push(Effect::ViewMonitor { op, target });
    if s.virtual_monitors.is_some_and(|v| v.viewed != target) {
        s.pending.push(PendingOp {
            op,
            expect: Expectation::Viewing(target),
            issued_ns: now_ns,
        });
    }
    op
}

/// Hand focus to a display's desktop, with its expectation. The single
/// emitter, shared by the command that moves onto an empty monitor and the
/// enforcement that holds it there.
fn push_desktop(
    s: &mut State,
    display: MonitorId,
    now_ns: u64,
    fx: &mut Vec<Effect>,
    notes: &mut Vec<Note>,
) -> OpId {
    let op = s.mint_op();
    fx.push(Effect::FocusDesktop { op, display });
    let expect = Expectation::DesktopFocused;
    retire_grants(s, op, &expect, s.focused.is_some(), now_ns, notes);
    op
}

/// Hand focus to `window`. The single emitter with `push_desktop`. `expect`
/// is false where the belief already holds this focus, so no delta will need
/// explaining.
fn push_focus(
    s: &mut State,
    window: WindowId,
    expect: bool,
    now_ns: u64,
    fx: &mut Vec<Effect>,
    notes: &mut Vec<Note>,
) -> OpId {
    let op = s.mint_op();
    fx.push(Effect::FocusWindow { op, window });
    retire_grants(s, op, &Expectation::Focused(window), expect, now_ns, notes);
    op
}

/// Book a new grant, `op`, against the older ones. Each app queue skips a
/// focus once a newer one is queued anywhere, so an older grant for another
/// target is moot the moment this one is issued, and retired. Then this one
/// is expected even where the last look already saw its window key
/// (`expected` false): the overtaken grant may have been sent already and
/// land late, and the newest grant, on its way behind it, is what
/// `enforce_focus` must wait for rather than grant over. With nothing
/// overtaken, an older grant for the SAME target stands in for this one, and
/// stays the record that a grant for it is on its way.
fn retire_grants(
    s: &mut State,
    op: OpId,
    expect: &Expectation,
    expected: bool,
    now_ns: u64,
    notes: &mut Vec<Note>,
) {
    let overtakes = s
        .pending
        .iter()
        .any(|p| is_focus_grant(&p.expect) && p.expect != *expect);
    let book = expected || overtakes;
    supersede(s, op, |e| is_focus_grant(e) && (book || e != expect), notes);
    if book {
        s.pending.push(PendingOp {
            op,
            expect: expect.clone(),
            issued_ns: now_ns,
        });
    }
}

fn is_focus_grant(e: &Expectation) -> bool {
    matches!(e, Expectation::Focused(_) | Expectation::DesktopFocused)
}

/// Switch every monitor to `target`. The single emitter, because a switch is
/// also Ordo's word on where the user is going: it replaces the workspace
/// intent, and the switch it overtakes can never be confirmed now.
fn push_switch(
    s: &mut State,
    target: WorkspaceId,
    now_ns: u64,
    fx: &mut Vec<Effect>,
    notes: &mut Vec<Note>,
) -> OpId {
    let op = s.mint_op();
    fx.push(Effect::SwitchWorkspace { op, target });
    supersede(
        s,
        op,
        |e| matches!(e, Expectation::AllMonitorsOn(_)),
        notes,
    );
    s.pending.push(PendingOp {
        op,
        expect: Expectation::AllMonitorsOn(target),
        issued_ns: now_ns,
    });
    s.workspace_intent = Some((op, target));
    op
}

/// Wait to see `expect` come true. A window's older move to a workspace is
/// moot once a newer one is issued; left pending, it would pass for where the
/// window is going (`State::declared_workspace_of`), and be retried over the
/// newer move once it expired.
fn expect(
    s: &mut State,
    op: OpId,
    expect: Expectation,
    now_ns: u64,
    notes: &mut Vec<Note>,
) {
    if let Expectation::WindowOn { window, .. } = expect {
        supersede(
            s,
            op,
            |e| matches!(e, Expectation::WindowOn { window: w, .. } if *w == window),
            notes,
        );
    }
    s.pending.push(PendingOp {
        op,
        expect,
        issued_ns: now_ns,
    });
}

fn supersede(
    s: &mut State,
    by: OpId,
    moot: impl Fn(&Expectation) -> bool,
    notes: &mut Vec<Note>,
) {
    s.pending.retain(|p| {
        let keep = !moot(&p.expect);
        if !keep {
            notes.push(Note::OpSuperseded { op: p.op, by });
        }
        keep
    });
}

/// An op left `pending` for good. The workspace intent is its switch's for
/// exactly as long as that switch is under way.
fn op_ended(s: &mut State, op: OpId) {
    if s.workspace_intent.is_some_and(|(o, _)| o == op) {
        s.workspace_intent = None;
    }
}

/// The monitor a focus target needs viewed, if it sits on a hidden one —
/// nobody can type into a parked window — with the projection the world will
/// be under once that view lands, for restacking and warping against. The
/// caller emits the focus grant AFTER the view, as a switch does: the grant
/// must follow the moves and un-hide that reveal the window.
fn view_for(s: &State, target: WindowId) -> (Option<VirtualMonitorId>, Projection) {
    let vm = s.windows[&target].vmonitor;
    let proj = s.projection();
    if proj.is_hosted(vm) || s.virtual_monitors.is_none() {
        return (None, proj);
    }
    (Some(vm), s.projection_with(Some(vm), None))
}

/// The user reached for focus through the OS rather than through Ordo (by any
/// gesture except a key press, below). Whatever Ordo declared is moot — the OS owns the slot until the next command — and
/// the next observation gets to read a hidden-workspace landing as the user
/// going there, but only if the gesture could have been aimed there: a click
/// INTO a window on the visible workspace keys that window (or a sheet or
/// child Ordo does not model), so a hidden landing after it is a fling.
///
/// A click that ends an open menu is classified as the menu's, not as a click
/// into whatever window the menu was drawn over: it may be a menu item, and a
/// menu item can open anything. A click on the Dock is like Cmd+Tab: it names
/// no window, and can bring up any.
///
/// A key press is none of that: typing into the window Ordo declared is not
/// reaching for focus elsewhere. It only explains the focus change after it,
/// within the app it was typed into (see `Away::within`), and leaves a
/// click's own landing waiting.
fn handle_gesture(s: &mut State, gesture: Gesture, now_ns: u64, notes: &mut Vec<Note>) {
    if let Gesture::OwnMenu { open } = gesture {
        s.own_menu = open;
        s.menu_open = open;
        // What the click that opened it left waiting is spent: the user was
        // in the menu, not on the screen.
        if !open {
            s.unseen_landing = Landing::default();
            s.navigation_gesture = false;
        }
        return;
    }
    // The tap drops clicks while Ordo's menu is open, so one reaching here
    // means it has closed, even if its close was never heard.
    if gesture != Gesture::Key {
        s.own_menu = false;
    }
    if gesture == Gesture::Key {
        s.unseen_landing.away = Some(Away {
            from: s.focused,
            since_ns: now_ns,
            by: Input::Key,
            within: key_app(s),
        });
        return;
    }
    s.declare_focus(FocusIntent::Deferred);
    let opens_menu = matches!(gesture, Gesture::MenuBar { .. });
    let menu_was_open = std::mem::replace(&mut s.menu_open, opens_menu);
    let hit: Vec<WindowId> = match gesture {
        Gesture::MouseDown { at } if !menu_was_open => {
            let here = s.current_workspace();
            s.windows
                .values()
                .filter(|r| Some(r.workspace) == here && r.frame.contains(at))
                .map(|r| r.id)
                .collect()
        }
        _ => Vec::new(),
    };
    let within = hit.first().copied();
    let armed = match gesture {
        Gesture::SystemSwitch | Gesture::Dock { .. } => true,
        Gesture::MenuBar { .. } | Gesture::Key | Gesture::OwnMenu { .. } => false,
        Gesture::MouseDown { .. } => within.is_none(),
    };
    // The shell saw the Dock take the click, so it was no menu item.
    let by = match gesture {
        Gesture::Dock { .. } => Input::Dock,
        _ if menu_was_open => Input::MenuBar,
        g => g.into(),
    };
    // After the declaration, which clears them.
    s.navigation_gesture = armed;
    s.unseen_landing = Landing {
        away: Some(Away {
            from: s.focused,
            since_ns: now_ns,
            by,
            within: None,
        }),
        awaited: if hit.is_empty() {
            Vec::new()
        } else {
            vec![Awaited {
                roots: hit.iter().map(|w| s.root_of(*w)).collect(),
                since_ns: now_ns,
                by: Input::Click,
            }]
        },
    };
    notes.push(Note::GestureClassified {
        gesture,
        armed,
        within,
    });
}

/// What a hotkey resolved to against the current belief, before anything is
/// minted or emitted. Resolution (which window, which workspace, or nothing
/// at all) is separated from execution so that `execute` can be total: every
/// command arm must produce the focus declaration it leaves behind, and a new
/// hotkey cannot be added without deciding it — the match on `HotkeyAction`
/// in `resolve` and the match on `Command` in `execute` are both exhaustive,
/// and `execute` returns a `FocusIntent`, never an `Option` of one. What the
/// screen stacks is no arm's business: it follows from the declarations,
/// once they are all made (`restack_to_intent`).
enum Command {
    Switch {
        target: WorkspaceId,
    },
    Carry {
        window: WindowId,
        target: WorkspaceId,
    },
    Focus {
        target: WindowId,
    },
    Demote {
        from: WindowId,
        to: WindowId,
    },
    MoveToMonitor {
        window: WindowId,
        target: VirtualMonitorId,
    },
    View {
        target: VirtualMonitorId,
    },
    ToggleVirtualMonitors,
    Merge {
        from: VirtualMonitorId,
        into: VirtualMonitorId,
    },
    AddMonitor,
    MoveWorkspace {
        from: WorkspaceId,
        to: WorkspaceId,
    },
    MoveMonitor {
        from: VirtualMonitorId,
        to: VirtualMonitorId,
    },
}

fn handle_hotkey(
    s: &mut State,
    action: HotkeyAction,
    now_ns: u64,
    fx: &mut Vec<Effect>,
    notes: &mut Vec<Note>,
) {
    // A hotkey that resolves to nothing (clamped at an edge, nothing focused)
    // touched neither the world nor the declaration.
    let Some(cmd) = resolve(s, action) else {
        return;
    };
    let before = desired_stack(s);
    let (focus, op) = execute(s, cmd, now_ns, fx, notes);
    s.declare_focus(focus);
    let revealed = fx.iter().any(reveals);
    restack_to_intent(s, &before, revealed, fx);
    fx.push(Effect::RequestRescan {
        reason: RescanTrigger::PostEffect { op },
    });
}

fn resolve(s: &State, action: HotkeyAction) -> Option<Command> {
    match action {
        HotkeyAction::WorkspacePrev
        | HotkeyAction::WorkspaceNext
        | HotkeyAction::WorkspaceSwitchTo(_) => {
            let cur = s.declared_workspace()?;
            let target = match action {
                HotkeyAction::WorkspacePrev if cur.0 > 1 => WorkspaceId(cur.0 - 1),
                HotkeyAction::WorkspaceNext if cur.0 < s.workspace_count => WorkspaceId(cur.0 + 1),
                HotkeyAction::WorkspaceSwitchTo(t)
                    if t != cur && t.0 >= 1 && t.0 <= s.workspace_count =>
                {
                    t
                }
                _ => return None, // clamped at the edge (or already there)
            };
            Some(Command::Switch { target })
        }

        HotkeyAction::CarryFocusedToWorkspacePrev | HotkeyAction::CarryFocusedToWorkspaceNext => {
            let window = s.declared_focus()?;
            let cur = s.declared_workspace()?;
            // You carry what's with you: dragging a window over from a hidden
            // workspace would materialize it from nowhere. Read against the
            // DECLARATION, so a fling onto a parked sibling between two chords
            // cannot make the second one do nothing (run 51 seq 22484), and
            // a second carry before the first is confirmed carries it on.
            if s.declared_workspace_of(window)? != cur {
                return None;
            }
            let target = match action {
                HotkeyAction::CarryFocusedToWorkspacePrev if cur.0 > 1 => WorkspaceId(cur.0 - 1),
                HotkeyAction::CarryFocusedToWorkspaceNext if cur.0 < s.workspace_count => {
                    WorkspaceId(cur.0 + 1)
                }
                _ => return None, // clamped at the edge
            };
            Some(Command::Carry { window, target })
        }

        HotkeyAction::MruWorkspace
        | HotkeyAction::MruMonitor
        | HotkeyAction::MruApp
        | HotkeyAction::MruOtherMonitor => {
            let cur_ws = s.current_workspace()?;
            let focused = s.declared_focus();
            let focused_rec = focused.and_then(|w| s.windows.get(&w));
            // The scoped variants are relative to the focused window; with
            // nothing focused there is no "same monitor/app" to speak of.
            if focused_rec.is_none() && action != HotkeyAction::MruWorkspace {
                return None;
            }
            let target = s.focus_history.most_recent(focused, |w| {
                let Some(r) = s.windows.get(&w) else {
                    return false;
                };
                if r.workspace != cur_ws {
                    return false;
                }
                // Monitor scoping reads the DECLARED virtual monitor, never
                // the display: two virtual monitors collapsed onto one
                // display are still two monitors to Alt+Shift+Tab.
                match action {
                    HotkeyAction::MruWorkspace => true,
                    HotkeyAction::MruMonitor => {
                        focused_rec.is_some_and(|f| r.vmonitor == f.vmonitor)
                    }
                    HotkeyAction::MruOtherMonitor => {
                        focused_rec.is_some_and(|f| r.vmonitor != f.vmonitor)
                    }
                    HotkeyAction::MruApp => focused_rec.is_some_and(|f| r.app == f.app),
                    _ => unreachable!(),
                }
            })?;
            Some(Command::Focus { target })
        }

        HotkeyAction::MruDemote => {
            let workspace = s.current_workspace()?;
            let from = s.declared_focus()?;
            // Demoting is only meaningful if focus can actually go somewhere
            // else; with nowhere to go, do nothing.
            let to = s.focus_history.most_recent(Some(from), |w| {
                s.windows.get(&w).is_some_and(|r| r.workspace == workspace)
            })?;
            Some(Command::Demote { from, to })
        }

        HotkeyAction::MoveFocusedToMonitorPrev | HotkeyAction::MoveFocusedToMonitorNext => {
            let window = s.declared_focus()?;
            let cur = s.windows.get(&window)?.vmonitor;
            let target = match action {
                HotkeyAction::MoveFocusedToMonitorPrev if cur.0 > 1 => VirtualMonitorId(cur.0 - 1),
                HotkeyAction::MoveFocusedToMonitorNext if cur.0 < s.monitor_count() => {
                    VirtualMonitorId(cur.0 + 1)
                }
                _ => return None, // clamped at the edge
            };
            Some(Command::MoveToMonitor { window, target })
        }

        HotkeyAction::ViewMonitorPrev | HotkeyAction::ViewMonitorNext => {
            let v = s.virtual_monitors?;
            s.current_workspace()?;
            let target = match action {
                HotkeyAction::ViewMonitorPrev if v.viewed.0 > 1 => VirtualMonitorId(v.viewed.0 - 1),
                HotkeyAction::ViewMonitorNext if v.viewed.0 < v.count => {
                    VirtualMonitorId(v.viewed.0 + 1)
                }
                _ => return None, // clamped at the edge
            };
            Some(Command::View { target })
        }

        HotkeyAction::ToggleVirtualMonitors => {
            s.virtual_monitors?;
            Some(Command::ToggleVirtualMonitors)
        }

        HotkeyAction::MergeMonitors { from, into } => {
            let v = s.virtual_monitors?;
            let spare = v.count as usize > s.monitors.len().max(1);
            let exists = |m: VirtualMonitorId| (1..=v.count).contains(&m.0);
            (spare && from != into && exists(from) && exists(into))
                .then_some(Command::Merge { from, into })
        }

        HotkeyAction::AddMonitor => {
            let v = s.virtual_monitors?;
            (v.count < u8::MAX).then_some(Command::AddMonitor)
        }

        // Refused while a switch or a view change is on its way: what it
        // aims at is a number the move would change under it.
        HotkeyAction::MoveWorkspace { from, to } => {
            let exists = |w: WorkspaceId| (1..=s.workspace_count).contains(&w.0);
            (from != to && exists(from) && exists(to) && s.workspace_intent.is_none())
                .then_some(Command::MoveWorkspace { from, to })
        }

        HotkeyAction::MoveMonitor { from, to } => {
            let v = s.virtual_monitors?;
            let exists = |m: VirtualMonitorId| (1..=v.count).contains(&m.0);
            let viewing = s.pending.iter().any(|p| matches!(p.expect, Expectation::Viewing(_)));
            (from != to && exists(from) && exists(to) && !viewing)
                .then_some(Command::MoveMonitor { from, to })
        }
    }
}

/// Carry out a resolved command and return the focus declaration it leaves
/// behind, with the op whose landing the look after it is for. Total over
/// `Command` by construction — see the type's doc.
fn execute(
    s: &mut State,
    cmd: Command,
    now_ns: u64,
    fx: &mut Vec<Effect>,
    notes: &mut Vec<Note>,
) -> (FocusIntent, OpId) {
    match cmd {
        Command::Switch { target } => {
            // Hand focus to the destination's MRU window, emitted AFTER the
            // switch: fronting an app that is still hidden un-hides it with
            // nothing holding its parked windows, so the grant has to follow
            // that app's moves and un-hide, and the shell carries out each
            // app's writes in the order they were emitted. No mouse warp: the
            // core's frame belief for that window is its parked sliver
            // position, so a warp would aim at the corner.
            //
            // The focus target must be the destination's MRU head, even when
            // it never left: the restack puts that head on top, and the top of
            // a restack must be the key window.
            //
            // Monitor selection is global, so a switch never moves the view —
            // not even onto a destination whose windows all sit on hidden
            // monitors. The user chose the monitor they are on; that
            // destination is empty here, and is treated as any empty one.
            let head = mru_stack(s, target).first().copied();
            // An empty workspace (here) is still a place to be: the desktop
            // takes focus, as on an empty monitor — the anchor's display, the one
            // a desktop declaration holds and new windows are corralled onto.
            // Left with the app it had, that app could never be hidden, and
            // its windows parked for other workspaces would line the screen's
            // edge.
            let desktop = match head {
                Some(_) => None,
                None => s
                    .virtual_monitors
                    .and_then(|v| s.host_of(v.viewed))
                    .or_else(|| s.focused_monitor()),
            };
            let op = push_switch(s, target, now_ns, fx, notes);
            if let Some(fw) = head {
                // Re-asserting focus the belief already holds produces no
                // delta, so there is nothing to attribute to an expectation.
                // Both halves of the belief must hold it: mid-burst the seen
                // focus is a look behind, and a quick 1 -> 2 -> 1 finds it
                // still on this window while a grant to 2's is in flight.
                let unheld = s.focused != Some(fw) || s.declared_focus() != Some(fw);
                push_focus(s, fw, unheld, now_ns, fx, notes);
            }
            if let Some(display) = desktop {
                push_desktop(s, display, now_ns, fx, notes);
            }
            let focus = match (head, desktop) {
                (Some(fw), _) => FocusIntent::Window(fw),
                (None, Some(_)) => FocusIntent::Desktop,
                (None, None) => FocusIntent::Deferred,
            };
            (focus, op)
        }

        Command::Carry { window, target } => {
            // Reassign first, then switch: when the switch lands, the carried
            // window is already a resident of the destination and comes along.
            // Assignment only — the window's frame and focus don't change, so
            // no frame write may be issued for it (a full move parked it and
            // the switch immediately restored it: two racing writes).
            let move_op = s.mint_op();
            fx.push(Effect::AssignWindowToWorkspace {
                op: move_op,
                window,
                target,
            });
            expect(
                s,
                move_op,
                Expectation::WindowOn {
                    window,
                    workspace: target,
                },
                now_ns,
                notes,
            );
            let switch_op = push_switch(s, target, now_ns, fx, notes);
            (FocusIntent::Window(window), switch_op)
        }

        Command::Focus { target } => {
            // A target on a hidden monitor gets its monitor viewed just before
            // the grant.
            let (view, proj) = view_for(s, target);
            let center = s.projected_frame_in(&s.windows[&target], &proj).center();
            if let Some(vm) = view {
                push_view(s, vm, now_ns, fx);
            }
            let op = push_focus(s, target, true, now_ns, fx, notes);
            // Warp optimistically off our own belief of the frame rather than
            // waiting for the focus to be observed — a mouse that lags its
            // window by a rescan round-trip feels broken. The mouse follows
            // Ordo-initiated switches only; warping on external focus changes
            // (the user clicking a window!) would fight the pointer.
            fx.push(Effect::WarpMouse { to: center });
            (FocusIntent::Window(target), op)
        }

        Command::Demote { from, to } => {
            let root = s.root_of(from);
            s.focus_history.demote(root);
            let (view, proj) = view_for(s, to);
            let center = s.projected_frame_in(&s.windows[&to], &proj).center();
            if let Some(vm) = view {
                push_view(s, vm, now_ns, fx);
            }
            let op = push_focus(s, to, true, now_ns, fx, notes);
            fx.push(Effect::WarpMouse { to: center });
            (FocusIntent::Window(to), op)
        }

        Command::MoveToMonitor { window, target } => {
            let rec = s.windows[&window].clone();
            // Declaration first — assignment only, no frame. If the target is
            // hidden, view it in the same breath: declaring first means the
            // view's park/restore plan finds the window already a resident of
            // a monitor that stays visible, so it is neither parked nor
            // restored — it just stays put while the scenery changes. The
            // frame write below is the only move it ever gets.
            let before = s.projection();
            let mut op = s.mint_op();
            if s.virtual_monitors.is_some() {
                fx.push(Effect::AssignWindowToMonitor { op, window, target });
                s.pending.push(PendingOp {
                    op,
                    expect: Expectation::WindowOnMonitor {
                        window,
                        monitor: target,
                    },
                    issued_ns: now_ns,
                });
            }
            let proj = if before.is_hosted(target) {
                before.clone()
            } else {
                push_view(s, target, now_ns, fx);
                s.projection_with(Some(target), None)
            };
            let moved = WindowRecord {
                vmonitor: target,
                ..rec.clone()
            };
            let frame = s.projected_frame_in(&moved, &proj);
            if !frame.approx_eq(&rec.frame, FRAME_EPSILON) {
                op = s.mint_op();
                fx.push(Effect::SetWindowFrame { op, window, frame });
                s.pending.push(PendingOp {
                    op,
                    expect: Expectation::WindowFramed { window, frame },
                    issued_ns: now_ns,
                });
            }
            fx.push(Effect::WarpMouse { to: frame.center() });
            (FocusIntent::Window(window), op)
        }

        Command::View { target } => {
            // The monitor twin of a workspace switch: focus goes to the
            // target monitor's MRU window on the current workspace once the
            // view has moved (emitted after it, for the switch's reason).
            // A monitor with nothing on it gets its display's desktop instead:
            // an empty monitor is still a place to be.
            let ws = s.current_workspace().expect("resolved against a workspace");
            let proj = s.projection_with(Some(target), None);
            let head = s.focus_history.iter().find(|w| {
                s.windows
                    .get(w)
                    .is_some_and(|r| r.workspace == ws && r.vmonitor == target)
            });
            let op = push_view(s, target, now_ns, fx);
            if let Some(fw) = head {
                let unheld = s.focused != Some(fw) || s.declared_focus() != Some(fw);
                push_focus(s, fw, unheld, now_ns, fx, notes);
                fx.push(Effect::WarpMouse {
                    to: s.projected_frame_in(&s.windows[&fw], &proj).center(),
                });
            }
            let desktop = match head {
                Some(_) => None,
                None => s.host_in(target, &proj),
            };
            if let Some(display) = desktop {
                push_desktop(s, display, now_ns, fx, notes);
                if let Some(m) = s.monitors.get(&display) {
                    fx.push(Effect::WarpMouse {
                        to: m.frame.center(),
                    });
                }
            }
            let focus = match (head, desktop) {
                (Some(fw), _) => FocusIntent::Window(fw),
                (None, Some(_)) => FocusIntent::Desktop,
                (None, None) => FocusIntent::Deferred,
            };
            (focus, op)
        }

        Command::ToggleVirtualMonitors => {
            let v = s.virtual_monitors.expect("resolved against a virtual layer");
            let enabled = !v.enabled;
            let op = s.mint_op();
            fx.push(Effect::SetVirtualMonitors { op, enabled });
            s.pending.push(PendingOp {
                op,
                expect: Expectation::VirtualMonitorsEnabled(enabled),
                issued_ns: now_ns,
            });
            // Turning virtualization on must not hide the window the user is
            // in: the anchor moves to its monitor first.
            if enabled {
                if let Some(vm) = s.declared_focus().and_then(|w| s.windows.get(&w)).map(|r| r.vmonitor) {
                    if vm != v.viewed {
                        push_view(s, vm, now_ns, fx);
                    }
                }
            }
            (s.focus_intent(), op)
        }

        Command::Merge { from, into } => {
            let v = s.virtual_monitors.expect("resolved against a virtual layer");
            let op = s.mint_op();
            fx.push(Effect::MergeMonitors { op, from, into });
            s.pending.push(PendingOp {
                op,
                expect: Expectation::MonitorsMerged {
                    from,
                    into,
                    count: v.count - 1,
                },
                issued_ns: now_ns,
            });
            (s.focus_intent(), op)
        }

        // Nothing on screen changes: the new monitor is empty and the anchor
        // moves only within the viewport.
        Command::AddMonitor => {
            let v = s.virtual_monitors.expect("resolved against a virtual layer");
            let op = s.mint_op();
            fx.push(Effect::AddMonitor { op });
            s.pending.push(PendingOp {
                op,
                expect: Expectation::MonitorCount { count: v.count + 1 },
                issued_ns: now_ns,
            });
            (s.focus_intent(), op)
        }

        // Nothing on screen changes: only the numbers do.
        Command::MoveWorkspace { from, to } => {
            let current = s.current_workspace().map_or(from, |c| {
                WorkspaceId(after_move(c.0, from.0, to.0))
            });
            let op = s.mint_op();
            fx.push(Effect::MoveWorkspace { op, from, to });
            s.pending.push(PendingOp {
                op,
                expect: Expectation::WorkspacesMoved { current },
                issued_ns: now_ns,
            });
            (s.focus_intent(), op)
        }

        Command::MoveMonitor { from, to } => {
            let v = s.virtual_monitors.expect("resolved against a virtual layer");
            let moved = |m: VirtualMonitorId| VirtualMonitorId(after_move(m.0, from.0, to.0));
            let viewed = moved(v.viewed);
            let op = s.mint_op();
            fx.push(Effect::MoveMonitor { op, from, to });
            s.pending.push(PendingOp {
                op,
                expect: Expectation::MonitorsMoved { from, to, viewed },
                issued_ns: now_ns,
            });
            (s.focus_intent(), op)
        }
    }
}

/// Collapse a burst of queued hotkeys into what the user meant by the LAST of
/// them. Hotkeys only queue while the engine is busy carrying out an earlier
/// one; replaying the backlog literally re-fights switches the user has
/// already visually moved past (a logged stale press once fired a whole extra
/// switch 1.7s late). Runs of Prev/Next fold into one direct jump via a
/// clamp-simulated walk — NOT net arithmetic, which is wrong at the edges
/// (at the top workspace, Next-then-Prev must land one BELOW, not stay put).
/// Non-switch actions pass through in order and fence the folding; a
/// single-action batch passes through untouched, so the common unqueued press
/// keeps its exact logged shape.
///
/// Lives in the core because "what a run of commands is equivalent to" is
/// command semantics; the engine only decides when a batch exists.
pub fn coalesce_hotkeys(s: &State, actions: &[HotkeyAction]) -> Vec<HotkeyAction> {
    if actions.len() <= 1 {
        return actions.to_vec();
    }
    let Some(cur) = s.declared_workspace() else {
        return actions.to_vec();
    };
    let mut out = Vec::new();
    // `sim` walks the workspace the queued presses would have landed on;
    // `walk_start` is where the current fold began, so a net-zero bounce
    // (including bounces off a clamped edge) emits nothing at all.
    let mut sim = cur;
    let mut walk_start = cur;
    let flush = |sim: WorkspaceId, walk_start: &mut WorkspaceId, out: &mut Vec<HotkeyAction>| {
        if sim != *walk_start {
            out.push(HotkeyAction::WorkspaceSwitchTo(sim));
            *walk_start = sim;
        }
    };
    for &a in actions {
        match a {
            HotkeyAction::WorkspacePrev => sim = WorkspaceId(sim.0.max(2) - 1),
            HotkeyAction::WorkspaceNext => sim = WorkspaceId((sim.0 + 1).min(s.workspace_count)),
            HotkeyAction::WorkspaceSwitchTo(t) if t.0 >= 1 && t.0 <= s.workspace_count => sim = t,
            other => {
                flush(sim, &mut walk_start, &mut out);
                out.push(other);
            }
        }
    }
    flush(sim, &mut walk_start, &mut out);
    out
}

fn handle_snapshot(
    pre: &State,
    s: &mut State,
    now_ns: u64,
    trigger: &RescanTrigger,
    snap: &WorldSnapshot,
    fx: &mut Vec<Effect>,
    notes: &mut Vec<Note>,
) {
    // The user's focus context from BEFORE this observation: a new window that
    // steals focus must not get to define where it "should" be.
    let anchor_ws = pre.current_workspace();
    let anchor_vm = pre.focused_vmonitor();
    let entry_expectations: Vec<Expectation> =
        pre.pending.iter().map(|p| p.expect.clone()).collect();
    // A gesture explains only the observation that follows it.
    let navigation_gesture = std::mem::take(&mut s.navigation_gesture);

    let deltas = reconcile::diff(pre, snap);
    reconcile::apply_snapshot(s, snap);
    keep_vanished_places(pre, s, now_ns);

    // Whatever the app keys in the look that closed the focused window is
    // the close's fallout, even inside the second after the click that
    // closed it: where focus goes next is Ordo's to say.
    let hand_on = hand_on_from_close(pre, s, &deltas);
    if hand_on.is_none() && !s.own_menu && !s.key_unmanaged {
        record_landing(s, trigger, now_ns, notes);
    }
    // A display came or went. Every window on the vanished display was just
    // re-homed by macOS, not by anyone's hand — nothing about a window's
    // placement in this snapshot says anything about intent. The FIRST
    // observation is not a change: every display arrives as an addition then,
    // and reading that as a plug event re-hosted straddling windows on every
    // daemon start (run 56).
    let topology_changed = !pre.monitors.is_empty()
        && deltas
            .iter()
            .any(|d| matches!(d, Delta::MonitorAdded(_) | Delta::MonitorRemoved(_)));

    // Resolve or age expectations against the fresh belief.
    let mut expired: Vec<PendingOp> = Vec::new();
    let mut still_pending: Vec<PendingOp> = Vec::new();
    for p in std::mem::take(&mut s.pending) {
        if expectation_satisfied(&p.expect, s) {
            notes.push(Note::SelfConfirmed { op: p.op });
            op_ended(s, p.op);
            // The world accepted this placement: this axis's fight is over.
            // Reset only the matching axis so a confirmed workspace move doesn't
            // wipe the budget an in-progress frame fight has accrued.
            if let (Some(w), Some(axis)) = (p.expect.window(), p.expect.axis()) {
                if let Some(r) = s.windows.get_mut(&w) {
                    match axis {
                        CorrectionAxis::Workspace => r.ws_corrections = 0,
                        CorrectionAxis::Frame => r.frame_corrections = 0,
                    }
                }
            }
        } else if now_ns.saturating_sub(p.issued_ns) >= EXPECTATION_TTL_NS {
            notes.push(Note::OpLost { op: p.op });
            op_ended(s, p.op);
            expired.push(p);
        } else {
            still_pending.push(p);
        }
    }
    s.pending = still_pending;

    for d in &deltas {
        // Title churn is constant (terminals, browsers) and never actionable;
        // it lives in the snapshot itself if anyone needs it.
        if matches!(d, Delta::TitleChanged(_)) {
            continue;
        }
        if entry_expectations.iter().any(|e| reconcile::explains(e, d)) {
            continue;
        }
        notes.push(Note::External { delta: d.clone() });
    }

    if s.mode == Mode::Rescued {
        return;
    }

    // Birth is a command: a brand-new window has no prior intent, and if it
    // took focus it opened FOR the user, so it is what should be key. Not
    // gated on the creation hint, unlike corralling: apps whose observer never
    // attached (Slack, System Settings in run 51 — 8 focused births seen only
    // by the periodic scan, against 2 announced) would otherwise have every
    // new window's focus yanked back to the standing declaration. Startup is
    // excluded because nothing was born then; the OS owns focus at start.
    if !matches!(trigger, RescanTrigger::Startup) {
        for d in &deltas {
            if let Delta::WindowCreated(w) = d {
                if s.focused == Some(*w) {
                    s.declare_focus(FocusIntent::Window(*w));
                } else if s.windows.get(w).is_some_and(|r| r.parent.is_none()) {
                    // Its app may key it a beat late: `record_landing`
                    // declares it then.
                    let awaited = &mut s.unseen_landing.awaited;
                    awaited.retain(|a| a.by != Input::Birth);
                    awaited.push(Awaited {
                        roots: vec![*w],
                        since_ns: now_ns,
                        by: Input::Birth,
                    });
                }
            }
        }
    }

    let mut last_op: Option<OpId> = None;

    let handed_on = hand_on.is_some();
    if let Some(hand_on) = hand_on {
        hand_on_focus(s, hand_on, now_ns, notes, &mut last_op, fx);
    }

    // The user's hands outrank stale intent: an unexplained frame change on
    // a window in THIS snapshot means someone is actively placing it (a drag
    // in progress, most likely) — a retry would yank it out from under them
    // mid-gesture. The op stays lost; a re-press is cheaper than a fight.
    let hands_on = |w: WindowId| {
        deltas.iter().any(|d| {
            let same_window = match d {
                Delta::WindowFrameChanged { window, .. }
                | Delta::WindowMonitorChanged { window, .. } => *window == w,
                _ => false,
            };
            same_window && !entry_expectations.iter().any(|e| reconcile::explains(e, d))
        })
    };

    // A placement op that expired while the window still sits in violation
    // gets retried — apps often re-apply their own autosaved frame after we
    // move them — but only under the damping limit. (Focus needs no such
    // pass: its declaration is a standing field, checked every snapshot.)
    for p in expired {
        match p.expect {
            Expectation::WindowOn { window, workspace }
                if s.windows
                    .get(&window)
                    .is_some_and(|r| r.workspace != workspace) =>
            {
                correct_window(
                    s,
                    window,
                    CorrectionAxis::Workspace,
                    now_ns,
                    notes,
                    &mut last_op,
                    fx,
                    |op| {
                        (
                            Effect::MoveWindowToWorkspace {
                                op,
                                window,
                                target: workspace,
                            },
                            Expectation::WindowOn { window, workspace },
                        )
                    },
                );
            }
            Expectation::WindowFramed { window, frame }
                if !framed_satisfied(s, window, &frame) && !hands_on(window) =>
            {
                correct_window(
                    s,
                    window,
                    CorrectionAxis::Frame,
                    now_ns,
                    notes,
                    &mut last_op,
                    fx,
                    |op| {
                        (
                            Effect::SetWindowFrame { op, window, frame },
                            Expectation::WindowFramed { window, frame },
                        )
                    },
                );
            }
            _ => {}
        }
    }

    // New-window corralling: only the creation hint authorizes it (a plain
    // rescan can't tell "new" from "previously missed" — see RescanTrigger).
    if let RescanTrigger::AxHint {
        pid,
        kind: AxHintKind::WindowCreated,
    } = trigger
    {
        if let (Some(anchor_ws), Some(anchor_vm)) = (anchor_ws, anchor_vm) {
            for d in &deltas {
                let Delta::WindowCreated(w) = d else { continue };
                // Only corral the window that actually took focus. A full rescan
                // reports every previously-unmodeled window as "created", so
                // without this a same-app window that was merely missed (e.g. it
                // sat on another Space) would be dragged along with a genuinely
                // new one — AeroSpace's "windows randomly jump" bug. The newly
                // created window is the one that came to focus; that's the one
                // we place. (Non-focus-stealing new windows are left where they
                // open, by design.)
                if s.focused != Some(*w) {
                    continue;
                }
                let Some(rec) = s.windows.get(w).cloned() else {
                    continue;
                };
                if pid.is_some_and(|p| rec.app != p) {
                    continue;
                }
                if rec.workspace != anchor_ws {
                    let window = *w;
                    correct_window(
                        s,
                        window,
                        CorrectionAxis::Workspace,
                        now_ns,
                        notes,
                        &mut last_op,
                        fx,
                        |op| {
                            (
                                Effect::MoveWindowToWorkspace {
                                    op,
                                    window,
                                    target: anchor_ws,
                                },
                                Expectation::WindowOn {
                                    window,
                                    workspace: anchor_ws,
                                },
                            )
                        },
                    );
                }
                // The monitor half: declare it onto the anchor monitor, and
                // put its frame on that monitor's display. The declaration
                // is a word to our own backend — uncontested, so undamped.
                if rec.vmonitor != anchor_vm && s.virtual_monitors.is_some() {
                    let op = s.mint_op();
                    fx.push(Effect::AssignWindowToMonitor {
                        op,
                        window: *w,
                        target: anchor_vm,
                    });
                    s.pending.push(PendingOp {
                        op,
                        expect: Expectation::WindowOnMonitor {
                            window: *w,
                            monitor: anchor_vm,
                        },
                        issued_ns: now_ns,
                    });
                    last_op = Some(op);
                }
                if let Some(host) = s.host_of(anchor_vm) {
                    if rec.monitor != host {
                        let placed = WindowRecord {
                            vmonitor: anchor_vm,
                            ..rec.clone()
                        };
                        let frame = s.projected_frame(&placed);
                        let window = *w;
                        correct_window(
                            s,
                            window,
                            CorrectionAxis::Frame,
                            now_ns,
                            notes,
                            &mut last_op,
                            fx,
                            |op| {
                                (
                                    Effect::SetWindowFrame { op, window, frame },
                                    Expectation::WindowFramed { window, frame },
                                )
                            },
                        );
                    }
                }
            }
        }
    }

    project_windows(
        s,
        &deltas,
        &entry_expectations,
        topology_changed,
        now_ns,
        notes,
        &mut last_op,
        fx,
    );

    let followed = enforce_focus(s, navigation_gesture, now_ns, notes, &mut last_op, fx);

    // Tear re-alignment: the product invariant is that a workspace spans all
    // monitors, so an externally-swiped display gets pulled back to the
    // focused monitor's workspace. In-flight switches legitimately tear for a
    // snapshot or two — the pending guard keeps us from double-switching.
    if !s.is_torn() {
        s.tear_corrections = 0;
    } else if !s
        .pending
        .iter()
        .any(|p| matches!(p.expect, Expectation::AllMonitorsOn(_)))
    {
        if s.tear_corrections < DAMPING_LIMIT {
            // Only realign toward a workspace every display can actually reach.
            // With asymmetric Space counts a monitor can sit on a workspace the
            // others don't have; targeting it would be an unsatisfiable, futile
            // swipe-storm, so leave that tear alone rather than fight it.
            let reachable = s
                .current_workspace()
                .filter(|t| t.0 >= 1 && t.0 <= s.workspace_count);
            if let Some(target) = reachable {
                let op = push_switch(s, target, now_ns, fx, notes);
                notes.push(Note::TearDetected { target });
                s.tear_corrections += 1;
                last_op = Some(op);
            }
        } else if s.tear_corrections == DAMPING_LIMIT {
            notes.push(Note::TearPersisting);
            // Saturate so the note fires once per episode, not per snapshot.
            s.tear_corrections += 1;
        }
    }

    // A look restacks only for what it decided itself, never for what it
    // merely sees. A hand-on may still supersede a switch's restack in
    // flight; that is fine, as its own restack takes focus for the window
    // it declared.
    let settled = settled_moves(s, &deltas, &entry_expectations);
    if handed_on || followed || settled {
        restack_to_intent(s, &desired_stack(pre), followed || settled, fx);
    }

    if let Some(op) = last_op {
        fx.push(Effect::RequestRescan {
            reason: RescanTrigger::PostEffect { op },
        });
    }
}

/// How long a key press or click explains the first focus change after it.
/// Apps come forward within a few hundred ms of the input that moved them
/// (gesture to landing in runs 40-49: p75 390 ms); much longer, and a key
/// press would explain whatever an app keyed on its own seconds later.
const AWAY_TTL_NS: u64 = 1_000_000_000;

/// How long a click or a birth waits to see a window it named come up key.
/// Longer than `AWAY_TTL_NS` because these name their windows, so waiting
/// risks less, and an app with no observer is seen only by the periodic look,
/// two seconds apart. Bounded all the same: a click the user moved on from
/// must not explain the app re-keying that window on its own minutes later.
const AWAIT_TTL_NS: u64 = 3_000_000_000;

/// Write into the MRU order the focus the user's input explains, and nothing
/// else the screen shows: an app keying a window with no input behind it, or
/// a flicker between two looks, is not where the user went (run 49 seq 22:
/// a 27 ms flicker onto a Chrome window headed every later restack of its
/// workspace). A key press or click explains the first focus change after
/// it, within `AWAY_TTL_NS`; a click also explains a landing on a window it
/// hit, within `AWAIT_TTL_NS`, and a birth explains a landing on the window
/// born, declared as a birth with focus is. A hidden landing is never
/// recorded: it is either navigation, which the follow declares, or a fling.
/// The OS's focus at startup is the user's last choice before Ordo ran. Not
/// called for a look in which Ordo hands focus on from a closed window
/// (`hand_on_from_close`): what the app keys there is the close's fallout,
/// and the order takes Ordo's declaration instead.
fn record_landing(s: &mut State, trigger: &RescanTrigger, now_ns: u64, notes: &mut Vec<Note>) {
    let Landing { away, mut awaited } = std::mem::take(&mut s.unseen_landing);
    let focused = s.focused;
    let focused_root = focused.map(|f| s.root_of(f));
    let focused_app = key_app(s);
    let mut landed: Option<(WindowId, Option<Input>)> = None;
    if matches!(trigger, RescanTrigger::Startup) {
        landed = focused.map(|f| (f, None));
    } else {
        let lapsed = |since: u64, ttl: u64| now_ns.saturating_sub(since) >= ttl;
        s.unseen_landing.away = match away {
            Some(a) if lapsed(a.since_ns, AWAY_TTL_NS) => None,
            Some(a)
                if focused != a.from
                    && a.within.is_none_or(|app| focused_app == Some(app)) =>
            {
                landed = focused.map(|f| (f, Some(a.by)));
                None
            }
            other => other,
        };
        awaited.retain(|a| !lapsed(a.since_ns, AWAIT_TTL_NS));
        let hit = focused_root.and_then(|r| awaited.iter().position(|a| a.roots.contains(&r)));
        if let Some(i) = hit {
            landed = focused.map(|f| (f, Some(awaited.remove(i).by)));
        }
        s.unseen_landing.awaited = awaited;
    }
    let Some((f, by)) = landed.filter(|(f, _)| s.windows.get(f).is_some_and(|r| s.is_visible(r)))
    else {
        return;
    };
    match s.focus_intent() {
        _ if by == Some(Input::Birth) => s.declare_focus(FocusIntent::Window(f)),
        FocusIntent::Deferred => {
            let root = s.root_of(f);
            s.focus_history.touch(root);
        }
        // Every gesture but a key press hands the slot to the OS, so a
        // declaration still standing means focus moved from the keyboard:
        // Spotlight, an app's window shortcut. It is the user's, and
        // enforcement must not take it back. Unless a grant of Ordo's own is
        // still on its way: then the change is that grant's doing, or its
        // fallout (a sibling keyed in its place).
        _ if s.pending.iter().any(|p| is_focus_grant(&p.expect)) => return,
        _ => s.declare_focus(FocusIntent::Window(f)),
    }
    if let Some(by) = by {
        notes.push(Note::LandingExplained { window: f, by });
    }
}

/// Where focus goes when the window holding it closes: the next window in
/// the MRU order on its monitor, or that monitor's desktop. A desktop on a
/// monitor that is not the anchor is viewed first, so the anchor moves to
/// it, and with it where new windows are corralled: the user is at that
/// monitor now.
enum HandOn {
    Window {
        closed: WindowId,
        next: WindowId,
    },
    Desktop {
        closed: WindowId,
        view: Option<VirtualMonitorId>,
        display: MonitorId,
    },
}

/// The focused window (or the declared one) closed in this look, and focus
/// stays on its monitor. Left alone, macOS fronts the app's next window
/// wherever it is, often on the other monitor (run 50 seq 159-161), and an
/// app's own pick is not where the user was working. Who closed it does not
/// matter: a dialog dismissing itself leaves the user where a close button
/// would. It relies on `WindowDestroyed` meaning the window server agrees
/// the window is gone (see `WorldSnapshot::unread`): a window one scan
/// missed would otherwise hand focus away while the user types into it.
fn hand_on_from_close(pre: &State, s: &State, deltas: &[Delta]) -> Option<HandOn> {
    if s.mode != Mode::Active {
        return None;
    }
    let closed = pre.declared_focus()?;
    let rec = pre.windows.get(&closed)?;
    // An attached window (a popup, a find bar) gives focus back to its root
    // by itself.
    if !deltas.contains(&Delta::WindowDestroyed(closed))
        || !pre.is_visible(rec)
        || rec.parent.is_some()
    {
        return None;
    }
    // A look that lost the windows of several apps at once is the screen
    // going away (the lock screen, a native Space, a display reconfiguring),
    // and they all come back a look or a few later. A close, or an app
    // quitting, takes one app's.
    let gone_apps: BTreeSet<Pid> = deltas
        .iter()
        .filter_map(|d| match d {
            Delta::WindowDestroyed(w) => pre.windows.get(w).map(|r| r.app),
            _ => None,
        })
        .collect();
    if gone_apps.len() > 1 {
        return None;
    }
    // A window born with focus declares itself.
    if deltas
        .iter()
        .any(|d| matches!(d, Delta::WindowCreated(w) if s.focused == Some(*w)))
    {
        return None;
    }
    if went_elsewhere(pre, s, rec) {
        return None;
    }
    let ws = s.current_workspace().filter(|ws| *ws == rec.workspace)?;
    if let Some(next) = mru_stack(s, ws)
        .into_iter()
        .find(|w| s.windows[w].vmonitor == rec.vmonitor)
    {
        return Some(HandOn::Window { closed, next });
    }
    // The desktop declaration is held on the anchor's display, so another
    // monitor's desktop needs the anchor moved there, which is only a focus
    // jump while it leaves every display showing what it shows.
    let (view, display) = match s.virtual_monitors {
        None => (None, rec.monitor),
        Some(v) if v.viewed == rec.vmonitor => (None, s.host_of(rec.vmonitor)?),
        Some(_) if s.projection_with(Some(rec.vmonitor), None) == s.projection() => {
            (Some(rec.vmonitor), s.host_of(rec.vmonitor)?)
        }
        Some(_) => return None,
    };
    Some(HandOn::Desktop {
        closed,
        view,
        display,
    })
}

/// The user's latest input, in the same look as the close, can't be what
/// closed the window, so it took them somewhere: Cmd+Tab, the Dock, a click
/// that missed the closed window (the desktop, another window; the click that
/// closed it hit it), or a key press that brought up another app (a
/// launcher). A key press within the closed window's app is Cmd+W, and a
/// menu's click may be its Close item. A key press or Dock click that leaves
/// the closed window's app with no windows is Cmd+Q or the Dock menu's Quit,
/// after which macOS keys whichever app it likes, often on the other monitor
/// or a hidden workspace: that other app is the quit's fallout, not where the
/// user went.
fn went_elsewhere(pre: &State, s: &State, closed: &WindowRecord) -> bool {
    let Landing { away, awaited } = &pre.unseen_landing;
    let root = pre.root_of(closed.id);
    let quit = !s.windows.values().any(|r| r.app == closed.app);
    match away.as_ref().map(|a| a.by) {
        Some(Input::Switcher) => true,
        Some(Input::Dock) => !quit,
        Some(Input::Click) => !awaited
            .iter()
            .any(|a| a.by == Input::Click && a.roots.contains(&root)),
        Some(Input::Key) => !quit && key_app(s).is_some_and(|app| app != closed.app),
        Some(Input::MenuBar | Input::Birth) | None => false,
    }
}

/// Grant what `hand_on_from_close` chose, as a command would: the
/// declaration it leaves is enforced against the app's own re-key, damped as
/// any other.
fn hand_on_focus(
    s: &mut State,
    hand_on: HandOn,
    now_ns: u64,
    notes: &mut Vec<Note>,
    last_op: &mut Option<OpId>,
    fx: &mut Vec<Effect>,
) {
    match hand_on {
        HandOn::Window { closed, next } => {
            let op = push_focus(s, next, s.focused != Some(next), now_ns, fx, notes);
            s.declare_focus(FocusIntent::Window(next));
            notes.push(Note::FocusHandedOn {
                closed,
                to: Some(next),
            });
            *last_op = Some(op);
        }
        HandOn::Desktop {
            closed,
            view,
            display,
        } => {
            if let Some(vm) = view {
                push_view(s, vm, now_ns, fx);
            }
            let op = push_desktop(s, display, now_ns, fx, notes);
            s.declare_focus(FocusIntent::Desktop);
            notes.push(Note::FocusHandedOn { closed, to: None });
            *last_op = Some(op);
        }
    }
}

/// How long a window missing from the scans keeps its place in the focus
/// history.
const VANISH_GRACE_NS: u64 = 10_000_000_000;

/// A window missing from one scan is usually still there: an app's AX read
/// fails now and then (a Ghostty window dropped out of single scans three
/// times in run 45). Forgetting its place would put it back at the end of the
/// history when it reappears, and the next restack would push it under
/// windows used less recently than it. So the place is kept, and only a
/// window gone for `VANISH_GRACE_NS` leaves the history. Every reader of the
/// history already skips windows that aren't in the model.
fn keep_vanished_places(pre: &State, s: &mut State, now_ns: u64) {
    for w in pre.windows.keys() {
        if !s.windows.contains_key(w) {
            s.vanished.entry(*w).or_insert(now_ns);
        }
    }
    let mut vanished = std::mem::take(&mut s.vanished);
    vanished.retain(|w, since| {
        if s.windows.contains_key(w) {
            false
        } else if now_ns.saturating_sub(*since) >= VANISH_GRACE_NS {
            s.focus_history.remove(*w);
            false
        } else {
            true
        }
    });
    s.vanished = vanished;
}

/// A window moved by a hand that wasn't Ordo's may now overlap others in any
/// order: as good as a reveal, for the stack. A user's drag clicks the window
/// on top first, so this matters for moves nobody clicked. It counts once the
/// window holds still, so a drag isn't restacked every look, and only with
/// no op pending, so it can't supersede a switch's restack and lose its
/// focus take-back.
fn settled_moves(s: &mut State, deltas: &[Delta], entry_expectations: &[Expectation]) -> bool {
    let visible = |s: &State, w: &WindowId| s.windows.get(w).is_some_and(|r| s.is_visible(r));
    let moved: BTreeSet<WindowId> = deltas
        .iter()
        .filter_map(|d| match d {
            Delta::WindowFrameChanged { window, .. }
                if !entry_expectations.iter().any(|e| reconcile::explains(e, d)) =>
            {
                Some(*window)
            }
            _ => None,
        })
        .filter(|w| visible(s, w))
        // An attached window's move changes its root's footprint.
        .map(|w| s.root_of(w))
        .collect();
    let settled: BTreeSet<WindowId> = s
        .moving
        .iter()
        .filter(|w| !moved.contains(w) && visible(s, w))
        .copied()
        .collect();
    s.moving = moved;
    if settled.is_empty() {
        return false;
    }
    if !s.pending.is_empty() {
        s.moving.extend(settled);
        return false;
    }
    true
}

/// How long a window must stand on another display before its monitor
/// declaration follows it. macOS re-homes a vanished display's windows before
/// it reports the display gone — in run 56 the re-homed Slack window and the
/// still-two-display list arrived in one snapshot, the removal in the next —
/// so the instant a window changes displays it is either a drag or the first
/// sign of an unplug. The display change, when it is one, arrives within the
/// settle window (`SETTLE` in the shell, one second); waiting that long costs
/// a drag one rescan of latency and costs an unplug nothing.
const ADOPTION_DELAY_NS: u64 = 1_000_000_000;

/// The projection, asserted: every visible window sits on the display its
/// virtual monitor is hosted by. A window found on some other display gets
/// one of two resolutions, and which one depends on nothing but whether the
/// display set changed in this snapshot:
///
/// - It did not: the window is on a LIVE display that stands for some other
///   monitor, and the only way it got there is a hand that was not Ordo's —
///   a drag, a resize carrying its center across the seam, an app placing
///   itself. Placement within a workspace is observational, so the monitor
///   declaration follows: the window is adopted onto the monitor that display
///   stands for, once it has stood there for `ADOPTION_DELAY_NS`. Never a
///   frame write: the first build corrected these, and a resize across the
///   seam became a fight against the user's own hands (run 56, 2026-09-04),
///   the exact fight this project exists to avoid.
/// - It did: macOS just re-homed every window of a vanished display, and a
///   returned display is hosting a monitor whose windows still sit where the
///   laptop had them. None of that is anyone's placement; the frame is
///   re-hosted onto the display its monitor is projected onto, damped on the
///   frame axis and yielding to a hand still on the window. Every pending
///   adoption is dropped: what looked like a drag was the unplug beginning.
///
/// Adoption is a word to Ordo's own backend and needs no damping; both
/// resolutions yield to an op already pending on the window.
#[allow(clippy::too_many_arguments)]
fn project_windows(
    s: &mut State,
    deltas: &[Delta],
    entry_expectations: &[Expectation],
    topology_changed: bool,
    now_ns: u64,
    notes: &mut Vec<Note>,
    last_op: &mut Option<OpId>,
    fx: &mut Vec<Effect>,
) {
    if s.virtual_monitors.is_none() {
        s.misplaced_since.clear();
        return;
    }
    let unexplained_move = |w: WindowId| {
        deltas.iter().any(|d| {
            let same = match d {
                Delta::WindowFrameChanged { window, .. }
                | Delta::WindowMonitorChanged { window, .. } => *window == w,
                _ => false,
            };
            same && !entry_expectations.iter().any(|e| reconcile::explains(e, d))
        })
    };
    let busy = |s: &State, w: WindowId| s.pending.iter().any(|p| p.expect.window() == Some(w));

    let misplaced: Vec<WindowRecord> = s
        .windows
        .values()
        .filter(|r| s.is_visible(r) && s.host_of(r.vmonitor).is_some_and(|h| h != r.monitor))
        .cloned()
        .collect();
    let still: std::collections::BTreeSet<WindowId> = misplaced.iter().map(|r| r.id).collect();
    s.misplaced_since.retain(|w, _| still.contains(w));
    if topology_changed {
        s.misplaced_since.clear();
    }

    for rec in misplaced {
        let window = rec.id;
        if busy(s, window) {
            continue;
        }
        if topology_changed {
            if unexplained_move(window) {
                continue;
            }
            let frame = s.projected_frame(&rec);
            correct_window(
                s,
                window,
                CorrectionAxis::Frame,
                now_ns,
                notes,
                last_op,
                fx,
                |op| {
                    (
                        Effect::SetWindowFrame { op, window, frame },
                        Expectation::WindowFramed { window, frame },
                    )
                },
            );
            continue;
        }
        let since = *s.misplaced_since.entry(window).or_insert(now_ns);
        if now_ns.saturating_sub(since) < ADOPTION_DELAY_NS {
            continue;
        }
        let Some(monitor) = s.canonical_vm_of(rec.monitor) else {
            continue;
        };
        if monitor == rec.vmonitor {
            continue;
        }
        let op = s.mint_op();
        fx.push(Effect::AssignWindowToMonitor {
            op,
            window,
            target: monitor,
        });
        s.pending.push(PendingOp {
            op,
            expect: Expectation::WindowOnMonitor { window, monitor },
            issued_ns: now_ns,
        });
        notes.push(Note::MonitorAdopted { window, monitor });
        *last_op = Some(op);
    }
}

/// Focus, after the snapshot has been absorbed. Two separate concerns:
///
/// The INVARIANT — the key window must be on the visible workspace — holds no
/// matter who owns the slot. Observed focus on a hidden workspace's window is
/// either the user going there (a witnessed gesture explains it: follow, as
/// native Spaces would) or unusable state (nobody can type into an invisible
/// window: declare the visible MRU window and pull focus back). This is the
/// whole of what the old close-fallout and settle-window guards were groping
/// toward, without inferring anything from timing.
///
/// The DECLARATION — `FocusIntent::Window(w)` — is enforced like a parked
/// frame: a contradicting observation is re-asserted while a grant is not
/// already in flight, under `DAMPING_LIMIT`, then stood down from loudly and
/// once (retiring the declaration — see below). Under `Deferred` there is
/// nothing to enforce. Because the default is "the declaration stands", a
/// fling from a cause nobody has catalogued yet costs no new rule here.
///
/// The stand-down concedes the slot to the APP that kept it, and the
/// invariant honours that concession for as long as that app holds focus —
/// through whichever of its windows, since AppKit key-window ownership is
/// per-application. Without this the two rules feed each other: retiring
/// resets the budget, the invariant re-declares against the very same
/// evidence, and the app that just won is fought (and its parked window
/// raised) again every few seconds, indefinitely — the focus twin of the
/// write loop `pending_repark` exists to prevent.
fn enforce_focus(
    s: &mut State,
    navigation_gesture: bool,
    now_ns: u64,
    notes: &mut Vec<Note>,
    last_op: &mut Option<OpId>,
    fx: &mut Vec<Effect>,
) -> bool {
    // Spent only by another app taking the slot. A vacuum says nothing about
    // whether the conceding app relented, and the invariant cannot fire on
    // one anyway; clearing there would only re-arm the loop for the app's
    // next hidden hop.
    if s.conceded
        .is_some_and(|app| key_app(s).is_some_and(|holder| holder != app))
    {
        s.conceded = None;
    }
    let Some(here) = s.current_workspace() else {
        return false;
    };
    // See `State::own_menu` and `State::key_unmanaged`.
    if s.own_menu || s.key_unmanaged {
        return false;
    }
    // A window born without focus is awaited (see `Landing::awaited`), and
    // what its app keys meanwhile, a hidden sibling of it or nothing, is the
    // opening, not to be fought or followed. Nothing keyed is a vacuum
    // whichever app holds it, so it is left alone too. A newborn out of
    // sight was not asked for here, and a visible sibling keyed is ordinary
    // focus, which the rules below already judge.
    let keyed = s.focused.and_then(|f| s.windows.get(&f));
    let opening = s
        .unseen_landing
        .awaited
        .iter()
        .filter(|a| a.by == Input::Birth)
        .flat_map(|a| &a.roots)
        .filter_map(|w| s.windows.get(w))
        .any(|born| {
            s.is_visible(born) && keyed.is_none_or(|r| r.app == born.app && !s.is_visible(r))
        });
    if opening {
        return false;
    }

    let landed_hidden = s
        .focused
        .and_then(|w| s.windows.get(&w))
        .filter(|r| !s.is_visible(r) && r.workspace.0 >= 1 && r.workspace.0 <= s.workspace_count)
        .cloned();
    if let (Some(rec), None) = (&landed_hidden, s.focus_target()) {
        // Neither the user going there nor a fling: see `State::menu_open`.
        if s.menu_open {
            return false;
        }
        if navigation_gesture {
            // Follow on whichever axes hide the window: the workspace, the
            // monitor, or both.
            let target = rec.workspace;
            let mut op = None;
            if target != here {
                op = Some(push_switch(s, target, now_ns, fx, notes));
            }
            let mut monitor = None;
            if !s.projection().is_hosted(rec.vmonitor) {
                op = Some(push_view(s, rec.vmonitor, now_ns, fx));
                monitor = Some(rec.vmonitor);
            }
            s.declare_focus(FocusIntent::Window(rec.id));
            notes.push(Note::FollowedFocus {
                window: rec.id,
                target,
                monitor,
            });
            *last_op = op;
            return true;
        }
        // An empty visible workspace has nothing to hold focus: leave it, the
        // next birth here declares itself. Under a desktop declaration the
        // desktop is what holds it, enforced below.
        if s.focus_intent() != FocusIntent::Desktop && s.conceded != Some(rec.app) {
            if let Some(head) = mru_stack(s, here).first().copied() {
                s.declare_focus(FocusIntent::Window(head));
                notes.push(Note::HeldFocus {
                    window: head,
                    from: rec.id,
                    from_app: rec.app,
                });
            }
        }
    }

    if s.focus_intent() == FocusIntent::Desktop {
        enforce_desktop(s, now_ns, notes, last_op, fx);
        return false;
    }
    let Some(w) = s.focus_target() else {
        return false;
    };
    if s.focused == Some(w) {
        s.focus_corrections = 0;
        return false;
    }
    // A declaration for a window that is not on screen is unenforceable (its
    // switch or view has not landed, or never will); granting it would put
    // the keyboard into an invisible window.
    if !s.is_visible(&s.windows[&w]) {
        return false;
    }
    // The grant is in flight; apps land it on their own schedule.
    if s.pending
        .iter()
        .any(|p| p.expect == Expectation::Focused(w))
    {
        return false;
    }
    if s.focus_corrections >= DAMPING_LIMIT {
        // The app has won the slot. Unlike a parked frame — where the
        // declaration is the user's filing and stays through a standoff —
        // a focus declaration is a claim about NOW, and a lost one only
        // misdirects the next carry or MRU chord toward a window the user is
        // visibly not in (run 51: Chrome kept 44267 key against a grant to
        // 41105 for 166 snapshots; every same-monitor Alt+Shift+Tab meanwhile
        // resolved against 41105). Hand the slot to the OS; the invariant
        // still holds, and the next command declares afresh.
        notes.push(Note::FocusDiverged {
            window: w,
            winner: s.focused,
            winner_app: key_app(s),
        });
        s.declare_focus(FocusIntent::Deferred);
        // After the declaration, which clears it.
        s.conceded = key_app(s);
        return false;
    }
    s.focus_corrections += 1;
    let op = push_focus(s, w, true, now_ns, fx, notes);
    notes.push(Note::FocusReasserted { window: w });
    *last_op = Some(op);
    false
}

/// The desktop twin of window enforcement: a window taking focus from the
/// empty monitor the user is on gets the desktop re-granted, damped and
/// conceded exactly as a window declaration is. A user's own click or switch
/// never reaches here — its gesture declared `Deferred` first.
fn enforce_desktop(
    s: &mut State,
    now_ns: u64,
    notes: &mut Vec<Note>,
    last_op: &mut Option<OpId>,
    fx: &mut Vec<Effect>,
) {
    let Some(from) = s.focused else {
        s.focus_corrections = 0;
        return;
    };
    let Some(display) = s.virtual_monitors.and_then(|v| s.host_of(v.viewed)) else {
        return;
    };
    if s.pending
        .iter()
        .any(|p| p.expect == Expectation::DesktopFocused)
    {
        return;
    }
    if s.focus_corrections >= DAMPING_LIMIT {
        notes.push(Note::DesktopDiverged {
            winner: s.focused,
            winner_app: key_app(s),
        });
        s.declare_focus(FocusIntent::Deferred);
        s.conceded = key_app(s);
        return;
    }
    s.focus_corrections += 1;
    let op = push_desktop(s, display, now_ns, fx, notes);
    notes.push(Note::DesktopReasserted { display, from });
    *last_op = Some(op);
}

/// The app holding the key window. Reconcile filters `focused` to windows in
/// the model, so `None` here is a focus vacuum, never an unknown holder.
fn key_app(s: &State) -> Option<Pid> {
    s.focused.and_then(|w| s.windows.get(&w)).map(|r| r.app)
}

/// Emit a placement corrective for `window` on `axis` unless that axis has hit
/// the damping limit, in which case note the divergence and stand down. Damping
/// is per axis so a window wrong on both workspace and frame gets a full retry
/// budget for each.
#[allow(clippy::too_many_arguments)]
fn correct_window(
    s: &mut State,
    window: WindowId,
    axis: CorrectionAxis,
    now_ns: u64,
    notes: &mut Vec<Note>,
    last_op: &mut Option<OpId>,
    fx: &mut Vec<Effect>,
    build: impl FnOnce(OpId) -> (Effect, Expectation),
) {
    let corrections = s.windows.get(&window).map_or(0, |r| match axis {
        CorrectionAxis::Workspace => r.ws_corrections,
        CorrectionAxis::Frame => r.frame_corrections,
    });
    if corrections >= DAMPING_LIMIT {
        notes.push(Note::Diverged { window });
        return;
    }
    let op = s.mint_op();
    let (effect, expect) = build(op);
    fx.push(effect);
    self::expect(s, op, expect, now_ns, notes);
    if let Some(r) = s.windows.get_mut(&window) {
        match axis {
            CorrectionAxis::Workspace => r.ws_corrections += 1,
            CorrectionAxis::Frame => r.frame_corrections += 1,
        }
    }
    *last_op = Some(op);
}

fn handle_effect_result(s: &mut State, op: OpId, outcome: &OpOutcome, notes: &mut Vec<Note>) {
    let detail = match outcome {
        OpOutcome::Ok => return, // success is confirmed by observation, not by the executor
        OpOutcome::Failed { detail } => detail.clone(),
        OpOutcome::Timeout => "timeout".to_string(),
    };
    if let Some(i) = s.pending.iter().position(|p| p.op == op) {
        s.pending.remove(i);
    }
    op_ended(s, op);
    notes.push(Note::OpFailed { op, detail });
}

fn expectation_satisfied(e: &Expectation, s: &State) -> bool {
    match e {
        Expectation::AllMonitorsOn(t) => {
            !s.monitor_ws.is_empty() && s.monitor_ws.values().all(|w| w == t)
        }
        Expectation::WindowOn { window, workspace } => s
            .windows
            .get(window)
            .is_some_and(|r| r.workspace == *workspace),
        Expectation::WindowFramed { window, frame } => framed_satisfied(s, *window, frame),
        Expectation::Focused(w) => s.focused == Some(*w),
        Expectation::DesktopFocused => s.focused.is_none(),
        Expectation::WindowOnMonitor { window, monitor } => s
            .windows
            .get(window)
            .is_some_and(|r| r.vmonitor == *monitor),
        Expectation::Viewing(vm) => s.virtual_monitors.is_some_and(|v| v.viewed == *vm),
        Expectation::VirtualMonitorsEnabled(e) => {
            s.virtual_monitors.is_some_and(|v| v.enabled == *e)
        }
        Expectation::MonitorCount { count } | Expectation::MonitorsMerged { count, .. } => {
            s.virtual_monitors.is_some_and(|v| v.count == *count)
        }
        Expectation::WorkspacesMoved { current } => {
            !s.monitor_ws.is_empty() && s.monitor_ws.values().all(|w| w == current)
        }
        Expectation::MonitorsMoved { viewed, .. } => {
            s.virtual_monitors.is_some_and(|v| v.viewed == *viewed)
        }
    }
}

/// A frame op's observable post-condition is ARRIVAL ON THE INTENDED
/// MONITOR, not the exact rect: macOS clamps frames into a display's
/// visible area (a requested y at the top of the second monitor's bounds
/// lands a menu-bar-height lower), so demanding the pixels made the
/// expectation unsatisfiable and turned every cross-monitor move into a
/// doomed retry fight — lived as "the rescan keeps yanking the window".
/// The exact rect matters only when the target frame is on no known
/// monitor and there is nothing better to check against.
fn framed_satisfied(s: &State, window: WindowId, frame: &Rect) -> bool {
    let Some(r) = s.windows.get(&window) else {
        return false;
    };
    let c = frame.center();
    match s.monitors.values().find(|m| m.frame.contains(c)) {
        Some(m) => r.monitor == m.id,
        None => r.frame.approx_eq(frame, FRAME_EPSILON),
    }
}
