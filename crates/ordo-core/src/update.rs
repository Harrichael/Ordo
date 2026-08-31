use serde::{Deserialize, Serialize};

use crate::effect::{CorrectionAxis, Effect, Expectation};
use crate::event::{AxHintKind, Event, HotkeyAction, OpOutcome, RescanTrigger, WorldSnapshot};
use crate::ids::{OpId, Rect, WindowId, WorkspaceId, FRAME_EPSILON};
use crate::reconcile::{self, Delta};
use crate::state::{Mode, PendingOp, State};

/// Snapshots an expectation survives unmet before it's declared lost. Three
/// covers the post-effect rescan plus slop for a slow executor without letting
/// a dead op suppress external-change attribution for long.
const EXPECTATION_RESCANS: u8 = 3;

/// Correctives per window (and per tear episode) before we stop fighting and
/// log instead. An app that re-places its own window wins after this many
/// rounds — divergence becomes a loud log line, never an effect loop.
const DAMPING_LIMIT: u8 = 3;

/// How long after a switch its focus fallout is still presumed in flight.
/// Expectations can't cover this: they're one-shot (kitty delivered a
/// DUPLICATE activation 520ms after its grant — run 38 seq 1410 — after the
/// first arrival had already consumed the expectation) and they expire on a
/// rescan count, which event-driven hints have compressed to well under the
/// measured ~1.6s app landing tail. A blanket wall-clock cap is the blunt but
/// safe answer; sharper schemes (per-window fallout horizon with early
/// expiry) are sketched in issues.txt if the lost-follow cost ever shows up.
const FOCUS_SETTLE_NS: u64 = 2_000_000_000;

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
    /// Focus moved externally to a window on a hidden workspace; we switched
    /// there to follow it (the Cmd+Tab / Dock-click hole in emulation).
    FollowedFocus {
        window: WindowId,
        target: WorkspaceId,
    },
    /// Focus fell onto a hidden workspace as fallout from a window closing;
    /// instead of following, focus was pulled back to this window here.
    HeldFocusOnClose { window: WindowId },
    /// Focus fell onto a hidden workspace inside the post-switch settle
    /// window — read as our own grant's late echo, not navigation; focus was
    /// pulled back to this window here.
    HeldFocusSettling { window: WindowId },
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
    }

    Step {
        state: s,
        effects,
        notes,
    }
}

/// The workspace's visual stacking, front-to-back, as the MRU history implies
/// it. Emitted alongside anything that reveals a workspace: parking and
/// app-hiding scramble real z-order, and MRU is the single source of truth
/// for what "on top" means here.
fn mru_stack(s: &State, ws: WorkspaceId) -> Vec<WindowId> {
    s.focus_history
        .iter()
        .filter(|w| s.windows.get(w).is_some_and(|r| r.workspace == ws))
        .collect()
}

fn push_restack(s: &State, ws: WorkspaceId, fx: &mut Vec<Effect>) {
    let order = mru_stack(s, ws);
    if order.len() >= 2 {
        fx.push(Effect::RestackWindows { order });
    }
}

