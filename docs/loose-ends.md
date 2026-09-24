# Loose ends (2026-09-24)

Handoff notes for whoever picks this up next. Each item has what is known, where the evidence is in the log, and the proposed next step. Nothing here is done unless it says so.

**The log** is `~/Library/Application Support/Ordo/log.db`. Runs cited below:

| Run | `started_wall` | Context |
|---|---|---|
| 21 | 1790230376266 | Laptop only; fly-by flash investigation |
| 22 | 1790231437192 | Laptop; long session; Outlook reminder |
| 23 | 1790269954428 | Replug while the screen was locked |
| 24 | 1790273323105 | Two external displays; focus stolen on 24 of 64 switches (10 of the first 24) |
| 25 | 1790276371863 | Same rig, with the focus-top change below |
| 26 | 1790278133521 | Same rig, with stacking-order reads (a 1→2→1 burst) |

**Queries:** the analysis scripts were ad hoc and aren't saved. The tables to read are `events`, `effects`, `notes`, `park_trace`, `restacks` + `raises`, `hotkey_batches`, `snapshots`.
- Time ordering inside one engine step: `park_trace.wall_ms` is stamped when the trace is drained, so use `rowid` for order.

## 1. The stacking worker owns the top of the stack

**State:** committed (9d93a0f). Open issues 1a and 1b below.

**Files:** `crates/ordo-core/src/{effect,update}.rs`, `crates/ordo-core/tests/update_test.rs`, `crates/ordo/src/platform/{zorder,restack_worker,effector}.rs`, `crates/ordo/src/{ports,logger,schema}.rs`, `crates/ordo/examples/{reassert,scenario}_probe.rs`.

