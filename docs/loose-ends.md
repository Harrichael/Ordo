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
| 46 | 1790720812195 | Whole-app drops from AX timeouts |
| 48 | 1790802567287 | Burst work; focus failures, escaped holds, a screen lock |

**Queries:** the switch-speed scripts are in `scripts/log-analysis/` (see `docs/switch-speed.md`); older analyses were ad hoc and aren't saved. The tables to read are `events`, `effects`, `notes`, `park_trace`, `restacks` + `raises`, `hotkey_batches`, `snapshots`.
- Time ordering inside one engine step: `park_trace.wall_ms` is stamped when the trace is drained, so use `rowid` for order.
- Stack reads at each step of a switch (`park_trace.kind = 'Stack'`, and `chain.show.stacks` on `AppChain`) are only logged in debug mode (menu bar Settings, off at every launch).

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
- **Partly addressed (53dbd9e):** the ledger now tracks which apps are hidden and hears every app's hide and show notifications. A hidden app with a window on screen is shown again at the next pass, and a switch catches unheard hides from the window list. The race above is narrower, not gone: a late hide after the switch's un-hide is heard, and undone at the next pass.

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
- The post-hide read-back is gone: it found 0 windows off their spot at every hide in runs 29–33, and with hides queued it would have read before the hide landed.

## 3. Notifications and app-initiated windows need a window role

**Evidence** (window subroles across the whole log):
- **Outlook's reminder** reports `AXStandardWindow` (400×127). It was adopted as a normal window: run 22, workspace 1, monitor 2. It would be parked when the user switches away.
- **Subroles flip on real windows.** Main windows (Outlook Inbox, OneNote, Teams, Terminal) sometimes report `AXDialog`.
- **Popups are clean.** Chrome and OneNote popups report `AXUnknown` or no subrole (about 150×22). They are currently managed as windows.
- **Chrome's find bar,** window 44863, lived 226 snapshots inside its Chrome window and holds its own ledger claim.

**Deployed (9585ccd):** snapshots record each window's `layer` and `parent`, from the window server, asked once per window. Run 38 saw 14 attached Chrome windows (address-bar suggestions, link-hover bubbles). Still to collect: Chrome Cmd+F, an Outlook reminder, dialogs, call popups.

