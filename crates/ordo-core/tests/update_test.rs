//! Every test drives the core the way the shell will: feed events, assert on
//! the emitted effects and resulting state. Internals (diffing, pendings) are
//! exercised only through that surface, so they stay free to change.

use ordo_core::*;

// --- fixtures -------------------------------------------------------------
// Two 1920x1080 monitors side by side, three workspaces, three windows:
//   w1 (pid 100) and w3 (pid 100) on monitor A, w2 (pid 200) on monitor B.

fn ts() -> Ts {
    Ts {
        wall_ms: 0,
        mono_ns: 0,
    }
}

fn mid(n: u8) -> MonitorId {
    MonitorId(n as u128)
}

fn wid(n: u32) -> WindowId {
    WindowId(n)
}

fn ws(n: u8) -> WorkspaceId {
    WorkspaceId(n)
}

fn rect(x: f64, y: f64) -> Rect {
    Rect {
        x,
        y,
        w: 400.0,
        h: 300.0,
    }
}

fn mon_a(active: u8) -> MonitorSnap {
    MonitorSnap {
        id: mid(1),
        frame: Rect {
            x: 0.0,
            y: 0.0,
            w: 1920.0,
            h: 1080.0,
        },
        is_main: true,
        active_workspace: ws(active),
        workspace_count: 3,
    }
}

fn mon_b(active: u8) -> MonitorSnap {
    MonitorSnap {
        id: mid(2),
        frame: Rect {
            x: 1920.0,
            y: 0.0,
            w: 1920.0,
            h: 1080.0,
        },
        is_main: false,
        active_workspace: ws(active),
        workspace_count: 3,
    }
}

fn win(id: u32, pid: i32, workspace: u8, frame: Rect) -> WindowSnap {
    WindowSnap {
        id: wid(id),
        app: Pid(pid),
        bundle_id: None,
        title: format!("w{id}"),
        frame,
        workspace: ws(workspace),
    }
}

fn std_windows() -> Vec<WindowSnap> {
    vec![
        win(1, 100, 1, rect(100.0, 100.0)),
        win(2, 200, 1, rect(2000.0, 100.0)),
        win(3, 100, 1, rect(600.0, 500.0)),
    ]
}

fn observed(
    monitors: Vec<MonitorSnap>,
    windows: Vec<WindowSnap>,
    focused: Option<u32>,
    trigger: RescanTrigger,
) -> Event {
    Event::WorldObserved {
        at: ts(),
        trigger,
        snap: WorldSnapshot {
            monitors,
            windows,
            focused: focused.map(wid),
        },
    }
}

fn hotkey(action: HotkeyAction) -> Event {
    Event::Hotkey { at: ts(), action }
}

/// Boot the standard world, then focus each window in `focus_seq` in order,
/// so the MRU history is exactly `focus_seq` reversed-into-front order.
fn booted(focus_seq: &[u32]) -> State {
    let mut s = update(
        &State::new(),
        &observed(
            vec![mon_a(1), mon_b(1)],
            std_windows(),
            None,
            RescanTrigger::Startup,
        ),
    )
    .state;
    for f in focus_seq {
        s = update(
            &s,
            &observed(
                vec![mon_a(1), mon_b(1)],
                std_windows(),
                Some(*f),
                RescanTrigger::Periodic,
            ),
        )
        .state;
    }
    s
}

fn focus_targets(effects: &[Effect]) -> Vec<WindowId> {
    effects
        .iter()
        .filter_map(|e| match e {
            Effect::FocusWindow { window, .. } => Some(*window),
            _ => None,
        })
        .collect()
}

fn count_switches(effects: &[Effect]) -> usize {
    effects
        .iter()
        .filter(|e| matches!(e, Effect::SwitchWorkspace { .. }))
        .count()
}

fn set_frame_for(effects: &[Effect], w: u32) -> Option<Rect> {
    effects.iter().find_map(|e| match e {
        Effect::SetWindowFrame { window, frame, .. } if *window == wid(w) => Some(*frame),
        _ => None,
    })
}