**What it does:**
- `Effect::RestackWindows` gained `focus_top: bool`. The core sets it at every emitter: true when the order's head is the window to focus; false for a view onto an empty monitor, where the desktop has focus. For Alt+End, toggle and merge it is true only if the head is the declared focus.
- A switch now sends a restack even for a single visible window.
- In `zorder::reassert_stack`, the worker makes the top key (`ax::focus`) after the presence wait if focus is elsewhere, and checks again after the final pass.
- The top is made key *before* the other raises, not after. Raises land below the key window (documented in `raise_pass`), so raising first would put everything under the window that stole focus. The artifact "Ordo Switch Pipeline" (https://claude.ai/artifact/WWZ5cgab8CxYfZrX1bdByA) still draws focus after the raises; update it if it matters.
- New `restacks.refocused` column (schema v5) counts how often the worker had to take focus back.

**Why:** un-hiding an app can make it active and steal the focus request that the switch issued first.
- Run 24: 24 lost switch focus grants in 64 switches (`notes.kind='op_lost'` joined to `effects.kind='focus_window'` in switch steps).
- Stacking 90th percentile 1328ms; one Chrome raise waited its full 1000ms landing timeout.
- Example switch: run 24, seq 1084. kitty took focus at +648ms; `op_lost` and `focus_reasserted` at seq 1090 (+1620ms).

**Result in run 25:** 0 lost grants in 114 switches; restack median 10ms / p90 70ms / max 161ms; 0 second passes; worker refocused 19 times in 18 of 76 restacks.

**Open issue (loose end 1a):** 4 restacks in run 25 ended `converged = 0`: `restack_id` 1279, 1307, 1310, 1322. All had kitty as top and none timed out.
- Likely cause: the final focus check runs just before the last read-back, widening the window for a late landing.
- An unconverged restack gets no ghost watch (`restack_worker.rs`: the watch requires `converged`), so a wrong order can stay until the next switch.
- Proposed fix: if the final `take_focus` refocused, re-read the order and run one more `raise_pass` if it is off. Also let the ghost watch run after an unconverged pass.

**Open issue (loose end 1b): the ghost watch re-takes focus after you've left.** Run 26: in 13 of 42 round trips, a ghost-watch re-run of workspace 1's order started after the switch away and made kitty (67722) key again, then was cancelled by the next switch (`restacks.ghost_pass = 1 AND refocused = 1`, e.g. 1606, 1609, 1612, 1618). Likely trigger: hiding the departing apps emits the 808/815 signals the watch listens for. Before focus-top, those re-runs only raised windows.

**Expected side effect:** replaying old logs will now report mismatches on `restack_windows`, because the new field defaults to false in old payloads. Accepted; behaviour changes already break exact replay of old runs.

## 2. Hidden apps pull their parked windows to x=0

Some apps, some of the time, move their parked windows to the display's left edge (x=0, keeping y) after Ordo hides them.
- **Timing:** 50–430ms after the hide returns, never during it. Every one of 322 post-hide read-backs in run 17 showed 0 off their spot (`park_trace.kind='AppHidden'` detail, `OffAfterHide` rows).
- **Apps:** OneNote, Acrobat, Terminal, Teams, Chrome, Outlook do it. Slack and kitty didn't.
- **Visible case:** run 22, `park_trace.kind='Reassert'` for window 55470 (Outlook Inbox) with detail `app 95048 hidden: no`. Outlook un-hid itself to show a reminder (new window 55638, title "1 Reminder"; `notes` External WindowCreated). The Inbox appeared at x=0 until the next scan re-parked it.
- **Fly-by episodes:** run 21 around seq 205–282 (workspaces 7–9 on monitor 2). Windows 62933, 70290, 70293, 70297 were pulled repeatedly, each with an AX `WindowCreated` hint from its own app.

**Proposed:** observe `AXApplicationShown` per app and immediately hold that app's parked windows (reuse the `show_apps` hold), instead of waiting for the next scan.
- Do not re-hide such an app blindly: the reminder case shows the app came back to show a window that belongs on screen.
- The post-hide read-back (`off_after_hide` in `workspaces.rs`) has answered its question. It costs one AX round trip per hidden app per switch and can be removed.

## 3. Notifications and app-initiated windows need a window role

**Evidence** (window subroles across the whole log):
- **Outlook's reminder** reports `AXStandardWindow` (400×127). It was adopted as a normal window: run 22, workspace 1, monitor 2. It would be parked when the user switches away.
- **Subroles flip on real windows.** Main windows (Outlook Inbox, OneNote, Teams, Terminal) sometimes report `AXDialog`.
- **Popups are clean.** Chrome and OneNote popups report `AXUnknown` or no subrole (about 150×22). They are currently managed as windows.
- **Chrome's find bar,** window 44863, lived 226 snapshots inside its Chrome window and holds its own ledger claim.

**Deployed (commit 9585ccd):** snapshots record each window's `layer` and `parent`, from the window server, asked once per window. No window had a parent at deploy time. Collect cases before choosing rules: Chrome Cmd+F, tab and link hovers, an Outlook reminder, dialogs, call popups.

**Proposed shape:**
1. **Attached popups** (have a parent): follow the parent, no workspace of their own.
2. **Independent transients** (born without focus while their app wasn't frontmost; reminders, PiP): follow the user across workspaces until touched.
3. **Normal windows:** as today.
4. **Per-app override rules** as an escape hatch.

AeroSpace comparison and sources were given in-session. Their popups go in a global container; they have no sticky support (their issue #2); their dialog heuristic keeps misfiring.

## 4. Snapshot cost leftovers

- **Six ledger windows are alive but never scanned:** 24998, 44850, 44863, 67652, 70295, 70296 (minimized, on another Space, or the Chrome find bar). Every snapshot therefore calls `existing_windows` (a full CG list, about 2ms). Cache the "alive but unscanned" answer between snapshots.
- **About 5–7ms per snapshot outside the window walk is untimed** (`snapshots.total_ms - walk_ms - enforce_ms`). Candidates: `focused_window`'s AXFrontmost loop, `believed_frames`, trace building, persistence.

## 5. Fly-by flashing of intermediate workspaces

Run 21: every press was a full switch, because the engine keeps up with presses about 170–300ms apart. Passing through workspaces 1–2, whose windows were on monitor 2, showed them briefly.

**Proposed, not built:** in a burst, hold a press that arrives within about 150ms of the previous one until about 100ms of quiet, then switch straight to the final target. The core already folds runs of Prev/Next (`coalesce_hotkeys`). Cost: about 100ms later landing on fast bursts only.

## 6. Switches reorder windows, and one slow raise holds up the rest

**The un-hide puts windows under ones that stayed shown.** Run 26 traces the stack at each switch (`park_trace.kind = 'Stack'`, details `before: …` and `after un-hides: …`) and `restacks.start_order`. Slack (44533) was above Chrome (67650) whenever you left workspace 1. Right after the un-hides on return, Chrome was above in 13 of 23 switches. The worker's start order matched, so the hide/un-hide is the step that moves it, not parking or the worker. Un-hide order didn't predict it. Lead, unproven: flips were near-constant while workspace 2's top was kitty and rare after its top became Chrome (from restack 1638).

**One slow raise holds up the rest, even irrelevant ones.** Raises go one at a time over one global stack. Run 25: restacks 1434 and 1451 left Chrome over Slack for 531 and 758ms while Slack was slow to answer its raise; each was cut off by the next switch (`aborted = 1`). Restack 1557 made Slack wait behind a 360ms Chrome raise that was only about Outlook, on the other display. Run 26: 1656 waited 907ms on the same kind of cross-display Chrome raise; 1645 waited 361ms raising kitty.

**Proposed:**
1. Hide an app only once the user has settled on a workspace (about half a second), so a quick round trip never hides and un-hides it. Also cuts the hides behind item 2 and item 5.
2. Order only windows that overlap on screen, and raise only what's out of place relative to them.

## 7. Smaller items

- **Startup "re-parks" that move nothing.** 7–13 per restart; `Reassert` with observed == requested, e.g. run 17 at start. The at-park check disagrees with an exact park position right after launch. Harmless but noisy.
- **`docs/desired-state-reconciler.md` still orders its plan with Focus first.** It should adopt the focus-top rule from item 1 before anything there is built.
- **Hotkey waits are logged per batch** (`hotkey_batches`, schema v3). Reference numbers: run 22 p90 0ms, worst 172ms; run 25 p90 120ms, but presses came a median 127ms apart.
- **Windows 44267 and 66425 were stacked on top of each other before the replug fix,** so their positions are not recoverable from the log. The user can move them apart once.
