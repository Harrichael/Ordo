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
            layer: None,
            parent: None,
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
        unread: Vec::new(),
        key_unmanaged: false,
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

fn dock(x: f64, y: f64) -> Event {
    gesture(Gesture::Dock { at: Point { x, y } })
}

/// Boot the standard world, then click each window in `focus_seq` in order,
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
        s = used(&s, *f, vec![mon_a(1), mon_b(1)], std_windows());
    }
    s
}

/// The user clicks window `w` and the next look sees it key: how the MRU
/// history learns of a window used outside Ordo.
fn used(s: &State, w: u32, monitors: Vec<Mon>, windows: Vec<Win>) -> State {
    let at = windows
        .iter()
        .find(|x| x.snap.id == wid(w))
        .expect("the window is in the world")
        .snap
        .frame
        .center();
    let clicked = update(s, &click(at.x, at.y));
    assert!(
        clicked.notes.iter().any(|n| matches!(
            n,
            Note::GestureClassified { within: Some(hit), .. } if *hit == wid(w)
        )),
        "the click hit w{w} first: {:?}",
        clicked.notes
    );
    update(
        &clicked.state,
        &observed(monitors, windows, Some(w), RescanTrigger::Periodic),
    )
    .state
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

/// The op a switch was issued under — not always the first minted, since a
/// switch hands out focus before it.
fn switch_op(effects: &[Effect]) -> OpId {
    effects
        .iter()
        .find_map(|e| match e {
            Effect::SwitchWorkspace { op, .. } => Some(*op),
            _ => None,
        })
        .expect("a switch was issued")
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
    assert!(step
        .state
        .pending
        .iter()
        .any(|p| p.expect == Expectation::AllMonitorsOn(ws(2))));

    let clamped = update(&s, &hotkey(HotkeyAction::WorkspacePrev));
    assert!(clamped.effects.is_empty(), "already at workspace 1");
}

/// A burst runs faster than the looks that confirm each switch. Every
/// "next" steps from where the last press went, not from the workspace the
/// last look saw, or the burst would switch to workspace 2 over and over.
#[test]
fn a_burst_steps_from_each_unconfirmed_switch() {
    let mut s = booted(&[1]);
    let mut targets = Vec::new();
    for press in [
        HotkeyAction::WorkspaceNext,
        HotkeyAction::WorkspaceNext,
        HotkeyAction::WorkspacePrev,
    ] {
        let step = update(&s, &hotkey(press));
        targets.extend(step.effects.iter().filter_map(|e| match e {
            Effect::SwitchWorkspace { target, .. } => Some(*target),
            _ => None,
        }));
        s = step.state;
    }
    assert_eq!(targets, [ws(2), ws(3), ws(2)]);
}

/// A look can confirm the newest switch of a burst while older ones are
/// still waiting to be seen. They never will be: the newest one superseded
/// them. The next press steps from where the user is, not from a switch
/// already overtaken (run 48: five "previous" presses in a row went nowhere).
#[test]
fn a_press_after_a_look_confirms_the_burst_steps_from_where_it_landed() {
    let s = booted(&[1]);
    let first = update(&s, &hotkey(HotkeyAction::WorkspaceNext));
    let second = update(&first.state, &hotkey(HotkeyAction::WorkspaceNext));
    let mut on_3 = std_windows();
    for w in &mut on_3 {
        w.workspace = ws(3);
    }
    let seen = update(
        &second.state,
        &observed(vec![mon_a(3), mon_b(3)], on_3, Some(1), RescanTrigger::Periodic),
    );
    let back = update(&seen.state, &hotkey(HotkeyAction::WorkspacePrev));
    let targets: Vec<WorkspaceId> = back
        .effects
        .iter()
        .filter_map(|e| match e {
            Effect::SwitchWorkspace { target, .. } => Some(*target),
            _ => None,
        })
        .collect();
    assert_eq!(targets, [ws(2)]);
}

/// The carry twin: a look confirms the second of two carries while the
/// first still waits. The window is where the second carry took it.
#[test]
fn a_carry_after_a_look_confirms_two_carries_steps_from_where_they_landed() {
    let s = booted(&[1]);
    let first = update(&s, &hotkey(HotkeyAction::CarryFocusedToWorkspaceNext));
    let second = update(&first.state, &hotkey(HotkeyAction::CarryFocusedToWorkspaceNext));
    let mut wins = std_windows();
    wins[0].workspace = ws(3);
    let seen = update(
        &second.state,
        &observed(vec![mon_a(3), mon_b(3)], wins, Some(1), RescanTrigger::Periodic),
    );
    let back = update(&seen.state, &hotkey(HotkeyAction::CarryFocusedToWorkspacePrev));
    let moved: Vec<WorkspaceId> = back
        .effects
        .iter()
        .filter_map(|e| match e {
            Effect::AssignWindowToWorkspace { window, target, .. } if *window == wid(1) => {
                Some(*target)
            }
            _ => None,
        })
        .collect();
    assert_eq!(moved, [ws(2)]);
}

/// Carrying a window on before the first carry is confirmed: it is still
/// the window the user has with them.
#[test]
fn a_second_carry_before_the_first_is_confirmed_carries_the_window_on() {
    let s = booted(&[1]);
    let first = update(&s, &hotkey(HotkeyAction::CarryFocusedToWorkspaceNext));
    let second = update(&first.state, &hotkey(HotkeyAction::CarryFocusedToWorkspaceNext));
    let moved: Vec<WorkspaceId> = second
        .effects
        .iter()
        .filter_map(|e| match e {
            Effect::AssignWindowToWorkspace { window, target, .. } if *window == wid(1) => {
                Some(*target)
            }
            _ => None,
        })
        .collect();
    assert_eq!(moved, [ws(3)]);
}

/// Out and straight back, before any look: the seen focus is still on the
/// window the second switch returns to, but a grant elsewhere was issued in
/// between. The return's grant must be expected, or the look that shows it
/// landing reads as someone else moving focus.
#[test]
fn a_quick_return_still_expects_its_own_focus_grant() {
    let mut wins = std_windows();
    wins[1].workspace = ws(2);
    let s = update(
        &booted(&[2, 1]),
        &observed(vec![mon_a(1), mon_b(1)], wins, Some(1), RescanTrigger::Periodic),
    )
    .state;
    let out = update(&s, &hotkey(HotkeyAction::WorkspaceNext));
    let back = update(&out.state, &hotkey(HotkeyAction::WorkspacePrev));
    assert_eq!(focus_targets(&back.effects), vec![wid(1)]);
    assert!(back
        .state
        .pending
        .iter()
        .any(|p| p.expect == Expectation::Focused(wid(1))));
}

/// A grant that repeats one still on its way, to a window the last look
/// already saw key, needs no expectation of its own, and must not retire the
/// older one: that one is the record that a grant is in flight. A look that
/// then catches focus elsewhere (an earlier grant landing late) waits for
/// it, rather than granting again over it.
#[test]
fn a_repeated_grant_keeps_the_one_in_flight_on_record() {
    let s = booted(&[3, 1, 2]); // w2 key, w1 behind it
    let away = update(&s, &hotkey(HotkeyAction::MruWorkspace)).state; // -> w1
    let back = update(&away, &hotkey(HotkeyAction::MruWorkspace)).state; // -> w2
    let viewed = update(&back, &hotkey(HotkeyAction::ViewMonitorNext));
    assert_eq!(focus_targets(&viewed.effects), vec![wid(2)]);

    let on_2 = VirtualMonitors {
        count: 2,
        viewed: vm(2),
        enabled: true,
    };
    let late = update(
        &viewed.state,
        &observed_view(
            on_2,
            vec![mon_a(1), mon_b(1)],
            std_windows(),
            Some(1),
            RescanTrigger::Periodic,
        ),
    );
    assert!(focus_targets(&late.effects).is_empty(), "{:?}", late.effects);
}

/// A grant to the window the last look saw key, issued while a grant to
/// another window may still land (it was overtaken, but may have been sent
/// already): the newest grant is expected, so a look that catches the old
/// one landing waits for it rather than granting again over it.
#[test]
fn a_grant_that_overtakes_another_is_expected_even_where_focus_already_is() {
    let s = booted(&[3, 1, 2]); // w2 key
    let to_1 = update(&s, &hotkey(HotkeyAction::MruWorkspace)).state; // grant to w1
    let deferred = update(&to_1, &gesture(Gesture::SystemSwitch)).state;
    let stale = look(&deferred, 2).state;
    let viewed = update(&stale, &hotkey(HotkeyAction::ViewMonitorNext));
    assert_eq!(focus_targets(&viewed.effects), vec![wid(2)]);

    let on_2 = VirtualMonitors {
        count: 2,
        viewed: vm(2),
        enabled: true,
    };
    let late = update(
        &viewed.state,
        &observed_view(
            on_2,
            vec![mon_a(1), mon_b(1)],
            std_windows(),
            Some(1),
            RescanTrigger::Periodic,
        ),
    );
    assert!(focus_targets(&late.effects).is_empty(), "{:?}", late.effects);
}

fn switch_targets(effects: &[Effect]) -> Vec<WorkspaceId> {
    effects
        .iter()
        .filter_map(|e| match e {
            Effect::SwitchWorkspace { target, .. } => Some(*target),
            _ => None,
        })
        .collect()
}

/// A switch steers the next press only while it is under way. One the
/// executor reports failed or timed out, one that no look confirmed within its lifetime,
/// and every one in flight when the kill switch fires stop being where Ordo
/// is taking the user: the next press steps from the workspace on screen.
#[test]
fn a_switch_steers_presses_only_while_it_is_under_way() {
    let s = booted(&[1]);
    let on_1 = || observed(vec![mon_a(1), mon_b(1)], std_windows(), Some(1), RescanTrigger::Periodic);

    let failed = update(&s, &hotkey(HotkeyAction::WorkspaceNext));
    let refused = update(
        &failed.state,
        &Event::EffectResult {
            at: ts(),
            op: switch_op(&failed.effects),
            outcome: OpOutcome::Failed {
                detail: "no such space".into(),
            },
        },
    );
    let after_failure = update(&refused.state, &hotkey(HotkeyAction::WorkspaceNext));
    assert_eq!(switch_targets(&after_failure.effects), [ws(2)]);

    let timed_out = update(&s, &hotkey(HotkeyAction::WorkspaceNext));
    let unanswered = update(
        &timed_out.state,
        &Event::EffectResult {
            at: ts(),
            op: switch_op(&timed_out.effects),
            outcome: OpOutcome::Timeout,
        },
    );
    let after_timeout = update(&unanswered.state, &hotkey(HotkeyAction::WorkspaceNext));
    assert_eq!(switch_targets(&after_timeout.effects), [ws(2)]);

    let lost = update(&s, &hotkey(HotkeyAction::WorkspaceNext));
    let mut unseen = lost.state;
    for _ in 0..6 {
        unseen = update(&unseen, &on_1()).state;
    }
    let after_loss = update(&unseen, &hotkey(HotkeyAction::WorkspaceNext));
    assert_eq!(switch_targets(&after_loss.effects), [ws(2)]);

    let rescued = update(&s, &hotkey(HotkeyAction::WorkspaceNext));
    let rescued = update(&rescued.state, &Event::RescueEngaged { at: ts() });
    let engaged = update(&rescued.state, &Event::Engaged { at: ts() });
    let after_rescue = update(&engaged.state, &hotkey(HotkeyAction::WorkspaceNext));
    assert_eq!(switch_targets(&after_rescue.effects), [ws(2)]);
}

/// Each press of a burst makes the last one's switch and focus grant moot:
/// the app queues skip the older grant, and the older switch can never be
/// seen now. They are retired at the press, not left to expire a second
/// later as lost (run 48: 32 `op_lost` notes, half of them for grants). A
/// look that catches the overtaken grant landing anyway reads it as not
/// ours and waits for the newest grant, which is still on its way.
#[test]
fn a_burst_retires_the_switches_and_grants_it_overtakes() {
    let wins = vec![
        win(1, 100, 1, rect(100.0, 100.0)),
        win(2, 200, 2, rect(100.0, 100.0)),
        win(3, 300, 3, rect(100.0, 100.0)),
    ];
    let on = |active: u8, focused: u32| {
        observed(
            vec![mon_a(active), mon_b(active)],
            wins.clone(),
            Some(focused),
            RescanTrigger::Periodic,
        )
    };
    let s = update(
        &State::new(),
        &observed(vec![mon_a(1), mon_b(1)], wins.clone(), Some(1), RescanTrigger::Startup),
    )
    .state;
    let first = update(&s, &hotkey(HotkeyAction::WorkspaceNext));
    let second = update(&first.state, &hotkey(HotkeyAction::WorkspaceNext));
    let op_of = |effects: &[Effect], focus: bool| {
        effects
            .iter()
            .find_map(|e| match e {
                Effect::FocusWindow { op, .. } if focus => Some(*op),
                Effect::SwitchWorkspace { op, .. } if !focus => Some(*op),
                _ => None,
            })
            .unwrap()
    };
    assert!(second.notes.contains(&Note::OpSuperseded {
        op: op_of(&first.effects, false),
        by: op_of(&second.effects, false),
    }));
    assert!(second.notes.contains(&Note::OpSuperseded {
        op: op_of(&first.effects, true),
        by: op_of(&second.effects, true),
    }));

    let overtaken = update(&second.state, &on(3, 2));
    assert!(focus_targets(&overtaken.effects).is_empty(), "the newest grant is in flight");
    assert!(overtaken.notes.iter().any(|n| matches!(
        n,
        Note::External { delta: Delta::FocusChanged { to: Some(w), .. } } if *w == wid(2)
    )));

    let mut settled = overtaken.state;
    let mut notes = Vec::new();
    for _ in 0..6 {
        let step = update(&settled, &on(3, 3));
        notes.extend(step.notes);
        settled = step.state;
    }
    assert!(!notes.iter().any(|n| matches!(n, Note::OpLost { .. })), "{notes:?}");
    assert!(settled.pending.is_empty());
}

#[test]
fn switching_to_an_empty_workspace_gives_focus_to_the_desktop_and_holds_it() {
    // The sliver on workspace 7: switching away from Chrome to an empty
    // workspace left Chrome key, the backend spares the focused app when it
    // hides apps, and Chrome's windows parked for other workspaces lined the
    // screen's left edge. The desktop takes focus instead, granted with
    // the switch so the hiding that follows it finds nobody to spare; when
    // the app takes focus back, the desktop is re-granted rather than the
    // app followed or anything grabbed.
    let s = booted(&[1]);
    let away = update(&s, &hotkey(HotkeyAction::WorkspaceNext)); // ws2 is empty
    let on_a = |e: &Effect| matches!(e, Effect::FocusDesktop { display, .. } if *display == mid(1));
    assert!(away.effects.iter().any(on_a), "the desktop is granted: {:?}", away.effects);
    assert_eq!(count_switches(&away.effects), 1);
    assert_eq!(away.state.focus_intent(), FocusIntent::Desktop);

    let world = |focused| observed(vec![mon_a(2), mon_b(2)], std_windows(), focused, RescanTrigger::Periodic);
    let landed = update(&away.state, &world(None));
    assert!(landed.effects.is_empty(), "{:?}", landed.effects);

    let churn = update(&landed.state, &world(Some(1)));
    assert!(focus_targets(&churn.effects).is_empty());
    assert_eq!(count_switches(&churn.effects), 0, "never followed back");
    assert!(churn.effects.iter().any(on_a), "{:?}", churn.effects);
}

#[test]
fn switching_hands_focus_to_the_destinations_mru_window() {
    // w2 lives on workspace 2. Its focus is issued AFTER the switch: its app
    // may still be hidden, and fronting a hidden app un-hides it before its
    // parked windows can be held, so the grant follows the switch's moves
    // and un-hides.
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
    assert!(switch_pos < focus_pos);
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
            Effect::RestackWindows { order, .. } => Some(order.clone()),
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

/// An app moving one of its windows over another (re-applying a saved frame,
/// say) leaves them in whatever order the stack had. Once the window holds
/// still, the stack is checked by MRU, taking focus only where Ordo has
/// declared it (here it hasn't); while it is still moving, it isn't, and once
/// checked it isn't again.
#[test]
fn a_window_moved_by_its_app_gets_its_stack_checked_once_it_settles() {
    let at = |x, y| {
        let mut wins = std_windows();
        wins[2].snap.frame = rect(x, y);
        wins
    };
    let see = |s: &State, wins| {
        update(
            s,
            &observed(
                vec![mon_a(1), mon_b(1)],
                wins,
                Some(1),
                RescanTrigger::Periodic,
            ),
        )
    };
    let moving = see(&booted(&[2, 3, 1]), at(400.0, 300.0));
    let still_moving = see(&moving.state, at(200.0, 150.0));
    let settled = see(&still_moving.state, at(200.0, 150.0));
    let after = see(&settled.state, at(200.0, 150.0));

    assert_eq!(restacks(&moving.effects), vec![]);
    assert_eq!(restacks(&still_moving.effects), vec![]);
    assert_eq!(
        restacks(&settled.effects),
        vec![(vec![wid(1), wid(3), wid(2)], false)]
    );
    assert_eq!(restacks(&after.effects), vec![]);
}

/// A popup attached to a window, like Chrome's address-bar suggestions, has
/// no place of its own in the stack: the window server always
/// draws it just above its parent. Restacks list roots only and carry the
/// popup as attached to its root, and the popup resizing checks its root's
/// stack once it settles.
#[test]
fn an_attached_popup_travels_with_its_root_window() {
    let with_popup = |h| {
        let mut wins = std_windows();
        let frame = Rect {
            x: 150.0,
            y: 120.0,
            w: 300.0,
            h,
        };
        let mut popup = win(4, 100, 1, frame);
        popup.snap.parent = Some(wid(1));
        wins.push(popup);
        wins
    };
    let see = |s: &State, wins, focused| {
        update(
            s,
            &observed(
                vec![mon_a(1), mon_b(1)],
                wins,
                Some(focused),
                RescanTrigger::Periodic,
            ),
        )
    };
    let restacks = |fx: &[Effect]| {
        fx.iter()
            .filter_map(|e| match e {
                Effect::RestackWindows {
                    order, attached, ..
                } => Some((order.clone(), attached.clone())),
                _ => None,
            })
            .collect::<Vec<_>>()
    };

    let opened = see(&booted(&[2, 3, 1]), with_popup(200.0), 1);
    let resizing = see(&opened.state, with_popup(160.0), 1);
    let settled = see(&resizing.state, with_popup(160.0), 1);

    assert_eq!(
        restacks(&settled.effects),
        vec![(vec![wid(1), wid(3), wid(2)], vec![(wid(4), wid(1))])]
    );
}

fn restacks(effects: &[Effect]) -> Vec<(Vec<WindowId>, bool)> {
    effects
        .iter()
        .filter_map(|e| match e {
            Effect::RestackWindows {
                order, focus_top, ..
            } => Some((order.clone(), *focus_top)),
            _ => None,
        })
        .collect()
}

/// w1 alone on workspace 1, w3 and w2 on workspace 2; history [1, 3, 2].
fn split_workspaces() -> (State, Vec<Win>) {
    let mut wins = std_windows();
    wins[1].workspace = ws(2);
    wins[2].workspace = ws(2);
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
    (s, wins)
}

/// A world first seen with w1 key: every window is new, and w1, born
/// focused, is declared.
fn born(view: VirtualMonitors, monitors: Vec<Mon>, windows: Vec<Win>) -> State {
    update(
        &State::new(),
        &observed_view(view, monitors, windows, Some(1), RescanTrigger::Periodic),
    )
    .state
}

/// Commands only declare; the stack follows from what they declared. Each
/// command that changes what is on screen, or who is on top, ends in one
/// restack: the declared focus on top and, behind it, what the declared
/// workspace and monitors show, by MRU. It comes after the switches, views
/// and grants it orders, since the shell waits for those to land.
#[test]
fn every_command_leaves_its_focus_on_top_of_the_visible_mru_order() {
    let (split, _) = split_workspaces();
    let together = booted(&[2, 3, 1]); // history [1, 3, 2], all on ws 1
    // w1 born focused, so declared; w2 hidden on monitor 2 until the toggle.
    let laptop_declared = born(laptop(1, true), vec![mon_a(1)], undocked_windows());
    let laptop = undocked(&[3, 2, 1]); // history [1, 2, 3]; w2 hidden on monitor 2
    // Three monitors on two displays; w2 on hidden monitor 3 until the merge.
    let three = born(
        VirtualMonitors {
            count: 3,
            viewed: vm(1),
            enabled: true,
        },
        vec![mon_a(1), mon_b(1)],
        vec![
            win(1, 100, 1, rect(100.0, 100.0)),
            on_monitor(win(2, 200, 1, rect(2000.0, 100.0)), 3),
            win(3, 100, 1, rect(2400.0, 500.0)),
        ],
    );
    let merge = HotkeyAction::MergeMonitors {
        from: vm(3),
        into: vm(1),
    };
    let cases = [
        ("switch", &split, HotkeyAction::WorkspaceNext, vec![3, 2]),
        ("carry", &split, HotkeyAction::CarryFocusedToWorkspaceNext, vec![1, 3, 2]),
        ("alt-tab", &together, HotkeyAction::MruWorkspace, vec![3, 1, 2]),
        ("demote", &together, HotkeyAction::MruDemote, vec![3, 2, 1]),
        ("view", &laptop, HotkeyAction::ViewMonitorNext, vec![2]),
        ("move to monitor", &laptop, HotkeyAction::MoveFocusedToMonitorNext, vec![1, 2]),
        ("toggle off", &laptop_declared, HotkeyAction::ToggleVirtualMonitors, vec![1, 2, 3]),
        ("merge", &three, merge, vec![1, 2, 3]),
    ];
    for (name, s, action, expected) in cases {
        let step = update(s, &hotkey(action));
        let expected: Vec<WindowId> = expected.into_iter().map(wid).collect();
        assert_eq!(step.state.focus_intent(), FocusIntent::Window(expected[0]), "{name}");
        assert_eq!(restacks(&step.effects), vec![(expected, true)], "{name}");
        let restack_at = step
            .effects
            .iter()
            .position(|e| matches!(e, Effect::RestackWindows { .. }))
            .unwrap();
        let ordered = step.effects.iter().rposition(|e| {
            matches!(
                e,
                Effect::SwitchWorkspace { .. }
                    | Effect::ViewMonitor { .. }
                    | Effect::FocusWindow { .. }
            )
        });
        assert!(ordered.is_none_or(|i| i < restack_at), "{name}: {:?}", step.effects);
    }
}

/// Every restack supersedes the one in flight. So a look that only sees the
/// world — before a command, while a switch lands, after it has — never
/// sends one, or it would cut the switch's restack short.
#[test]
fn a_look_that_changes_no_intent_emits_no_restack() {
    let (s, wins) = split_workspaces();
    let quiet = update(
        &s,
        &observed(vec![mon_a(1), mon_b(1)], wins.clone(), Some(1), RescanTrigger::Periodic),
    );
    assert_eq!(restacks(&quiet.effects), vec![]);

    let switched = update(&s, &hotkey(HotkeyAction::WorkspaceNext));
    assert_eq!(restacks(&switched.effects).len(), 1);
    let mut s = switched.state;
    for focused in [1, 3, 3] {
        let step = update(
            &s,
            &observed(vec![mon_a(2), mon_b(2)], wins.clone(), Some(focused), RescanTrigger::Periodic),
        );
        assert_eq!(restacks(&step.effects), vec![], "focus on w{focused}");
        s = step.state;
    }
}

/// An app moving a window while a switch is landing must not restack over
/// the switch's own restack, which would lose its focus take-back. The
/// settled move is checked once the switch's grant has landed, against the
/// stack the switch declared.
#[test]
fn a_settled_outside_move_waits_for_the_switch_in_flight() {
    let (s, mut wins) = split_workspaces();
    let switched = update(&s, &hotkey(HotkeyAction::WorkspaceNext)).state;
    let look = |s: &State, wins: &[Win], focused| {
        update(
            s,
            &observed(vec![mon_a(2), mon_b(2)], wins.to_vec(), Some(focused), RescanTrigger::Periodic),
        )
    };

    // The grant to w3 is still on its way while w2's app re-places it.
    wins[1].snap.frame = rect(2200.0, 300.0);
    let moving = look(&switched, &wins, 1);
    let settled = look(&moving.state, &wins, 1);
    let landed = look(&settled.state, &wins, 3);

    assert_eq!(restacks(&moving.effects), vec![]);
    assert_eq!(restacks(&settled.effects), vec![]);
    assert_eq!(restacks(&landed.effects), vec![(vec![wid(3), wid(2)], true)]);
}

/// An app's AX read fails now and then, and one scan misses a window that is
/// still there. It keeps its place in the MRU order when it reappears, so
/// Alt+Tab and restacks don't treat it as the least recently used. A window
/// gone for longer than that really closed, and one that shows up with its
/// id later starts at the back.
#[test]
fn a_window_missing_from_one_scan_keeps_its_place() {
    let without_w3 = || {
        let mut wins = std_windows();
        wins.retain(|w| w.snap.id != wid(3));
        wins
    };
    let at = |s: &State, t: Ts, wins| {
        update(
            s,
            &observed_at(
                t,
                vec![mon_a(1), mon_b(1)],
                wins,
                Some(1),
                RescanTrigger::Periodic,
            ),
        )
        .state
    };
    let s = booted(&[2, 3, 1]); // history: [1, 3, 2]
    let t = ts();

    let missed = at(&s, t, without_w3());
    let back = at(&missed, plus_ms(t, 2_000), std_windows());
    let tab = update(&back, &hotkey(HotkeyAction::MruWorkspace));
    assert_eq!(focus_targets(&tab.effects), vec![wid(3)]);

    let closed = at(&back, plus_ms(t, 4_000), without_w3());
    let still_gone = at(&closed, plus_ms(t, 16_000), without_w3());
    let reborn = at(&still_gone, plus_ms(t, 18_000), std_windows());
    let tab = update(&reborn, &hotkey(HotkeyAction::MruWorkspace));
    assert_eq!(focus_targets(&tab.effects), vec![wid(2)]);
}

/// An app busy past the read's timeout lists no windows, and that is not
/// the same as having none (run 46: all six of kitty's windows dropped out
/// of one scan while kitty was the app the scan waited on). The shell names
/// such windows, where the window server still has them, and they stay in
/// the model as last seen; once the app answers without them, they are gone.
#[test]
fn a_window_its_app_did_not_answer_for_stays() {
    let s = booted(&[3, 1]);
    let look = |s: &State, unread: Vec<WindowId>| {
        let mut snap = world(&[mon_a(1), mon_b(1)], &std_windows(), Some(2));
        snap.windows.retain(|w| w.app == Pid(200));
        snap.unread = unread;
        update(
            s,
            &Event::WorldObserved {
                at: ts(),
                trigger: RescanTrigger::Periodic,
                snap,
            },
        )
    };

    let busy = look(&s, vec![wid(1), wid(3)]);
    assert!(busy.state.windows.contains_key(&wid(1)));
    assert!(busy.state.windows.contains_key(&wid(3)));
    assert!(!busy
        .notes
        .iter()
        .any(|n| matches!(n, Note::External { delta: Delta::WindowDestroyed(_) })));

    let answered = look(&busy.state, Vec::new());
    assert!(!answered.state.windows.contains_key(&wid(1)));
    assert!(answered
        .notes
        .contains(&Note::External { delta: Delta::WindowDestroyed(wid(1)) }));
}

/// A carry whose look finds the carried window's app not answering: the
/// window is kept from the last look, but it sits where the backend's word
/// puts it, so the carry is confirmed and nothing fights to move it again.
#[test]
fn a_carry_is_confirmed_while_the_carried_windows_app_does_not_answer() {
    let s = booted(&[1]);
    let carry = update(&s, &hotkey(HotkeyAction::CarryFocusedToWorkspaceNext));
    let move_op = carry
        .effects
        .iter()
        .find_map(|e| match e {
            Effect::AssignWindowToWorkspace { op, .. } => Some(*op),
            _ => None,
        })
        .unwrap();
    let mut wins = std_windows();
    wins[0].workspace = ws(2);
    let mut state = carry.state;
    let mut notes = Vec::new();
    let mut effects = Vec::new();
    for _ in 0..8 {
        let mut snap = world(&[mon_a(2), mon_b(2)], &wins, Some(1));
        snap.windows.retain(|w| w.app != Pid(100));
        snap.unread = vec![wid(1), wid(3)];
        let step = update(
            &state,
            &Event::WorldObserved {
                at: ts(),
                trigger: RescanTrigger::Periodic,
                snap,
            },
        );
        notes.extend(step.notes);
        effects.extend(step.effects);
        state = step.state;
    }
    assert!(notes.contains(&Note::SelfConfirmed { op: move_op }), "{notes:?}");
    assert!(!effects
        .iter()
        .any(|e| matches!(e, Effect::MoveWindowToWorkspace { .. })), "{effects:?}");
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
            Effect::RestackWindows { order, .. } => Some(order.clone()),
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
fn a_menu_bar_click_moves_focus_to_its_display_but_never_follows_a_hidden_window() {
    // w3 declared on the left display; the right display shows nothing on
    // this workspace; w2 is parked on workspace 2. Clicking the right
    // display's menu bar (its Ordo icon, say) makes macOS move focus to that
    // display — its desktop, since it is empty. That is the user's doing and
    // stands. Without the click, the same loss of focus is fought.
    let mut wins = std_windows();
    wins[1].workspace = ws(2);
    let obs = |focused: Option<u32>| {
        observed(
            vec![mon_a(1), mon_b(1)],
            wins.clone(),
            focused,
            RescanTrigger::Periodic,
        )
    };
    let s = update(&booted(&[3, 1]), &obs(Some(1))).state;
    let s = update(&s, &hotkey(HotkeyAction::MruWorkspace)).state;
    let s = update(&s, &obs(Some(3))).state;
    assert_eq!(s.focus_intent(), FocusIntent::Window(wid(3)));

    let unexplained = update(&s, &obs(None));
    assert_eq!(focus_targets(&unexplained.effects), vec![wid(3)], "fought");

    let menu_bar = Gesture::MenuBar {
        at: Point { x: 2500.0, y: 10.0 },
    };
    let clicked = update(&s, &gesture(menu_bar)).state;
    let moved = update(&clicked, &obs(None));
    assert!(moved.effects.is_empty(), "{:?}", moved.effects);

    // And a parked window keyed after a menu bar click is not followed.
    let clicked = update(&s, &gesture(menu_bar)).state;
    let landed = update(&clicked, &obs(Some(2)));
    assert_eq!(count_switches(&landed.effects), 0);
}

/// While a menu from the menu bar is open, the front app may report one of
/// its windows on a hidden workspace as focused. That is the menu tracking,
/// not the user going there and not an app grabbing focus: refocusing in
/// answer closes the menu under the user's cursor, so nothing is done. The
/// click that ends the menu may be one of its items — "System Settings…",
/// whose only window lives on workspace 2 — and the landing after it is the
/// user going there, followed as Cmd+Tab's would be, even though the click
/// fell where the menu hung over a visible window.
#[test]
fn an_open_menu_is_left_alone_and_the_app_its_item_opens_is_followed() {
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
    let menu = gesture(Gesture::MenuBar {
        at: Point { x: 20.0, y: 10.0 },
    });

    let open = update(&s, &menu).state;
    let tracking = update(&open, &obs(2));
    assert!(tracking.effects.is_empty(), "{:?}", tracking.effects);
    let still_open = update(&tracking.state, &obs(2));
    assert!(still_open.effects.is_empty(), "{:?}", still_open.effects);

    let item = update(&update(&s, &menu).state, &click(200.0, 200.0));
    assert!(item.notes.contains(&Note::GestureClassified {
        gesture: Gesture::MouseDown {
            at: Point { x: 200.0, y: 200.0 }
        },
        armed: true,
        within: None,
    }));
    let opened = update(&item.state, &obs(2));
    assert!(opened
        .effects
        .iter()
        .any(|e| matches!(e, Effect::SwitchWorkspace { target, .. } if *target == ws(2))));
    assert_eq!(opened.state.focus_intent(), FocusIntent::Window(wid(2)));

    // Once the menu is closed, a click into a window is that window's again.
    let after = update(&item.state, &click(200.0, 200.0)).state;
    let fling = update(&after, &obs(2));
    assert_eq!(count_switches(&fling.effects), 0);
    assert_eq!(focus_targets(&fling.effects), vec![wid(1)]);
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

    // A click outside every visible window — the desktop, say — can be
    // aimed at anything, and a hidden landing right after it is the user
    // navigating.
    let elsewhere = update(&s, &click(960.0, 1075.0));
    assert!(elsewhere.notes.contains(&Note::GestureClassified {
        gesture: Gesture::MouseDown {
            at: Point {
                x: 960.0,
                y: 1075.0
            }
        },
        armed: true,
        within: None,
    }));
    let clicked_out = elsewhere.state;
    let followed = update(&clicked_out, &obs(2));
    assert_eq!(count_switches(&followed.effects), 1);
    assert!(followed
        .notes
        .iter()
        .any(|n| matches!(n, Note::FollowedFocus { .. })));

    // A gesture explains exactly the observation after it. One uneventful
    // observation later, the same landing is a fling again.
    let quiet = update(&clicked_out, &obs(1)).state;
    let late = update(&quiet, &obs(2));
    assert_eq!(count_switches(&late.effects), 0, "the gesture was spent");
    assert_eq!(focus_targets(&late.effects), vec![wid(1)]);

    // A command in between spends it too: that click, then Cmd+Right to the
    // empty workspace 3. Its own post-effect snapshot shows w1 still focused
    // — parking does not defocus — and now hidden. That is the switch's
    // doing, not the click's; bouncing back to workspace 1 would be wrong.
    let switched = update(
        &clicked_out,
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
    // confirmed, then Cmd+W. Focus is handed on to w3's monitor's next
    // window rather than held, and lands in the same place.
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

/// Run 50's shape: the user works on the right monitor (w2, w4, w5), and
/// w4's app (pid 100) also has w1 and w3 on the left.
fn closing_world() -> Vec<Win> {
    let mut wins = std_windows();
    wins.push(win(4, 100, 1, rect(2400.0, 500.0)));
    wins.push(win(5, 300, 1, rect(3000.0, 100.0)));
    wins
}

/// Boot `wins` and click each window of `focus_seq` in turn.
fn booted_with(wins: &[Win], focus_seq: &[u32]) -> State {
    let mut s = update(
        &State::new(),
        &observed(
            vec![mon_a(1), mon_b(1)],
            wins.to_vec(),
            None,
            RescanTrigger::Startup,
        ),
    )
    .state;
    for f in focus_seq {
        s = used(&s, *f, vec![mon_a(1), mon_b(1)], wins.to_vec());
    }
    s
}

fn without(wins: &[Win], gone: &[u32]) -> Vec<Win> {
    wins.iter()
        .filter(|w| !gone.iter().any(|g| w.snap.id == wid(*g)))
        .cloned()
        .collect()
}

fn click_on(s: &State, wins: &[Win], w: u32) -> State {
    let at = wins.iter().find(|x| x.snap.id == wid(w)).unwrap().snap.frame.center();
    update(s, &click(at.x, at.y)).state
}

#[test]
fn closing_the_focused_window_hands_focus_to_its_monitors_next_window() {
    // Run 50 seq 159-161: a click closes Chrome's window on the right
    // monitor, the look shows it gone and nothing key, and 2.7 s later
    // macOS fronts Chrome's window on the LEFT monitor. The user was working
    // on the right. Focus goes to the right monitor's next window in the MRU
    // order (w2), not the next one overall (w1, on the left), and it heads
    // the workspace's restack. The left display's order is unchanged, which
    // the stacking worker sees and leaves alone.
    let wins = closing_world();
    let s = booted_with(&wins, &[5, 2, 1, 4]); // history [4, 1, 2, 5, 3]
    let s = click_on(&s, &wins, 4);
    let step = update(
        &s,
        &observed(
            vec![mon_a(1), mon_b(1)],
            without(&wins, &[4]),
            None,
            RescanTrigger::Periodic,
        ),
    );
    assert_eq!(focus_targets(&step.effects), vec![wid(2)]);
    assert!(step.effects.iter().any(|e| matches!(
        e,
        Effect::RestackWindows { order, focus_top: true, .. }
            if *order == vec![wid(2), wid(1), wid(5), wid(3)]
    )));
    assert_eq!(step.state.focus_intent(), FocusIntent::Window(wid(2)));
    assert!(step.notes.contains(&Note::FocusHandedOn {
        closed: wid(4),
        to: Some(wid(2)),
    }));
}

#[test]
fn the_apps_own_rekey_after_a_close_is_fought_and_not_recorded() {
    // The app keys its window on the left in the very look that shows the
    // close, inside the second after the click that closed it, which would
    // otherwise pass for where that click went. It is the close's fallout:
    // Ordo's choice is granted over it, the MRU order is untouched, and when
    // the app keeps w1 key after the grant's expectation has lapsed, the
    // grant is re-issued.
    let wins = closing_world();
    let s = booted_with(&wins, &[5, 2, 1, 4]);
    let s = click_on(&s, &wins, 4);
    let at = ts();
    let rekeyed = update(
        &s,
        &observed_at(
            at,
            vec![mon_a(1), mon_b(1)],
            without(&wins, &[4]),
            Some(1),
            RescanTrigger::Periodic,
        ),
    );
    assert_eq!(focus_targets(&rekeyed.effects), vec![wid(2)]);
    // Only Ordo's choice moved.
    let mut expected = vec![wid(2)];
    expected.extend(history(&s).into_iter().filter(|w| *w != wid(2)));
    assert_eq!(history(&rekeyed.state), expected);

    let later = update(
        &rekeyed.state,
        &observed_at(
            plus_ms(at, 2_700),
            vec![mon_a(1), mon_b(1)],
            without(&wins, &[4]),
            Some(1),
            RescanTrigger::Periodic,
        ),
    );
    assert_eq!(focus_targets(&later.effects), vec![wid(2)]);
    assert!(later.notes.contains(&Note::FocusReasserted { window: wid(2) }));
    for step in [&rekeyed, &later] {
        assert!(!step
            .notes
            .iter()
            .any(|n| matches!(n, Note::LandingExplained { .. })));
    }
    assert_eq!(history(&later.state), expected);
}

#[test]
fn closing_the_last_window_on_a_monitor_gives_that_monitor_its_desktop() {
    // w2 is all the right monitor holds. Once it closes, the right
    // monitor's desktop takes focus (its monitor becomes the anchor, where a
    // desktop declaration is held), and the app that keys w1 on the left
    // afterwards is fought for it.
    let s = booted(&[1, 2]);
    let s = click_on(&s, &std_windows(), 2);
    let at = ts();
    let closed = update(
        &s,
        &observed_at(
            at,
            vec![mon_a(1), mon_b(1)],
            without(&std_windows(), &[2]),
            None,
            RescanTrigger::Periodic,
        ),
    );
    assert!(focus_targets(&closed.effects).is_empty());
    assert!(closed
        .effects
        .iter()
        .any(|e| matches!(e, Effect::FocusDesktop { display, .. } if *display == mid(2))));
    assert_eq!(closed.state.focus_intent(), FocusIntent::Desktop);
    assert!(closed.notes.contains(&Note::FocusHandedOn {
        closed: wid(2),
        to: None,
    }));

    let viewing_b = VirtualMonitors {
        count: 2,
        viewed: vm(2),
        enabled: true,
    };
    let rekeyed = update(
        &closed.state,
        &Event::WorldObserved {
            at: plus_ms(at, 1_500),
            trigger: RescanTrigger::Periodic,
            snap: world_view(
                viewing_b,
                &[mon_a(1), mon_b(1)],
                &without(&std_windows(), &[2]),
                Some(1),
            ),
        },
    );
    assert!(rekeyed.notes.contains(&Note::DesktopReasserted {
        display: mid(2),
        from: wid(1),
    }));
}

#[test]
fn closing_an_unfocused_window_changes_nothing() {
    let s = booted(&[2, 1]); // w1 key
    let step = update(
        &s,
        &observed(
            vec![mon_a(1), mon_b(1)],
            without(&std_windows(), &[2]),
            Some(1),
            RescanTrigger::Periodic,
        ),
    );
    assert!(step.effects.is_empty());
    assert_eq!(step.state.focus_intent(), s.focus_intent());
}

#[test]
fn a_click_on_another_window_in_the_same_look_as_a_close_is_where_the_user_went() {
    // An app with no observer is seen only by the periodic look, seconds
    // apart: the user closes w4 and clicks w5 before any look, and the look
    // finds the app has keyed w1. Nothing is handed on, and when w5 comes up
    // key it is the click's.
    let wins = closing_world();
    let s = booted_with(&wins, &[5, 2, 1, 4]);
    let s = click_on(&s, &wins, 4);
    let s = click_on(&s, &wins, 5);
    let rest = without(&wins, &[4]);
    let closed = update(
        &s,
        &observed(vec![mon_a(1), mon_b(1)], rest.clone(), Some(1), RescanTrigger::Periodic),
    );
    assert!(focus_targets(&closed.effects).is_empty());
    let landed = update(
        &closed.state,
        &observed(vec![mon_a(1), mon_b(1)], rest, Some(5), RescanTrigger::Periodic),
    );
    assert!(landed.notes.contains(&Note::LandingExplained {
        window: wid(5),
        by: Input::Click,
    }));
    assert_eq!(history(&landed.state)[0], wid(5));
}

#[test]
fn a_dock_click_or_a_launcher_after_cmd_w_is_where_the_user_went() {
    // Cmd+W closes w4, and before the look the user either clicks the Dock
    // (Chrome's icon, bringing up its w1) or types into a launcher that
    // brings up w5's app. Either way the look shows where the user went, and
    // it stands. Cmd+W alone, with the app keying its own w1, is the close's
    // fallout and is handed on as ever, and so is File > Close: the click on
    // the menu item hits no window either, but it is the menu's.
    let wins = closing_world();
    let s = booted_with(&wins, &[5, 2, 1, 4]);
    let typed = update(&s, &gesture(Gesture::Key)).state;
    let rest = without(&wins, &[4]);
    let look_at = |s: &State, focused: u32| {
        update(
            s,
            &observed(vec![mon_a(1), mon_b(1)], rest.clone(), Some(focused), RescanTrigger::Periodic),
        )
    };

    let docked = look_at(&update(&typed, &dock(960.0, 1075.0)).state, 1);
    assert!(focus_targets(&docked.effects).is_empty());
    assert_eq!(history(&docked.state)[0], wid(1));

    let launched = look_at(&update(&typed, &gesture(Gesture::Key)).state, 5);
    assert!(focus_targets(&launched.effects).is_empty());
    assert_eq!(launched.state.focused, Some(wid(5)));

    let rekeyed = look_at(&typed, 1);
    assert_eq!(focus_targets(&rekeyed.effects), vec![wid(2)]);

    let menu = update(&s, &gesture(Gesture::MenuBar { at: Point { x: 2000.0, y: 5.0 } })).state;
    let menu_closed = look_at(&update(&menu, &click(2050.0, 60.0)).state, 1);
    assert_eq!(focus_targets(&menu_closed.effects), vec![wid(2)]);
}

#[test]
fn quitting_the_focused_app_hands_focus_on_within_its_monitor() {
    // Cmd+Q in app 100 (w4 key, right monitor) takes all its windows, and
    // macOS keys an app of its own choosing (w5's). That looks like a
    // launcher's pick after a key press, but nothing was launched: the app
    // the key was typed into is gone. Focus goes to the right monitor's next
    // window in the MRU order, as for Cmd+W.
    let wins = closing_world();
    let s = booted_with(&wins, &[5, 2, 1, 4]);
    let typed = update(&s, &gesture(Gesture::Key)).state;
    let step = update(
        &typed,
        &observed(
            vec![mon_a(1), mon_b(1)],
            without(&wins, &[1, 3, 4]),
            Some(5),
            RescanTrigger::Periodic,
        ),
    );
    assert_eq!(focus_targets(&step.effects), vec![wid(2)]);
    assert_eq!(step.state.focus_intent(), FocusIntent::Window(wid(2)));
}

/// kitty (app 200) has a window w2 parked on workspace 2; the user works in
/// w1 on workspace 1. App 300 has w5 parked too. Returned with the windows
/// once kitty has opened w4 on workspace 1.
fn kitty_at_work() -> (State, Vec<Win>) {
    let before = vec![
        win(1, 100, 1, rect(100.0, 100.0)),
        win(2, 200, 2, rect(2000.0, 100.0)),
        win(3, 300, 1, rect(2400.0, 300.0)),
        win(5, 300, 2, rect(2600.0, 400.0)),
    ];
    let mons = || vec![mon_a(1), mon_b(1)];
    let s = update(&State::new(), &observed(mons(), before.clone(), None, RescanTrigger::Startup)).state;
    let s = used(&s, 1, mons(), before.clone());
    let mut after = before;
    after.push(win(4, 200, 1, rect(2100.0, 200.0)));
    (s, after)
}

/// The user asks kitty for a new window. The new w4 is born unfocused, kitty
/// then keys w2, the window it last had key, and only about a second later
/// keys w4 (runs 54 and 55).
fn kitty_opening_a_window() -> (State, Vec<Win>, Ts) {
    let (s, after) = kitty_at_work();
    let mons = || vec![mon_a(1), mon_b(1)];
    let born_at = ts();
    let born = update(
        &s,
        &observed_at(born_at, mons(), after.clone(), None, RescanTrigger::Periodic),
    );
    assert!(focus_targets(&born.effects).is_empty());
    (born.state, after, born_at)
}

#[test]
fn a_new_window_its_app_keys_a_beat_late_is_where_focus_goes() {
    // While kitty is keying its old, parked window on the way to the new
    // one, Ordo neither takes focus back nor follows it to workspace 2. Once
    // kitty keys the new window, it is declared as a birth with focus would
    // be: top of the MRU order and of the restack.
    let (s, wins, born_at) = kitty_opening_a_window();
    let mons = || vec![mon_a(1), mon_b(1)];
    let detour = update(
        &s,
        &observed_at(plus_ms(born_at, 30), mons(), wins.clone(), Some(2), RescanTrigger::Periodic),
    );
    assert!(focus_targets(&detour.effects).is_empty(), "{:?}", detour.effects);
    assert_eq!(count_switches(&detour.effects), 0);

    let keyed = update(
        &detour.state,
        &observed_at(plus_ms(born_at, 1_300), mons(), wins, Some(4), RescanTrigger::Periodic),
    );
    assert_eq!(keyed.state.focus_intent(), FocusIntent::Window(wid(4)));
    assert_eq!(history(&keyed.state)[0], wid(4));
    // kitty raised and keyed it itself: nothing to grant or restack.
    assert!(focus_targets(&keyed.effects).is_empty());
    assert!(restacks(&keyed.effects).is_empty());
}

#[test]
fn the_grace_covers_only_the_opening_app_and_ends_at_a_command() {
    // Another app flinging focus to its parked window during kitty's beat is
    // held as ever. And once the user commands elsewhere, kitty keying its
    // new window later is kitty's doing, not the opening the user asked for.
    let (s, wins, born_at) = kitty_opening_a_window();
    let mons = || vec![mon_a(1), mon_b(1)];
    let fling = update(
        &s,
        &observed_at(plus_ms(born_at, 30), mons(), wins.clone(), Some(5), RescanTrigger::Periodic),
    );
    assert_eq!(focus_targets(&fling.effects), vec![wid(1)]);

    let moved_on = update(&s, &hotkey(HotkeyAction::MruWorkspace)).state;
    let keyed = update(
        &moved_on,
        &observed_at(plus_ms(born_at, 1_300), mons(), wins, Some(4), RescanTrigger::Periodic),
    );
    assert!(!keyed.notes.iter().any(|n| matches!(n, Note::LandingExplained { by: Input::Birth, .. })));
    assert_ne!(keyed.state.focus_intent(), FocusIntent::Window(wid(4)));
}

#[test]
fn an_app_that_never_keys_its_new_window_is_held_after_the_grace() {
    // kitty still has its parked window key three seconds after the birth:
    // that is a fling after all, and focus goes back to the visible MRU head.
    let (s, wins, born_at) = kitty_opening_a_window();
    let held = update(
        &s,
        &observed_at(plus_ms(born_at, 3_100), vec![mon_a(1), mon_b(1)], wins, Some(2), RescanTrigger::Periodic),
    );
    assert_eq!(focus_targets(&held.effects), vec![wid(1)]);
}

/// Run 55 seq 3205-3213: the user clicks kitty's icon in the Dock, then New
/// Window in its Dock menu. Both clicks are the Dock's, which license a
/// follow, and kitty keys its parked w2 in the very look that shows w4 born,
/// the one those clicks arm. That is the opening, not the user going to
/// workspace 2: nothing is followed or fought, and when kitty keys w4 it is
/// declared.
#[test]
fn a_window_opened_from_the_dock_menu_is_where_focus_goes_with_no_follow() {
    let (s, wins) = kitty_at_work();
    let mons = || vec![mon_a(1), mon_b(1)];
    let s = update(&s, &dock(1053.0, 1050.0)).state;
    let s = update(&s, &dock(1120.0, 850.0)).state;
    let born_at = ts();
    let detour = update(
        &s,
        &observed_at(born_at, mons(), wins.clone(), Some(2), RescanTrigger::Periodic),
    );
    assert_eq!(count_switches(&detour.effects), 0);
    assert!(focus_targets(&detour.effects).is_empty(), "{:?}", detour.effects);

    let keyed = update(
        &detour.state,
        &observed_at(plus_ms(born_at, 1_300), mons(), wins, Some(4), RescanTrigger::Periodic),
    );
    assert!(keyed.notes.contains(&Note::LandingExplained {
        window: wid(4),
        by: Input::Birth,
    }));
    assert_eq!(keyed.state.focus_intent(), FocusIntent::Window(wid(4)));
    assert_eq!(count_switches(&keyed.effects), 0);
}

/// The Dock is how the user goes to an app whose window lives elsewhere. A
/// click on it, even where it lies over a window (here w1, as an auto-hidden
/// Dock does), is no click into that window: the app keying its window
/// parked on workspace 2 is followed there, as after Cmd+Tab.
#[test]
fn a_dock_click_over_a_window_is_navigation() {
    let mut wins = std_windows();
    wins[1].workspace = ws(2);
    let obs = |focused: u32| {
        observed(vec![mon_a(1), mon_b(1)], wins.clone(), Some(focused), RescanTrigger::Periodic)
    };
    let s = update(&booted(&[1]), &obs(1)).state;
    let docked = update(&s, &dock(200.0, 200.0));
    assert!(docked.notes.contains(&Note::GestureClassified {
        gesture: Gesture::Dock {
            at: Point { x: 200.0, y: 200.0 }
        },
        armed: true,
        within: None,
    }));
    let followed = update(&docked.state, &obs(2));
    assert!(followed
        .effects
        .iter()
        .any(|e| matches!(e, Effect::SwitchWorkspace { target, .. } if *target == ws(2))));
    assert!(followed.notes.contains(&Note::FollowedFocus {
        window: wid(2),
        target: ws(2),
        monitor: None,
    }));
}

/// A Dock click names no window, not even the one it fell over. The app it
/// brings up keying w3 is its doing, and where the user went; w1, beneath the
/// click, coming up key a look later is not.
#[test]
fn a_dock_click_explains_where_focus_goes_but_not_the_window_beneath_it() {
    let s = booted(&[3, 1]);
    let obs = |focused: u32| {
        observed(vec![mon_a(1), mon_b(1)], std_windows(), Some(focused), RescanTrigger::Periodic)
    };
    let docked = update(&s, &dock(300.0, 250.0)).state;
    let brought_up = update(&docked, &obs(3));
    assert!(brought_up.notes.contains(&Note::LandingExplained {
        window: wid(3),
        by: Input::Dock,
    }));
    let beneath = update(&brought_up.state, &obs(1));
    assert!(!beneath
        .notes
        .iter()
        .any(|n| matches!(n, Note::LandingExplained { .. })));
    assert_eq!(history(&beneath.state)[0], wid(3));
}

/// Quit from kitty's Dock menu while kitty's w4 is key on the right monitor.
/// Every kitty window goes, and macOS keys w5, which app 300 has parked on
/// workspace 2. The Dock click licenses a follow, but that pick is the quit's
/// fallout, as after Cmd+Q: focus goes to the right monitor's next window.
#[test]
fn quitting_the_focused_app_from_the_dock_hands_focus_on_within_its_monitor() {
    let (s, wins, _) = kitty_opening_a_window();
    let mons = || vec![mon_a(1), mon_b(1)];
    let s = used(&s, 4, mons(), wins.clone());
    let docked = update(&s, &dock(1120.0, 850.0)).state;
    let quit = update(
        &docked,
        &observed(mons(), without(&wins, &[2, 4]), Some(5), RescanTrigger::Periodic),
    );
    assert_eq!(count_switches(&quit.effects), 0);
    assert_eq!(focus_targets(&quit.effects), vec![wid(3)]);
    assert!(quit.notes.contains(&Note::FocusHandedOn {
        closed: wid(4),
        to: Some(wid(3)),
    }));
}

#[test]
fn a_window_floating_above_the_ordinary_layer_is_never_restacked() {
    // The screenshot tool's window (w4) floats at layer 3, above every
    // ordinary window whatever is raised; the restack worker reads only layer
    // 0 and would wait for it on every restack. It belongs to workspace 1 all
    // the same.
    let mut floating = win(4, 400, 1, rect(300.0, 600.0));
    floating.snap.layer = Some(3);
    let wins = vec![
        win(1, 100, 1, rect(100.0, 100.0)),
        floating,
        win(2, 200, 2, rect(2000.0, 100.0)),
    ];
    let mons = || vec![mon_a(1), mon_b(1)];
    let s = update(&State::new(), &observed(mons(), wins.clone(), None, RescanTrigger::Startup)).state;
    let s = used(&s, 1, mons(), wins.clone());
    let s = used(&s, 4, mons(), wins);
    let away = update(&s, &hotkey(HotkeyAction::WorkspaceSwitchTo(WorkspaceId(2))));
    let back = update(&away.state, &hotkey(HotkeyAction::WorkspaceSwitchTo(WorkspaceId(1))));
    let orders: Vec<_> = restacks(&away.effects).into_iter().chain(restacks(&back.effects)).collect();
    assert!(!orders.is_empty());
    assert!(orders.iter().all(|(order, _)| !order.contains(&wid(4))), "{orders:?}");
    assert_eq!(back.state.windows[&wid(4)].workspace, WorkspaceId(1));
}

/// kitty (app 100) is key in w1; its w2 is parked on workspace 2. The user
/// opens Ordo's menu, and kitty, losing the keyboard to it, names w2 as
/// focused (run 54, on every opening).
fn ordos_menu_opened_over_kitty() -> (State, Vec<Win>) {
    let wins = vec![
        win(1, 100, 1, rect(100.0, 100.0)),
        win(2, 100, 2, rect(2000.0, 100.0)),
        win(3, 300, 1, rect(2400.0, 300.0)),
    ];
    let mons = || vec![mon_a(1), mon_b(1)];
    let s = update(&State::new(), &observed(mons(), wins.clone(), None, RescanTrigger::Startup)).state;
    let s = used(&s, 1, mons(), wins.clone());
    let s = update(&s, &gesture(Gesture::MenuBar { at: Point { x: 1500.0, y: 10.0 } })).state;
    let s = update(&s, &gesture(Gesture::OwnMenu { open: true })).state;
    (s, wins)
}

#[test]
fn ordos_own_menu_neither_records_nor_fights_focus_while_open() {
    // Taking focus back while the menu is open closed it (run 54: after each
    // add-monitor pick, kitty's w2 was "held" against and the menu closed),
    // and the flip was written into the MRU order as the user's. A pick from
    // the menu, which arrives as a hotkey, changes neither.
    let (s, wins) = ordos_menu_opened_over_kitty();
    let mons = || vec![mon_a(1), mon_b(1)];
    let flipped = update(&s, &observed(mons(), wins.clone(), Some(2), RescanTrigger::Periodic));
    assert!(focus_targets(&flipped.effects).is_empty(), "{:?}", flipped.effects);
    let picked = update(
        &flipped.state,
        &hotkey(HotkeyAction::MoveWorkspace {
            from: WorkspaceId(3),
            to: WorkspaceId(2),
        }),
    );
    let after_pick = update(&picked.state, &observed(mons(), wins.clone(), Some(2), RescanTrigger::Periodic));
    assert!(focus_targets(&after_pick.effects).is_empty(), "{:?}", after_pick.effects);

    let closed = update(&after_pick.state, &gesture(Gesture::OwnMenu { open: false })).state;
    let back = update(&closed, &observed(mons(), wins, Some(1), RescanTrigger::Periodic));
    assert!(focus_targets(&back.effects).is_empty());
    assert_eq!(history(&back.state)[0], wid(1));
}

#[test]
fn a_flip_that_outlasts_ordos_menu_is_held_as_ever() {
    // Once the menu has closed, kitty still naming its parked window is an
    // ordinary fling, and focus goes back to the visible MRU head.
    let (s, wins) = ordos_menu_opened_over_kitty();
    let closed = update(&s, &gesture(Gesture::OwnMenu { open: false })).state;
    let held = update(&closed, &observed(vec![mon_a(1), mon_b(1)], wins, Some(2), RescanTrigger::Periodic));
    assert_eq!(focus_targets(&held.effects), vec![wid(1)]);
}

#[test]
fn a_key_window_ordo_does_not_manage_is_left_alone() {
    // Cmd+Shift+5 (a key press, so the declaration stands) brings up the
    // screenshot tool's capture bar, which Ordo does not manage: focus reads
    // as no window of the model, yet someone holds it. Taking it back for w1
    // would front w1's app over the capture. When the capture ends and w1 is
    // key again, nothing was fought and nothing recorded.
    let wins = std_windows();
    let mons = || vec![mon_a(1), mon_b(1)];
    let s = update(&State::new(), &observed(mons(), wins.clone(), None, RescanTrigger::Startup)).state;
    let s = used(&s, 1, mons(), wins.clone());
    let s = used(&s, 3, mons(), wins.clone());
    let s = update(&s, &hotkey(HotkeyAction::MruWorkspace)).state;
    assert_eq!(s.focus_intent(), FocusIntent::Window(wid(1)));
    let s = update(&s, &observed(mons(), wins.clone(), Some(1), RescanTrigger::Periodic)).state;
    let s = update(&s, &gesture(Gesture::Key)).state;
    let mut snap = world(&mons(), &wins, None);
    snap.key_unmanaged = true;
    let capturing = update(
        &s,
        &Event::WorldObserved {
            at: ts(),
            trigger: RescanTrigger::Periodic,
            snap,
        },
    );
    assert!(focus_targets(&capturing.effects).is_empty(), "{:?}", capturing.effects);
    assert_eq!(capturing.state.focus_intent(), FocusIntent::Window(wid(1)));

    let done = update(&capturing.state, &observed(mons(), wins, Some(1), RescanTrigger::Periodic));
    assert!(focus_targets(&done.effects).is_empty());
    assert_eq!(history(&done.state)[0], wid(1));
}

#[test]
fn closing_an_attached_window_leaves_focus_to_its_root() {
    // w6 hangs off w4 (an omnibox popup, a find bar). When it is dismissed
    // the app keys w4 again by itself, so there is nothing to hand on.
    let mut wins = closing_world();
    let mut popup = win(6, 100, 1, rect(2850.0, 700.0));
    popup.snap.parent = Some(wid(4));
    wins.push(popup);
    let s = booted_with(&wins, &[5, 2, 1, 4, 6]);
    assert_eq!(s.focused, Some(wid(6)));
    let step = update(
        &s,
        &observed(
            vec![mon_a(1), mon_b(1)],
            without(&wins, &[6]),
            Some(4),
            RescanTrigger::Periodic,
        ),
    );
    assert!(step.effects.is_empty(), "{:?}", step.effects);
}

#[test]
fn a_window_one_scan_missed_while_the_window_server_lists_it_is_not_a_close() {
    // The shell lists a window its app's AX read dropped as `unread` while
    // the window server still has it (run 45 seq 1969: a focused Slack
    // window gone for one scan). The model keeps it and nothing moves.
    let wins = closing_world();
    let s = booted_with(&wins, &[5, 2, 1, 4]);
    let mut snap = world(&[mon_a(1), mon_b(1)], &without(&wins, &[4]), None);
    snap.unread = vec![wid(4)];
    let step = update(
        &s,
        &Event::WorldObserved {
            at: ts(),
            trigger: RescanTrigger::Periodic,
            snap,
        },
    );
    assert!(step.effects.is_empty());
    assert!(step.state.windows.contains_key(&wid(4)));
}

#[test]
fn the_screen_going_away_is_not_a_close() {
    // The lock screen, or a native Space, empties a look of every app's
    // windows, and they all come back a few looks later. Nothing is handed
    // on, so nothing is fought when they return.
    let s = booted(&[1, 2]);
    let gone = update(
        &s,
        &observed(vec![mon_a(1), mon_b(1)], Vec::new(), None, RescanTrigger::Periodic),
    );
    assert!(gone.effects.is_empty());
    let back = update(
        &gone.state,
        &observed(
            vec![mon_a(1), mon_b(1)],
            std_windows(),
            Some(2),
            RescanTrigger::Periodic,
        ),
    );
    assert!(focus_targets(&back.effects).is_empty());
    assert!(!back
        .effects
        .iter()
        .any(|e| matches!(e, Effect::FocusDesktop { .. })));
}

#[test]
fn confirmed_switch_is_attributed_to_ourselves() {
    // To the empty workspace 2, whose desktop takes focus.
    let s = booted(&[1]);
    let step = update(&s, &hotkey(HotkeyAction::WorkspaceNext));
    let op = switch_op(&step.effects);
    let obs = update(
        &step.state,
        &observed(
            vec![mon_a(2), mon_b(2)],
            std_windows(),
            None,
            RescanTrigger::PostEffect { op },
        ),
    );
    assert!(obs.notes.contains(&Note::SelfConfirmed { op }));
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
    let mut step = update(&s, &hotkey(HotkeyAction::WorkspaceNext));
    let op = switch_op(&step.effects);
    let mut lost = false;
    // Long enough for the expectation's TTL to run out at TICK_MS per event.
    for _ in 0..5 {
        step = update(
            &step.state,
            &observed(
                vec![mon_a(1), mon_b(1)],
                std_windows(),
                None,
                RescanTrigger::Periodic,
            ),
        );
        lost |= step.notes.contains(&Note::OpLost { op });
    }
    assert!(lost);
    assert!(step.state.pending.is_empty());
}

#[test]
fn executor_failure_drops_the_pending_op() {
    let s = booted(&[1]);
    let step = update(&s, &hotkey(HotkeyAction::WorkspaceNext));
    let op = switch_op(&step.effects);
    let failed = update(
        &step.state,
        &Event::EffectResult {
            at: ts(),
            op,
            outcome: OpOutcome::Failed {
                detail: "gesture failed".into(),
            },
        },
    );
    assert!(!failed.state.pending.iter().any(|p| p.op == op));
    assert!(failed.notes.contains(&Note::OpFailed {
        op,
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
            |e| matches!(e, Effect::RestackWindows { order, .. } if order.last() == Some(&wid(1))),
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
            Effect::RestackWindows { order, .. } => Some(order.clone()),
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
    // Built by clicks, since a focus seen with no gesture behind it is not
    // the user's and no longer enters the history.
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
    s = used(&s, 2, vec![mon_a(2), mon_b(2)], wins.clone());
    s = update(
        &s,
        &observed(vec![mon_a(1), mon_b(1)], wins.clone(), None, RescanTrigger::Periodic),
    )
    .state;
    s = used(&s, 1, vec![mon_a(1), mon_b(1)], wins.clone());
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
fn mru_records_declarations_and_where_a_click_lands() {
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

    // After a click the OS owns the slot, and where the click lands is the
    // record.
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

fn history(s: &State) -> Vec<WindowId> {
    s.focus_history.iter().collect()
}

/// A look at the standard world, focus on `focused`.
fn look(s: &State, focused: u32) -> Step {
    update(
        s,
        &observed(
            vec![mon_a(1), mon_b(1)],
            std_windows(),
            Some(focused),
            RescanTrigger::Periodic,
        ),
    )
}

/// `look` repeated `n` times, each a tick later.
fn looks(s: &State, focused: u32, n: usize) -> State {
    (0..n).fold(s.clone(), |s, _| look(&s, focused).state)
}

/// Run 49 seq 11-23: the user clicked, and ten seconds later Chrome keyed
/// another of its windows for 27 ms. The core wrote that flicker into the
/// MRU order, and every later restack of the workspace put that window
/// second. A click explains the focus change right after it, and a landing
/// on the window it hit a little later, however many looks it takes the app
/// to come forward; nothing else the screen shows is where the user went.
#[test]
fn only_what_a_click_explains_enters_the_mru_order() {
    let s = booted(&[3, 2, 1]);
    assert_eq!(history(&s), [wid(1), wid(2), wid(3)]);

    let clicked = update(&s, &click(700.0, 600.0)).state; // inside w3
    let not_yet = looks(&clicked, 1, 5);
    let flicker = look(&not_yet, 2).state;
    assert_eq!(history(&flicker), [wid(1), wid(2), wid(3)]);

    let landed = look(&flicker, 3).state;
    assert_eq!(history(&landed), [wid(3), wid(1), wid(2)]);

    let later_flicker = look(&landed, 2).state;
    let back = look(&later_flicker, 3).state;
    assert_eq!(history(&back), [wid(3), wid(1), wid(2)]);
}

/// A click inside one window can front another: a link that opens in a
/// window already open, a button that raises a panel. What comes up key
/// right after the click is where it took the user.
#[test]
fn a_click_that_fronts_another_window_counts_for_that_window() {
    let s = booted(&[3, 2, 1]);
    let clicked = update(&s, &click(700.0, 600.0)).state; // inside w3
    let fronted = look(&clicked, 2).state;
    assert_eq!(history(&fronted), [wid(2), wid(1), wid(3)]);
}

/// A click whose window never came up key, and that the user moved on
/// from, does not explain that window keying itself much later.
#[test]
fn a_click_the_user_moved_on_from_explains_nothing_later() {
    let s = booted(&[3, 2, 1]);
    let clicked = update(&s, &click(700.0, 600.0)).state; // inside w3
    let moved_on = looks(&clicked, 1, 16);
    let rekeyed = look(&moved_on, 3).state;
    assert_eq!(history(&rekeyed), [wid(1), wid(2), wid(3)]);
}

/// Focus moved from the keyboard within the app typed into (a window
/// shortcut) is the user's: a key press explains the focus change right
/// after it, which becomes the declaration and the head of the MRU order,
/// and is not fought. The same change with no key press behind it, or long
/// after one, is an app's doing: pulled back, and not recorded.
#[test]
fn a_key_press_explains_the_focus_change_right_after_it_and_nothing_later() {
    let s = booted(&[3, 2, 1]);
    let to_2 = update(&s, &hotkey(HotkeyAction::MruWorkspace)).state;
    let to_1 = update(&look(&to_2, 2).state, &hotkey(HotkeyAction::MruWorkspace)).state;
    let on_1 = look(&to_1, 1).state;
    assert_eq!(on_1.focus_intent(), FocusIntent::Window(wid(1)));
    assert_eq!(history(&on_1), [wid(1), wid(2), wid(3)]);

    let pressed = update(&on_1, &gesture(Gesture::Key)).state;
    let shortcut = look(&pressed, 3); // w1's app keys its other window
    assert!(focus_targets(&shortcut.effects).is_empty(), "not fought");
    assert!(shortcut.notes.contains(&Note::LandingExplained {
        window: wid(3),
        by: Input::Key,
    }));
    assert_eq!(shortcut.state.focus_intent(), FocusIntent::Window(wid(3)));
    assert_eq!(history(&shortcut.state), [wid(3), wid(1), wid(2)]);

    let unprompted = look(&on_1, 3);
    assert_eq!(focus_targets(&unprompted.effects), vec![wid(1)]);
    assert_eq!(history(&unprompted.state), [wid(1), wid(2), wid(3)]);

    let long_before = looks(&update(&on_1, &gesture(Gesture::Key)).state, 1, 6);
    let late = look(&long_before, 3);
    assert_eq!(focus_targets(&late.effects), vec![wid(1)]);
    assert_eq!(history(&late.state), [wid(1), wid(2), wid(3)]);
}

/// An Outlook reminder or a Slack huddle grabbing focus while the user
/// types is that app's doing, however recent the last key press: typing
/// explains a change only within the app typed into. Pulled back, and kept
/// out of the MRU order.
#[test]
fn another_app_grabbing_focus_while_the_user_types_is_fought() {
    let s = booted(&[3, 2, 1]);
    let to_2 = update(&s, &hotkey(HotkeyAction::MruWorkspace)).state;
    let to_1 = update(&look(&to_2, 2).state, &hotkey(HotkeyAction::MruWorkspace)).state;
    let typing = update(&look(&to_1, 1).state, &gesture(Gesture::Key)).state;
    let typing = update(&look(&typing, 1).state, &gesture(Gesture::Key)).state;
    let grabbed = look(&typing, 2); // another app's window
    assert_eq!(focus_targets(&grabbed.effects), vec![wid(1)]);
    assert_eq!(grabbed.state.focus_intent(), FocusIntent::Window(wid(1)));
    assert_eq!(history(&grabbed.state), [wid(1), wid(2), wid(3)]);
}

/// A key typed into no window of the model (Spotlight, Raycast, Alfred, the
/// desktop) can send focus anywhere: the pick that lands next is the user's.
#[test]
fn a_launcher_pick_is_where_the_user_went() {
    let s = booted(&[3, 2, 1]);
    let launcher = look(&s, 1);
    let launcher = update(
        &launcher.state,
        &observed(vec![mon_a(1), mon_b(1)], std_windows(), None, RescanTrigger::Periodic),
    )
    .state;
    let typed = update(&launcher, &gesture(Gesture::Key)).state;
    let picked = look(&typed, 2);
    assert_eq!(history(&picked.state), [wid(2), wid(1), wid(3)]);
    assert!(picked.notes.contains(&Note::LandingExplained {
        window: wid(2),
        by: Input::Key,
    }));
}

/// Cmd+Tab can key any window, so it explains the first focus change after
/// it, if that comes soon. One that moved nothing must not be left open to
/// explain whatever an app keys on its own a while later.
#[test]
fn cmd_tab_explains_where_focus_goes_next_but_not_a_change_long_after() {
    let s = booted(&[3, 2, 1]);
    let switched = update(&s, &gesture(Gesture::SystemSwitch)).state;
    let landed = look(&switched, 2).state;
    assert_eq!(history(&landed), [wid(2), wid(1), wid(3)]);

    let unmoved = looks(&update(&landed, &gesture(Gesture::SystemSwitch)).state, 2, 6);
    let stolen = look(&unmoved, 3).state;
    assert_eq!(history(&stolen), [wid(2), wid(1), wid(3)]);
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
    assert_eq!(away.state.focus_intent(), FocusIntent::Desktop);

    // Parked: the desktop has focus, and nothing here to pull it anywhere.
    let parked = update(
        &away.state,
        &observed(
            vec![mon_a(2), mon_b(2)],
            std_windows(),
            None,
            RescanTrigger::PostEffect {
                op: switch_op(&away.effects),
            },
        ),
    );
    assert_eq!(count_switches(&parked.effects), 0);
    assert!(parked.effects.is_empty());

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

/// Observe `snap_of` twice: now, and again once the adoption delay has passed
/// — the shape of a drag that has come to rest. Returns both steps.
fn drag_then_settle(state: &State, snap_of: impl Fn() -> WorldSnapshot) -> (Step, Step) {
    let first = update(
        state,
        &Event::WorldObserved {
            at: ts(),
            trigger: RescanTrigger::Periodic,
            snap: snap_of(),
        },
    );
    let later = update(
        &first.state,
        &Event::WorldObserved {
            at: plus_ms(ts(), 1_100),
            trigger: RescanTrigger::Periodic,
            snap: snap_of(),
        },
    );
    (first, later)
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
    // The focus is issued AFTER the view, as a switch issues it after the
    // switch: the grant follows the moves and un-hides that reveal w2.
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
    assert!(view_at < focus_at);

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
    // alone, handed over all the same so the stacking worker can take focus
    // back from whatever the view's un-hides key.
    assert!(step.effects.iter().any(|e| matches!(
        e,
        Effect::RestackWindows { order, focus_top: true, .. } if *order == vec![wid(2)]
    )));

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
fn moving_onto_an_empty_monitor_gives_focus_to_its_desktop_and_holds_it_there() {
    // Three monitors on two displays, viewing 1+2. The user is in w1 (a
    // terminal) on monitor 1; monitor 2 has nothing on this workspace; w2
    // sits on hidden monitor 3. Cmd+Alt+K slides the view to 2+3, hiding w1.
    // Focus must land on the empty monitor itself — its display's desktop —
    // not on w2 across the way just because w2 is visible. And when macOS
    // hands focus back to w1's app (it does, on a click in the menu bar),
    // the desktop is re-granted rather than w2 grabbed or the view followed.
    let view = |viewed| VirtualMonitors {
        count: 3,
        viewed: vm(viewed),
        enabled: true,
    };
    let windows = vec![
        win(1, 100, 1, rect(100.0, 100.0)),
        on_monitor(win(2, 200, 1, rect(2000.0, 100.0)), 3),
    ];
    let world = |viewed, focused: Option<u32>| {
        observed_view(
            view(viewed),
            vec![mon_a(1), mon_b(1)],
            windows.clone(),
            focused,
            RescanTrigger::Periodic,
        )
    };
    let s = update(&State::new(), &world(1, Some(1))).state;

    let slide = update(&s, &hotkey(HotkeyAction::ViewMonitorNext));
    assert_eq!(view_targets(&slide.effects), vec![vm(2)]);
    assert!(
        focus_targets(&slide.effects).is_empty(),
        "nothing on another display is grabbed"
    );
    // Monitor 2 now stands on the LEFT display: the desktop focused is that one.
    assert!(slide
        .effects
        .iter()
        .any(|e| matches!(e, Effect::FocusDesktop { display, .. } if *display == mid(1))));
    assert_eq!(slide.state.focus_intent(), FocusIntent::Desktop);
    assert!(
        !slide
            .effects
            .iter()
            .any(|e| matches!(e, Effect::RestackWindows { focus_top: true, .. })),
        "the stacking worker is never told to focus a window over the desktop"
    );

    // The view lands and the desktop has focus: nothing more to do.
    let landed = update(&slide.state, &world(2, None));
    assert!(landed.effects.is_empty(), "{:?}", landed.effects);

    // macOS re-activates the terminal, now hidden with monitor 1.
    let churn = update(&landed.state, &world(2, Some(1)));
    assert!(focus_targets(&churn.effects).is_empty());
    assert!(
        view_targets(&churn.effects).is_empty(),
        "the view stays on 2+3"
    );
    assert!(churn
        .effects
        .iter()
        .any(|e| matches!(e, Effect::FocusDesktop { display, .. } if *display == mid(1))));
}

/// A switch's un-hides can hand focus to whichever app they reveal, undoing
/// the focus request that went first. So the stacking worker, which builds
/// the stack after them, is told its top is the window to focus — even when
/// the destination shows a single window and there is nothing else to order.
#[test]
fn a_switch_hands_its_focus_target_to_the_stacking_worker_even_alone() {
    let mut wins = std_windows();
    wins[1].workspace = ws(2); // w2 is workspace 2's only window
    let mut s = update(
        &State::new(),
        &observed(vec![mon_a(1), mon_b(1)], wins.clone(), None, RescanTrigger::Startup),
    )
    .state;
    s = update(
        &s,
        &observed(vec![mon_a(1), mon_b(1)], wins, Some(1), RescanTrigger::Periodic),
    )
    .state;

    let step = update(&s, &hotkey(HotkeyAction::WorkspaceNext));

    assert_eq!(focus_targets(&step.effects), vec![wid(2)]);
    assert!(step.effects.iter().any(|e| matches!(
        e,
        Effect::RestackWindows { order, focus_top: true, .. } if *order == vec![wid(2)]
    )));
}

#[test]
fn a_merge_from_the_menu_folds_one_monitor_into_another_while_one_is_spare() {
    // Three monitors on two displays; w2 lives on hidden monitor 3. Dragging
    // monitor 3 onto monitor 1 in the menu bar asks the backend for the fold,
    // and the backend's word of two monitors, w2 now on 1, is our own echo.
    // Then two monitors on two displays: nothing is spare, and a merge is
    // refused. A monitor is never merged into itself.
    let view = |count| VirtualMonitors {
        count,
        viewed: vm(1),
        enabled: true,
    };
    let three = observed_view(
        view(3),
        vec![mon_a(1), mon_b(1)],
        vec![
            win(1, 100, 1, rect(100.0, 100.0)),
            on_monitor(win(2, 200, 1, rect(2000.0, 100.0)), 3),
        ],
        Some(1),
        RescanTrigger::Periodic,
    );
    let s = update(&State::new(), &three).state;
    let merge = |s: &State, from, into| {
        update(
            s,
            &hotkey(HotkeyAction::MergeMonitors {
                from: vm(from),
                into: vm(into),
            }),
        )
    };
    assert!(merge(&s, 2, 2).effects.is_empty());

    let merged = merge(&s, 3, 1);
    assert!(merged.effects.iter().any(|e| matches!(
        e,
        Effect::MergeMonitors { from, into, .. } if *from == vm(3) && *into == vm(1)
    )));

    let two = observed_view(
        view(2),
        vec![mon_a(1), mon_b(1)],
        vec![
            win(1, 100, 1, rect(100.0, 100.0)),
            on_monitor(win(2, 200, 1, rect(300.0, 300.0)), 1),
        ],
        Some(1),
        RescanTrigger::Periodic,
    );
    let landed = update(&merged.state, &two).state;
    assert_eq!(landed.windows[&wid(2)].vmonitor, vm(1));
    assert!(landed.pending.is_empty(), "{:?}", landed.pending);
    assert!(merge(&landed, 2, 1).effects.is_empty(), "no monitor is spare");
}

#[test]
fn adding_a_monitor_from_the_menu_is_our_own_echo_when_the_backend_reports_it() {
    // Two monitors on two displays, the anchor on the right one. The menu's
    // plus asks the backend for a third; its word — three monitors, the
    // anchor moved to 1 so the viewport stays where it was — is the echo of
    // our own command, not an external change to follow.
    let view = |count, viewed| VirtualMonitors {
        count,
        viewed: vm(viewed),
        enabled: true,
    };
    let windows = || {
        vec![
            win(1, 100, 1, rect(100.0, 100.0)),
            on_monitor(win(2, 200, 1, rect(2000.0, 100.0)), 2),
        ]
    };
    let two = observed_view(view(2, 2), vec![mon_a(1), mon_b(1)], windows(), Some(2), RescanTrigger::Periodic);
    let s = update(&State::new(), &two).state;

    let added = update(&s, &hotkey(HotkeyAction::AddMonitor));
    assert!(added.effects.iter().any(|e| matches!(e, Effect::AddMonitor { .. })));

    let three = observed_view(view(3, 1), vec![mon_a(1), mon_b(1)], windows(), Some(2), RescanTrigger::Periodic);
    let landed = update(&added.state, &three);
    assert_eq!(landed.state.virtual_monitors, Some(view(3, 1)));
    assert!(landed.state.pending.is_empty(), "{:?}", landed.state.pending);
    assert!(
        !landed.notes.iter().any(|n| matches!(n, Note::External { .. })),
        "{:?}",
        landed.notes
    );
    assert!(!landed.effects.iter().any(|e| matches!(e, Effect::ViewMonitor { .. })));
}

#[test]
fn moving_a_workspace_from_the_menu_renumbers_them_and_the_echo_is_our_own() {
    // On workspace 3, the user drags it to the front. Nothing on screen
    // moves: the backend renumbers, 3 becomes 1 and 1 and 2 step over, and
    // its word is the echo of our own command, not a switch to follow. While
    // a switch is on its way the move is refused, since the switch aims at a
    // number the move would change.
    let before = vec![
        win(1, 100, 1, rect(100.0, 100.0)),
        win(2, 200, 2, rect(2000.0, 100.0)),
        win(3, 300, 3, rect(600.0, 500.0)),
    ];
    let s = update(
        &State::new(),
        &observed(vec![mon_a(3), mon_b(3)], before, Some(3), RescanTrigger::Periodic),
    )
    .state;
    let to_front = hotkey(HotkeyAction::MoveWorkspace {
        from: WorkspaceId(3),
        to: WorkspaceId(1),
    });

    let switching = update(&s, &hotkey(HotkeyAction::WorkspacePrev)).state;
    assert!(update(&switching, &to_front).effects.is_empty());

    let moved = update(&s, &to_front);
    assert!(moved.effects.iter().any(|e| matches!(
        e,
        Effect::MoveWorkspace { from, to, .. } if *from == WorkspaceId(3) && *to == WorkspaceId(1)
    )));
    assert_eq!(count_switches(&moved.effects), 0);

    let after = vec![
        win(1, 100, 2, rect(100.0, 100.0)),
        win(2, 200, 3, rect(2000.0, 100.0)),
        win(3, 300, 1, rect(600.0, 500.0)),
    ];
    let landed = update(
        &moved.state,
        &observed(vec![mon_a(1), mon_b(1)], after, Some(3), RescanTrigger::Periodic),
    );
    assert_eq!(landed.state.current_workspace(), Some(WorkspaceId(1)));
    assert!(landed.state.pending.is_empty(), "{:?}", landed.state.pending);
    assert!(
        !landed.notes.iter().any(|n| matches!(n, Note::External { .. })),
        "{:?}",
        landed.notes
    );
    assert_eq!(count_switches(&landed.effects), 0);
}

#[test]
fn moving_a_monitor_from_the_menu_shows_what_the_new_order_puts_in_view() {
    // Three monitors on two displays, 1 and 2 in view. Dragging monitor 3
    // between 1 and 2 makes it the second, so it comes into view and the old
    // 2, now 3, goes out. What comes up is restacked in MRU order, and the
    // backend's word is our own echo.
    let view = VirtualMonitors {
        count: 3,
        viewed: vm(1),
        enabled: true,
    };
    let s = update(
        &State::new(),
        &observed_view(
            view,
            vec![mon_a(1), mon_b(1)],
            vec![
                win(1, 100, 1, rect(100.0, 100.0)),
                on_monitor(win(2, 200, 1, rect(2000.0, 100.0)), 2),
                on_monitor(win(3, 300, 1, rect(2400.0, 300.0)), 3),
            ],
            Some(1),
            RescanTrigger::Periodic,
        ),
    )
    .state;
    let moved = update(
        &s,
        &hotkey(HotkeyAction::MoveMonitor {
            from: vm(3),
            to: vm(2),
        }),
    );
    assert!(moved.effects.iter().any(|e| matches!(
        e,
        Effect::MoveMonitor { from, to, .. } if *from == vm(3) && *to == vm(2)
    )));
    assert_eq!(restacks(&moved.effects), vec![(vec![wid(1), wid(3)], true)]);

    let landed = update(
        &moved.state,
        &observed_view(
            view,
            vec![mon_a(1), mon_b(1)],
            vec![
                win(1, 100, 1, rect(100.0, 100.0)),
                on_monitor(win(2, 200, 1, rect(2000.0, 100.0)), 3),
                on_monitor(win(3, 300, 1, rect(2400.0, 300.0)), 2),
            ],
            Some(1),
            RescanTrigger::Periodic,
        ),
    );
    assert_eq!(landed.state.windows[&wid(3)].vmonitor, vm(2));
    assert!(landed.state.pending.is_empty(), "{:?}", landed.state.pending);
    assert!(
        !landed.notes.iter().any(|n| matches!(n, Note::External { .. })),
        "{:?}",
        landed.notes
    );
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
fn a_switch_never_moves_the_view_even_onto_a_workspace_empty_here() {
    // w2 lives on workspace 2 AND monitor 2; the anchor is monitor 1. The
    // user chose monitor 1, so workspace 2 is empty here: the desktop of
    // monitor 1's display takes focus, and the view stays.
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
    assert!(focus_targets(&step.effects).is_empty());
    assert!(view_targets(&step.effects).is_empty());
    assert!(step
        .effects
        .iter()
        .any(|e| matches!(e, Effect::FocusDesktop { display, .. } if *display == mid(1))));
    assert_eq!(step.state.focus_intent(), FocusIntent::Desktop);

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
    let (moving, drag) =
        drag_then_settle(&s, || world(&[mon_a(1), mon_b(1)], &wins, Some(1)));
    // The instant it lands nothing is declared: this could be an unplug's
    // first symptom (see the re-home test). Once it has rested a second, the
    // declaration follows.
    assert!(moving.effects.is_empty(), "{:?}", moving.effects);
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
    let (_, drag) = drag_then_settle(&s, || {
        world_view(view, &[mon_a(1), mon_b(1)], &dragged, Some(1))
    });
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

#[test]
fn a_window_standing_on_another_live_display_is_adopted_never_reframed() {
    // A window whose center has come to rest on the second display while its
    // declaration still says monitor 1 — however it got there: a resize that
    // carried the center over the seam, an adoption whose op was lost, a
    // straddling window. With no display change there is nothing to re-host;
    // the declaration follows the window, and no frame is ever written. (The
    // first build wrote the frame back, size and all, and every resize
    // across the seam became a fight.)
    let mut wins = std_windows();
    wins[0] = on_monitor(win(1, 100, 1, rect(1800.0, 100.0)), 1); // center at 2000: on B, says 1
    // A standing position needs no move delta — only the second of rest.
    let booted = update(
        &State::new(),
        &observed(vec![mon_a(1), mon_b(1)], wins.clone(), Some(1), RescanTrigger::Startup),
    );
    assert!(booted.effects.is_empty(), "{:?}", booted.effects);
    let (_, settled) =
        drag_then_settle(&booted.state, || world(&[mon_a(1), mon_b(1)], &wins, Some(1)));
    assert_eq!(monitor_assignments(&settled.effects), vec![(wid(1), vm(2))]);
    assert_eq!(count_set_frames(&settled.effects, 1), 0, "never a frame write");
    assert!(settled.notes.contains(&Note::MonitorAdopted {
        window: wid(1),
        monitor: vm(2)
    }));
    // While the adoption is in flight nothing is repeated.
    let again = update(
        &settled.state,
        &observed(vec![mon_a(1), mon_b(1)], wins, Some(1), RescanTrigger::Periodic),
    );
    assert!(again.effects.is_empty(), "{:?}", again.effects);
}

#[test]
fn a_rehome_ahead_of_the_display_removal_is_never_adopted() {
    // The real unplug, as run 56 logged it: macOS moved the second display's
    // windows onto the laptop while CoreGraphics still listed both displays,
    // and the removal arrived in the NEXT snapshot. Read alone, the first
    // snapshot is indistinguishable from a drag — so a drag is not adopted on
    // sight, and the removal cancels the adoption that was waiting.
    let s = booted(&[1]);
    let mut wins = std_windows();
    wins[1].snap.frame = rect(80.0, 34.0);
    wins[1] = on_monitor(wins[1].clone(), 2);
    let rehomed = update(
        &s,
        &observed(vec![mon_a(1), mon_b(1)], wins.clone(), Some(1), RescanTrigger::Periodic),
    );
    assert!(rehomed.effects.is_empty(), "nothing on sight: {:?}", rehomed.effects);

    let removed = update(
        &rehomed.state,
        &observed_view(laptop(1, true), vec![mon_a(1)], wins.clone(), Some(1), RescanTrigger::Periodic),
    );
    assert!(monitor_assignments(&removed.effects).is_empty());
    assert_eq!(removed.state.windows[&wid(2)].vmonitor, vm(2), "still a monitor-2 window");
    assert!(!removed.state.is_visible(&removed.state.windows[&wid(2)]));

    // Long after, still undocked: the window is hidden, not a drag, and so
    // viewing monitor 2 is what brings it up — the whole point of the memory.
    let mut later = removed.state;
    for _ in 0..8 {
        later = update(
            &later,
            &observed_view(laptop(1, true), vec![mon_a(1)], wins.clone(), Some(1), RescanTrigger::Periodic),
        )
        .state;
    }
    assert_eq!(later.windows[&wid(2)].vmonitor, vm(2));
    let view = update(&later, &hotkey(HotkeyAction::ViewMonitorNext));
    assert_eq!(focus_targets(&view.effects), vec![wid(2)], "Slack comes up on monitor 2");
}