// --- observation & belief --------------------------------------------------

#[test]
fn startup_populates_state_without_acting() {
    let step = update(
        &State::new(),
        &observed(
            vec![mon_a(1), mon_b(1)],
            std_windows(),
            Some(1),
            RescanTrigger::Startup,
        ),
    );
    assert!(step.effects.is_empty());
    let s = &step.state;
    assert_eq!(s.workspace_count, 3);
    assert_eq!(s.windows.len(), 3);
    assert_eq!(s.focused, Some(wid(1)));
    assert_eq!(s.windows[&wid(2)].monitor, mid(2), "derived from frame");
    assert_eq!(s.current_workspace(), Some(ws(1)));
}

#[test]
fn external_workspace_switch_is_absorbed_not_fought() {
    let s = booted(&[1]);
    let obs = update(
        &s,
        &observed(
            vec![mon_a(2), mon_b(2)],
            std_windows(),
            Some(1),
            RescanTrigger::Periodic,
        ),
    );
    // Coherent external switch: belief follows, nothing to correct.
    assert!(obs.effects.is_empty());
    assert_eq!(obs.state.monitor_ws[&mid(1)], ws(2));
    let externals = obs
        .notes
        .iter()
        .filter(|n| {
            matches!(
                n,
                Note::External {
                    delta: Delta::MonitorWorkspaceChanged { .. }
                }
            )
        })
        .count();
    assert_eq!(externals, 2);
}

#[test]
fn destroyed_window_leaves_the_mru_history() {
    let s = booted(&[3, 2, 1]);
    let mut wins = std_windows();
    wins.remove(1); // w2 closes
    let s = update(
        &s,
        &observed(
            vec![mon_a(1), mon_b(1)],
            wins,
            Some(1),
            RescanTrigger::Periodic,
        ),
    )
    .state;
    let step = update(&s, &hotkey(HotkeyAction::MruWorkspace));
    assert_eq!(focus_targets(&step.effects), vec![wid(3)]);
}

// --- MRU hotkeys -------------------------------------------------------------

#[test]
fn alt_tab_focuses_mru_in_workspace_and_warps_mouse() {
    let s = booted(&[3, 2, 1]); // history: [1, 2, 3]
    let step = update(&s, &hotkey(HotkeyAction::MruWorkspace));
    assert_eq!(focus_targets(&step.effects), vec![wid(2)]);
    // Mouse comes along, to the center of w2's frame (2000,100,400,300).
    assert!(step
        .effects
        .iter()
        .any(|e| matches!(e, Effect::WarpMouse { to } if to.x == 2200.0 && to.y == 250.0)));
    assert!(step
        .effects
        .iter()
        .any(|e| matches!(e, Effect::RequestRescan { .. })));
}

#[test]
fn alt_tab_skips_windows_on_other_workspaces() {
    let s = booted(&[3, 2, 1]);
    let mut wins = std_windows();
    wins[1].workspace = ws(2); // w2 drifted to workspace 2
    let s = update(
        &s,
        &observed(
            vec![mon_a(1), mon_b(1)],
            wins,
            Some(1),
            RescanTrigger::Periodic,
        ),
    )
    .state;
    let step = update(&s, &hotkey(HotkeyAction::MruWorkspace));
    assert_eq!(focus_targets(&step.effects), vec![wid(3)]);
}

#[test]
fn alt_shift_tab_stays_on_the_focused_monitor() {
    let s = booted(&[3, 2, 1]); // focused w1 on A; w2 is MRU but lives on B
    let step = update(&s, &hotkey(HotkeyAction::MruMonitor));
    assert_eq!(focus_targets(&step.effects), vec![wid(3)]);
}

#[test]
fn alt_backtick_stays_in_the_focused_app() {
    let s = booted(&[3, 2, 1]); // focused w1 (pid 100); w2 is MRU but pid 200
    let step = update(&s, &hotkey(HotkeyAction::MruApp));
    assert_eq!(focus_targets(&step.effects), vec![wid(3)]);
}

