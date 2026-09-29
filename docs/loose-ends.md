# Loose ends

Handoff notes for whoever picks this up next. Each item is open: what is known, where the evidence is in the log, and the proposed next step. Items are removed once done.

**The log** is `~/Library/Application Support/Ordo/log.db`. Runs cited below:

| Run | `started_wall` | Context |
|---|---|---|
| 21 | 1790230376266 | Laptop only; fly-by flash investigation |
| 22 | 1790231437192 | Laptop; long session; Outlook reminder |
| 25 | 1790276371863 | Two external displays, with focus-top (9d93a0f) |
| 29 | 1790627142362 | Workspace 3 with 8 Preview windows; restacking flicker |
| 33 | 1790639352913 | Only-hidden un-hides, before the revealed-app hold |
| 35 | 1790678196817 | With the revealed-app hold (3e936ef) |

**Queries:** the analysis scripts were ad hoc and aren't saved. The tables to read are `events`, `effects`, `notes`, `park_trace`, `restacks` + `raises`, `hotkey_batches`, `snapshots`.
- Time ordering inside one engine step: `park_trace.wall_ms` is stamped when the trace is drained, so use `rowid` for order.
- Stack reads at each step of a switch (`park_trace.kind = 'Stack'`, and `hold.stacks` on `AppShown`) are only logged in debug mode (menu bar Settings, off at every launch).

## 1. Hide/un-hide races with macOS

A race here is Ordo against the app's main thread, AppKit and the window server, all outside Ordo's process. Inside Ordo, hides and un-hides are both issued from the engine thread, and the per-app un-hide threads are joined before it moves on. So a mutex orders nothing that matters. Every "check, then act" against macOS can go stale in between. `show_app_holding` (`ax.rs`) is one: it asks whether the app is showing, then decides.

**1a. Focusing the desktop un-hides a just-hidden Finder, with nothing holding its parked window.** Seen once.
- **What happens:** a switch to an empty workspace focuses the desktop, which activates Finder. The revealed-app hold (3e936ef) only checks apps with a window on the destination. Finder has none, so its parked window is never checked.
- **Evidence:** run 35. `AppHidden` for pid 914 at rowid 52844, a deferred hide. Then a switch 123ms later (52845, "parking 2, restoring 0"), and a `Reassert` of window 74777 at x=0.0, "app 914 hidden: no; front app: 914" (52848).
- **The general case:** a switch focuses *before* it un-hides (`Command::Switch` in `update.rs`: `FocusWindow` or `FocusDesktop` goes out before `SwitchWorkspace`). Focusing a window of a hidden app un-hides that app with nothing held, and AppKit drags its parked windows on screen. The revealed-app hold covers the destination's apps. It doesn't cover the desktop's owner, or any other app the focus lands on.
- **Before the hold:** run 33 had 21 windows found at x=0 in 179 switches; run 35 had 3 revealed holds and 0 escapes for the destination's app.

**1b. Nothing un-hides a hidden app that owns a window on screen.** Not seen.
- **How it could happen:** `hide_idle_apps` sends `AXHidden = true` and moves on. If a switch back reads the app as showing before the hide has taken effect, the app is left alone and then finishes hiding. It stays invisible on its own workspace.
- **Why nothing fixes it:** `hide_idle_apps` skips apps with a window here, and `enforce_placement` has no "hidden, but should be showing" check.
- **Evidence it hasn't happened:** the shortest hide→un-hide gaps in the log (Chrome 211ms and 361ms, Finder 499ms) all read hidden correctly.

**1c. A deferred hide can fire just as the user switches.** Seen.
- **What happens:** the periodic rescan runs the due hides, and a hotkey can land milliseconds later, so the apps are hidden and then un-hidden at once.
- **Evidence:** run 33, `AppHidden` at rowids 51117–51120 with hotkey seq 979 6ms later; again at 51306–51309, +41ms. 1a's Finder case followed a hide by 123ms.
- **Cost:** a Dock flicker and one slow held un-hide per app, and it sets up 1a. A hide pass also takes 32ms on average (max 117ms) on the engine thread: `snapshots.enforce_ms` on the snapshots with a hide.

