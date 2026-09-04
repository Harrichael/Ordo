//! Every test drives the core the way the shell will: feed events, assert on
//! the emitted effects and resulting state. Internals (diffing, pendings) are
//! exercised only through that surface, so they stay free to change.

use ordo_core::*;

// --- fixtures -------------------------------------------------------------
// Two 1920x1080 monitors side by side, three workspaces, three windows:
//   w1 (pid 100) and w3 (pid 100) on monitor A, w2 (pid 200) on monitor B.

/// Time between one event and the next. Expectations expire by ELAPSED TIME,
/// so a fixture that stamped every event with the same instant would never
/// expire one and every expiry test below would pass vacuously. Per-thread, so
/// each test gets its own timeline no matter how the harness schedules them.
const TICK_MS: u64 = 200;

thread_local! {
    static CLOCK_MS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

fn ts() -> Ts {
    stamp(CLOCK_MS.with(|c| {
        c.set(c.get() + TICK_MS);
        c.get()
    }))
}

fn stamp(ms: u64) -> Ts {
    Ts {
        wall_ms: ms as i64,
        mono_ns: ms * 1_000_000,
    }
}

fn plus_ms(base: Ts, ms: u64) -> Ts {
    stamp(base.wall_ms as u64 + ms)
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

/// Same pairing for a window: the observation plus its assignments. `monitor`
/// None = declared on the virtual monitor of the display its frame sits on,
/// which is what the emulated backend adopts for a window it first meets there.
#[derive(Clone)]
struct Win {
    snap: WindowSnap,
    workspace: WorkspaceId,
    monitor: Option<VirtualMonitorId>,
}

fn vm(n: u8) -> VirtualMonitorId {
    VirtualMonitorId(n)
}

/// Declare a window onto a virtual monitor other than the one its frame implies.
fn on_monitor(mut w: Win, n: u8) -> Win {
    w.monitor = Some(vm(n));
    w
}

/// The layout the emulated backend reports for a rig with one virtual monitor
/// per display: the degenerate, one-to-one projection.
fn full_rig(monitors: &[Mon]) -> VirtualMonitors {
    VirtualMonitors {
        count: monitors.len() as u8,
        viewed: vm(1),
        enabled: true,
    }
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
        monitor: None,
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
    world_view(full_rig(monitors), monitors, windows, focused)
}

/// The virtual monitor a frame's display stands at, left to right.
fn position_of(frame: &Rect, monitors: &[Mon]) -> VirtualMonitorId {
    let mut ms: Vec<&Mon> = monitors.iter().collect();
    ms.sort_by(|a, b| a.snap.frame.x.total_cmp(&b.snap.frame.x));
    let c = frame.center();
    let i = ms
        .iter()
        .position(|m| m.snap.frame.contains(c))
        .unwrap_or(0);
    vm(i as u8 + 1)
}

fn world_view(
    view: VirtualMonitors,
    monitors: &[Mon],
    windows: &[Win],
    focused: Option<u32>,
) -> WorldSnapshot {
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
            virtual_monitors: Some(VirtualMonitorsWord {
                view,
                assignments: windows
                    .iter()
                    .map(|w| {
                        (
                            w.snap.id,
                            w.monitor
                                .unwrap_or_else(|| position_of(&w.snap.frame, monitors)),
                        )
                    })
                    .collect(),
            }),
        },
    }
}

/// `observed` under a virtual-monitor layout other than the full rig.
fn observed_view(
    view: VirtualMonitors,
    monitors: Vec<Mon>,
    windows: Vec<Win>,
    focused: Option<u32>,
    trigger: RescanTrigger,
) -> Event {
    Event::WorldObserved {
        at: ts(),
        trigger,
        snap: world_view(view, &monitors, &windows, focused),
    }
}

fn observed(
    monitors: Vec<Mon>,
    windows: Vec<Win>,
    focused: Option<u32>,
    trigger: RescanTrigger,
) -> Event {
    observed_at(ts(), monitors, windows, focused, trigger)
}

/// `observed` with the timestamp pinned, for tests that care where an
/// observation falls relative to an expectation's lifetime.
fn observed_at(
    at: Ts,
    monitors: Vec<Mon>,
    windows: Vec<Win>,
    focused: Option<u32>,
    trigger: RescanTrigger,
) -> Event {
    Event::WorldObserved {
        at,
        trigger,
        snap: world(&monitors, &windows, focused),
    }
}

fn hotkey(action: HotkeyAction) -> Event {
    Event::Hotkey { at: ts(), action }
}

fn gesture(gesture: Gesture) -> Event {
    Event::Gesture { at: ts(), gesture }
}