#[test]
fn alt_tab_toggles_between_top_two() {
    let s = booted(&[3, 2, 1]);
    let step = update(&s, &hotkey(HotkeyAction::MruWorkspace));
    assert_eq!(focus_targets(&step.effects), vec![wid(2)]);

    // The world confirms the focus change; that echo is ours, not external.
    let obs = update(
        &step.state,
        &observed(
            vec![mon_a(1), mon_b(1)],
            std_windows(),
            Some(2),
            RescanTrigger::Periodic,
        ),
    );
    assert!(obs.notes.contains(&Note::SelfConfirmed { op: OpId(1) }));
    assert!(!obs.notes.iter().any(|n| matches!(
        n,
        Note::External {
            delta: Delta::FocusChanged { .. }
        }
    )));

    let step2 = update(&obs.state, &hotkey(HotkeyAction::MruWorkspace));
    assert_eq!(focus_targets(&step2.effects), vec![wid(1)]);
}

// --- workspace switching ------------------------------------------------------

#[test]
fn workspace_next_switches_and_prev_clamps_at_the_edge() {
    let s = booted(&[1]);
    let step = update(&s, &hotkey(HotkeyAction::WorkspaceNext));
    assert!(step
        .effects
        .iter()
        .any(|e| matches!(e, Effect::SwitchWorkspace { target, .. } if *target == ws(2))));
    assert_eq!(step.state.pending.len(), 1);

    let clamped = update(&s, &hotkey(HotkeyAction::WorkspacePrev));
    assert!(clamped.effects.is_empty(), "already at workspace 1");
}

#[test]
fn confirmed_switch_is_attributed_to_ourselves() {
    let s = booted(&[1]);
    let step = update(&s, &hotkey(HotkeyAction::WorkspaceNext)); // op 1
    let obs = update(
        &step.state,
        &observed(
            vec![mon_a(2), mon_b(2)],
            std_windows(),
            Some(1),
            RescanTrigger::PostEffect { op: OpId(1) },
        ),
    );
    assert!(obs.notes.contains(&Note::SelfConfirmed { op: OpId(1) }));
    assert!(!obs.notes.iter().any(|n| matches!(n, Note::External { .. })));
    assert!(obs.effects.is_empty());
    assert!(obs.state.pending.is_empty());
}

#[test]
fn torn_monitors_are_realigned_to_the_focused_monitors_workspace() {
    let s = booted(&[1]);
    // The user swiped monitor A to workspace 2 behind our back.
    let obs = update(
        &s,
        &observed(
            vec![mon_a(2), mon_b(1)],
            std_windows(),
            Some(1),
            RescanTrigger::Periodic,
        ),
    );
    assert!(obs
        .effects
        .iter()
        .any(|e| matches!(e, Effect::SwitchWorkspace { target, .. } if *target == ws(2))));
    assert!(obs.notes.contains(&Note::TearDetected { target: ws(2) }));
}

#[test]
fn tear_realignment_gives_up_after_the_damping_limit() {
    // A world that refuses to come coherent: realign at most 3 times total
    // (each retry waits out its expectation first), then declare it and stop.
    let mut s = booted(&[1]);
    let mut switches = 0;
    let mut persisting = false;
    for _ in 0..12 {
        let step = update(
            &s,
            &observed(
                vec![mon_a(2), mon_b(1)],
                std_windows(),
                Some(1),
                RescanTrigger::Periodic,
            ),
        );
        switches += count_switches(&step.effects);
        persisting |= step.notes.contains(&Note::TearPersisting);
        s = step.state;
    }
    assert_eq!(switches, 3);
    assert!(persisting);
}

// --- new-window placement ------------------------------------------------------

