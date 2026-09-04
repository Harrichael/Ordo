# Ordo

A macOS workspace/window-navigation daemon, in Rust. Workspaces are the root
concept: a workspace is a *scene* spanning every monitor — and the monitors
are *virtual*, so the scene survives unplugging the display it was arranged
on. Ordo switches workspaces, jumps between windows by most-recent-use, keeps
the mouse with the focused window, drops new windows where you're working,
remembers which monitor every window belongs to, logs everything for
after-the-fact debugging, and has a kill switch for when it all goes wrong.

## Design: a functional core in an imperative shell

The dependency arrow points inward only.

```
ordo-core         pure decisions: update(&State, &Event) -> (State, [Effect])
  ▲                 no I/O, no clock, no OS types — replayable by construction
  │
ordo (shell)      observes macOS, executes effects, logs, kill switch
  │                 binds one workspace mechanism to the WorkspaceBackend trait:
  ├── ordo-emulated    Ordo owns workspaces (park/restore, ledger, state file),
  │                      OS touches behind its Desktop port
  └── ordo-skylight-sys  native Spaces: every private/undocumented symbol,
                           quarantined in one crate
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

- **Workspace switching**: `Cmd+Left` / `Cmd+Right` to the adjacent workspace,
  or `Cmd+Alt+1`…`Cmd+Alt+9` to jump straight to one. The arrows are swallowed
  while Ordo is engaged; the digits are Alt-qualified so apps keep their
  `Cmd`-digit tab switching.
- **MRU window focus**: `Alt+Tab` (in workspace), `Alt+Shift+Tab` (+ monitor),
  `Alt+Backtick` (+ app), `Ctrl+Alt+Tab` (the *other* monitor). The mouse warps
  to the focused window's center.
- **New-window placement**: a newly created window is corralled onto the focused
  workspace and monitor.
- **Move window to the adjacent monitor**: `Cmd+Shift+Left/Right` — focus and
  mouse travel with the window. If that monitor is hidden, the view follows.
- **Virtual monitors** (emulated backend): every window is declared onto a
  virtual monitor — a position, left to right, not a piece of hardware. With a
  display per monitor they map one to one. With fewer displays (the laptop
  unplugged), `Cmd+Alt+J/K` view the previous/next monitor (no wrap; global,
  not per workspace), showing its windows and hiding the current one's, and
  `Cmd+Alt+V` toggles virtualization: off collapses every monitor onto the
  display(s) present. The view follows focus — Alt+Tab or Cmd+Tab onto a hidden
  monitor's window reveals it. Plug the display back in and its windows return
  to it, at the frames they had, within about a second. Mirrored displays
  count once. Persisted in `state.json`; virtualization is on by default.
- **Carry window to another workspace**: `Ctrl+Cmd+Left/Right` — the focused
  window moves to the adjacent workspace and the view switches with it.
- **MRU demote**: `Alt+End` sends the focused window to the back of the MRU
  history *and* the back of the visual stack, then focuses the next one.
- **Structured SQLite log** at `~/Library/Application Support/Ordo/log.db`.
- **Kill switch**: `Ctrl+Alt+Cmd+Escape` twice within 2s, or `ordo rescue` —
  disengages interception and gathers displaced windows back on-screen.
- **Engage switch**: `Ctrl+Alt+Cmd+O` — the reverse: re-engage after a rescue,
  or bring a `run --paused` daemon alive for the first time.

Two workspace backends behind one trait:

- **`emulated`** (default) — Ordo owns workspaces AeroSpace-style, parking
  hidden windows off-screen — hidden by workspace or by virtual monitor alike. Instant and animation-free, unlimited workspaces,
  no private Space APIs. Best with a single native Space per display.
  Cmd+Tab, Cmd+` and Dock clicks are followed: the tap witnesses them, and
  a focus landing on a hidden workspace right after one switches Ordo there,
  like native Spaces would. An app flinging focus there on its own is pulled
  back instead. Apps with no
  window on the visible workspace are hidden (Cmd+H-style) so the Dock dims
  them — run `defaults write com.apple.dock showhidden -bool true; killall
  Dock` once to render hidden apps translucent.
- **`native`** — drives real macOS Spaces (pre-created in Mission Control) by
  pulling Mission Control's own rebound keyboard shortcut per display. Real
  Spaces, but every switch plays the system animation.

## Build & run

```sh
cargo build --release
./target/release/ordo run                 # native backend, active
./target/release/ordo run --observe       # decide + log, execute nothing
./target/release/ordo run --paused        # start disengaged; Ctrl+Alt+Cmd+O engages
./target/release/ordo run --workspaces 9          # emulated is the default backend
./target/release/ordo run --backend native        # real macOS Spaces instead
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
  (Validated on Tahoe 26.6.)
- **Native Space switching drives Mission Control's own keyboard shortcut**
  ("Move left/right a space", rebound to Ctrl+Alt+Cmd+arrows in System
  Settings — required setup). The shortcut acts on the display under the
  pointer, so Ordo warps the pointer per display and restores it. Every
  private switching API half-works or no-ops on Tahoe; see `issues.txt` and
  the probes in `crates/ordo/examples/`.
- **`SLSMoveWindowsToManagedSpace`** may be restricted from a non-Dock process on
  recent macOS; the move verifies and reports failure, where the emulated
  backend is the fallback.
- Windows whose Space can't be resolved default to workspace 1.
