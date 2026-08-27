# Ordo

A macOS workspace/window-navigation daemon, in Rust. Workspaces are the root
concept: a workspace is a *scene* spanning every monitor. Ordo switches
workspaces, jumps between windows by most-recent-use, keeps the mouse with the
focused window, drops new windows where you're working, logs everything for
after-the-fact debugging, and has a kill switch for when it all goes wrong.

## Design: a functional core in an imperative shell

The dependency arrow points inward only.

```
ordo-core         pure decisions: update(&State, &Event) -> (State, [Effect])
  ▲                 no I/O, no clock, no OS types — replayable by construction
  │
ordo (shell)      observes macOS, executes effects, logs, kill switch
  │
ordo-skylight-sys  every private/undocumented symbol, quarantined in one crate
```

The two rules that make it work:

- **The core is clockless and deterministic.** Time and identifiers ride on
  events and on a counter inside `State`, so a logged event stream replays
  byte-for-byte. `ordo replay` re-runs a logged session and asserts the core
  still decides exactly what it decided live.
- **Belief follows observation, never intent.** Issuing an effect changes
  nothing; `State` only moves when a fresh `WorldSnapshot` confirms what
  actually happened. macOS co-owns much of this state (you can switch Spaces
  behind Ordo's back), so intent-ahead bookkeeping would inevitably diverge.
  Snapshot changes that match a pending expectation are attributed to Ordo's own
  actions; the rest are external and absorbed. Misbehaving apps are *damped*,
  not fought.

## Features (v0)

- **Workspace switching**: `Cmd+Left` / `Cmd+Right` to the adjacent workspace.
- **MRU window focus**: `Alt+Tab` (in workspace), `Alt+Shift+Tab` (+ monitor),
  `Alt+Backtick` (+ app). The mouse warps to the focused window's center.
- **New-window placement**: a newly created window is corralled onto the focused
  workspace and monitor.
- **Move window to the other monitor** (operation implemented; no key bound yet).
- **Structured SQLite log** at `~/Library/Application Support/Ordo/log.db`.
- **Kill switch**: `Ctrl+Alt+Cmd+Escape` twice within 2s, or `ordo rescue` —
  disengages interception and gathers displaced windows back on-screen.

Two workspace backends behind one trait:

- **`native`** (default) — drives real macOS Spaces via private SkyLight APIs
  (no SIP disable). You pre-create the Spaces in Mission Control.
- **`emulated`** — Ordo owns workspaces AeroSpace-style, parking hidden windows
  off-screen. Unlimited workspaces, no private Space APIs.

## Build & run

```sh
cargo build --release
./target/release/ordo run                 # native backend, active
./target/release/ordo run --observe       # decide + log, execute nothing
./target/release/ordo run --backend emulated --workspaces 9
./target/release/ordo rescue              # kill switch from the CLI
./target/release/ordo replay              # verify the last run replays clean
```

**Permission**: Ordo needs Accessibility (System Settings → Privacy & Security →
Accessibility). Without it, it degrades to observe-without-hotkeys rather than
crashing. It must be non-sandboxed and is best code-signed with a stable
identity (TCC grants bind to code identity).

**macOS settings for the native backend**: "Displays have separate Spaces" on,
"Automatically rearrange Spaces based on most recent use" off.

## Tests

`cargo test` — the pure core and all shell logic that doesn't need a live
WindowServer are covered with real implementations (Chicago style); the OS edge
is faked only at the `WorldSource`/`Effector` seam and the pure key/geometry
tables are unit-tested directly.

## Known caveats (want on-device validation)

These are isolated and flagged in-source; the architecture surfaces failures
honestly (verify-and-report) rather than desyncing silently.

- **SkyLight CFDictionary schema** (`platform/skylight.rs`) is undocumented and
  shifts across macOS releases — the single most version-fragile piece.
- **Dock-swipe gesture field numbers** (`platform/gesture.rs`) for animation-free
  Space switching are private; switching verifies afterward and retries once.
- **`SLSMoveWindowsToManagedSpace`** may be restricted from a non-Dock process on
  recent macOS; the move verifies and reports failure, where the emulated
  backend is the fallback.
- Windows whose Space can't be resolved default to workspace 1.