#[test]
fn new_window_is_corralled_to_the_focused_workspace_and_monitor() {
    let s = booted(&[1]); // user is on w1: monitor A, workspace 1
    let mut wins = std_windows();
    // New window appears on workspace 2, on monitor B, and steals focus. The
    // anchor is the user's context from BEFORE it appeared.
    wins.push(win(9, 300, 2, rect(2100.0, 300.0)));
    let obs = update(
        &s,
        &observed(
            vec![mon_a(1), mon_b(1)],
            wins,
            Some(9),
            RescanTrigger::AxHint {
                pid: Some(Pid(300)),
                kind: AxHintKind::WindowCreated,
            },
        ),
    );
    assert!(obs.effects.iter().any(|e| matches!(
        e,
        Effect::MoveWindowToWorkspace { window, target, .. }
            if *window == wid(9) && *target == ws(1)
    )));
    let frame = set_frame_for(&obs.effects, 9).expect("frame corrective");
    assert!(
        frame.x >= 0.0 && frame.x + frame.w <= 1920.0,
        "landed on monitor A: {frame:?}"
    );
    assert!(obs
        .effects
        .iter()
        .any(|e| matches!(e, Effect::RequestRescan { .. })));
}

#[test]
fn plain_rescans_never_corral() {
    // The same discovery through a periodic scan must NOT move the window: a
    // rescan can't distinguish "new" from "previously missed".
    let s = booted(&[1]);
    let mut wins = std_windows();
    wins.push(win(9, 300, 2, rect(2100.0, 300.0)));
    let obs = update(
        &s,
        &observed(
            vec![mon_a(1), mon_b(1)],
            wins,
            Some(9),
            RescanTrigger::Periodic,
        ),
    );
    assert!(!obs.effects.iter().any(|e| matches!(
        e,
        Effect::MoveWindowToWorkspace { .. } | Effect::SetWindowFrame { .. }
    )));
}

#[test]
fn placement_retries_are_damped_when_the_app_fights_back() {
    let s = booted(&[1]);
    let mut wins = std_windows();
    wins.push(win(9, 300, 1, rect(2100.0, 300.0))); // right workspace, wrong monitor
    let created = observed(
        vec![mon_a(1), mon_b(1)],
        wins.clone(),
        Some(9),
        RescanTrigger::AxHint {
            pid: Some(Pid(300)),
            kind: AxHintKind::WindowCreated,
        },
    );
    let mut step = update(&s, &created);
    let mut frame_correctives = if set_frame_for(&step.effects, 9).is_some() {
        1
    } else {
        0
    };
    let mut diverged = false;
    // The app snaps its window back to monitor B every time (the snapshot
    // never shows our frame sticking).
    for _ in 0..20 {
        let next = update(
            &step.state,
            &observed(
                vec![mon_a(1), mon_b(1)],
                wins.clone(),
                Some(9),
                RescanTrigger::Periodic,
            ),
        );
        if set_frame_for(&next.effects, 9).is_some() {
            frame_correctives += 1;
        }
        diverged |= next.notes.contains(&Note::Diverged { window: wid(9) });
        step = next;
    }
    assert_eq!(
        frame_correctives, 3,
        "initial corrective + 2 damped retries"
    );
    assert!(diverged);
}

// --- ops, failure, rescue ----------------------------------------------------

#[test]
fn unconfirmed_ops_expire_as_lost() {
    let s = booted(&[1]);
    let mut step = update(&s, &hotkey(HotkeyAction::WorkspaceNext)); // op 1
    let mut lost = false;
    for _ in 0..3 {
        step = update(
            &step.state,
            &observed(
                vec![mon_a(1), mon_b(1)],
                std_windows(),
                Some(1),
                RescanTrigger::Periodic,
            ),
        );
        lost |= step.notes.contains(&Note::OpLost { op: OpId(1) });
    }
    assert!(lost);
    assert!(step.state.pending.is_empty());
}

#[test]
fn executor_failure_drops_the_pending_op() {
    let s = booted(&[1]);
    let step = update(&s, &hotkey(HotkeyAction::WorkspaceNext));
    let failed = update(
        &step.state,
        &Event::EffectResult {
            at: ts(),
            op: OpId(1),
            outcome: OpOutcome::Failed {
                detail: "gesture failed".into(),
            },
        },
    );
    assert!(failed.state.pending.is_empty());
    assert!(failed.notes.contains(&Note::OpFailed {
        op: OpId(1),
        detail: "gesture failed".into(),
    }));
}