**1d. The front-app exemption reads one moment's answer.** Not seen.
- `hide_idle_apps` asks `frontmost_app()` once. If a focus handoff hasn't landed yet, the departing app is spared and stays undimmed, which is harmless.
- If `frontmost_app()` returns None, the exemption is void and the front app could be hidden, which throws focus somewhere arbitrary.

**Proposed, as two approaches:**
1. **Remove the race by ordering.** Un-hide and hold first, then focus. The focus then lands on an app already showing, and nothing is revealed behind Ordo's back. This is a structural change.
   - Focus-first was chosen so that the app you leave is never the front app when it's hidden: hiding the front app throws focus somewhere arbitrary.
   - Hides are now deferred by 500ms (98d68a1), so that reason may no longer hold within the switch itself.
   - Read the core's reasoning and the effect order before flipping it.
2. **Reconcile every snapshot.** On each snapshot, un-hide any hidden app that owns a visible window, alongside the existing re-park of parked windows found off their spot. This doesn't prevent the wrong moment, but guarantees it ends. It's needed regardless, because apps and the user (Cmd+Tab) un-hide things behind Ordo's back. Cost: a flash of up to one scan interval, so this is the safety net, not the fix.

**Tests:** the fake desktop lands every hide and un-hide instantly. These races need a fake that can finish an app's hide or un-hide *later*. Then check against the log's counts: `Reassert` rows with `observed.x = 0.0`, and `AppShown` rows whose detail contains "revealed".

## 2. Apps un-hiding themselves pull their parked windows to x=0

Some apps move their parked windows to the display's left edge (x=0, keeping y) when they un-hide themselves. The switch's own un-hides hold parked windows in place (`show_apps`); an app's own reveal is held by nothing.
- **Visible case:** run 22, `park_trace.kind='Reassert'` for window 55470 (Outlook Inbox), detail `app 95048 hidden: no`. Outlook un-hid itself to show a reminder (new window 55638, "1 Reminder"; `notes` External WindowCreated). The Inbox appeared at x=0 until the next scan re-parked it.
- **Fly-by episodes:** run 21 around seq 205–282 (workspaces 7–9 on monitor 2). Windows 62933, 70290, 70293, 70297 were pulled repeatedly, each with an AX `WindowCreated` hint from its own app.
- **Apps seen doing it:** OneNote, Acrobat, Terminal, Teams, Chrome, Outlook. Slack and kitty weren't.

**Proposed:** observe `AXApplicationShown` per app and immediately hold that app's parked windows (reuse the `show_apps` hold), instead of waiting for the next scan.
- Don't re-hide such an app blindly: the reminder case shows the app came back to show a window that belongs on screen.
- The post-hide read-back (`off_after_hide` in `workspaces.rs`) has answered its question: 0 windows off their spot at every hide in runs 29–33. It costs one AX round trip per hidden app and can be removed.

## 3. Notifications and app-initiated windows need a window role

**Evidence** (window subroles across the whole log):
- **Outlook's reminder** reports `AXStandardWindow` (400×127). It was adopted as a normal window: run 22, workspace 1, monitor 2. It would be parked when the user switches away.
- **Subroles flip on real windows.** Main windows (Outlook Inbox, OneNote, Teams, Terminal) sometimes report `AXDialog`.
- **Popups are clean.** Chrome and OneNote popups report `AXUnknown` or no subrole (about 150×22). They are currently managed as windows.
- **Chrome's find bar,** window 44863, lived 226 snapshots inside its Chrome window and holds its own ledger claim.

**Deployed (9585ccd):** snapshots record each window's `layer` and `parent`, from the window server, asked once per window. No window had a parent at deploy time. Collect cases before choosing rules: Chrome Cmd+F, tab and link hovers, an Outlook reminder, dialogs, call popups.