**Proposed shape:**
1. **Attached popups** (have a parent): follow the parent, no workspace of their own. Done for stacking and the MRU order (3e5d609: the history holds root windows, and a restack carries each root's attached windows as part of its footprint). Parking and workspace placement still treat them as windows of their own.
2. **Independent transients** (born without focus while their app wasn't frontmost; reminders, PiP): follow the user across workspaces until touched.
3. **Normal windows:** as today.
4. **Per-app override rules** as an escape hatch.

AeroSpace for comparison: popups go in a global container; there's no sticky support (their issue #2); their dialog heuristic keeps misfiring.

## 4. The settled-move restack enforces MRU on the whole workspace

When a window moved by a hand that isn't Ordo's holds still, the core asks for a restack (`restack_settled_moves` in `update.rs`). The restack enforces the MRU order on every overlapping pair on the workspace, not just the pairs that include the window that moved.
- **Seen:** run 45, restacks 3384 and 3385. A Chrome popup resized on the right monitor, and the left monitor was reordered.
- **Proposed:** the effect carries the moved roots, and the planner (`restack.rs`) filters its edges to pairs touching them. Switch restacks stay whole-workspace.

## 5. Unconverged restacks with no timeout

Run 25 (old planner): 4 restacks ended `converged = 0`, `restack_id` 1279, 1307, 1310, 1322, all with kitty as top. With the overlap-only planner (2c92712): the 8 in run 37 were the Chrome popup fight, fixed by 3e5d609; 1 in run 39; none in runs 38 and 45.
- **Consequence, still true:** an unconverged restack gets no ghost watch (`restack_worker.rs`: the watch requires `converged`), so a wrong order can stay until the next switch.
- **Proposed:** watch the rate. If it stays near zero, drop this item. Otherwise let the ghost watch run after an unconverged pass too.

## 6. Snapshot cost leftovers

- **Six ledger windows are alive but never scanned:** 24998, 44850, 44863, 67652, 70295, 70296 (minimized, on another Space, or the Chrome find bar). Every snapshot therefore calls `existing_windows`, a full CG list at about 2ms. Cache the "alive but unscanned" answer between snapshots. Most are Chrome helper popups the ledger adopted once and can't forget (see §9).
- **About 5–7ms per snapshot outside the window walk is untimed** (`snapshots.total_ms - walk_ms - enforce_ms`). Candidates: `focused_window`'s AXFrontmost loop, `believed_frames`, trace building, persistence.

## 7. Fly-by flashing of intermediate workspaces

Run 21: every press was a full switch, because the engine keeps up with presses about 170–300ms apart. Passing through workspaces 1–2, whose windows were on monitor 2, showed them briefly.

**Proposed, not built:** in a burst, hold a press that arrives within about 150ms of the previous one until about 100ms of quiet, then switch straight to the final target. The core already folds runs of Prev/Next (`coalesce_hotkeys`). Cost: about 100ms later landing, on fast bursts only.

## 8. Smaller items

- **Startup "re-parks" that move nothing.** 10–14 per restart, `Reassert` rows with observed == requested; every restart on 2026-09-28/29 had them. The at-park check disagrees with an exact park position right after launch. Harmless but noisy.
- **`docs/desired-state-reconciler.md` still orders its plan with Focus first.** Before anything there is built, it should adopt the rule that the stacking worker makes the top key before the other raises (9d93a0f).
- **The restack conflates the top window with the key window.** `RestackWindows { focus_top }` takes focus back only for `order[0]`. A floating declared focus (the screenshot window, layer 3) is left out of the order, so the worker never takes focus back to it after an un-hide; `enforce_focus` re-grants it on the next look instead, damped. The fix is `focus: Option<WindowId>` on the effect, taken back whether or not it is in the order: one line in the worker's take-back.
- **Shutdown takes up to 2s.** After SIGINT, the periodic-rescan thread only sends `Msg::Shutdown` between sleeps (`main.rs`, `period` = the rescan interval), so hotkeys keep being handled meanwhile. A restart during a burst saw 8 switches land after the signal.

## 9. Windows the scans miss, and holds on windows that aren't there

Investigated from run 48 (and 46, 47, 49). What was fixed, and what is left.

**Fixed:**
- **"focus: window not found" was a hidden app, not a missing window.** All 5 in run 48 (seqs 261, 451, 554, 603, 955) and all 10 in run 49 targeted a kitty window while kitty was still hidden: the switch had just queued kitty's un-hide, and `zorder::owner_of` asked the `kCGWindowListOptionIncludingWindow` list, which leaves out a hidden app's windows. The window was in the model before and after each one; the `focus_reasserted` that followed was the core retrying. `owner_of` now asks by name (`describe`), which answers for hidden apps.
- **An app that doesn't answer loses its windows for one scan.** Run 46 seq 3704: all six kitty windows vanished at once, the scan's slowest app was kitty at 204 ms (the 0.2 s timeout), and they came back 1.8 s later. Run 47 had 15 such whole-app drops, 14 with `walk_ms` of 195 or more. `app_windows` read a timed-out `AXWindows` as no windows. The snapshot now carries `unread`: the windows of apps that returned `kAXErrorCannotComplete` which the window server's full list still has, so an app that keeps timing out can't keep closed windows alive. The core keeps them as last seen, placed by the backend's word.
- **A locked screen emptied the model.** Run 48 seq 662-670: two displays, 11 apps, 0 windows, for 13 minutes; every window was destroyed in the model, and after the 10 s vanish grace lost its MRU place. Each of the windows named in the handoff (41128, 67722, 44837, 80126, 77899) vanished in run 48 only in this one episode. `MacWorldSource::snapshot` now reports a scan in which no app lists a window, while the window server's full list still has windows the last snapshot held, as no displays at all (the existing "unobservable" path), and says so once on stderr. A scan whose missing windows are gone from that list too is believed: the last window closing.
- **"Escaped" holds were ghost ledger entries.** All 10 non-converged Shows in run 48 escaped only windows their app never lists in `AXWindows`: kitty's 22570 (64x64) on every kitty hold, and 11 Chrome ids (23310, 24998, 44850, 44863, 66427, 67652, 72006, 73055, 73389, 75111, 77901; omnibox dropdowns, a find bar, a status bubble), each an id next to a real Chrome window's. None appears in the scans around the holds (only 24998 and 75111 appear in run 48 at all, briefly, as untitled popups); most never appear in any run since 30. No window the app lists escaped, and nothing points to a refused write. The hold never wrote to them (they weren't in the chase), but the final check counted them. `show_app_holding` now drops held windows the window server doesn't have as this app's, reports the ones the app lists nothing for as `unreachable` rather than escaped, skips the enhanced-UI toggle when a showing app's listed windows already hold, and retries a refused write once through a fresh element before giving up on it. An app that doesn't answer for its window list is asked again after the un-hide; its windows stay chased, and read as escaped if they don't hold.

**Still open:**
- **The ledger adopts helper popups and never forgets them.** `note_scan` adopts any window an AX scan lists, `AXUnknown` popups included, and forgets one only when the window server's full list drops it. Chrome keeps its helper windows alive while they're hidden, so they stay in the ledger across restarts (all of the ids above are in `state.json`), are parked and held on every switch, and cost `existing_windows` a full CG list per snapshot (§6). This is §3's problem: decide which windows are managed before adopting them. A narrower fix would be to hold, park and check only windows the latest scan listed, keeping the claim for when one reappears.
- **Chrome's un-hide takes 177-257 ms**, most of it the un-hide round trip itself, not the hold waiting. Preview's plain un-hide, with nothing held, has a median of about 107 ms.
- **A window whose own reads time out is still dropped.** Only a timed-out `AXWindows` list marks the app unread; a timed-out `AXPosition` or `AXSize` on one window drops just that window. No case seen: run 47's 99 single-window drops were Chrome popups coming and going, none with a slow walk.
- **`snapshots.slowest_pid` names one app.** When two apps time out together, the one that lost its windows may not be the one named.
