//! Shows Ordo's menu bar item with a scripted view, and no daemon behind it —
//! for looking at the design without letting Ordo loose on your windows.
//! Picks from the menu are printed instead of acted on.
//!
//!   cargo run --example menubar_preview            # 9 workspaces, on 3
//!   cargo run --example menubar_preview -- paused  # the rescued look
//!
//! Apps named in the menu are whatever is running now (pids from this
//! machine), since the menu resolves names live.

use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy, NSWorkspace};
use ordo::engine::Msg;
use ordo::menubar::{MenuBarView, WorkspaceEntry};
use ordo_core::{HotkeyAction, Pid, WorkspaceId};

fn main() {
    let paused = std::env::args().any(|a| a == "paused");
    let mtm = objc2::MainThreadMarker::new().unwrap();

    let running: Vec<Pid> = NSWorkspace::sharedWorkspace()
        .runningApplications()
        .iter()
        .filter(|a| a.activationPolicy() == NSApplicationActivationPolicy::Regular)
        .map(|a| Pid(a.processIdentifier()))
        .collect();
    let take = |from: usize, n: usize| running.iter().skip(from).take(n).copied().collect();

    let apps: [Vec<Pid>; 9] = [
        take(0, 2),
        take(2, 5),
        take(7, 1),
        vec![],
        take(8, 1),
        vec![],
        vec![],
        vec![],
        vec![],
    ];
    let view = MenuBarView {
        workspaces: apps
            .into_iter()
            .enumerate()
            .map(|(i, apps)| WorkspaceEntry {
                id: WorkspaceId(i as u8 + 1),
                apps,
            })
            .collect(),
        current: Some(WorkspaceId(3)),
        engaged: !paused,
    };

    let (tx, rx) = crossbeam_channel::unbounded::<Msg>();
    let menubar = ordo::platform::status_item::install(tx);
    menubar.show(view);
    std::thread::spawn(move || {
        while let Ok(msg) = rx.recv() {
            if let Msg::Hotkey(HotkeyAction::WorkspaceSwitchTo(ws)) = msg {
                println!("picked workspace {}", ws.0);
            }
        }
    });

    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Prohibited);
    app.run();
}