**Proposed shape:**
1. **Attached popups** (have a parent): follow the parent, no workspace of their own.
2. **Independent transients** (born without focus while their app wasn't frontmost; reminders, PiP): follow the user across workspaces until touched.
3. **Normal windows:** as today.
4. **Per-app override rules** as an escape hatch.

AeroSpace for comparison: popups go in a global container; there's no sticky support (their issue #2); their dialog heuristic keeps misfiring.

## 4. Stack ordering: overlapping windows only

**The problem:** the worker imposes one total order across every visible window, one raise at a time. So it re-raises windows already in the right place, and a slow raise holds up the one the user can see.
- **Flicker:** run 29, restacks 2223 and 2228 re-raised 8 Preview windows that were already in order on the main display, because windows on the other display sat between them in the order. That's about 850–925ms of visible restacking per arrival.
- **Slow cross-display raises:** run 25, restack 1557 made Slack wait behind a 360ms Chrome raise that only ordered Chrome against Outlook on the other display.

**Design, not built:** `docs/stack-order-design.md` (2026-09-29). Its main points:
- Keep the core's MRU order, but enforce it only between overlapping windows. The MRU is the only stored structure; overlap groups are rebuilt at each restack, not kept.
- The shell computes the overlaps from the same window-list read that gives the stack. Measured: the bounds parse and overlap math add about 2 µs to a 0.26 ms read.
- Geometry changes should trigger a restack check: the core emits `RestackWindows` when a visible window's frame change settles, and the shell's plan is empty when nothing is violated.
- Raise the unique minimal set, bottom-up.
- Independent overlap groups raise in parallel.
- Overlap detection is brute force: fixed arrays and a bitset, more than 2pt of overlap on both axes.
- Ignore the "Displays have separate Spaces" setting; geometry alone is correct in both modes.

Replayed on the log: restack 2228 goes from 20 raises to 2, and all 99 restacks of run 29 from 421 to 182.

**Open decision:** the user described not remembering the order of windows that don't overlap at all. The design keeps it but doesn't enforce it.

## 5. Unconverged restacks with no timeout

Run 25: 4 restacks ended `converged = 0`, `restack_id` 1279, 1307, 1310, 1322. All had kitty as top and none timed out.
- **Likely cause:** the final focus check runs just before the last read-back, widening the window for a late landing.
- **Consequence:** an unconverged restack gets no ghost watch (`restack_worker.rs`: the watch requires `converged`), so a wrong order can stay until the next switch.
- **Proposed:** if the final `take_focus` refocused, re-read the order and run one more `raise_pass` if it's off. Let the ghost watch run after an unconverged pass too. Revisit after item 4, which replaces the raise pass.

## 6. Snapshot cost leftovers

- **Six ledger windows are alive but never scanned:** 24998, 44850, 44863, 67652, 70295, 70296 (minimized, on another Space, or the Chrome find bar). Every snapshot therefore calls `existing_windows`, a full CG list at about 2ms. Cache the "alive but unscanned" answer between snapshots.
- **About 5–7ms per snapshot outside the window walk is untimed** (`snapshots.total_ms - walk_ms - enforce_ms`). Candidates: `focused_window`'s AXFrontmost loop, `believed_frames`, trace building, persistence.

## 7. Fly-by flashing of intermediate workspaces

Run 21: every press was a full switch, because the engine keeps up with presses about 170–300ms apart. Passing through workspaces 1–2, whose windows were on monitor 2, showed them briefly.

**Proposed, not built:** in a burst, hold a press that arrives within about 150ms of the previous one until about 100ms of quiet, then switch straight to the final target. The core already folds runs of Prev/Next (`coalesce_hotkeys`). Cost: about 100ms later landing, on fast bursts only.

## 8. Smaller items

- **Startup "re-parks" that move nothing.** 10–14 per restart, `Reassert` rows with observed == requested; every restart on 2026-09-28/29 had them. The at-park check disagrees with an exact park position right after launch. Harmless but noisy.
- **`docs/desired-state-reconciler.md` still orders its plan with Focus first.** Before anything there is built, it should adopt the rule that the stacking worker makes the top key before the other raises (9d93a0f).
- **Shutdown takes up to 2s.** After SIGINT, the periodic-rescan thread only sends `Msg::Shutdown` between sleeps (`main.rs`, `period` = the rescan interval), so hotkeys keep being handled meanwhile. A restart during a burst saw 8 switches land after the signal.