fn handle_hotkey(s: &mut State, action: HotkeyAction, now_ns: u64, fx: &mut Vec<Effect>) {
    match action {
        HotkeyAction::WorkspacePrev
        | HotkeyAction::WorkspaceNext
        | HotkeyAction::WorkspaceSwitchTo(_) => {
            let Some(cur) = s.current_workspace() else {
                return;
            };
            let target = match action {
                HotkeyAction::WorkspacePrev if cur.0 > 1 => WorkspaceId(cur.0 - 1),
                HotkeyAction::WorkspaceNext if cur.0 < s.workspace_count => WorkspaceId(cur.0 + 1),
                HotkeyAction::WorkspaceSwitchTo(t)
                    if t != cur && t.0 >= 1 && t.0 <= s.workspace_count =>
                {
                    t
                }
                _ => return, // clamped at the edge (or already there): nothing to do
            };
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
            let stack = mru_stack(s, target);
            if let Some(&fw) = stack.first() {
                let op = s.mint_op();
                fx.push(Effect::FocusWindow { op, window: fw });
                // Re-asserting focus the belief already holds produces no
                // delta, so there is nothing to attribute to an expectation.
                if s.focused != Some(fw) {
                    s.pending.push(PendingOp {
                        op,
                        expect: Expectation::Focused(fw),
                        rescans_left: EXPECTATION_RESCANS,
                    });
                }
            }
            let op = s.mint_op();
            fx.push(Effect::SwitchWorkspace { op, target });
            s.last_switch_mono_ns = Some(now_ns);
            s.pending.push(PendingOp {
                op,
                expect: Expectation::AllMonitorsOn(target),
                rescans_left: EXPECTATION_RESCANS,
            });
            if stack.len() >= 2 {
                fx.push(Effect::RestackWindows { order: stack });
            }
            fx.push(Effect::RequestRescan {
                reason: RescanTrigger::PostEffect { op },
            });
        }

        HotkeyAction::CarryFocusedToWorkspacePrev | HotkeyAction::CarryFocusedToWorkspaceNext => {
            // Declared focus, not observed: mid-handoff the observation is
            // stale and a carry grabbed the PREVIOUS window (run 38).
            let Some(focused) = s.declared_focus() else {
                return;
            };
            if !s.windows.contains_key(&focused) {
                return;
            }
            let Some(cur) = s.current_workspace() else {
                return;
            };
            // You carry what's with you: focus can legitimately sit on a
            // hidden window (landing on an empty workspace leaves it behind),
            // and dragging that one over would materialize it from nowhere.
            if s.windows[&focused].workspace != cur {
                return;
            }
            let target = match action {
                HotkeyAction::CarryFocusedToWorkspacePrev if cur.0 > 1 => WorkspaceId(cur.0 - 1),
                HotkeyAction::CarryFocusedToWorkspaceNext if cur.0 < s.workspace_count => {
                    WorkspaceId(cur.0 + 1)
                }
                _ => return, // clamped at the edge: nothing to do
            };
            // Reassign first, then switch: when the switch lands, the carried
            // window is already a resident of the destination and comes along.
            // Assignment only — the window's frame and focus don't change, so
            // no frame write may be issued for it (a full move parked it and
            // the switch immediately restored it: two racing writes).
            let move_op = s.mint_op();
            fx.push(Effect::AssignWindowToWorkspace {
                op: move_op,
                window: focused,
                target,
            });
            s.pending.push(PendingOp {
                op: move_op,
                expect: Expectation::WindowOn {
                    window: focused,
                    workspace: target,
                },
                rescans_left: EXPECTATION_RESCANS,
            });
            let switch_op = s.mint_op();
            fx.push(Effect::SwitchWorkspace {
                op: switch_op,
                target,
            });
            s.last_switch_mono_ns = Some(now_ns);
            s.pending.push(PendingOp {
                op: switch_op,
                expect: Expectation::AllMonitorsOn(target),
                rescans_left: EXPECTATION_RESCANS,
            });
            push_restack(s, target, fx);
            fx.push(Effect::RequestRescan {
                reason: RescanTrigger::PostEffect { op: switch_op },
            });
        }

        HotkeyAction::MruWorkspace
        | HotkeyAction::MruMonitor
        | HotkeyAction::MruApp
        | HotkeyAction::MruOtherMonitor => {
            let Some(cur_ws) = s.current_workspace() else {
                return;
            };
            let focused = s.declared_focus();
            let focused_rec = focused.and_then(|w| s.windows.get(&w)).cloned();
            // The scoped variants are relative to the focused window; with
            // nothing focused there is no "same monitor/app" to speak of.
            if focused_rec.is_none() && action != HotkeyAction::MruWorkspace {
                return;
            }
            let target = {
                let windows = &s.windows;
                s.focus_history.most_recent(focused, |w| {
                    let Some(r) = windows.get(&w) else {
                        return false;
                    };
                    if r.workspace != cur_ws {
                        return false;
                    }
                    match action {
                        HotkeyAction::MruWorkspace => true,
                        HotkeyAction::MruMonitor => {
                            focused_rec.as_ref().is_some_and(|f| r.monitor == f.monitor)
                        }
                        HotkeyAction::MruOtherMonitor => {
                            focused_rec.as_ref().is_some_and(|f| r.monitor != f.monitor)
                        }
                        HotkeyAction::MruApp => {
                            focused_rec.as_ref().is_some_and(|f| r.app == f.app)
                        }
                        _ => unreachable!(),
                    }
                })
            };
            let Some(target) = target else {
                return;
            };
            let center = s.windows[&target].frame.center();
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
                rescans_left: EXPECTATION_RESCANS,
            });
            fx.push(Effect::RequestRescan {
                reason: RescanTrigger::PostEffect { op },
            });
        }

        HotkeyAction::MruDemote => {
            let Some(cur_ws) = s.current_workspace() else {
                return;
            };
            let Some(focused) = s.declared_focus() else {
                return;
            };
            // Demoting is only meaningful if focus can actually leave the
            // window — otherwise the next observation touches it straight back
            // to the front. With nowhere else to go, do nothing.
            let target = {
                let windows = &s.windows;
                s.focus_history.most_recent(Some(focused), |w| {
                    windows.get(&w).is_some_and(|r| r.workspace == cur_ws)
                })
            };
            let Some(target) = target else {
                return;
            };
            s.focus_history.demote(focused);
            let center = s.windows[&target].frame.center();
            let op = s.mint_op();
            fx.push(Effect::FocusWindow { op, window: target });
            fx.push(Effect::WarpMouse { to: center });
            // Bury it visually too, AFTER the focus: restacking raises
            // everything above the demoted window, and raises land below the
            // key window — so the new focus must already be key. The history
            // was demoted above, so the MRU order now ends with `focused`.
            push_restack(s, cur_ws, fx);
            s.pending.push(PendingOp {
                op,
                expect: Expectation::Focused(target),
                rescans_left: EXPECTATION_RESCANS,
            });
            fx.push(Effect::RequestRescan {
                reason: RescanTrigger::PostEffect { op },
            });
        }

        HotkeyAction::MoveFocusedToOtherMonitor => {
            let Some(focused) = s.declared_focus() else {
                return;
            };
            let Some(rec) = s.windows.get(&focused).cloned() else {
                return;
            };
            let order = s.monitors_by_position();
            if order.len() < 2 {
                return;
            }
            let Some(i) = order.iter().position(|m| *m == rec.monitor) else {
                return;
            };
            let to_id = order[(i + 1) % order.len()];
            let (Some(from_mon), Some(to_mon)) =
                (s.monitors.get(&rec.monitor), s.monitors.get(&to_id))
            else {
                return;
            };
            let frame = rec.frame.translate_between(&from_mon.frame, &to_mon.frame);
            let op = s.mint_op();
            fx.push(Effect::SetWindowFrame {
                op,
                window: focused,
                frame,
            });
            fx.push(Effect::WarpMouse { to: frame.center() });
            s.pending.push(PendingOp {
                op,
                expect: Expectation::WindowFramed {
                    window: focused,
                    frame,
                },
                rescans_left: EXPECTATION_RESCANS,
            });
            fx.push(Effect::RequestRescan {
                reason: RescanTrigger::PostEffect { op },
            });
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
    let anchor_mon = pre.focused_monitor();
    let entry_expectations: Vec<Expectation> =
        pre.pending.iter().map(|p| p.expect.clone()).collect();

    let deltas = reconcile::diff(pre, snap);
    reconcile::apply_snapshot(s, snap);

    // Resolve or age expectations against the fresh belief.
    //
    // Focus is one global slot, so a LANDED grant makes every older pending
    // grant dead intent. Aging those out normally is harmless for delta
    // attribution, but declared_focus() would keep reading the stale one —
    // and a carry mid-burst would grab a window from a workspace behind us.
    // Newer unlanded grants stay: they remain the freshest declaration.
    let last_landed_focus = s
        .pending
        .iter()
        .rposition(|p| matches!(p.expect, Expectation::Focused(w) if s.focused == Some(w)));
    let mut expired: Vec<PendingOp> = Vec::new();
    let mut still_pending: Vec<PendingOp> = Vec::new();
    for (i, mut p) in std::mem::take(&mut s.pending).into_iter().enumerate() {
        if matches!(p.expect, Expectation::Focused(_))
            && !expectation_satisfied(&p.expect, s)
            && last_landed_focus.is_some_and(|j| i < j)
        {
            notes.push(Note::OpLost { op: p.op });
            expired.push(p);
            continue;
        }
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
        } else {
            p.rescans_left = p.rescans_left.saturating_sub(1);
            if p.rescans_left == 0 {
                notes.push(Note::OpLost { op: p.op });
                expired.push(p);
            } else {
                still_pending.push(p);
            }
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
    // move them — but only under the damping limit.
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
        if let (Some(anchor_ws), Some(anchor_mon)) = (anchor_ws, anchor_mon) {
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
                if rec.monitor != anchor_mon {
                    if let (Some(from_mon), Some(to_mon)) =
                        (s.monitors.get(&rec.monitor), s.monitors.get(&anchor_mon))
                    {
                        let frame = rec.frame.translate_between(&from_mon.frame, &to_mon.frame);
                        let window = *w;
                        correct_window(
                            s,
                            window,
                            CorrectionAxis::Frame,
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

    // Follow-the-focus: macOS handed focus to a window on a hidden workspace
    // (Cmd+Tab, a Dock click, an app self-activating) — the one hole the
    // emulated backend leaves open, since the app switcher is machine-global.
    // Mirror what native Spaces would do and bring that workspace over. Only
    // an EXTERNAL focus change qualifies: our own switches legitimately leave
    // focus sitting on a freshly parked window (parking doesn't defocus), and
    // `explains` attributes those focus deltas to the pending switch.
    //
    // A pending Focused expectation also holds this off: a switch's focus
    // handoff is fire-and-forget and lands on the target app's schedule,
    // while the switch itself (ledger-backed on the emulated backend)
    // self-confirms on the very first snapshot — so AllMonitorsOn alone
    // stopped guarding the handoff window once the restack moved off-thread
    // and snapshots got fast. A transient re-key onto some hidden-workspace
    // window mid-handoff (Dock dimming's app-hide can fling focus) must not
    // read as the user navigating away; the expectation resolves or expires
    // within EXPECTATION_RESCANS, so a genuine follow is deferred, not lost.
    //
    // Neither guard survives the full fallout tail (expectations are one-shot
    // and rescan-counted; apps re-fling focus for seconds), so inside
    // FOCUS_SETTLE_NS of our last switch a stray focus is treated like close
    // fallout: hold the workspace, pull focus back. The pull-back's own echo
    // can't recurse — its target is on the current workspace.
    if !s.pending.iter().any(|p| {
        matches!(
            p.expect,
            Expectation::AllMonitorsOn(_) | Expectation::Focused(_)
        )
    }) {
        let external_focus = deltas.iter().any(|d| {
            matches!(d, Delta::FocusChanged { .. })
                && !entry_expectations.iter().any(|e| reconcile::explains(e, d))
        });
        if external_focus {
            if let Some(rec) = s.focused.and_then(|w| s.windows.get(&w)).cloned() {
                let target = rec.workspace;
                let here = s.current_workspace();
                if Some(target) != here && target.0 >= 1 && target.0 <= s.workspace_count {
                    // Close fallout is not navigation: when a window dies,
                    // macOS hands focus to the app's next window wherever it
                    // lives — the user asked to close something, not to go
                    // somewhere. Any destroy in this snapshot marks the focus
                    // change as suspect; hold the workspace and pull focus
                    // back to its MRU window (typing into a hidden window is
                    // the alternative). A genuine Cmd+Tab landing in the same
                    // snapshot as an unrelated close loses one follow — the
                    // user re-presses; a wrong yank costs far more.
                    let close_fallout = deltas
                        .iter()
                        .any(|d| matches!(d, Delta::WindowDestroyed(_)));
                    let settling = s
                        .last_switch_mono_ns
                        .is_some_and(|t| now_ns.saturating_sub(t) < FOCUS_SETTLE_NS);
                    if close_fallout || settling {
                        if let Some(back) = here.and_then(|h| mru_stack(s, h).into_iter().next()) {
                            let op = s.mint_op();
                            fx.push(Effect::FocusWindow { op, window: back });
                            s.pending.push(PendingOp {
                                op,
                                expect: Expectation::Focused(back),
                                rescans_left: EXPECTATION_RESCANS,
                            });
                            notes.push(if close_fallout {
                                Note::HeldFocusOnClose { window: back }
                            } else {
                                Note::HeldFocusSettling { window: back }
                            });
                            last_op = Some(op);
                        }
                    } else {
                        let op = s.mint_op();
                        fx.push(Effect::SwitchWorkspace { op, target });
                        s.last_switch_mono_ns = Some(now_ns);
                        s.pending.push(PendingOp {
                            op,
                            expect: Expectation::AllMonitorsOn(target),
                            rescans_left: EXPECTATION_RESCANS,
                        });
                        push_restack(s, target, fx);
                        notes.push(Note::FollowedFocus {
                            window: rec.id,
                            target,
                        });
                        last_op = Some(op);
                    }
                }
            }
        }
    }

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
                s.last_switch_mono_ns = Some(now_ns);
                s.pending.push(PendingOp {
                    op,
                    expect: Expectation::AllMonitorsOn(target),
                    rescans_left: EXPECTATION_RESCANS,
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

/// Emit a placement corrective for `window` on `axis` unless that axis has hit
/// the damping limit, in which case note the divergence and stand down. Damping
/// is per axis so a window wrong on both workspace and frame gets a full retry
/// budget for each.
fn correct_window(
    s: &mut State,
    window: WindowId,
    axis: CorrectionAxis,
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
        rescans_left: EXPECTATION_RESCANS,
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
