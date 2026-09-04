use serde::{Deserialize, Serialize};

use crate::effect::{CorrectionAxis, Effect, Expectation};
use crate::event::{
    AxHintKind, Event, Gesture, HotkeyAction, OpOutcome, RescanTrigger, WorldSnapshot,
};
use crate::ids::{OpId, Pid, Rect, VirtualMonitorId, WindowId, WorkspaceId, FRAME_EPSILON};
use crate::project::Projection;
use crate::reconcile::{self, Delta};
use crate::state::{FocusIntent, Mode, PendingOp, State, WindowRecord};

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
    /// A change we didn't cause. Belief absorbed it.
    External { delta: Delta },
    /// The verdict `handle_gesture` reached on a witnessed gesture: whether
    /// it armed a follow and, when a mouse-down did not, which visible window
    /// swallowed the point. This exists because an UNARMED follow is otherwise
    /// invisible — the gesture event is logged, the hidden landing is held,
    /// and nothing in between says why (the same absence-of-evidence hole
    /// `park_trace` closed for parking). It also proves whether `SystemSwitch`
    /// reaches the core at all, and shows a Dock click landing inside the
    /// window beneath an auto-hidden Dock.
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
    /// A visible window was moved between two live displays by a hand that
    /// was not Ordo's — a drag, or an app placing itself — and its monitor
    /// declaration followed. Placement within a workspace is observational;
    /// this is the one observation that writes a monitor declaration, and it
    /// is a delta (the move itself), never a standing position.
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
            if s.mode == Mode::Active {
                handle_hotkey(&mut s, *action, at.mono_ns, &mut effects);
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
        Event::Gesture { gesture, .. } => handle_gesture(&mut s, *gesture, &mut notes),
    }

    Step {
        state: s,
        effects,
        notes,
    }
}

/// The workspace's visual stacking, front-to-back, as the MRU history implies
/// it — restricted to the windows the projection puts on screen, since a
/// raise is a focus and a parked window must not be handed either. Emitted
/// alongside anything that reveals a workspace: parking and app-hiding
/// scramble real z-order, and MRU is the single source of truth for what "on
/// top" means here.
fn mru_stack(s: &State, ws: WorkspaceId) -> Vec<WindowId> {
    visible_stack(s, ws, &s.projection())
}

/// `mru_stack` under a projection that is not (yet) the one in force — what a
/// view change is about to show.
fn visible_stack(s: &State, ws: WorkspaceId, proj: &Projection) -> Vec<WindowId> {
    s.focus_history
        .iter()
        .filter(|w| {
            s.windows
                .get(w)
                .is_some_and(|r| r.workspace == ws && proj.is_hosted(r.vmonitor))
        })
        .collect()
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

/// The monitor a focus target needs viewed, if it sits on a hidden one —
/// nobody can type into a parked window — with the projection the world will
/// be under once that view lands, for restacking and warping against. The
/// caller emits the focus grant BEFORE the view, as a switch hands focus
/// before it parks: the window being revealed should already be key when its
/// app is un-hidden, and the app being hidden must not still hold focus.
fn view_for(s: &State, target: WindowId) -> (Option<VirtualMonitorId>, Projection) {
    let vm = s.windows[&target].vmonitor;
    let proj = s.projection();
    if proj.is_hosted(vm) || s.virtual_monitors.is_none() {
        return (None, proj);
    }
    (Some(vm), s.projection_with(Some(vm), None))
}

/// The user reached for focus through the OS rather than through Ordo. Whatever
/// Ordo declared is moot — the OS owns the slot until the next command — and
/// the next observation gets to read a hidden-workspace landing as the user
/// going there, but only if the gesture could have been aimed there: a click
/// INTO a window on the visible workspace keys that window (or a sheet or
/// child Ordo does not model), so a hidden landing after it is a fling.
fn handle_gesture(s: &mut State, gesture: Gesture, notes: &mut Vec<Note>) {
    s.declare_focus(FocusIntent::Deferred);
    let within = match gesture {
        Gesture::SystemSwitch => None,
        Gesture::MouseDown { at } => {
            let here = s.current_workspace();
            s.windows
                .values()
                .find(|r| Some(r.workspace) == here && r.frame.contains(at))
                .map(|r| r.id)
        }
    };
    // After the declaration, which clears it.
    s.navigation_gesture = within.is_none();
    notes.push(Note::GestureClassified {
        gesture,
        armed: s.navigation_gesture,
        within,
    });
}

/// What a hotkey resolved to against the current belief, before anything is
/// minted or emitted. Resolution (which window, which workspace, or nothing
/// at all) is separated from execution so that `execute` can be total: every
/// command arm must produce the focus declaration it leaves behind, and a new
/// hotkey cannot be added without deciding it — the match on `HotkeyAction`
/// in `resolve` and the match on `Command` in `execute` are both exhaustive,
/// and `execute` returns a bare `FocusIntent`, not an `Option`.
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
        workspace: WorkspaceId,
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
}