fn click(x: f64, y: f64) -> Event {
    gesture(Gesture::MouseDown { at: Point { x, y } })
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
fn a_witnessed_switch_to_a_hidden_window_is_followed_and_an_unwitnessed_one_is_held() {
    // w2 is parked on workspace 2; the user is on workspace 1 with w1.
    let mut wins = std_windows();
    wins[1].workspace = ws(2);
    let s = update(
        &booted(&[1]),
        &observed(
            vec![mon_a(1), mon_b(1)],
            wins.clone(),
            Some(1),
            RescanTrigger::Periodic,
        ),
    )
    .state;
    assert_eq!(
        s.focus_intent(),
        FocusIntent::Deferred,
        "nothing commanded yet"
    );

    // Focus appears on w2 out of nowhere: nobody can type into a parked
    // window, so the visible workspace's MRU window is declared and focus
    // pulled back there. The workspace does not move.
    let held = update(
        &s,
        &observed(
            vec![mon_a(1), mon_b(1)],
            wins.clone(),
            Some(2),
            RescanTrigger::Periodic,
        ),
    );
    assert_eq!(
        count_switches(&held.effects),
        0,
        "a fling is not navigation"
    );
    assert_eq!(focus_targets(&held.effects), vec![wid(1)]);
    assert!(held.notes.contains(&Note::HeldFocus {
        window: wid(1),
        from: wid(2),
        from_app: Pid(200),
    }));
    assert_eq!(held.state.focus_intent(), FocusIntent::Window(wid(1)));

    // The same observation right after Cmd+Tab is the user going to w2:
    // Ordo brings workspace 2 over, as native Spaces would, and w2 heads
    // both the declaration and the restack.
    let cmd_tab = update(&s, &gesture(Gesture::SystemSwitch));
    assert!(cmd_tab.notes.contains(&Note::GestureClassified {
        gesture: Gesture::SystemSwitch,
        armed: true,
        within: None,
    }));
    let after_cmd_tab = cmd_tab.state;
    assert_eq!(after_cmd_tab.focus_intent(), FocusIntent::Deferred);
    let followed = update(
        &after_cmd_tab,
        &observed(
            vec![mon_a(1), mon_b(1)],
            wins,
            Some(2),
            RescanTrigger::Periodic,
        ),
    );
    assert!(followed
        .effects
        .iter()
        .any(|e| matches!(e, Effect::SwitchWorkspace { target, .. } if *target == ws(2))));
    assert!(
        focus_targets(&followed.effects).is_empty(),
        "the user already has the focus they asked for"
    );
    assert!(followed.notes.contains(&Note::FollowedFocus {
        window: wid(2),
        target: ws(2),
        monitor: None,
    }));
    assert_eq!(followed.state.focus_intent(), FocusIntent::Window(wid(2)));
}

#[test]
fn a_click_into_a_visible_window_does_not_license_a_follow_but_a_click_elsewhere_does() {
    // Same world: w2 parked on workspace 2, w1 (100,100 400x300) visible.
    let mut wins = std_windows();
    wins[1].workspace = ws(2);
    let obs = |focused: u32| {
        observed(
            vec![mon_a(1), mon_b(1)],
            wins.clone(),
            Some(focused),
            RescanTrigger::Periodic,
        )
    };
    let s = update(&booted(&[1]), &obs(1)).state;

    // A click INTO w1 keys w1 (or a sheet of it) — so focus turning up on
    // parked w2 afterwards is a fling, not the click's doing.
    let clicked = update(&s, &click(200.0, 200.0));
    assert!(clicked.notes.contains(&Note::GestureClassified {
        gesture: Gesture::MouseDown {
            at: Point { x: 200.0, y: 200.0 }
        },
        armed: false,
        within: Some(wid(1)),
    }));
    let clicked_w1 = clicked.state;
    assert_eq!(clicked_w1.focus_intent(), FocusIntent::Deferred);
    let fling = update(&clicked_w1, &obs(2));
    assert_eq!(count_switches(&fling.effects), 0);
    assert_eq!(focus_targets(&fling.effects), vec![wid(1)]);

    // A click outside every visible window — the Dock, say — can be aimed at
    // anything, and a hidden landing right after it is the user navigating.
    let dock = update(&s, &click(960.0, 1075.0));
    assert!(dock.notes.contains(&Note::GestureClassified {
        gesture: Gesture::MouseDown {
            at: Point {
                x: 960.0,
                y: 1075.0
            }
        },
        armed: true,
        within: None,
    }));
    let clicked_dock = dock.state;
    let followed = update(&clicked_dock, &obs(2));
    assert_eq!(count_switches(&followed.effects), 1);
    assert!(followed
        .notes
        .iter()
        .any(|n| matches!(n, Note::FollowedFocus { .. })));

    // A gesture explains exactly the observation after it. One uneventful
    // observation later, the same landing is a fling again.
    let quiet = update(&clicked_dock, &obs(1)).state;
    let late = update(&quiet, &obs(2));
    assert_eq!(count_switches(&late.effects), 0, "the gesture was spent");
    assert_eq!(focus_targets(&late.effects), vec![wid(1)]);

    // A command in between spends it too: Dock click, then Cmd+Right to the
    // empty workspace 3. Its own post-effect snapshot shows w1 still focused
    // — parking does not defocus — and now hidden. That is the switch's
    // doing, not the click's; bouncing back to workspace 1 would be wrong.
    let switched = update(
        &clicked_dock,
        &hotkey(HotkeyAction::WorkspaceSwitchTo(ws(3))),
    )
    .state;
    let after = update(
        &switched,
        &observed(
            vec![mon_a(3), mon_b(3)],
            wins.clone(),
            Some(1),
            RescanTrigger::PostEffect { op: OpId(1) },
        ),
    );
    assert_eq!(count_switches(&after.effects), 0, "no bounce back");
}

#[test]
fn a_stray_focus_under_a_standing_declaration_is_reasserted_however_late_it_arrives() {
    // Run 38's snap-back, replayed: rapid switching leaves focus grants in
    // flight, and an app can land (or duplicate) one seconds later — after
    // the switch and its focus expectation have both confirmed and cleared.
    // The declaration does not expire with the expectation: the stray focus
    // contradicts it, so it is re-asserted, at 500ms and equally at 3s. Only
    // a witnessed gesture makes such a landing navigation.
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
    assert_eq!(s.focus_intent(), FocusIntent::Window(wid(2)));
    // ...and the very next snapshot confirms everything: monitors on ws2,
    // focus on w2. All expectations resolve.
    let s = update(&s, &obs(1_200_000_000, 2, wins.clone(), 2)).state;
    assert!(s.pending.is_empty());

    // 500ms after the switch, the stale grant echoes: focus pops back to w1
    // on hidden ws1. Re-asserted, not followed.
    let early = update(&s, &obs(1_500_000_000, 2, wins.clone(), 1));
    assert_eq!(count_switches(&early.effects), 0, "no snap-back");
    assert_eq!(focus_targets(&early.effects), vec![wid(2)]);
    assert!(early
        .notes
        .contains(&Note::FocusReasserted { window: wid(2) }));

    // The re-assertion lands. Well past any settle window the same stray
    // landing is still a violation — no gesture, no navigation.
    let s = update(&early.state, &obs(1_600_000_000, 2, wins.clone(), 2)).state;
    let late = update(&s, &obs(4_000_000_000, 2, wins, 1));
    assert_eq!(count_switches(&late.effects), 0);
    assert_eq!(focus_targets(&late.effects), vec![wid(2)]);
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
    assert!(step.notes.contains(&Note::HeldFocus {
        window: wid(1),
        from: wid(2),
        from_app: Pid(200),
    }));

    // The same when the window closing is the DECLARED one: Alt+Tab to w3,
    // confirmed, then Cmd+W. A declaration about a window that no longer
    // exists is vacuous, so the hidden landing is held exactly as above.
    let mut wins = std_windows();
    wins[1].workspace = ws(2);
    let s = update(
        &booted(&[2, 3, 1]),
        &observed(
            vec![mon_a(1), mon_b(1)],
            wins.clone(),
            Some(1),
            RescanTrigger::Periodic,
        ),
    )
    .state;
    let s = update(&s, &hotkey(HotkeyAction::MruWorkspace)).state; // -> w3
    assert_eq!(s.focus_intent(), FocusIntent::Window(wid(3)));
    let s = update(
        &s,
        &observed(
            vec![mon_a(1), mon_b(1)],
            wins.clone(),
            Some(3),
            RescanTrigger::Periodic,
        ),
    )
    .state;
    wins.retain(|w| w.snap.id != wid(3));
    let closed = update(
        &s,
        &observed(
            vec![mon_a(1), mon_b(1)],
            wins,
            Some(2),
            RescanTrigger::Periodic,
        ),
    );
    assert_eq!(count_switches(&closed.effects), 0);
    assert_eq!(focus_targets(&closed.effects), vec![wid(1)]);
    assert_eq!(closed.state.focus_intent(), FocusIntent::Window(wid(1)));
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
    for _ in 0..24 {
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
    assert_eq!(obs.state.focus_intent(), FocusIntent::Window(wid(9)));
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
    // But the focused newcomer IS declared: apps whose observer never
    // attached announce no births, and their new windows must not have
    // focus yanked back to whatever was declared before.
    assert_eq!(obs.state.focus_intent(), FocusIntent::Window(wid(9)));
    assert!(focus_targets(&obs.effects).is_empty());
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
fn a_grant_the_app_answers_slowly_is_confirmed_not_re_issued() {
    // The measured regression: apps accept a focus grant at a median of 398ms
    // (p75 647ms), while a workspace switch's own post-effect rescan plus the
    // burst of accessibility hints it provokes delivered three snapshots
    // inside ~150ms. Counting snapshots, that spent the whole budget before
    // the app could answer — the grant was declared lost and re-issued on
    // roughly one switch in four, doubling how long a switch took to settle.
    // Ordo must simply wait: the same grant, landing at +650ms, is ours.
    let mut wins = std_windows();
    wins[1].workspace = ws(2);
    let s = update(
        &booted(&[2, 1]),
        &observed(
            vec![mon_a(1), mon_b(1)],
            wins.clone(),
            Some(1),
            RescanTrigger::Periodic,
        ),
    )
    .state;

    let issued = ts();
    let mut step = update(
        &s,
        &Event::Hotkey {
            at: issued,
            action: HotkeyAction::WorkspaceNext,
        },
    );
    assert_eq!(focus_targets(&step.effects), vec![wid(2)]);
    let grant = OpId(1);

    // The switch lands; the app has not answered the grant yet. The hint
    // storm the switch itself provokes arrives while it is still thinking.
    let hint = RescanTrigger::AxHint {
        pid: Some(Pid(100)),
        kind: AxHintKind::Other("AXFocusedWindowChanged".into()),
    };
    let mut notes = Vec::new();
    let mut effects = Vec::new();
    for (offset, trigger) in [
        (20, RescanTrigger::PostEffect { op: OpId(2) }),
        (50, hint.clone()),
        (100, hint),
    ] {
        step = update(
            &step.state,
            &observed_at(
                plus_ms(issued, offset),
                vec![mon_a(2), mon_b(2)],
                wins.clone(),
                Some(1),
                trigger,
            ),
        );
        notes.extend(step.notes.clone());
        effects.extend(step.effects.clone());
    }

    let landed = update(
        &step.state,
        &observed_at(
            plus_ms(issued, 650),
            vec![mon_a(2), mon_b(2)],
            wins,
            Some(2),
            RescanTrigger::Periodic,
        ),
    );
    notes.extend(landed.notes.clone());
    effects.extend(landed.effects.clone());

    assert!(
        notes.contains(&Note::SelfConfirmed { op: grant }),
        "the grant we issued is the one that landed: {notes:?}"
    );
    assert!(
        !notes.contains(&Note::OpLost { op: grant }),
        "an app answering at the measured p75 is not a lost op: {notes:?}"
    );
    assert!(
        !notes
            .iter()
            .any(|n| matches!(n, Note::FocusReasserted { .. })),
        "nothing to re-assert while the grant is still in flight: {notes:?}"
    );
    assert_eq!(
        focus_targets(&effects),
        Vec::new(),
        "the grant was not re-issued"
    );
}

#[test]
fn unconfirmed_ops_expire_as_lost() {
    let s = booted(&[1]);
    let mut step = update(&s, &hotkey(HotkeyAction::WorkspaceNext)); // op 1
    let mut lost = false;
    // Long enough for the expectation's TTL to run out at TICK_MS per event.
    for _ in 0..5 {
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
    let step = update(&s, &hotkey(HotkeyAction::MoveFocusedToMonitorNext));
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
    let step = update(&s, &hotkey(HotkeyAction::MoveFocusedToMonitorNext));
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
    for _ in 0..4 {
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
fn a_carry_rides_on_top_of_the_destination_and_survives_a_fling_to_a_sibling() {
    // w2 and w3 live on workspace 2; w1 (same app as w3) is carried there.
    // Run 51: every carry's restack omitted the carried window — its
    // assignment was still pending, so the destination's MRU stack didn't
    // contain it — and AppKit keyed the resident sibling at the head instead.
    // The next carry chord then read that flung focus and did nothing.
    let mut wins = std_windows();
    wins[1].workspace = ws(2);
    wins[2].workspace = ws(2);
    let s = update(
        &booted(&[2, 3, 1]), // history [1, 3, 2]
        &observed(
            vec![mon_a(1), mon_b(1)],
            wins.clone(),
            Some(1),
            RescanTrigger::Periodic,
        ),
    )
    .state;

    let carried = update(&s, &hotkey(HotkeyAction::CarryFocusedToWorkspaceNext));
    let restack = carried
        .effects
        .iter()
        .find_map(|e| match e {
            Effect::RestackWindows { order } => Some(order.clone()),
            _ => None,
        })
        .expect("restack effect");
    assert_eq!(
        restack,
        vec![wid(1), wid(3), wid(2)],
        "carried window on top"
    );
    assert!(
        focus_targets(&carried.effects).is_empty(),
        "it keeps its focus"
    );
    assert_eq!(carried.state.focus_intent(), FocusIntent::Window(wid(1)));

    // The world lands the carry but keys sibling w3 instead of w1.
    wins[0].workspace = ws(2);
    let flung = update(
        &carried.state,
        &observed(
            vec![mon_a(2), mon_b(2)],
            wins.clone(),
            Some(3),
            RescanTrigger::PostEffect { op: OpId(2) },
        ),
    );
    assert_eq!(count_switches(&flung.effects), 0);
    assert_eq!(focus_targets(&flung.effects), vec![wid(1)], "re-asserted");
    assert_eq!(
        flung.state.focus_history.iter().next(),
        Some(wid(1)),
        "the fling did not reorder MRU"
    );

    // The next chord carries w1 — the declaration — not the flung-to w3.
    let again = update(
        &flung.state,
        &hotkey(HotkeyAction::CarryFocusedToWorkspaceNext),
    );
    assert!(again.effects.iter().any(|e| {
        matches!(e, Effect::AssignWindowToWorkspace { window, target, .. }
            if *window == wid(1) && *target == ws(3))
    }));
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

// --- focus: declaration vs observation ----------------------------------------

#[test]
fn a_grant_that_never_lands_is_reasserted_then_stood_down_and_the_standoff_holds() {
    // Run 51: 36 of 255 switch grants never landed and were never retried.
    // Here the switch's grant to w2 is ignored forever (focus stays on parked
    // w1). Ordo re-asserts under the damping limit, then stands down once and
    // loudly — and never reads the stuck focus as navigation back to ws1.
    //
    // The part that matters is AFTER the stand-down, so the world is observed
    // long past it. Focus is still on a hidden window — the very shape the
    // visible-key-window invariant corrects — but the standoff has already
    // shown this window will not yield, and re-declaring against the same
    // evidence would raise the parked window every few seconds forever. The
    // concession has to hold until a command or the world moves.
    let mut wins = std_windows();
    wins[1].workspace = ws(2);
    let on_ws2 = |focused: u32| {
        observed(
            vec![mon_a(2), mon_b(2)],
            wins.clone(),
            Some(focused),
            RescanTrigger::Periodic,
        )
    };
    let s = update(
        &booted(&[2, 1]),
        &observed(
            vec![mon_a(1), mon_b(1)],
            wins.clone(),
            Some(1),
            RescanTrigger::Periodic,
        ),
    )
    .state;

    // Drive the stuck world for `rounds` observations after `first`, returning
    // (grants, stand-downs, switches, holds) — every kind of write or
    // re-declaration the fight could produce.
    let fight = |first: Step, rounds: usize| {
        let mut step = first;
        let mut grants = focus_targets(&step.effects).len();
        let mut diverged = 0;
        let mut switches = 0;
        let mut held = 0;
        for _ in 0..rounds {
            step = update(&step.state, &on_ws2(1));
            grants += focus_targets(&step.effects).len();
            switches += count_switches(&step.effects);
            diverged += step
                .notes
                .iter()
                .filter(|n| {
                    **n == Note::FocusDiverged {
                        window: wid(2),
                        winner: Some(wid(1)),
                        winner_app: Some(Pid(100)),
                    }
                })
                .count();
            held += step
                .notes
                .iter()
                .filter(|n| matches!(n, Note::HeldFocus { .. }))
                .count();
        }
        (step, grants, diverged, switches, held)
    };

    let switched = update(&s, &hotkey(HotkeyAction::WorkspaceNext));
    assert_eq!(focus_targets(&switched.effects), vec![wid(2)]);
    let (step, grants, diverged, switches, held) = fight(switched, 60);
    assert_eq!(
        grants, 4,
        "the command's grant + 3 damped re-assertions, then nothing"
    );
    assert_eq!(diverged, 1, "stood down once, and stayed down");
    assert_eq!(switches, 0, "a stuck focus is never followed");
    assert_eq!(
        held, 0,
        "the invariant does not re-litigate the window that won the slot"
    );
    assert_eq!(
        step.state.focus_intent(),
        FocusIntent::Deferred,
        "the lost claim is retired, not left to misdirect the next chord"
    );

    // A command is fresh evidence that the user wants the slot moved: Alt+Tab
    // to w2 re-opens the fight with a full budget, which ends the same way.
    let retried = update(&step.state, &hotkey(HotkeyAction::MruWorkspace));
    assert_eq!(focus_targets(&retried.effects), vec![wid(2)]);
    let (step, grants, diverged, _, held) = fight(retried, 60);
    assert_eq!(grants, 4);
    assert_eq!(diverged, 1);
    assert_eq!(held, 0);

    // Once the world agrees the concession is spent, and a later fling to the
    // same hidden window is a fresh violation for the invariant to hold.
    let agreed = update(&step.state, &on_ws2(2)).state;
    let flung = update(&agreed, &on_ws2(1));
    assert_eq!(focus_targets(&flung.effects), vec![wid(2)]);
    assert!(flung.notes.contains(&Note::HeldFocus {
        window: wid(2),
        from: wid(1),
        from_app: Pid(100),
    }));
}

#[test]
fn a_standoff_is_conceded_to_the_app_not_to_whichever_of_its_windows_was_key() {
    // AppKit key-window ownership is per-application: the app that beat the
    // grant decides which of ITS windows is key, and hops between them are
    // routine (Chrome's key window wandered through run 51's standoff; Cmd+H
    // churn does the same). The shape of the test above, except the side
    // that won has two hidden windows and alternates focus between them. A
    // concession keyed on the window is spent by every hop, and the loop it
    // exists to stop comes back at full rate.
    //
    // w1 and w3 (pid 100) and w4 (pid 300) stay hidden on ws1; w2 is the
    // visible MRU head on ws2.
    let mut wins = std_windows();
    wins[1].workspace = ws(2);
    wins.push(win(4, 300, 1, rect(300.0, 700.0)));
    let on_ws2 = |focused: Option<u32>| {
        observed(
            vec![mon_a(2), mon_b(2)],
            wins.clone(),
            focused,
            RescanTrigger::Periodic,
        )
    };
    let s = update(
        &booted(&[2, 1]),
        &observed(
            vec![mon_a(1), mon_b(1)],
            wins.clone(),
            Some(1),
            RescanTrigger::Periodic,
        ),
    )
    .state;
    let tally = |step: &Step| {
        let count = |f: fn(&Note) -> bool| step.notes.iter().filter(|n| f(n)).count();
        (
            focus_targets(&step.effects).len(),
            count(|n| matches!(n, Note::FocusDiverged { .. })),
            count(|n| matches!(n, Note::HeldFocus { .. })),
        )
    };

    let mut step = update(&s, &hotkey(HotkeyAction::WorkspaceNext));
    let (mut grants, mut diverged, mut held) = tally(&step);
    for i in 0..60 {
        let who = if i % 2 == 0 { 1 } else { 3 };
        step = update(&step.state, &on_ws2(Some(who)));
        let (g, d, h) = tally(&step);
        grants += g;
        diverged += d;
        held += h;
    }
    assert_eq!(
        grants, 4,
        "the command's grant + 3 damped re-assertions, then nothing"
    );
    assert_eq!(
        diverged, 1,
        "stood down once, and stayed down through every hop"
    );
    assert_eq!(held, 0, "a hop within the app is not the world moving on");
    assert_eq!(step.state.focus_intent(), FocusIntent::Deferred);

    // A focus vacuum is not the app relenting: nobody else took the slot, so
    // when the same app re-keys a hidden window nothing is re-litigated.
    let vacuum = update(&step.state, &on_ws2(None));
    assert!(vacuum.effects.is_empty());
    let rekeyed = update(&vacuum.state, &on_ws2(Some(3)));
    assert!(rekeyed.effects.is_empty());
    assert_eq!(tally(&rekeyed).2, 0);

    // A DIFFERENT app taking the slot — even into another hidden window — is
    // the world moving on: the invariant holds against it afresh.
    let usurped = update(&rekeyed.state, &on_ws2(Some(4)));
    assert_eq!(focus_targets(&usurped.effects), vec![wid(2)]);
    assert!(usurped.notes.contains(&Note::HeldFocus {
        window: wid(2),
        from: wid(4),
        from_app: Pid(300),
    }));
}

#[test]
fn a_grant_landing_on_a_sibling_is_corrected_and_does_not_reorder_mru() {
    // Chrome keeps its own key window: a grant to w2 lands on its sibling w4
    // (same app, same workspace). The grant is retried once its expectation
    // has expired, and the sibling's stolen focus never becomes "most recent".
    let wins = vec![
        win(1, 100, 1, rect(100.0, 100.0)),
        win(2, 200, 2, rect(2000.0, 100.0)),
        win(3, 100, 1, rect(600.0, 500.0)),
        win(4, 200, 2, rect(2400.0, 500.0)),
    ];
    let obs = |active: u8, focused: u32| {
        observed(
            vec![mon_a(active), mon_b(active)],
            wins.clone(),
            Some(focused),
            RescanTrigger::Periodic,
        )
    };
    // History built while ws2 was up: w4, then w2; then the user is on ws1.
    let mut s = update(
        &State::new(),
        &observed(
            vec![mon_a(2), mon_b(2)],
            wins.clone(),
            Some(4),
            RescanTrigger::Startup,
        ),
    )
    .state;
    for (active, focused) in [(2, 2), (1, 1)] {
        s = update(&s, &obs(active, focused)).state;
    }
    let step = update(&s, &hotkey(HotkeyAction::WorkspaceNext));
    assert_eq!(focus_targets(&step.effects), vec![wid(2)]);

    let mut s = step.state;
    let mut regrants = 0;
    for _ in 0..5 {
        let next = update(&s, &obs(2, 4)); // sibling holds key
        regrants += focus_targets(&next.effects).len();
        assert_eq!(count_switches(&next.effects), 0);
        s = next.state;
    }
    assert_eq!(
        regrants, 1,
        "one retry, after the grant's expectation expired"
    );
    assert_eq!(s.focus_history.iter().next(), Some(wid(2)));
    assert_eq!(s.focus_intent(), FocusIntent::Window(wid(2)));
}

#[test]
fn mru_records_declarations_and_only_the_os_s_visible_choice_when_deferred() {
    let s = booted(&[3, 2, 1]); // history [1, 2, 3], all on ws1
    let step = update(&s, &hotkey(HotkeyAction::MruWorkspace)); // -> w2
    assert_eq!(
        step.state.focus_history.iter().next(),
        Some(wid(2)),
        "declared before any observation confirms it"
    );

    // The world keys w3 instead: the declaration stands and so does the order.
    let flung = update(
        &step.state,
        &observed(
            vec![mon_a(1), mon_b(1)],
            std_windows(),
            Some(3),
            RescanTrigger::Periodic,
        ),
    );
    assert_eq!(flung.state.focus_history.iter().next(), Some(wid(2)));
    assert_eq!(flung.state.focus_intent(), FocusIntent::Window(wid(2)));

    // After a click the OS owns the slot, and its choice is the record.
    let clicked = update(&flung.state, &click(700.0, 600.0)).state; // inside w3
    let seen = update(
        &clicked,
        &observed(
            vec![mon_a(1), mon_b(1)],
            std_windows(),
            Some(3),
            RescanTrigger::Periodic,
        ),
    );
    assert!(seen.effects.is_empty(), "nothing to enforce while deferred");
    assert_eq!(seen.state.focus_history.iter().next(), Some(wid(3)));
}

#[test]
fn a_fling_after_a_birth_on_an_empty_workspace_is_reasserted_not_followed() {
    // Run 51 seq 12750-12769: switch to empty ws5; Cmd+N; the new window is
    // born and focused; 32ms later macOS flings focus to a kitty window on
    // ws4; follow-the-focus switched there. The birth is a command that
    // declares the newborn, so the fling meets a standing declaration.
    let s = booted(&[1]);
    let away = update(&s, &hotkey(HotkeyAction::WorkspaceNext)); // ws2 is empty
    assert!(focus_targets(&away.effects).is_empty());
    assert_eq!(away.state.focus_intent(), FocusIntent::Deferred);

    // Parked: focus still sits on hidden w1 with nothing here to pull it to.
    let parked = update(
        &away.state,
        &observed(
            vec![mon_a(2), mon_b(2)],
            std_windows(),
            Some(1),
            RescanTrigger::PostEffect { op: OpId(1) },
        ),
    );
    assert_eq!(count_switches(&parked.effects), 0);
    assert!(focus_targets(&parked.effects).is_empty());

    let mut wins = std_windows();
    wins.push(win(9, 300, 2, rect(100.0, 100.0)));
    let born = update(
        &parked.state,
        &observed(
            vec![mon_a(2), mon_b(2)],
            wins.clone(),
            Some(9),
            RescanTrigger::AxHint {
                pid: Some(Pid(300)),
                kind: AxHintKind::WindowCreated,
            },
        ),
    );
    assert_eq!(born.state.focus_intent(), FocusIntent::Window(wid(9)));

    let flung = update(
        &born.state,
        &observed(
            vec![mon_a(2), mon_b(2)],
            wins,
            Some(1),
            RescanTrigger::AxHint {
                pid: Some(Pid(100)),
                kind: AxHintKind::Other("AXFocusedWindowChanged".into()),
            },
        ),
    );
    assert_eq!(count_switches(&flung.effects), 0, "not navigation");
    assert_eq!(focus_targets(&flung.effects), vec![wid(9)]);
    assert!(flung
        .notes
        .contains(&Note::FocusReasserted { window: wid(9) }));
}

#[test]
fn a_gesture_hands_focus_to_the_os_and_every_command_takes_it_back() {
    // Michael's concern: `Deferred` is a standing state, so it must never
    // outlive the next command. Every hotkey that does anything declares.
    assert_eq!(State::new().focus_intent(), FocusIntent::Deferred);
    let s = booted(&[3, 2, 1]);
    assert_eq!(
        s.focus_intent(),
        FocusIntent::Deferred,
        "start: the OS owns focus"
    );

    let declared = update(&s, &hotkey(HotkeyAction::MruWorkspace)).state;
    assert_eq!(declared.focus_intent(), FocusIntent::Window(wid(2)));
    let deferred = update(&declared, &gesture(Gesture::SystemSwitch)).state;
    assert_eq!(deferred.focus_intent(), FocusIntent::Deferred);

    // While deferred, a focus change on the visible workspace is not fought.
    let seen = update(
        &deferred,
        &observed(
            vec![mon_a(1), mon_b(1)],
            std_windows(),
            Some(3),
            RescanTrigger::Periodic,
        ),
    );
    assert!(seen.effects.is_empty());

    // History is now [3, 2, 1] and the OS's choice, w3, is what commands read.
    for (action, expect) in [
        (HotkeyAction::MruWorkspace, FocusIntent::Window(wid(2))),
        (HotkeyAction::MruDemote, FocusIntent::Window(wid(2))),
        (
            HotkeyAction::MoveFocusedToMonitorNext,
            FocusIntent::Window(wid(3)),
        ),
        (
            HotkeyAction::CarryFocusedToWorkspaceNext,
            FocusIntent::Window(wid(3)),
        ),
    ] {
        let after = update(&seen.state, &hotkey(action)).state;
        assert_eq!(after.focus_intent(), expect, "{action:?}");
    }
    // A hotkey that does nothing (clamped at the edge) changes nothing.
    let noop = update(&seen.state, &hotkey(HotkeyAction::WorkspacePrev)).state;
    assert_eq!(noop, seen.state);
    // Rescue hands the desktop back wholesale.
    let rescued = update(&declared, &Event::RescueEngaged { at: ts() }).state;
    assert_eq!(rescued.focus_intent(), FocusIntent::Deferred);
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

// --- virtual monitors --------------------------------------------------------
// The laptop rig: ONE display, two virtual monitors, virtualization on. w1 and
// w3 (pid 100) are declared on monitor 1, w2 (pid 200) on monitor 2 — hidden
// while the anchor is monitor 1. Every frame sits on the one display, since
// that is where macOS put them when the external display went away.

fn laptop(viewed: u8, enabled: bool) -> VirtualMonitors {
    VirtualMonitors {
        count: 2,
        viewed: vm(viewed),
        enabled,
    }
}

fn undocked_windows() -> Vec<Win> {
    vec![
        win(1, 100, 1, rect(100.0, 100.0)),
        on_monitor(win(2, 200, 1, rect(600.0, 100.0)), 2),
        win(3, 100, 1, rect(600.0, 500.0)),
    ]
}

/// Boot the laptop rig with the anchor on monitor 1, then focus each window
/// in `focus_seq` — viewing its monitor for the observation, since a hidden
/// window's focus is never recorded — and end back on monitor 1.
fn undocked(focus_seq: &[u32]) -> State {
    let mut s = update(
        &State::new(),
        &observed_view(
            laptop(1, true),
            vec![mon_a(1)],
            undocked_windows(),
            None,
            RescanTrigger::Startup,
        ),
    )
    .state;
    for f in focus_seq {
        let viewed = if *f == 2 { 2 } else { 1 };
        s = update(
            &s,
            &observed_view(
                laptop(viewed, true),
                vec![mon_a(1)],
                undocked_windows(),
                Some(*f),
                RescanTrigger::Periodic,
            ),
        )
        .state;
    }
    let last = focus_seq.last().copied();
    update(
        &s,
        &observed_view(
            laptop(1, true),
            vec![mon_a(1)],
            undocked_windows(),
            last,
            RescanTrigger::Periodic,
        ),
    )
    .state
}

fn view_targets(effects: &[Effect]) -> Vec<VirtualMonitorId> {
    effects
        .iter()
        .filter_map(|e| match e {
            Effect::ViewMonitor { target, .. } => Some(*target),
            _ => None,
        })
        .collect()
}

fn monitor_assignments(effects: &[Effect]) -> Vec<(WindowId, VirtualMonitorId)> {
    effects
        .iter()
        .filter_map(|e| match e {
            Effect::AssignWindowToMonitor { window, target, .. } => Some((*window, *target)),
            _ => None,
        })
        .collect()
}

#[test]
fn viewing_the_next_monitor_hands_focus_to_its_mru_window_and_clamps_at_the_ends() {
    let s = undocked(&[2, 1]); // history [1, 2]; anchor on monitor 1
    assert_eq!(s.virtual_monitors, Some(laptop(1, true)));
    assert!(!s.is_visible(&s.windows[&wid(2)]), "w2 is hidden with its monitor");

    let step = update(&s, &hotkey(HotkeyAction::ViewMonitorNext));
    assert_eq!(focus_targets(&step.effects), vec![wid(2)]);
    assert_eq!(view_targets(&step.effects), vec![vm(2)]);
    assert_eq!(step.state.focus_intent(), FocusIntent::Window(wid(2)));
    // The focus is issued BEFORE the view, as a switch hands focus before it
    // parks the old workspace.
    let focus_at = step
        .effects
        .iter()
        .position(|e| matches!(e, Effect::FocusWindow { .. }))
        .unwrap();
    let view_at = step
        .effects
        .iter()
        .position(|e| matches!(e, Effect::ViewMonitor { .. }))
        .unwrap();
    assert!(focus_at < view_at);

    // The backend's word confirms the view: our own echo.
    let confirmed = update(
        &step.state,
        &observed_view(
            laptop(2, true),
            vec![mon_a(1)],
            undocked_windows(),
            Some(2),
            RescanTrigger::Periodic,
        ),
    );
    assert!(confirmed
        .notes
        .iter()
        .any(|n| matches!(n, Note::SelfConfirmed { .. })));
    assert!(!confirmed
        .notes
        .iter()
        .any(|n| matches!(n, Note::External { delta: Delta::ViewedMonitorChanged { .. } })));

    // Clamped at both ends; no wrap.
    assert!(update(&confirmed.state, &hotkey(HotkeyAction::ViewMonitorNext))
        .effects
        .is_empty());
    assert!(update(&s, &hotkey(HotkeyAction::ViewMonitorPrev))
        .effects
        .is_empty());
}

#[test]
fn mru_chords_reach_hidden_monitors_and_view_them_first() {
    let s = undocked(&[3, 2, 1]); // history [1, 2, 3]; w2 hidden on monitor 2

    // Alt+Tab: the whole workspace, hidden monitors included — and the view
    // follows the focus there.
    let step = update(&s, &hotkey(HotkeyAction::MruWorkspace));
    assert_eq!(focus_targets(&step.effects), vec![wid(2)]);
    assert_eq!(view_targets(&step.effects), vec![vm(2)]);
    // Nothing hidden is ever restacked: on one display the revealed set is w2
    // alone, so there is no order to impose.
    assert!(!step
        .effects
        .iter()
        .any(|e| matches!(e, Effect::RestackWindows { .. })));

    // Ctrl+Alt+Tab: the OTHER monitor is the hidden one.
    let other = update(&s, &hotkey(HotkeyAction::MruOtherMonitor));
    assert_eq!(focus_targets(&other.effects), vec![wid(2)]);
    assert_eq!(view_targets(&other.effects), vec![vm(2)]);

    // Alt+Shift+Tab: same monitor, so w3 — and nothing to view.
    let same = update(&s, &hotkey(HotkeyAction::MruMonitor));
    assert_eq!(focus_targets(&same.effects), vec![wid(3)]);
    assert!(view_targets(&same.effects).is_empty());
}

#[test]
fn moving_a_window_to_a_hidden_monitor_views_it_and_never_touches_its_frame() {
    let s = undocked(&[1]);
    let step = update(&s, &hotkey(HotkeyAction::MoveFocusedToMonitorNext));
    assert_eq!(monitor_assignments(&step.effects), vec![(wid(1), vm(2))]);
    assert_eq!(view_targets(&step.effects), vec![vm(2)]);
    // Both monitors project onto the one display: the window stays put.
    assert_eq!(count_set_frames(&step.effects, 1), 0);
    assert_eq!(step.state.focus_intent(), FocusIntent::Window(wid(1)));
    // The assignment comes first so the view's plan finds the window already
    // a resident of the monitor being revealed.
    let assign_at = step
        .effects
        .iter()
        .position(|e| matches!(e, Effect::AssignWindowToMonitor { .. }))
        .unwrap();
    let view_at = step
        .effects
        .iter()
        .position(|e| matches!(e, Effect::ViewMonitor { .. }))
        .unwrap();
    assert!(assign_at < view_at);

    // Clamped at the edge.
    assert!(update(&s, &hotkey(HotkeyAction::MoveFocusedToMonitorPrev))
        .effects
        .is_empty());

    // On the full rig the target IS hosted: assignment plus the frame move,
    // and nothing to view.
    let docked = update(&booted(&[1]), &hotkey(HotkeyAction::MoveFocusedToMonitorNext));
    assert_eq!(monitor_assignments(&docked.effects), vec![(wid(1), vm(2))]);
    assert!(view_targets(&docked.effects).is_empty());
    let frame = set_frame_for(&docked.effects, 1).expect("frame effect");
    assert!(frame.x >= 1920.0, "landed on the second display: {frame:?}");
}

#[test]
fn toggling_virtualization_on_views_the_focused_windows_monitor() {
    let s = undocked(&[1]);
    // Off: everything collapses onto the display; no view change needed.
    let off = update(&s, &hotkey(HotkeyAction::ToggleVirtualMonitors));
    assert!(off
        .effects
        .iter()
        .any(|e| matches!(e, Effect::SetVirtualMonitors { enabled: false, .. })));
    assert!(view_targets(&off.effects).is_empty());
    assert_eq!(off.state.focus_intent(), s.focus_intent(), "focus untouched");

    // The user clicks into w2, now visible on the shared display.
    let collapsed = update(
        &off.state,
        &observed_view(
            laptop(1, false),
            vec![mon_a(1)],
            undocked_windows(),
            Some(1),
            RescanTrigger::Periodic,
        ),
    )
    .state;
    assert!(collapsed.is_visible(&collapsed.windows[&wid(2)]));
    let clicked = update(&collapsed, &click(700.0, 200.0)).state;
    let on_w2 = update(
        &clicked,
        &observed_view(
            laptop(1, false),
            vec![mon_a(1)],
            undocked_windows(),
            Some(2),
            RescanTrigger::Periodic,
        ),
    )
    .state;
    assert_eq!(on_w2.declared_focus(), Some(wid(2)));

    // Back on: w2's monitor must not vanish from under the user, so the
    // anchor moves to it in the same breath.
    let on = update(&on_w2, &hotkey(HotkeyAction::ToggleVirtualMonitors));
    assert!(on
        .effects
        .iter()
        .any(|e| matches!(e, Effect::SetVirtualMonitors { enabled: true, .. })));
    assert_eq!(view_targets(&on.effects), vec![vm(2)]);
}

#[test]
fn a_switch_onto_a_workspace_hidden_by_its_monitor_views_the_mru_heads_monitor() {
    // w2 lives on workspace 2 AND monitor 2; the anchor is monitor 1.
    let mut wins = undocked_windows();
    wins[1].workspace = ws(2);
    let s = update(
        &State::new(),
        &observed_view(
            laptop(1, true),
            vec![mon_a(1)],
            wins.clone(),
            Some(1),
            RescanTrigger::Startup,
        ),
    )
    .state;
    let step = update(&s, &hotkey(HotkeyAction::WorkspaceNext));
    assert_eq!(count_switches(&step.effects), 1);
    assert_eq!(focus_targets(&step.effects), vec![wid(2)], "not a blank screen");
    assert_eq!(view_targets(&step.effects), vec![vm(2)]);
    assert_eq!(step.state.focus_intent(), FocusIntent::Window(wid(2)));

    // With something visible on the destination the anchor stays global.
    wins.push(win(9, 300, 2, rect(200.0, 200.0))); // workspace 2, monitor 1
    let s = update(
        &State::new(),
        &observed_view(
            laptop(1, true),
            vec![mon_a(1)],
            wins,
            Some(1),
            RescanTrigger::Startup,
        ),
    )
    .state;
    let step = update(&s, &hotkey(HotkeyAction::WorkspaceNext));
    assert_eq!(focus_targets(&step.effects), vec![wid(9)]);
    assert!(view_targets(&step.effects).is_empty());
}

#[test]
fn a_new_window_is_corralled_onto_the_focused_monitor_by_declaration_and_frame() {
    let s = booted(&[1]); // user on w1: monitor 1
    let mut wins = std_windows();
    // Born on the second display (its app remembers a position there), and it
    // takes focus.
    wins.push(win(9, 300, 1, rect(2100.0, 300.0)));
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
    assert_eq!(monitor_assignments(&obs.effects), vec![(wid(9), vm(1))]);
    let frame = set_frame_for(&obs.effects, 9).expect("frame corrective");
    assert!(frame.x + frame.w <= 1920.0, "onto monitor 1's display: {frame:?}");
    assert!(
        !obs.notes.iter().any(|n| matches!(n, Note::MonitorAdopted { .. })),
        "a birth is corralled, not adopted"
    );
}

#[test]
fn replugging_rehosts_a_windows_frame_without_adopting() {
    // Undocked, w2 declared on monitor 2 but sitting on the laptop display.
    let s = undocked(&[1]);
    // The external display returns: monitor 2 is hosted again, but w2's frame
    // is still where the laptop had it. It goes home; its declaration stands.
    let replug = update(
        &s,
        &observed_view(
            laptop(1, true),
            vec![mon_a(1), mon_b(1)],
            undocked_windows(),
            Some(1),
            RescanTrigger::BackendHint {
                kind: "display_reconfigured".into(),
            },
        ),
    );
    assert!(replug
        .notes
        .iter()
        .any(|n| matches!(n, Note::External { delta: Delta::MonitorAdded(_) })));
    let frame = set_frame_for(&replug.effects, 2).expect("w2 re-hosted");
    assert!(frame.x >= 1920.0, "onto the returned display: {frame:?}");
    assert!(monitor_assignments(&replug.effects).is_empty());
    assert!(!replug
        .notes
        .iter()
        .any(|n| matches!(n, Note::MonitorAdopted { .. })));
    assert_eq!(count_set_frames(&replug.effects, 1), 0, "w1 was already home");

    // The write lands: our own echo, and nothing more to do.
    let mut wins = undocked_windows();
    wins[1].snap.frame = frame;
    let landed = update(
        &replug.state,
        &observed_view(
            laptop(1, true),
            vec![mon_a(1), mon_b(1)],
            wins.clone(),
            Some(1),
            RescanTrigger::Periodic,
        ),
    );
    assert!(landed
        .notes
        .iter()
        .any(|n| matches!(n, Note::SelfConfirmed { .. })));
    let quiet = update(
        &landed.state,
        &observed_view(
            laptop(1, true),
            vec![mon_a(1), mon_b(1)],
            wins,
            Some(1),
            RescanTrigger::Periodic,
        ),
    );
    assert!(quiet.effects.is_empty(), "{:?}", quiet.effects);
}

#[test]
fn unplugging_never_adopts_the_windows_macos_rehomed() {
    // Full rig, w2 on the second display and declared on monitor 2.
    let s = booted(&[1]);
    // The display vanishes and macOS drops w2 onto the remaining one, in the
    // same observation. That landing is nobody's intent.
    let mut wins = std_windows();
    wins[1].snap.frame = rect(80.0, 34.0);
    wins[1] = on_monitor(wins[1].clone(), 2); // the backend's word: still monitor 2
    let unplug = update(
        &s,
        &observed_view(
            laptop(1, true),
            vec![mon_a(1)],
            wins.clone(),
            Some(1),
            RescanTrigger::BackendHint {
                kind: "display_reconfigured".into(),
            },
        ),
    );
    assert!(monitor_assignments(&unplug.effects).is_empty());
    assert!(!unplug
        .notes
        .iter()
        .any(|n| matches!(n, Note::MonitorAdopted { .. })));
    assert_eq!(count_set_frames(&unplug.effects, 2), 0, "hidden: the backend parks it");
    assert_eq!(unplug.state.windows[&wid(2)].vmonitor, vm(2), "declaration kept");
    assert!(!unplug.state.is_visible(&unplug.state.windows[&wid(2)]));

    // And on a later, quiet scan the (parked, hidden) window is still no
    // drag: its monitor is not hosted, so there is nothing to adopt onto.
    let later = update(
        &unplug.state,
        &observed_view(
            laptop(1, true),
            vec![mon_a(1)],
            wins,
            Some(1),
            RescanTrigger::Periodic,
        ),
    );
    assert!(later.effects.is_empty(), "{:?}", later.effects);
}

#[test]
fn a_drag_across_live_displays_adopts_the_new_monitor_once() {
    let s = booted(&[1]);
    // w2 is dragged from the second display onto the first — an unexplained
    // move between two live displays, with no display change.
    let mut wins = std_windows();
    wins[1].snap.frame = rect(800.0, 100.0);
    wins[1] = on_monitor(wins[1].clone(), 2); // the word has not moved yet
    let drag = update(
        &s,
        &observed(
            vec![mon_a(1), mon_b(1)],
            wins.clone(),
            Some(1),
            RescanTrigger::Periodic,
        ),
    );
    assert_eq!(monitor_assignments(&drag.effects), vec![(wid(2), vm(1))]);
    assert!(drag.notes.contains(&Note::MonitorAdopted {
        window: wid(2),
        monitor: vm(1)
    }));
    assert_eq!(count_set_frames(&drag.effects, 2), 0, "the user's hands win, no fight");

    // The word follows the adoption: confirmed, and the window is left alone.
    wins[1] = on_monitor(wins[1].clone(), 1);
    let settled = update(
        &drag.state,
        &observed(
            vec![mon_a(1), mon_b(1)],
            wins.clone(),
            Some(1),
            RescanTrigger::Periodic,
        ),
    );
    assert!(settled
        .notes
        .iter()
        .any(|n| matches!(n, Note::SelfConfirmed { .. })));
    let quiet = update(
        &settled.state,
        &observed(
            vec![mon_a(1), mon_b(1)],
            wins,
            Some(1),
            RescanTrigger::Periodic,
        ),
    );
    assert!(quiet.effects.is_empty(), "{:?}", quiet.effects);
}

#[test]
fn a_drag_onto_a_shared_display_adopts_the_monitor_it_stands_for() {
    // Three virtual monitors, two displays, collapsed: the second display
    // hosts monitors 2 and 3. A window dragged onto it becomes a monitor-2
    // window (the first the display absorbs) — never a fight.
    let view = VirtualMonitors {
        count: 3,
        viewed: vm(1),
        enabled: false,
    };
    let wins = vec![
        win(1, 100, 1, rect(100.0, 100.0)),
        on_monitor(win(2, 200, 1, rect(2000.0, 100.0)), 3),
    ];
    let s = update(
        &State::new(),
        &observed_view(
            view,
            vec![mon_a(1), mon_b(1)],
            wins.clone(),
            Some(1),
            RescanTrigger::Startup,
        ),
    )
    .state;
    assert!(s.is_visible(&s.windows[&wid(2)]));
    // w2, declared on monitor 3, sits on the display hosting 3: no violation.
    let quiet = update(
        &s,
        &observed_view(
            view,
            vec![mon_a(1), mon_b(1)],
            wins.clone(),
            Some(1),
            RescanTrigger::Periodic,
        ),
    );
    assert!(quiet.effects.is_empty(), "{:?}", quiet.effects);

    let mut dragged = wins;
    dragged[0].snap.frame = rect(2200.0, 300.0);
    dragged[0] = on_monitor(dragged[0].clone(), 1);
    let drag = update(
        &s,
        &observed_view(
            view,
            vec![mon_a(1), mon_b(1)],
            dragged,
            Some(1),
            RescanTrigger::Periodic,
        ),
    );
    assert_eq!(monitor_assignments(&drag.effects), vec![(wid(1), vm(2))]);
    assert_eq!(count_set_frames(&drag.effects, 1), 0);
}

#[test]
fn a_gesture_landing_on_a_hidden_monitor_is_followed_and_an_unwitnessed_one_held() {
    let s = undocked(&[1]); // w2 hidden on monitor 2

    // Cmd+Tab to w2's app: the user went there; the view follows, on the
    // monitor axis only — the workspace is already current.
    let switched = update(&s, &gesture(Gesture::SystemSwitch)).state;
    let followed = update(
        &switched,
        &observed_view(
            laptop(1, true),
            vec![mon_a(1)],
            undocked_windows(),
            Some(2),
            RescanTrigger::Periodic,
        ),
    );
    assert_eq!(view_targets(&followed.effects), vec![vm(2)]);
    assert_eq!(count_switches(&followed.effects), 0);
    assert!(followed.notes.contains(&Note::FollowedFocus {
        window: wid(2),
        target: ws(1),
        monitor: Some(vm(2)),
    }));
    assert_eq!(followed.state.focus_intent(), FocusIntent::Window(wid(2)));

    // The same landing with no gesture is a fling into an invisible window:
    // focus is pulled back to the visible MRU window.
    let held = update(
        &s,
        &observed_view(
            laptop(1, true),
            vec![mon_a(1)],
            undocked_windows(),
            Some(2),
            RescanTrigger::Periodic,
        ),
    );
    assert!(view_targets(&held.effects).is_empty());
    assert!(held
        .notes
        .iter()
        .any(|n| matches!(n, Note::HeldFocus { window, .. } if *window == wid(1))));
    assert_eq!(focus_targets(&held.effects), vec![wid(1)]);
}

#[test]
fn without_a_virtual_layer_a_monitor_is_its_display() {
    // A native-backend world (or a pre-monitor log): no word at all.
    let mut snap = world(&[mon_a(1), mon_b(1)], &std_windows(), Some(1));
    snap.workspaces.virtual_monitors = None;
    let s = update(
        &State::new(),
        &Event::WorldObserved {
            at: ts(),
            trigger: RescanTrigger::Startup,
            snap,
        },
    )
    .state;
    assert_eq!(s.virtual_monitors, None);
    assert_eq!(s.windows[&wid(2)].vmonitor, vm(2), "position of its display");
    assert!(s.is_visible(&s.windows[&wid(2)]));

    // Monitor chords still work physically; the virtual-only ones are inert.
    let other = update(&s, &hotkey(HotkeyAction::MruOtherMonitor));
    assert_eq!(focus_targets(&other.effects), vec![wid(2)]);
    let moved = update(&s, &hotkey(HotkeyAction::MoveFocusedToMonitorNext));
    assert!(monitor_assignments(&moved.effects).is_empty(), "nothing to declare to");
    assert!(set_frame_for(&moved.effects, 1).is_some_and(|f| f.x >= 1920.0));
    assert!(update(&s, &hotkey(HotkeyAction::ViewMonitorNext)).effects.is_empty());
    assert!(update(&s, &hotkey(HotkeyAction::ToggleVirtualMonitors)).effects.is_empty());
}