#[test]
fn rescue_makes_the_core_inert_but_still_observing() {
    let s = booted(&[3, 2, 1]);
    let step = update(&s, &Event::RescueEngaged { at: ts() });
    assert_eq!(step.state.mode, Mode::Rescued);
    assert!(step
        .effects
        .contains(&Effect::SetIntercepting { enabled: false }));

    let dead_key = update(&step.state, &hotkey(HotkeyAction::MruWorkspace));
    assert!(dead_key.effects.is_empty());

    // A torn world after rescue: belief still tracks it, but no correctives.
    let obs = update(
        &dead_key.state,
        &observed(
            vec![mon_a(2), mon_b(1)],
            std_windows(),
            Some(1),
            RescanTrigger::Periodic,
        ),
    );
    assert!(obs.effects.is_empty());
    assert_eq!(obs.state.monitor_ws[&mid(1)], ws(2));
}

#[test]
fn engage_undoes_a_rescue_and_hotkeys_come_back() {
    let s = booted(&[3, 2, 1]);
    let rescued = update(&s, &Event::RescueEngaged { at: ts() });

    // While rescued, a hotkey is dead; after Engaged, the same key acts again.
    assert!(update(&rescued.state, &hotkey(HotkeyAction::WorkspaceNext))
        .effects
        .is_empty());

    let engaged = update(&rescued.state, &Event::Engaged { at: ts() });
    assert_eq!(engaged.state.mode, Mode::Active);
    assert!(engaged
        .effects
        .contains(&Effect::SetIntercepting { enabled: true }));

    let step = update(&engaged.state, &hotkey(HotkeyAction::WorkspaceNext));
    assert_eq!(count_switches(&step.effects), 1);

    // Engaging an already-active core is a harmless re-assertion, not a reset.
    let again = update(&engaged.state, &Event::Engaged { at: ts() });
    assert_eq!(again.state.mode, Mode::Active);
    assert_eq!(again.state, engaged.state);
}

#[test]
fn demote_banishes_the_focused_window_and_moves_on() {
    // History (front-first): [1, 2, 3], focused 1.
    let s = booted(&[3, 2, 1]);
    let step = update(&s, &hotkey(HotkeyAction::MruDemote));

    // Focus moves to the next MRU window, mouse in tow…
    assert_eq!(focus_targets(&step.effects), vec![wid(2)]);
    assert!(step
        .effects
        .iter()
        .any(|e| matches!(e, Effect::WarpMouse { .. })));
    // …and the demoted window sits at the very back of the history.
    assert_eq!(step.state.focus_history.iter().last(), Some(wid(1)));

    // Once the world confirms focus on 2, Alt+Tab toggles 2 <-> 3: the
    // demoted window stopped being offered.
    let confirmed = update(
        &step.state,
        &observed(
            vec![mon_a(1), mon_b(1)],
            std_windows(),
            Some(2),
            RescanTrigger::Periodic,
        ),
    );
    let toggle = update(&confirmed.state, &hotkey(HotkeyAction::MruWorkspace));
    assert_eq!(focus_targets(&toggle.effects), vec![wid(3)]);
}

#[test]
fn demote_with_nowhere_to_go_does_nothing() {
    // Only one window in the whole workspace: demoting it would be futile —
    // it stays focused and the next scan would re-front it anyway.
    let mut s = booted(&[1]);
    s.windows.retain(|w, _| *w == wid(1));
    s.focus_history = {
        let mut h = ordo_core::FocusHistory::new();
        h.touch(wid(1));
        h
    };
    let step = update(&s, &hotkey(HotkeyAction::MruDemote));
    assert!(step.effects.is_empty());
    assert_eq!(step.state, s);
}

// --- move to other monitor -----------------------------------------------------

