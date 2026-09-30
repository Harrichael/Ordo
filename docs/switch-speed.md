# Switch speed

Where a workspace switch spends its time, what has been done about it, and what to do next. Written 2026-09-29, after runs 37–46.

**The goal the user set:** 16 ms median (one display frame). The realistic floor, from the analysis below, is about 10 ms for switches that only move light apps (kitty, Slack) and about 35 ms when Chrome or Outlook windows must move. Every visible change goes through the target app's main thread, and Chrome and Outlook take 15–20 ms per position write.

## What "switch time" means

There are three clocks, and they have different critical paths:
1. **Press → destination visible and stacked:** what the eye sees. This is the one to optimize.
2. **Press → keyboard focus on the destination's head window:** what typing feels. It sits inside clock 1.
3. **Press → engine free for the next hotkey:** throughput in a burst. The post-switch rescan and queueing live here.

The log has no timestamp yet for "the window server shows the window at its restored frame". The restack worker's push stream (808/815 events) could stamp it; that would give clock 1 directly.

## Anatomy of a switch

On the engine thread, in order (see also the page at https://claude.ai/artifact/57hRd5aEAPUWSYSHo4Q3fp, private to the user):
1. **Hotkey waits in the queue** while the engine finishes the previous switch, including its rescan. Runs of next/previous presses already waiting are folded into one switch (`coalesce_hotkeys`).
2. **Core decides; the focus effect runs.** `ax::focus` walks the apps' window lists serially to find the window, then SLPS front-process, make-key records, AXMain, AXFocused, AXRaise.
3. **Read every window's frame** (`current_frames`, an AX walk of every app) and the display geometry. The outgoing windows' frames become their saved positions.
4. **Save `state.json`**, before any window moves: a crash mid-switch never loses where a window belongs.
5. **Move windows**, one thread per app (`ax::move_windows`). The batch takes as long as the slowest app.
6. **Show the destination's apps**, one thread per app (`ax::show_apps`). Only apps Ordo hid are asked and un-hidden. The un-hide carries the park positions of that app's other windows, and holds them (an un-hide re-homes every window the app owns). Hides of apps left with nothing on screen run 500 ms later (`HIDE_SETTLE`).
7. **Hand the MRU order to the restack worker.** It takes microseconds, and the restack runs on its own thread (`restack.rs`: overlap-only plan, lanes per overlap group).
8. **Rescan** (a full AX walk) so the core can confirm the switch. The next hotkey waits for it.

Rule: an app's moves must come before its own un-hide, or the un-hide reveals windows where they still stand. Different apps don't depend on each other.

Facts measured along the way:
- Windows of a hidden app can be moved; they appear where they were sent when it is un-hidden (472 of 476).
- An un-hidden app's windows re-enter the stack as one block directly under the front window (16 of 16, run 37).
- SkyLight has no side door from a SIP-on daemon (the brainstorm agent probed this on a window of its own). `SLSMoveWindow`, `SLSTransactionMoveWindowWithGroup`, `SLSSetWindowAlpha` and `SLSSetWindowTransform` all return success but change nothing on screen; `SLSOrderWindow` is refused (1000).
- `CGWindowListCopyWindowInfo` with bounds takes about 0.26 ms.

## Measurements

All numbers are in ms, from the log. Medians don't add up to a real switch.

**Run 39 (before the hiding change, 206 switches) against run 45 (after it, 309 switches):**

| | Run 39 | Run 45 |
|---|---|---|
| Engine per switch (press → rescan done), median / 90th | 181 / 399 | 98 / 304 |
| Frame read, median | 19 | 23 |
| Moves (slowest app), median | 45 | 12 |
| Un-hide step, switches that un-hid nothing: median (apps asked per switch) | 10.7 (2.1) | 1.2 (0) |
| Un-hide step, switches with a real un-hide: median | 145 | 99 |
| Rescan walk, median | ~31 | 29 |
| Hotkey queue wait, 90th | 183 | 178, with faster presses |
| Restack on the worker, median / 90th | 1 / 81 | 2 / 131 |

The moves also got faster after the hiding change. The workload was similar (about 6 windows over 3 apps per switch). The likely reason is fewer calls queued on the same apps' main threads, but that isn't proven.

**Moves per app, run 39 (median / 90th):** Chrome 22 / 87, Outlook 18 / 69, kitty 3.8 / 10, Slack 2.6 / 7. A single position write is under 1 ms typically, 13 ms at the 90th percentile, and up to 106 ms. Reading an app's window list costs about 0.5 ms. Enhanced UI was never toggled.

**Bursts:** a fast burst sends a press every 40–100 ms against about 100–180 ms of engine time per switch, so the queue grows. Queued presses wait mostly behind the previous switch's rescan (95 of 129 rescans seen during waits in run 39).

## Done

| Commit | What | Effect |
|---|---|---|
| 2c92712 | Restacks order only overlapping windows, with one lane per overlap group | Restack median 11 → 4 ms. It runs on the worker, off the engine's path, so switch time barely changed. |
| 3e5d609 | Stacking and MRU are about root windows (attached popups ride with their root) | Ended a 2 s futile fight on each Chrome address-bar popup resize. |
| 2f37f98 | Switch timings, always on: `write` on each write row, `AppMoved` rows per app, `cost` on Switch/View rows | Made the breakdown above possible. |
| 53dbd9e | Ordo alone decides which apps are hidden; a switch asks only the apps Ordo hid | Un-hide step 10.7 → 1.2 ms on most switches; engine median 181 → 98 ms. |
| dc1b946 | A window missing from one scan keeps its MRU place for 10 s | Correctness: one flaky AX read no longer sends a window to the back of the MRU order. |

## Next, ranked

1. **Take the next hotkey before the rescan.** The rescan only confirms, and nothing needs it before the next press. Confirm from a window-list read (about 1 ms) plus the writes' results, and run the full walk later. Saves about 30 ms per switch on clock 3, and most of the queueing in bursts. This touches how the core confirms ops, so design it with the pending-op expiry in mind.
2. **Real un-hides (about 100 ms typical, 238 ms at the 90th percentile).** These are now the biggest single cost. Two directions: the per-app pipeline (item 4), or keeping fewer apps hidden between nearby switches.
3. **Frames from the window list instead of the AX walk** (about 20 ms). `current_frames` only needs the frames of windows about to be parked; hidden apps' windows are absent from the list, but the ledger holds those. Verify first that CG bounds and AX frames agree exactly, since these become saved positions written to disk.
4. **One long-lived thread per app doing restores, then its held un-hide, then focus (for the head app), then parks,** with all apps in parallel and cached AX elements. Light-app switches could reach about 8–12 ms. This also removes the known race where a switch focuses a window before its app is un-hidden (loose ends 1a). It's the hardest item, so design it properly; the user is wary of quick fixes to races.
5. **Stop redundant focus take-backs on the restack worker:** 36 of 309 restacks in run 45, about 184 ms each. Wait briefly for the switch's own focus to land before focusing again. It's off the engine thread, but it queues calls on Chrome while the next switch moves Chrome's windows.
6. **Focus by pid, or with a cached element,** instead of walking the apps one by one (10–20 ms).

Not viable: SkyLight moves, alpha or ordering (probed); native Spaces (SIP). Low value: an overlay screenshot of the destination. It feels instant, but it hides the lag instead of removing it, and keystrokes in the gap go to the old window.

## Correctness items found along the way

- **The settled-move restack enforces MRU across the whole workspace.** It should enforce only the pairs that include the window that moved. In run 45, a Chrome popup on the right monitor reordered the left one. `restack_settled_moves` in `update.rs` would pass the moved roots, and the planner would filter edges to pairs touching them.
- **Cmd+H undo has only been tested against the fake.** No outside hide happened in run 45 or 46. One Cmd+H on an app with a window on the current workspace would confirm it live.

## Working setup

- **kitty's Accessibility grant is broken** (since run 39's session). Processes started from kitty, including this Claude Code session's shell, can't create the event tap or see windows. Ghostty's grant works. The fix is to remove kitty from System Settings > Privacy & Security > Accessibility, add it back, and relaunch kitty.
- **Until then, restarts go through the `ordo-manager` Claude Code session in Ghostty** (SendMessage to `ordo-manager`). It needs `setsid` so Ordo survives its tool shell. The restart steps:
  1. Copy `state.json` aside.
  2. `kill -INT` the pid in `ordo.pid`, and wait for it to exit (up to 2 s).
  3. `cmp` `state.json` against the copy.
  4. Start `./target/release/ordo run --paused`, detached.
  5. Check the log has no "could not create event tap" and there's no `.rejected` file.
  6. Engage with Ctrl+Alt+Cmd+O (osascript key code 31 with control, option, command).
