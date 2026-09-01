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

/// A monitor observation plus the backend's workspace word for it, kept
/// together so fixtures read like one record; `world()` routes each half onto
/// its own snapshot channel.
#[derive(Clone)]
struct Mon {
    snap: MonitorSnap,
    active: WorkspaceId,
    count: u8,
}

/// Same pairing for a window: the observation plus its assignment.
#[derive(Clone)]
struct Win {
    snap: WindowSnap,
    workspace: WorkspaceId,
}

fn mon_a(active: u8) -> Mon {
    Mon {
        snap: MonitorSnap {
            id: mid(1),
            frame: Rect {
                x: 0.0,
                y: 0.0,
                w: 1920.0,
                h: 1080.0,
            },
            is_main: true,
        },
        active: ws(active),
        count: 3,
    }
}

fn mon_b(active: u8) -> Mon {
    Mon {
        snap: MonitorSnap {
            id: mid(2),
            frame: Rect {
                x: 1920.0,
                y: 0.0,
                w: 1920.0,
                h: 1080.0,
            },
            is_main: false,
        },
        active: ws(active),
        count: 3,
    }
}

fn win(id: u32, pid: i32, workspace: u8, frame: Rect) -> Win {
    Win {
        snap: WindowSnap {
            id: wid(id),
            app: Pid(pid),
            bundle_id: None,
            title: format!("w{id}"),
            frame,
            subrole: None,
        },
        workspace: ws(workspace),
    }
}

fn std_windows() -> Vec<Win> {
    vec![
        win(1, 100, 1, rect(100.0, 100.0)),
        win(2, 200, 1, rect(2000.0, 100.0)),
        win(3, 100, 1, rect(600.0, 500.0)),
    ]
}

fn world(monitors: &[Mon], windows: &[Win], focused: Option<u32>) -> WorldSnapshot {
    WorldSnapshot {
        monitors: monitors.iter().map(|m| m.snap.clone()).collect(),
        windows: windows.iter().map(|w| w.snap.clone()).collect(),
        focused: focused.map(wid),
        workspaces: WorkspaceSnap {
            monitors: monitors
                .iter()
                .map(|m| {
                    (
                        m.snap.id,
                        MonitorWs {
                            active: m.active,
                            count: m.count,
                        },
                    )
                })
                .collect(),
            assignments: windows.iter().map(|w| (w.snap.id, w.workspace)).collect(),
        },
    }
}