#[test]
fn move_focused_window_to_other_monitor_brings_the_mouse() {
    let s = booted(&[1]); // focused w1 on monitor A
    let step = update(&s, &hotkey(HotkeyAction::MoveFocusedToOtherMonitor));
    let frame = set_frame_for(&step.effects, 1).expect("frame effect");
    assert!(
        frame.x >= 1920.0 && frame.x + frame.w <= 3840.0,
        "landed on monitor B: {frame:?}"
    );
    let center = frame.center();
    assert!(step
        .effects
        .iter()
        .any(|e| matches!(e, Effect::WarpMouse { to } if *to == center)));
}

// --- review fixes --------------------------------------------------------------

fn count_moves_to_ws(effects: &[Effect], w: u32) -> usize {
    effects
        .iter()
        .filter(|e| matches!(e, Effect::MoveWindowToWorkspace { window, .. } if *window == wid(w)))
        .count()
}

fn count_set_frames(effects: &[Effect], w: u32) -> usize {
    effects
        .iter()
        .filter(|e| matches!(e, Effect::SetWindowFrame { window, .. } if *window == wid(w)))
        .count()
}

#[test]
fn corral_leaves_previously_missed_same_app_windows_alone() {
    // F2: a full rescan reports every unmodeled window as "created". Two windows
    // of the same app surface at once — w9 is the genuinely new one that took
    // focus, w8 was merely missed (sitting on another workspace, unfocused).
    // Only the focused newcomer should be corralled.
    let s = booted(&[1]); // anchor: workspace 1, monitor A
    let mut wins = std_windows();
    wins.push(win(8, 300, 2, rect(2100.0, 400.0))); // same pid, not focused
    wins.push(win(9, 300, 2, rect(2200.0, 300.0))); // same pid, focused (new)
    let obs = update(
        &s,
        &observed(
            vec![mon_a(1), mon_b(1)],
            wins,
            Some(9),
            RescanTrigger::AxHint {
                pid: Some(Pid(300)),
                kind: AxHintKind::WindowCreated,
            },
        ),
    );
    assert!(count_moves_to_ws(&obs.effects, 9) >= 1, "w9 corralled");
    assert_eq!(count_moves_to_ws(&obs.effects, 8), 0, "w8 left alone");
    assert_eq!(count_set_frames(&obs.effects, 8), 0, "w8 not reframed");
}

#[test]
fn tear_realign_skips_workspaces_no_display_can_reach() {
    // F3: monitor A has 5 spaces and sits on space 5; monitor B has only 3.
    // The world is torn, but no display can reach workspace 5, so Ordo must not
    // fire a futile switch.
    let mon_a5 = MonitorSnap {
        id: mid(1),
        frame: Rect {
            x: 0.0,
            y: 0.0,
            w: 1920.0,
            h: 1080.0,
        },
        is_main: true,
        active_workspace: ws(5),
        workspace_count: 5,
    };
    let mon_b3 = MonitorSnap {
        id: mid(2),
        frame: Rect {
            x: 1920.0,
            y: 0.0,
            w: 1920.0,
            h: 1080.0,
        },
        is_main: false,
        active_workspace: ws(1),
        workspace_count: 3,
    };
    let obs = update(
        &State::new(),
        &Event::WorldObserved {
            at: ts(),
            trigger: RescanTrigger::Startup,
            snap: WorldSnapshot {
                monitors: vec![mon_a5, mon_b3],
                windows: vec![win(1, 100, 5, rect(100.0, 100.0))],
                focused: Some(wid(1)),
            },
        },
    );
    assert!(obs.state.is_torn());
    assert_eq!(obs.state.current_workspace(), Some(ws(5)));
    assert!(
        count_switches(&obs.effects) == 0,
        "no realign toward an unreachable workspace"
    );
}