- **A locked screen** makes every scan see no displays. Ordo discards those scans, so a run started while locked logs nothing until unlock.

## Reproducing the numbers

The scripts are in `scripts/log-analysis/`. They're read-only against `~/Library/Application Support/Ordo/log.db`.
- `switch-breakdown.py RUN`: queue wait, engine time and its parts, and the restack. Engine time is from the hotkey event to the post-switch rescan event of that switch's op.
- `switch-costs.py RUN`: the `cost` fields, per-app move times, and single writes (runs from 2f37f98 on).
- `visibility-split.py`: the un-hide step with and without a real un-hide.
- `engine-busy.py`: rescans by trigger, and what ran while hotkeys waited.
- `app-chains.py RUN`: runs with the app queues (`docs/app-queue-design.md`). What each app's queue did, when each switch's last chain finished and its focus ran, and how long restacks waited for their apps. `switch-costs.py` still reads their `cost` fields; their per-app moves are only here.

| Run | `started_wall` | Context |
|---|---|---|
| 37 | 1790685295040 | First overlap-only restacks; debug mode on (stack reads) |
| 38 | 1790692128638 | Root windows (popups attached) |
| 39 | 1790693111844 | Switch timings; last run before the hiding change |
| 45 | 1790698057621 | Ordo alone decides what's hidden |
| 46 | 1790720812195 | MRU grace period (dc1b946) |