fn observed(
    monitors: Vec<Mon>,
    windows: Vec<Win>,
    focused: Option<u32>,
    trigger: RescanTrigger,
) -> Event {
    Event::WorldObserved {
        at: ts(),
        trigger,
        snap: world(&monitors, &windows, focused),
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

#[test]
fn an_unresolved_workspace_is_unknown_not_a_fact() {
    // The workspace layer travels on its own channel, and absence there means
    // UNKNOWN: belief keeps what it had, and nothing is fabricated. (The old
    // single-record snapshot defaulted unresolved windows to workspace 1 —
    // an unknown laundered into a "fact" that then rewrote declarations.)
    // Move the world to workspace 2 first, so "kept" is distinguishable from
    // the old fabrication default (workspace 1).
    let s = booted(&[1]);
    let mut wins = std_windows();
    wins[1].workspace = ws(2);
    let s = update(
        &s,
        &observed(
            vec![mon_a(2), mon_b(2)],
            wins.clone(),
            Some(2),
            RescanTrigger::Periodic,
        ),
    )
    .state;

    // This scan, the backend has no word on w2, none on monitor B, and none
    // on the never-before-seen w9.
    wins.push(win(9, 300, 2, rect(700.0, 100.0)));
    let mut snap = world(&[mon_a(2), mon_b(2)], &wins, Some(2));
    snap.workspaces.assignments.remove(&wid(2));
    snap.workspaces.assignments.remove(&wid(9));
    snap.workspaces.monitors.remove(&mid(2));
    let obs = update(
        &s,
        &Event::WorldObserved {
            at: ts(),
            trigger: RescanTrigger::Periodic,
            snap,
        },
    );

    // w2 keeps its workspace; the gap is not read as a change (and is not
    // defaulted back to workspace 1).
    assert_eq!(obs.state.windows[&wid(2)].workspace, ws(2));
    // Monitor B keeps its last known active workspace.
    assert_eq!(obs.state.monitor_ws[&mid(2)], ws(2));
    // w9 stays out of the model until the backend can place it.
    assert!(!obs.state.windows.contains_key(&wid(9)));
    assert!(
        obs.notes
            .iter()
            .all(|n| !matches!(n, Note::External { .. })),
        "unknowns produced no external deltas: {:?}",
        obs.notes
    );
    assert!(obs.effects.is_empty());
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
fn ctrl_alt_tab_jumps_to_the_mru_window_on_the_other_monitor() {
    let s = booted(&[3, 2, 1]); // focused w1 on A; w2 lives on B, w3 on A
    let step = update(&s, &hotkey(HotkeyAction::MruOtherMonitor));
    assert_eq!(focus_targets(&step.effects), vec![wid(2)]);
    // The mouse crosses over with the focus, to w2's center.
    assert!(step
        .effects
        .iter()
        .any(|e| matches!(e, Effect::WarpMouse { to } if to.x == 2200.0 && to.y == 250.0)));

    // With every window on the focused monitor there's nowhere to jump.
    let all_on_a = vec![
        win(1, 100, 1, rect(100.0, 100.0)),
        win(2, 200, 1, rect(500.0, 100.0)), // w2 moved over to A
        win(3, 100, 1, rect(600.0, 500.0)),
    ];
    let same_side = update(
        &booted(&[1]),
        &observed(
            vec![mon_a(1), mon_b(1)],
            all_on_a,
            Some(1),
            RescanTrigger::Periodic,
        ),
    )
    .state;
    assert!(update(&same_side, &hotkey(HotkeyAction::MruOtherMonitor))
        .effects
        .is_empty());
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
fn switching_hands_focus_to_the_destinations_mru_window() {
    // w2 lives on workspace 2; it should be focused BEFORE the switch lands
    // (typing must never keep flowing into a freshly parked window).
    let mut wins = std_windows();
    wins[1].workspace = ws(2);
    let s = update(
        &booted(&[2, 1]),
        &observed(
            vec![mon_a(1), mon_b(1)],
            wins,
            Some(1),
            RescanTrigger::Periodic,
        ),
    )
    .state;

    let step = update(&s, &hotkey(HotkeyAction::WorkspaceNext));
    let focus_pos = step
        .effects
        .iter()
        .position(|e| matches!(e, Effect::FocusWindow { window, .. } if *window == wid(2)))
        .expect("focus effect");
    let switch_pos = step
        .effects
        .iter()
        .position(|e| matches!(e, Effect::SwitchWorkspace { target, .. } if *target == ws(2)))
        .expect("switch effect");
    assert!(focus_pos < switch_pos);
    assert_eq!(step.state.pending.len(), 2, "focus + switch both expected");
    // No warp: the core's frame belief for a parked window is its sliver.
    assert!(!step
        .effects
        .iter()
        .any(|e| matches!(e, Effect::WarpMouse { .. })));
}

#[test]
fn switching_restacks_the_destination_by_mru() {
    // w2 and w3 both live on workspace 2; w3 was focused more recently, so
    // the switch must reassert w3-on-top — stacking IS the MRU order.
    let mut wins = std_windows();
    wins[1].workspace = ws(2);
    wins[2].workspace = ws(2);
    let s = update(
        &booted(&[2, 3, 1]), // history: [1, 3, 2]
        &observed(
            vec![mon_a(1), mon_b(1)],
            wins,
            Some(1),
            RescanTrigger::Periodic,
        ),
    )
    .state;

    let step = update(&s, &hotkey(HotkeyAction::WorkspaceNext));
    let restack = step
        .effects
        .iter()
        .find_map(|e| match e {
            Effect::RestackWindows { order } => Some(order.clone()),
            _ => None,
        })
        .expect("restack effect");
    assert_eq!(restack, vec![wid(3), wid(2)]);
    let switch_pos = step
        .effects
        .iter()
        .position(|e| matches!(e, Effect::SwitchWorkspace { .. }))
        .unwrap();
    let restack_pos = step
        .effects
        .iter()
        .position(|e| matches!(e, Effect::RestackWindows { .. }))
        .unwrap();
    assert!(
        switch_pos < restack_pos,
        "restack lands after the reveal it orders"
    );
}

#[test]
fn round_trip_through_empty_workspace_refocuses_the_same_window() {
    // Leaving for an empty workspace never moves focus (parking doesn't
    // defocus), so coming back must re-focus the SAME window — and the focus
    // target must be the restack's head, or the reassert's physics are
    // unsatisfiable (the key window can't be ordered underneath anything).
    // The regression: alt-tab's skip-the-focused selection here focused the
    // second MRU window on every return, flip-flopping the top window.
    let s = booted(&[2, 3, 1]); // history: [1, 3, 2], all on ws 1
    let away = update(&s, &hotkey(HotkeyAction::WorkspaceNext));
    assert!(
        focus_targets(&away.effects).is_empty(),
        "empty destination: nobody to focus"
    );
    let parked = update(
        &away.state,
        &observed(
            vec![mon_a(2), mon_b(2)],
            std_windows(),
            Some(1),
            RescanTrigger::PostEffect { op: OpId(1) },
        ),
    )
    .state;

    let back = update(&parked, &hotkey(HotkeyAction::WorkspacePrev));
    let restack = back
        .effects
        .iter()
        .find_map(|e| match e {
            Effect::RestackWindows { order } => Some(order.clone()),
            _ => None,
        })
        .expect("restack effect");
    assert_eq!(focus_targets(&back.effects), vec![wid(1)]);
    assert_eq!(restack, vec![wid(1), wid(3), wid(2)]);
}

#[test]
fn external_focus_on_a_hidden_workspaces_window_is_followed() {
    // The user Cmd+Tabbed to w2, which is parked on workspace 2: Ordo brings
    // workspace 2 over, mirroring native Spaces.
    let mut wins = std_windows();
    wins[1].workspace = ws(2);
    let step = update(
        &booted(&[1]),
        &observed(
            vec![mon_a(1), mon_b(1)],
            wins,
            Some(2),
            RescanTrigger::Periodic,
        ),
    );
    assert!(step
        .effects
        .iter()
        .any(|e| matches!(e, Effect::SwitchWorkspace { target, .. } if *target == ws(2))));
    assert!(step
        .notes
        .iter()
        .any(|n| matches!(n, Note::FollowedFocus { window, target }
            if *window == wid(2) && *target == ws(2))));

    // But a focus change we caused ourselves is not followed: mid-switch,
    // focus lands on the destination window while the monitors still show
    // the old workspace — that's our own op's echo, not a Cmd+Tab.
    let mut wins = std_windows();
    wins[1].workspace = ws(2);
    let mid = update(
        &booted(&[2, 1]),
        &observed(
            vec![mon_a(1), mon_b(1)],
            wins.clone(),
            Some(1),
            RescanTrigger::Periodic,
        ),
    )
    .state;
    let issued = update(&mid, &hotkey(HotkeyAction::WorkspaceNext));
    let obs = update(
        &issued.state,
        &observed(
            vec![mon_a(1), mon_b(1)],
            wins,
            Some(2),
            RescanTrigger::PostEffect { op: OpId(2) },
        ),
    );
    assert_eq!(count_switches(&obs.effects), 0, "no follow of our own echo");
}

#[test]
fn late_focus_echo_inside_the_settle_window_is_held_not_followed() {
    // Run 38's snap-back, replayed: rapid switching leaves focus grants in
    // flight, and an app can land (or duplicate) one seconds later — after
    // the switch and its focus expectation have both confirmed and cleared.
    // Inside the settle window that arrival is our own echo: hold the
    // workspace, pull focus back. Past the window it's the user navigating.
    let at = |mono_ns: u64| Ts {
        wall_ms: 0,
        mono_ns,
    };
    let obs = |mono_ns: u64, active: u8, wins: Vec<Win>, focused: u32| Event::WorldObserved {
        at: at(mono_ns),
        trigger: RescanTrigger::Periodic,
        snap: world(&[mon_a(active), mon_b(active)], &wins, Some(focused)),
    };
    let mut wins = std_windows();
    wins[1].workspace = ws(2); // w2 lives on workspace 2

    // On ws1 focused w1; switch to ws2 (grants focus to w2)...
    let s = update(&booted(&[2, 1]), &obs(0, 1, wins.clone(), 1)).state;
    let s = update(
        &s,
        &Event::Hotkey {
            at: at(1_000_000_000),
            action: HotkeyAction::WorkspaceNext,
        },
    )
    .state;
    // ...and the very next snapshot confirms everything: monitors on ws2,
    // focus on w2. All expectations resolve; the old guards are now down.
    let s = update(&s, &obs(1_200_000_000, 2, wins.clone(), 2)).state;

    // 500ms after the switch, the stale grant echoes: focus pops back to w1
    // on hidden ws1. Held, not followed.
    let held = update(&s, &obs(1_500_000_000, 2, wins.clone(), 1));
    assert_eq!(count_switches(&held.effects), 0, "no snap-back");
    assert_eq!(
        focus_targets(&held.effects),
        vec![wid(2)],
        "focus pulled back to the current workspace's MRU window"
    );
    assert!(held
        .notes
        .iter()
        .any(|n| matches!(n, Note::HeldFocusSettling { window } if *window == wid(2))));

    // The pull-back confirms; well past the settle window the same focus
    // change is genuine navigation (Cmd+Tab) and is followed.
    let s = update(&held.state, &obs(1_600_000_000, 2, wins.clone(), 2)).state;
    let followed = update(&s, &obs(4_000_000_000, 2, wins, 1));
    assert!(followed
        .effects
        .iter()
        .any(|e| matches!(e, Effect::SwitchWorkspace { target, .. } if *target == ws(1))));
    assert!(followed
        .notes
        .iter()
        .any(|n| matches!(n, Note::FollowedFocus { window, target }
            if *window == wid(1) && *target == ws(1))));
}

#[test]
fn closing_a_window_never_follows_focus_to_another_workspace() {
    // Closing a window makes macOS hand focus to the app's next window,
    // wherever it lives — including a hidden workspace. That's fallout, not
    // navigation: the user asked to close something, not to go somewhere.
    // Hold the workspace and pull focus back to its MRU window instead.
    let mut wins = std_windows();
    wins[1].workspace = ws(2); // w2 parked on workspace 2
    let s = update(
        &booted(&[2, 3, 1]), // history: [1, 3, 2]; focused w1
        &observed(
            vec![mon_a(1), mon_b(1)],
            wins.clone(),
            Some(1),
            RescanTrigger::Periodic,
        ),
    )
    .state;

    // w3 closes; macOS gives focus to w2 (on hidden workspace 2).
    wins.retain(|w| w.snap.id != wid(3));
    let step = update(
        &s,
        &observed(
            vec![mon_a(1), mon_b(1)],
            wins,
            Some(2),
            RescanTrigger::AxHint {
                pid: Some(Pid(100)),
                kind: AxHintKind::Other("AXFocusedWindowChanged".into()),
            },
        ),
    );
    assert_eq!(count_switches(&step.effects), 0, "no yank on close");
    assert_eq!(
        focus_targets(&step.effects),
        vec![wid(1)],
        "focus returns to this workspace's MRU window"
    );
    assert!(step
        .notes
        .iter()
        .any(|n| matches!(n, Note::HeldFocusOnClose { window } if *window == wid(1))));
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

    // It's buried visually too — the restack order is the demoted MRU, so w1
    // comes last — and only after the focus handoff, because raises land
    // below the key window.
    let lower_pos = step
        .effects
        .iter()
        .position(
            |e| matches!(e, Effect::RestackWindows { order } if order.last() == Some(&wid(1))),
        )
        .expect("restack effect with w1 at the back");
    let focus_pos = step
        .effects
        .iter()
        .position(|e| matches!(e, Effect::FocusWindow { .. }))
        .unwrap();
    assert!(focus_pos < lower_pos);

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

#[test]
fn clamped_landing_on_the_target_monitor_confirms_the_move() {
    // macOS clamps frames into a display's visible area (the requested y at
    // the top of monitor B's bounds lands a menu-bar-height lower). The op's
    // intent is the MONITOR, so the clamped landing must confirm it — the
    // old exact-rect check could never be satisfied and re-asserted the
    // doomed frame until damping, fighting the user the whole way.
    let s = booted(&[1]);
    let step = update(&s, &hotkey(HotkeyAction::MoveFocusedToOtherMonitor));
    let f = set_frame_for(&step.effects, 1).expect("frame effect");

    let mut wins = std_windows();
    wins[0].snap.frame = Rect { y: f.y + 33.0, ..f }; // clamped below the menu bar
    let landed = update(
        &step.state,
        &observed(
            vec![mon_a(1), mon_b(1)],
            wins.clone(),
            Some(1),
            RescanTrigger::Periodic,
        ),
    );
    assert!(
        landed
            .notes
            .iter()
            .any(|n| matches!(n, Note::SelfConfirmed { .. })),
        "clamped landing is our own echo: {:?}",
        landed.notes
    );
    let next = update(
        &landed.state,
        &observed(
            vec![mon_a(1), mon_b(1)],
            wins,
            Some(1),
            RescanTrigger::Periodic,
        ),
    );
    assert_eq!(
        count_set_frames(&next.effects, 1),
        0,
        "no retry against the clamp"
    );
}

#[test]
fn expired_placement_yields_to_a_window_being_dragged() {
    // A retry that fires while the user is dragging the window teleports it
    // out of their hand. An unexplained frame change in the expiry snapshot
    // means someone is actively placing the window: the op stays lost.
    let s = booted(&[1]);
    let mut wins = std_windows();
    wins.push(win(9, 300, 1, rect(2100.0, 300.0))); // new window on monitor B
    let mut step = update(
        &s,
        &observed(
            vec![mon_a(1), mon_b(1)],
            wins.clone(),
            Some(9),
            RescanTrigger::AxHint {
                pid: Some(Pid(300)),
                kind: AxHintKind::WindowCreated,
            },
        ),
    );
    assert!(
        set_frame_for(&step.effects, 9).is_some(),
        "corral places the new window on the focused monitor"
    );
    // The corral never sticks (the snapshot keeps the old frame) …
    for _ in 0..2 {
        step = update(
            &step.state,
            &observed(
                vec![mon_a(1), mon_b(1)],
                wins.clone(),
                Some(9),
                RescanTrigger::Periodic,
            ),
        );
    }
    // … and in the expiry snapshot the user is dragging it (still on B).
    let mut dragged = std_windows();
    dragged.push(win(9, 300, 1, rect(2400.0, 500.0)));
    let expiry = update(
        &step.state,
        &observed(
            vec![mon_a(1), mon_b(1)],
            dragged,
            Some(9),
            RescanTrigger::Periodic,
        ),
    );
    assert!(
        expiry
            .notes
            .iter()
            .any(|n| matches!(n, Note::OpLost { .. })),
        "op expires: {:?}",
        expiry.notes
    );
    assert_eq!(
        count_set_frames(&expiry.effects, 9),
        0,
        "no retry while the user's hands are on the window"
    );
}

#[test]
fn carry_moves_the_focused_window_and_switches_with_it() {
    let s = booted(&[1]);
    let step = update(&s, &hotkey(HotkeyAction::CarryFocusedToWorkspaceNext));

    // The window is reassigned, then the view follows: assignment (never a
    // frame-touching move — the carried window must not park) before switch.
    let kinds: Vec<&Effect> = step.effects.iter().collect();
    let move_pos = kinds
        .iter()
        .position(|e| {
            matches!(e, Effect::AssignWindowToWorkspace { window, target, .. }
                if *window == wid(1) && *target == ws(2))
        })
        .expect("assign effect");
    let switch_pos = kinds
        .iter()
        .position(|e| matches!(e, Effect::SwitchWorkspace { target, .. } if *target == ws(2)))
        .expect("switch effect");
    assert!(move_pos < switch_pos, "reassign before the view follows");

    // The window stays put on screen and keeps focus: no frame write, no
    // focus effect, no mouse warp.
    assert_eq!(count_set_frames(&step.effects, 1), 0);
    assert!(focus_targets(&step.effects).is_empty());
    assert!(!step
        .effects
        .iter()
        .any(|e| matches!(e, Effect::WarpMouse { .. })));
    assert_eq!(step.state.pending.len(), 2, "move + switch both expected");

    // Both expectations confirm from one snapshot of the settled world.
    let settled = vec![
        win(1, 100, 2, rect(100.0, 100.0)),
        win(2, 200, 1, rect(2000.0, 100.0)),
        win(3, 100, 1, rect(600.0, 500.0)),
    ];
    let obs = update(
        &step.state,
        &observed(
            vec![mon_a(2), mon_b(2)],
            settled,
            Some(1),
            RescanTrigger::PostEffect { op: OpId(2) },
        ),
    );
    assert!(obs.state.pending.is_empty());
    assert!(!obs.notes.iter().any(|n| matches!(n, Note::External { .. })));
}

#[test]
fn a_carry_mid_handoff_takes_the_granted_window_not_the_stale_focus() {
    // Run 38 seq 20447: Alt+Tab issued a focus grant, the user carried before
    // the echo arrived, and the carry read observation-lagged focus — moving
    // the PREVIOUS window instead of the one visibly focused. Commands must
    // read the declared focus.
    let s = booted(&[3, 2, 1]); // history [1, 2, 3], observed focus w1
    let step = update(&s, &hotkey(HotkeyAction::MruWorkspace)); // grant -> w2
    assert_eq!(focus_targets(&step.effects), vec![wid(2)]);

    // No snapshot yet: observation still says w1. Carry anyway.
    let carried = update(
        &step.state,
        &hotkey(HotkeyAction::CarryFocusedToWorkspaceNext),
    );
    assert!(
        carried.effects.iter().any(|e| {
            matches!(e, Effect::AssignWindowToWorkspace { window, target, .. }
                if *window == wid(2) && *target == ws(2))
        }),
        "carried the granted window, not the stale one: {:?}",
        carried.effects
    );
}

#[test]
fn carry_at_the_edge_or_with_nothing_focused_does_nothing() {
    let s = booted(&[1]); // on workspace 1: prev is clamped
    assert!(
        update(&s, &hotkey(HotkeyAction::CarryFocusedToWorkspacePrev))
            .effects
            .is_empty()
    );

    let unfocused = booted(&[]);
    assert!(update(
        &unfocused,
        &hotkey(HotkeyAction::CarryFocusedToWorkspaceNext)
    )
    .effects
    .is_empty());
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
    let mon_a5 = Mon {
        active: ws(5),
        count: 5,
        ..mon_a(1)
    };
    let mon_b3 = Mon {
        active: ws(1),
        count: 3,
        ..mon_b(1)
    };
    let obs = update(
        &State::new(),
        &observed(
            vec![mon_a5, mon_b3],
            vec![win(1, 100, 5, rect(100.0, 100.0))],
            Some(1),
            RescanTrigger::Startup,
        ),
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

// --- hotkey coalescing ------------------------------------------------------
// A queued burst of presses is one user gesture: runs of Prev/Next fold into
// a single direct jump, walked with clamping (net arithmetic is wrong at the
// edges), and a net-zero bounce vanishes entirely.

#[test]
fn coalescing_folds_a_run_into_one_direct_jump() {
    let s = booted(&[1]);
    let folded = coalesce_hotkeys(
        &s,
        &[HotkeyAction::WorkspaceNext, HotkeyAction::WorkspaceNext],
    );
    assert_eq!(folded, vec![HotkeyAction::WorkspaceSwitchTo(ws(3))]);

    let fx = update(&s, &hotkey(HotkeyAction::WorkspaceSwitchTo(ws(3)))).effects;
    assert_eq!(count_switches(&fx), 1);
    assert!(fx
        .iter()
        .any(|e| matches!(e, Effect::SwitchWorkspace { target, .. } if *target == ws(3))));
}

#[test]
fn coalescing_annihilates_a_bounce() {
    let s = booted(&[1]);
    let folded = coalesce_hotkeys(
        &s,
        &[HotkeyAction::WorkspaceNext, HotkeyAction::WorkspacePrev],
    );
    assert_eq!(folded, Vec::new());
}

#[test]
fn coalescing_walks_the_clamped_edges_instead_of_summing() {
    // From ws1 with 3 workspaces: Next,Next,Next,Prev walks 2,3,3(clamped),2.
    // Net arithmetic (+3-1 from 1) would land on 3 — the walk must not.
    let s = booted(&[1]);
    let folded = coalesce_hotkeys(
        &s,
        &[
            HotkeyAction::WorkspaceNext,
            HotkeyAction::WorkspaceNext,
            HotkeyAction::WorkspaceNext,
            HotkeyAction::WorkspacePrev,
        ],
    );
    assert_eq!(folded, vec![HotkeyAction::WorkspaceSwitchTo(ws(2))]);
}

#[test]
fn coalescing_passes_single_and_fenced_actions_through() {
    let s = booted(&[1]);
    // A lone press keeps its exact shape (the common unqueued case).
    assert_eq!(
        coalesce_hotkeys(&s, &[HotkeyAction::WorkspaceNext]),
        vec![HotkeyAction::WorkspaceNext]
    );
    // A non-switch action fences the fold on both sides, order preserved.
    let folded = coalesce_hotkeys(
        &s,
        &[
            HotkeyAction::WorkspaceNext,
            HotkeyAction::MruWorkspace,
            HotkeyAction::WorkspaceNext,
        ],
    );
    assert_eq!(
        folded,
        vec![
            HotkeyAction::WorkspaceSwitchTo(ws(2)),
            HotkeyAction::MruWorkspace,
            HotkeyAction::WorkspaceSwitchTo(ws(3)),
        ]
    );
}

#[test]
fn direct_jump_ignores_the_current_and_out_of_range_workspaces() {
    let s = booted(&[1]);
    assert_eq!(
        update(&s, &hotkey(HotkeyAction::WorkspaceSwitchTo(ws(1)))).effects,
        Vec::new()
    );
    assert_eq!(
        update(&s, &hotkey(HotkeyAction::WorkspaceSwitchTo(ws(9)))).effects,
        Vec::new()
    );
}

#[test]
fn follow_the_focus_holds_while_a_focus_handoff_is_in_flight() {
    // w2 lives on ws2; everything else on ws1. Get w2 into the MRU history
    // legitimately (focused while ws2 was up), then return to ws1.
    let wins = || {
        vec![
            win(1, 100, 1, rect(10.0, 10.0)),
            win(2, 200, 2, rect(500.0, 10.0)),
            win(3, 100, 1, rect(900.0, 10.0)),
        ]
    };
    let mut s = update(
        &State::new(),
        &observed(
            vec![mon_a(1), mon_b(1)],
            wins(),
            Some(3),
            RescanTrigger::Startup,
        ),
    )
    .state;
    for (active, focused) in [(2, 2), (1, 1)] {
        s = update(
            &s,
            &observed(
                vec![mon_a(active), mon_b(active)],
                wins(),
                Some(focused),
                RescanTrigger::Periodic,
            ),
        )
        .state;
    }

    // Switch toward ws2: mints a Focused(w2) handoff that macOS will land on
    // w2's app's own schedule.
    let step = update(&s, &hotkey(HotkeyAction::WorkspaceNext));
    assert_eq!(focus_targets(&step.effects), vec![wid(2)]);
    let s = step.state;

    // First snapshot: the switch itself is already confirmed (the emulated
    // backend echoes it instantly) but focus still sits on w1 — the handoff
    // is in flight.
    let s = update(
        &s,
        &observed(
            vec![mon_a(2), mon_b(2)],
            wins(),
            Some(1),
            RescanTrigger::Periodic,
        ),
    )
    .state;

    // Mid-handoff, focus transiently re-keys onto w3 — a hidden-ws1 window
    // (Dock dimming's app-hide can fling focus anywhere). Following it would
    // spontaneously bounce the user back to ws1.
    let step = update(
        &s,
        &observed(
            vec![mon_a(2), mon_b(2)],
            wins(),
            Some(3),
            RescanTrigger::Periodic,
        ),
    );
    assert_eq!(count_switches(&step.effects), 0);
    assert!(!step
        .notes
        .iter()
        .any(|n| matches!(n, Note::FollowedFocus { .. })));
}