fn handle_hotkey(s: &mut State, action: HotkeyAction, now_ns: u64, fx: &mut Vec<Effect>) {
    // A hotkey that resolves to nothing (clamped at an edge, nothing focused)
    // touched neither the world nor the declaration.
    let Some(cmd) = resolve(s, action) else {
        return;
    };
    let focus = execute(s, cmd, now_ns, fx);
    s.declare_focus(focus);
}

fn resolve(s: &State, action: HotkeyAction) -> Option<Command> {
    match action {
        HotkeyAction::WorkspacePrev
        | HotkeyAction::WorkspaceNext
        | HotkeyAction::WorkspaceSwitchTo(_) => {
            let cur = s.current_workspace()?;
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
            let cur = s.current_workspace()?;
            // You carry what's with you: dragging a window over from a hidden
            // workspace would materialize it from nowhere. Read against the
            // DECLARATION, so a fling onto a parked sibling between two chords
            // cannot make the second one do nothing (run 51 seq 22484).
            if s.windows.get(&window)?.workspace != cur {
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
            Some(Command::Demote {
                workspace,
                from,
                to,
            })
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
    }
}

/// Carry out a resolved command and return the focus declaration it leaves
/// behind. Total over `Command` by construction — see the type's doc.
fn execute(s: &mut State, cmd: Command, now_ns: u64, fx: &mut Vec<Effect>) -> FocusIntent {
    match cmd {
        Command::Switch { target } => {
            // Hand focus to the destination's MRU window BEFORE switching —
            // otherwise the keyboard keeps typing into a window that just got
            // parked off-screen, and the backend's app dimming (which spares
            // the focused app) could never dim the workspace being left. No
            // mouse warp: the core's frame belief for that window is its
            // parked sliver position, so a warp would aim at the corner.
            //
            // The focus target IS the head of the restack order — one list
            // feeds both. The restack's physics require its designated top to
            // be the key window, and this must hold even when the focused
            // window never left (a round trip through an empty workspace):
            // alt-tab's skip-the-focused selection here handed focus to the
            // SECOND MRU window on each return while the restack still named
            // the first, which made the ordering unsatisfiable and flipped
            // the top window every round trip.
            //
            // Monitor selection is global, so a switch leaves the anchor alone
            // — except when that would land the user on a blank screen: a
            // destination whose windows all sit on hidden monitors. Then the
            // view follows the focus the switch hands out, as it does
            // everywhere else, to the destination's overall MRU head.
            let mut stack = mru_stack(s, target);
            let mut view: Option<VirtualMonitorId> = None;
            if stack.is_empty() && s.virtual_monitors.is_some() {
                let anywhere = s
                    .focus_history
                    .iter()
                    .find(|w| s.windows.get(w).is_some_and(|r| r.workspace == target));
                if let Some(w) = anywhere {
                    let vm = s.windows[&w].vmonitor;
                    view = Some(vm);
                    stack = visible_stack(s, target, &s.projection_with(Some(vm), None));
                }
            }
            let head = stack.first().copied();
            if let Some(fw) = head {
                let op = s.mint_op();
                fx.push(Effect::FocusWindow { op, window: fw });
                // Re-asserting focus the belief already holds produces no
                // delta, so there is nothing to attribute to an expectation.
                if s.focused != Some(fw) {
                    s.pending.push(PendingOp {
                        op,
                        expect: Expectation::Focused(fw),
                        issued_ns: now_ns,
                    });
                }
            }
            if let Some(vm) = view {
                push_view(s, vm, now_ns, fx);
            }
            let op = s.mint_op();
            fx.push(Effect::SwitchWorkspace { op, target });
            s.pending.push(PendingOp {
                op,
                expect: Expectation::AllMonitorsOn(target),
                issued_ns: now_ns,
            });
            if stack.len() >= 2 {
                fx.push(Effect::RestackWindows { order: stack });
            }
            fx.push(Effect::RequestRescan {
                reason: RescanTrigger::PostEffect { op },
            });
            // An empty destination has nothing that could hold focus; whatever
            // the OS keeps key is its business until something is born here.
            head.map_or(FocusIntent::Deferred, FocusIntent::Window)
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
            s.pending.push(PendingOp {
                op: move_op,
                expect: Expectation::WindowOn {
                    window,
                    workspace: target,
                },
                issued_ns: now_ns,
            });
            let switch_op = s.mint_op();
            fx.push(Effect::SwitchWorkspace {
                op: switch_op,
                target,
            });
            s.pending.push(PendingOp {
                op: switch_op,
                expect: Expectation::AllMonitorsOn(target),
                issued_ns: now_ns,
            });
            // The carried window rides on top. Its assignment is only pending,
            // so the destination's MRU stack does not contain it yet — and a
            // restack headed by a resident sibling makes AppKit key THAT
            // window instead (run 51: every carry's restack omitted the
            // carried window, and the next chord found focus on a sibling).
            let mut order = vec![window];
            order.extend(mru_stack(s, target).into_iter().filter(|w| *w != window));
            if order.len() >= 2 {
                fx.push(Effect::RestackWindows { order });
            }
            fx.push(Effect::RequestRescan {
                reason: RescanTrigger::PostEffect { op: switch_op },
            });
            FocusIntent::Window(window)
        }

        Command::Focus { target } => {
            // A target on a hidden monitor gets its monitor viewed right after
            // the grant, and the newly visible set restacked with it on top.
            let (view, proj) = view_for(s, target);
            let center = s.projected_frame_in(&s.windows[&target], &proj).center();
            let op = s.mint_op();
            fx.push(Effect::FocusWindow { op, window: target });
            // Warp optimistically off our own belief of the frame rather than
            // waiting for the focus to be observed — a mouse that lags its
            // window by a rescan round-trip feels broken. The mouse follows
            // Ordo-initiated switches only; warping on external focus changes
            // (the user clicking a window!) would fight the pointer.
            fx.push(Effect::WarpMouse { to: center });
            s.pending.push(PendingOp {
                op,
                expect: Expectation::Focused(target),
                issued_ns: now_ns,
            });
            if let Some(vm) = view {
                push_view(s, vm, now_ns, fx);
                if let Some(ws) = s.current_workspace() {
                    let mut order = vec![target];
                    order.extend(visible_stack(s, ws, &proj).into_iter().filter(|w| *w != target));
                    if order.len() >= 2 {
                        fx.push(Effect::RestackWindows { order });
                    }
                }
            }
            fx.push(Effect::RequestRescan {
                reason: RescanTrigger::PostEffect { op },
            });
            FocusIntent::Window(target)
        }

        Command::Demote {
            workspace,
            from,
            to,
        } => {
            s.focus_history.demote(from);
            let (view, proj) = view_for(s, to);
            let center = s.projected_frame_in(&s.windows[&to], &proj).center();
            let op = s.mint_op();
            fx.push(Effect::FocusWindow { op, window: to });
            fx.push(Effect::WarpMouse { to: center });
            if let Some(vm) = view {
                push_view(s, vm, now_ns, fx);
            }
            // Bury it visually too, AFTER the focus: restacking raises
            // everything above the demoted window, and raises land below the
            // key window — so the new focus must already be key. The history
            // was demoted above, so the MRU order now ends with `from`.
            let order = visible_stack(s, workspace, &proj);
            if order.len() >= 2 {
                fx.push(Effect::RestackWindows { order });
            }
            s.pending.push(PendingOp {
                op,
                expect: Expectation::Focused(to),
                issued_ns: now_ns,
            });
            fx.push(Effect::RequestRescan {
                reason: RescanTrigger::PostEffect { op },
            });
            FocusIntent::Window(to)
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
            if proj != before {
                if let Some(ws) = s.current_workspace() {
                    let mut order = vec![window];
                    order.extend(visible_stack(s, ws, &proj).into_iter().filter(|w| *w != window));
                    if order.len() >= 2 {
                        fx.push(Effect::RestackWindows { order });
                    }
                }
            }
            fx.push(Effect::RequestRescan {
                reason: RescanTrigger::PostEffect { op },
            });
            FocusIntent::Window(window)
        }

        Command::View { target } => {
            // The monitor twin of a workspace switch: focus goes to the
            // target monitor's MRU window on the current workspace before the
            // view moves, and the newly visible set is restacked under it.
            let ws = s.current_workspace().expect("resolved against a workspace");
            let proj = s.projection_with(Some(target), None);
            let head = s.focus_history.iter().find(|w| {
                s.windows
                    .get(w)
                    .is_some_and(|r| r.workspace == ws && r.vmonitor == target)
            });
            if let Some(fw) = head {
                let op = s.mint_op();
                fx.push(Effect::FocusWindow { op, window: fw });
                if s.focused != Some(fw) {
                    s.pending.push(PendingOp {
                        op,
                        expect: Expectation::Focused(fw),
                        issued_ns: now_ns,
                    });
                }
                fx.push(Effect::WarpMouse {
                    to: s.projected_frame_in(&s.windows[&fw], &proj).center(),
                });
            }
            let op = push_view(s, target, now_ns, fx);
            let mut order: Vec<WindowId> = head.into_iter().collect();
            order.extend(visible_stack(s, ws, &proj).into_iter().filter(|w| Some(*w) != head));
            if order.len() >= 2 {
                fx.push(Effect::RestackWindows { order });
            }
            fx.push(Effect::RequestRescan {
                reason: RescanTrigger::PostEffect { op },
            });
            head.map_or(FocusIntent::Deferred, FocusIntent::Window)
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
            let mut viewed = v.viewed;
            if enabled {
                if let Some(vm) = s.declared_focus().and_then(|w| s.windows.get(&w)).map(|r| r.vmonitor) {
                    if vm != v.viewed {
                        push_view(s, vm, now_ns, fx);
                        viewed = vm;
                    }
                }
            }
            if let Some(ws) = s.current_workspace() {
                let order = visible_stack(s, ws, &s.projection_with(Some(viewed), Some(enabled)));
                if order.len() >= 2 {
                    fx.push(Effect::RestackWindows { order });
                }
            }
            fx.push(Effect::RequestRescan {
                reason: RescanTrigger::PostEffect { op },
            });
            s.focus_intent()
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
    let Some(cur) = s.current_workspace() else {
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

    // While the OS owns the slot, its choice among the VISIBLE windows is the
    // only record of where the user went. A hidden landing is never recorded
    // from observation: it is either navigation, which the follow below
    // declares, or a fling, which must not touch the history at all.
    if s.focus_target().is_none() {
        if let Some(f) = s.focused {
            if s.windows.get(&f).is_some_and(|r| s.is_visible(r)) {
                s.focus_history.touch(f);
            }
        }
    }
    // A display came or went. Every window on the vanished display was just
    // re-homed by macOS, not by anyone's hand — nothing about a window's
    // placement in this snapshot says anything about intent.
    let topology_changed = deltas
        .iter()
        .any(|d| matches!(d, Delta::MonitorAdded(_) | Delta::MonitorRemoved(_)));

    // Resolve or age expectations against the fresh belief.
    let mut expired: Vec<PendingOp> = Vec::new();
    let mut still_pending: Vec<PendingOp> = Vec::new();
    for p in std::mem::take(&mut s.pending) {
        if expectation_satisfied(&p.expect, s) {
            notes.push(Note::SelfConfirmed { op: p.op });
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
                }
            }
        }
    }

    let mut last_op: Option<OpId> = None;

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

    enforce_focus(s, navigation_gesture, now_ns, notes, &mut last_op, fx);

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
                let op = s.mint_op();
                fx.push(Effect::SwitchWorkspace { op, target });
                s.pending.push(PendingOp {
                    op,
                    expect: Expectation::AllMonitorsOn(target),
                    issued_ns: now_ns,
                });
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

    if let Some(op) = last_op {
        fx.push(Effect::RequestRescan {
            reason: RescanTrigger::PostEffect { op },
        });
    }
}

/// The projection, asserted: every visible window sits on the display its
/// virtual monitor is hosted by. Two resolutions for a window found on some
/// other display, and telling them apart is the whole job:
///
/// - It MOVED there this snapshot, between two live displays, by a hand that
///   was not Ordo's (a drag, an app placing itself): placement is
///   observational, so the monitor declaration follows — the window is
///   adopted onto the virtual monitor that display stands for. Delta-driven
///   on purpose: a standing position says nothing, only the move does.
/// - It merely SITS there — the display set changed under it (a replug
///   brought its monitor's display back while it waits on the laptop), or a
///   declaration moved it (a corral, a view): a violation of the projection,
///   corrected by re-hosting the frame, damped on the frame axis.
///
/// Adoption is suppressed outright while the display set is changing (macOS
/// re-homes every window of a vanished display, and those moves are nobody's
/// intent), and for a window whose own monitor is hidden (it should not be on
/// screen at all — that is the backend's park to assert, not a drag). Both
/// resolutions yield to a pending op on the window and to a hand still on it.
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

    if !topology_changed {
        for d in deltas {
            let Delta::WindowMonitorChanged { window, to, .. } = d else {
                continue;
            };
            if entry_expectations.iter().any(|e| reconcile::explains(e, d)) {
                continue;
            }
            let Some(rec) = s.windows.get(window).cloned() else {
                continue;
            };
            if !s.is_visible(&rec) || busy(s, *window) {
                continue;
            }
            let Some(v) = s.canonical_vm_of(*to) else { continue };
            if v == rec.vmonitor {
                continue;
            }
            let op = s.mint_op();
            fx.push(Effect::AssignWindowToMonitor {
                op,
                window: *window,
                target: v,
            });
            s.pending.push(PendingOp {
                op,
                expect: Expectation::WindowOnMonitor {
                    window: *window,
                    monitor: v,
                },
                issued_ns: now_ns,
            });
            notes.push(Note::MonitorAdopted {
                window: *window,
                monitor: v,
            });
            *last_op = Some(op);
        }
    }

    let misplaced: Vec<WindowRecord> = s
        .windows
        .values()
        .filter(|r| s.is_visible(r) && s.host_of(r.vmonitor).is_some_and(|h| h != r.monitor))
        .cloned()
        .collect();
    for rec in misplaced {
        let window = rec.id;
        if busy(s, window) || unexplained_move(window) {
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
) {
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
        return;
    };

    let landed_hidden = s
        .focused
        .and_then(|w| s.windows.get(&w))
        .filter(|r| !s.is_visible(r) && r.workspace.0 >= 1 && r.workspace.0 <= s.workspace_count)
        .cloned();
    if let (Some(rec), None) = (&landed_hidden, s.focus_target()) {
        if navigation_gesture {
            // Follow on whichever axes hide the window: the workspace, the
            // monitor, or both.
            let target = rec.workspace;
            let mut op = None;
            if target != here {
                let o = s.mint_op();
                fx.push(Effect::SwitchWorkspace { op: o, target });
                s.pending.push(PendingOp {
                    op: o,
                    expect: Expectation::AllMonitorsOn(target),
                    issued_ns: now_ns,
                });
                op = Some(o);
            }
            let mut proj = s.projection();
            let mut monitor = None;
            if !proj.is_hosted(rec.vmonitor) {
                op = Some(push_view(s, rec.vmonitor, now_ns, fx));
                proj = s.projection_with(Some(rec.vmonitor), None);
                monitor = Some(rec.vmonitor);
            }
            // Declared first so the window the user chose heads the restack.
            s.declare_focus(FocusIntent::Window(rec.id));
            let order = visible_stack(s, target, &proj);
            if order.len() >= 2 {
                fx.push(Effect::RestackWindows { order });
            }
            notes.push(Note::FollowedFocus {
                window: rec.id,
                target,
                monitor,
            });
            *last_op = op;
            return;
        }
        // An empty visible workspace has nothing to hold focus: leave it, the
        // next birth here declares itself.
        if s.conceded != Some(rec.app) {
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

    let Some(w) = s.focus_target() else {
        return;
    };
    if s.focused == Some(w) {
        s.focus_corrections = 0;
        return;
    }
    // A declaration for a window that is not on screen is unenforceable (its
    // switch or view has not landed, or never will); granting it would put
    // the keyboard into an invisible window.
    if !s.is_visible(&s.windows[&w]) {
        return;
    }
    // The grant is in flight; apps land it on their own schedule.
    if s.pending
        .iter()
        .any(|p| p.expect == Expectation::Focused(w))
    {
        return;
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
        return;
    }
    s.focus_corrections += 1;
    let op = s.mint_op();
    fx.push(Effect::FocusWindow { op, window: w });
    s.pending.push(PendingOp {
        op,
        expect: Expectation::Focused(w),
        issued_ns: now_ns,
    });
    notes.push(Note::FocusReasserted { window: w });
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
    s.pending.push(PendingOp {
        op,
        expect,
        issued_ns: now_ns,
    });
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
        Expectation::WindowOnMonitor { window, monitor } => s
            .windows
            .get(window)
            .is_some_and(|r| r.vmonitor == *monitor),
        Expectation::Viewing(vm) => s.virtual_monitors.is_some_and(|v| v.viewed == *vm),
        Expectation::VirtualMonitorsEnabled(e) => {
            s.virtual_monitors.is_some_and(|v| v.enabled == *e)
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