#[test]
fn damping_budgets_are_independent_per_axis() {
    // F4: a new focused window is wrong on BOTH workspace and monitor, and the
    // app resists both. Each axis must get its own full retry budget (initial +
    // 2 retries = 3), i.e. 6 correctives total — impossible if the two shared
    // one counter.
    let s = booted(&[1]); // anchor workspace 1, monitor A
    let mut wins = std_windows();
    wins.push(win(9, 300, 2, rect(2100.0, 300.0))); // wrong ws (2) AND wrong monitor (B)
    let created = observed(
        vec![mon_a(1), mon_b(1)],
        wins.clone(),
        Some(9),
        RescanTrigger::AxHint {
            pid: Some(Pid(300)),
            kind: AxHintKind::WindowCreated,
        },
    );
    let mut step = update(&s, &created);
    let mut ws_moves = count_moves_to_ws(&step.effects, 9);
    let mut frame_sets = count_set_frames(&step.effects, 9);
    let mut diverged = false;
    for _ in 0..25 {
        let next = update(
            &step.state,
            &observed(
                vec![mon_a(1), mon_b(1)],
                wins.clone(),
                Some(9),
                RescanTrigger::Periodic,
            ),
        );
        ws_moves += count_moves_to_ws(&next.effects, 9);
        frame_sets += count_set_frames(&next.effects, 9);
        diverged |= next.notes.contains(&Note::Diverged { window: wid(9) });
        step = next;
    }
    assert_eq!(ws_moves, 3, "workspace axis: initial + 2 retries");
    assert_eq!(frame_sets, 3, "frame axis: initial + 2 retries");
    assert!(diverged);
}

// --- replay & determinism -----------------------------------------------------

#[test]
fn event_stream_replays_identically_through_serde() {
    let events: Vec<Event> = vec![
        observed(
            vec![mon_a(1), mon_b(1)],
            std_windows(),
            Some(3),
            RescanTrigger::Startup,
        ),
        observed(
            vec![mon_a(1), mon_b(1)],
            std_windows(),
            Some(2),
            RescanTrigger::Periodic,
        ),
        observed(
            vec![mon_a(1), mon_b(1)],
            std_windows(),
            Some(1),
            RescanTrigger::Periodic,
        ),
        hotkey(HotkeyAction::MruWorkspace), // op 1
        observed(
            vec![mon_a(1), mon_b(1)],
            std_windows(),
            Some(2),
            RescanTrigger::PostEffect { op: OpId(1) },
        ),
        hotkey(HotkeyAction::WorkspaceNext), // op 2
        // Mid-switch tear: monitor B hasn't landed yet. The in-flight op's
        // expectation must keep the realigner quiet.
        observed(
            vec![mon_a(2), mon_b(1)],
            std_windows(),
            Some(2),
            RescanTrigger::PostEffect { op: OpId(2) },
        ),
        observed(
            vec![mon_a(2), mon_b(2)],
            std_windows(),
            Some(2),
            RescanTrigger::Periodic,
        ),
    ];

    fn run(events: &[Event]) -> (State, Vec<Effect>, Vec<Note>) {
        let mut s = State::new();
        let mut effects = Vec::new();
        let mut notes = Vec::new();
        for e in events {
            let step = update(&s, e);
            s = step.state;
            effects.extend(step.effects);
            notes.extend(step.notes);
        }
        (s, effects, notes)
    }

    let json = serde_json::to_string(&events).unwrap();
    let roundtripped: Vec<Event> = serde_json::from_str(&json).unwrap();

    let (s1, e1, n1) = run(&events);
    let (s2, e2, n2) = run(&roundtripped);
    assert_eq!(s1, s2);
    assert_eq!(e1, e2);
    assert_eq!(n1, n2);
    assert!(!e1.is_empty());
    assert_eq!(count_switches(&e1), 1, "the tear guard held");

    // Checkpoints are serialized State: it must round-trip exactly.
    let state_json = serde_json::to_string(&s1).unwrap();
    let restored: State = serde_json::from_str(&state_json).unwrap();
    assert_eq!(s1, restored);
}

#[test]
fn update_is_a_pure_function() {
    let s = booted(&[3, 2, 1]);
    let e = hotkey(HotkeyAction::MruWorkspace);
    assert_eq!(update(&s, &e), update(&s, &e));
}
