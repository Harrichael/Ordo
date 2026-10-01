# Keeping up with a burst: design

Built on top of `02456b1`, which fixed the un-hide hold (call 3). The code: `State::declared_workspace` (ordo-core), `app_queue.rs` (cancellation, focus placement, the fronting lock, the desktop, `focus_if_current`, `idle`), `platform/look_gate.rs` and `Engine::with_look_gate`.

The goal: spam Cmd+→ through nine workspaces, and have the apps go from wherever they are to workspace 9, doing nothing for the workspaces passed on the way. Today the engine is busy 90 ms per switch (74 of it the post-switch scan), so fast presses pile up. The piled-up presses are folded into one switch, which is why bursts sometimes work now, but only by accident of the engine being slow.

## 0. The calls, most opinionated first

1. **The engine never looks at the screen while the apps are busy with our writes.** An AX read waits behind our own writes in the app's queue: the post-switch scan went from 29 ms to 74 ms once the writes moved onto the apps' queues. Every full scan is held until the queues are idle, up to 250 ms. A thread outside the engine holds the request, and the engine checks again that the queues are idle before it scans, since a press can start new writes in between.
2. **The core resolves presses against the workspace it declared, not the one it last saw.** Today "next" resolves against the workspace the last scan confirmed, so without a scan between presses a burst would switch to workspace 2 over and over. The core already has this pattern for focus: `declared_focus()` reads Ordo's declaration while one stands, else the observation. `declared_workspace()` does the same: the target of the switch under way, else the observed workspace. Confirmation stays on the look, and no new event is needed. (As first built, it read the newest pending `AllMonitorsOn`, which borrowed the intent's lifetime from the confirmation bookkeeping; §7 replaces that.)
3. **The queue keeps each app's un-hide consistent with the newest plan.** A queued un-hide holds the windows that were parked when it was planned. A later switch in the burst can park another window of that app. The un-hide must hold that one too, or revealing the app drags it on screen. This is a bug in the build that's running now. Bursts make it common.
4. **A write that undoes a queued write cancels both, but only if the last write was seen to land.** Passing workspace 5 queues a restore; passing 6 queues a park of the same window. When the window's previous write is known to have landed at the park spot, both go.
5. **Focus goes as early as it's safe, and fronting the app happens one at a time across apps.** A focus is placed after its app's queued un-hide and after its own window's queued move, else first. Only the step that fronts the app is serialized, with the generation checked inside the lock, so a stuck app can't hold every other focus behind its slow Accessibility writes.

Not in this design: retracting an unsent un-hide for an app on a workspace flown past. It costs that app about 100 ms and delays nothing else. Measure first.

## 1. A burst, after this

Presses at 40 ms apart, from 1 to 9. On the engine, for each press:
1. Hotkey. The core resolves "next" against `declared_workspace()`.
2. `SwitchWorkspace`: frames from the window server, the plan, `state.json` saved, jobs queued. About 4 ms.
3. `FocusWindow`: queued, placed early on the head window's app.
4. `RestackWindows`: marker and submit.
5. `RequestRescan`: handed to the scan gate, and the engine returns.

On each app's queue, each switch replaces what the last one queued. Moves are replaced or cancelled, older focuses skipped, and an unsent un-hide holds whatever that app has parked. The queues keep converging on workspace 9.

The scan gate holds the requested look until the queues are idle, or until 250 ms have passed. The first look lands once workspace 9 has landed, and it confirms the switches, focus and frames.

## 2. `declared_workspace` (ordo-core)

- `State::declared_workspace()`: the target in `State::workspace_intent`, else `current_workspace()` (see §7).
- Used by `resolve` for Prev, Next and SwitchTo, by `coalesce_hotkeys`, and by Carry. Carry also reads a window's pending `WindowOn` as its workspace, so a second carry at burst speed resolves.
- At the head-focus expectation (`update.rs`, switch and view), compare against `declared_focus()`, not `s.focused`. On a quick bounce (1 → 2 → 1) the observed focus is stale and the expectation would otherwise be skipped.
- Nothing else changes. `restack_settled_moves` stays gated on no pending ops, and the tear guard keeps the pending switch, which is what a burst needs.

## 3. The scan gate (shell)

- A small thread holding the latest requested look and its trigger. Creation hints keep their pids, as `collapse_rescans` does now.
- It waits on `AppQueues::wait_idle(deadline)`, woken by a condition variable (including by `abandon`), then sends `Msg::Rescan(trigger)` to the engine.
- The engine, on dequeuing a gated rescan, checks `queues.idle()` again. If the queues are busy, it hands the request back with its original deadline instead of scanning. So a look only runs when the engine itself sees the apps idle, or when the bound has passed.
- Through the gate: the post-switch rescan, periodic rescans, the observer's app hints, the space watcher, and `display_watch`'s hints.
- Not through it: startup and engage, when nothing is queued, and the look after a gesture (`SystemSwitch`, clicks). A gesture queues no writes, and its look must come before a later press clears the gesture's navigation mark.
- The enforcement pass's deferred hides (`hides_due`) wait while any lane is busy, so a long burst doesn't hide and un-hide the same apps mid-burst.

## 4. The queues

**Un-hide holds follow the newest plan.** `Move` carries whether it parks the window, which the model knows. When a park for a window of app A is queued, it's added to the hold of any unsent `Show` for A, at the park target. A restore still takes the window out of the hold, as now.

**Round-trip cancellation.** Per window, the queue keeps `landed_at`: the target of the last write that returned success, set only when that write returned and nothing since has cast doubt on it. It's cleared when a write is refused, when a hold lets the window escape, and when any `Show` for the app runs. That last one is because AppKit re-homes windows on an un-hide without telling us. When a `Move` replaces an unsent one and its target equals `landed_at` (to 1 point), both go, and the window is no longer in flight. A `Move` with nothing to replace is always queued: it may be a re-park after the app moved the window itself.

**Focus placement.** A `Focus` is inserted after the last unsent `Show` for its app, and after the last unsent `Move` or `Frame` for its own window; failing both, at the front. Everything else stays in order.

**The fronting lock.** A global mutex around the SLPS pair (front the process, make the window key), with the generation re-checked inside it. The Accessibility writes that follow (main, focused, raise) run outside it. A lane that sees it has been overtaken while waiting for the lock skips without taking it.

**The desktop.** `FocusDesktop` becomes its own job kind on the desktop owner's queue (Finder), since the desktop isn't an Accessibility element. It takes a generation like a focus. If Finder is hidden, a `Show` with Finder's parked windows in its hold is queued ahead of it: the backend gains `hold_for(pid)`, which the model answers from `apps_on_screen`. This is the loose-ends 1a case.

**The restack worker's take-back** goes through `AppQueues::focus_if_current(pid, window, gen)`. The gen is the one current when the worker's job was submitted, and the take-back is dropped if a newer focus exists. It never mints a new generation, so it can't overtake a switch's focus.

## 5. Risks

- **The core runs on intent for up to 250 ms.** Between a switch and its look, frames and focus are as of the last look. Commands resolve against the declared workspace and declared focus, so they follow the burst. A gesture gets its own look straight away.
- **A look that is always deferred.** If an app never idles, every look waits the full 250 ms. That's a bound, not a hang, and the gate logs each look's wait.
- **Fewer folded presses.** `coalesce_hotkeys` folds only what arrives while the engine runs one switch or one look. That's the point: each press runs, and the queues throw away what's been passed.

## 6. Tests

- **Core:**
  - three "next" presses with no snapshot between them land on workspace 4;
  - two carries at burst speed both resolve;
  - a 1 → 2 → 1 bounce with stale observed focus pushes one `Focused` expectation per switch, and the look notes no outside focus change.
- **Queues, with fake apps:**
  - the three-press un-hide scenario: the `Show` reaches the app holding the window the later switch parked;
  - a restore then a park of the same window, both unsent, after a landed park: nothing reaches the app;
  - the same after a hold that let the window escape: the park is written;
  - a move equal to where the window landed, with nothing to replace, is still written;
  - a focus lands after its app's queued un-hide but before its unrelated moves;
  - two focuses on two apps never front out of order;
  - a stuck app's focus doesn't hold up another app's;
  - the worker's take-back is dropped once a newer focus is queued;
  - a desktop focus for a hidden Finder comes after a `Show` holding Finder's parked window;
  - `wait_idle` returns once every lane is drained.
- **Model:** with the fake holding moves back, a burst through an app's workspaces leaves every one of its windows either at its restore target or at its park spot.
- **Engine, with fakes:**
  - a burst takes no snapshot while the queues are busy, and one once they're idle;
  - a gated look dequeued while they're busy is handed back, not run;
  - a gesture during a burst is followed.
- **Live:**
  - `app-chains.py` for `cancelled` and `replaced` counts;
  - press-to-landed for the last press of a burst;
  - the gate's wait per look;
  - the engine's time per switch.

  Promise against the whole engine step, not the 4 ms of switch work: in run 47 the step was 16 ms at the median excluding the scan (90 − 74). Where the other ~12 ms goes (core, logging, the effects' own work) gets measured first.

## 7. Intent is not observation, and not bookkeeping

A rule behind the fixes after run 48 and 49: `State` keeps what the user or Ordo decided (the workspaces, the MRU window order, declared focus, the workspace a switch is taking the user to) apart from what the screen showed, and apart from the bookkeeping that confirms an op. Each bug in this batch was intent derived from one of the other two.

**The workspace intent is its own field** (`State::workspace_intent`, the switch's op and target). `push_switch` in `update.rs` is the single emitter of `SwitchWorkspace`: presses, carries, a followed gesture and tear re-alignment all go through it, and each writes the intent. The intent ends with its own op: confirmed, failed (`OpFailed`), lost (`OpLost`, which only a look can declare, so a run of presses with no look between them never loses one), or the kill switch. A newer switch overwrites it. `AllMonitorsOn` stays in `pending` only to confirm and attribute the switch, and there is at most one: the new switch retires the old one with `Note::OpSuperseded { op, by }`. Before, the intent was the newest pending `AllMonitorsOn`, and an intermediate switch whose look was folded into a later one could never confirm: presses stepped from it for a second (run 48).

`declared_workspace_of` (the carry's read) stays a read of the pending `WindowOn`. A window's workspace is the backend's word, which every snapshot carries back, so the pending move only bridges the gap until it does. What made it wrong was a second pending move for the same window, and that is gone: a newer move retires the older one (`OpSuperseded`), so at most one is pending per window, and an expired older one can't be retried over the newer.

**A newer focus grant retires the older one.** Each app queue skips a focus once a newer one is queued anywhere, so a grant's expectation is moot the moment the next `FocusWindow` or `FocusDesktop` is issued (`push_focus`, `push_desktop`, through `retire_grants`). They are retired with `OpSuperseded` rather than left to expire as `OpLost` a second later (32 `op_lost` notes in run 48, half of them focus). Two exceptions keep a grant in flight on record. A declaration with no new grant (a followed gesture, a held focus) retires nothing. A new grant to the same target that books no expectation of its own (the last look already saw that window key) leaves the older grant for it pending, since that is what tells enforcement a grant is on its way. `enforce_focus`'s "grant in flight" guard reads the one grant left, which is the newest: a look that catches an overtaken grant landing reads it as someone else's focus change and waits for the newest.

**Only the user writes the MRU order.** Ordo's own commands write it through `declare_focus`, as before, and so does a window born with focus. From observation, only a focus change the user's input explains is written (`record_landing` in `update.rs`, `State::unseen_landing`):
- Any click, Cmd+Tab or menu bar click explains the first focus change after it, onto any visible window, within `AWAY_TTL_NS` (one second). That covers a click in one window that fronts another.
- A key press explains the first focus change after it within the same second, but only one that stays within the app focused when the key was pressed (an app's window shortcut), or any change when focus was then on no model window (Spotlight, Raycast, Alfred, the desktop). Another app grabbing focus while the user types (a reminder, a huddle window) stays unexplained and is fought as before; without the limit, a key every 300 ms would keep the window open for good. Each later key press restarts the window, judged against the focus at that press.
- Each explained landing is logged as `Note::LandingExplained { window, by }`, so the adoption rate by kind of input can be read from the log.
- A click also explains a landing on a window it hit, whatever came first, within `INTO_TTL_NS` (three seconds): an app with no observer is seen only by the periodic look, two seconds apart.
- A command clears both. The OS's focus at startup is also written, as the user's last choice before Ordo ran.

**A window closing hands focus on, by Ordo's choice.** When the focused (or declared) window closes, macOS fronts the app's next window wherever it is: in run 50 (seq 159-161) a click closed Chrome's window on the right monitor and, 2.7 s later, Chrome's window on the left came up key and on top. A close is the user's act (or the app's, for a dialog that dismisses itself, treated alike), and what it means for focus is Ordo's to say, so `hand_on_from_close` in `update.rs` declares the next window in the MRU order on the closed window's monitor and workspace, grants it and restacks that monitor under it, or gives that monitor its desktop when nothing is left there (moving the anchor to it when that changes nothing on screen, since the desktop declaration is held on the anchor's display). Whatever the app keys in the look that shows the close is fallout, not where the click went, so that look writes nothing into the MRU order; the declaration is then enforced against the app's later re-key like any other, damped. It relies on a closed window being one the window server agrees is gone: the shell lists every window a scan missed but the window server still has as `unread`, whether or not its app answered, since an app's AX read drops a window now and then (run 45 seq 1969: a focused Slack window, for one scan), and a hand-on there would move the keyboard while the user types. The price is that an app ordering a window out instead of closing it leaves a ghost the model keeps; the shell logs each `Unread` episode to `park_trace` (begun, turned ghost after 10 s while its app answers, ended seen again or gone), so whether that happens can be read off the log. It stands aside when the closed window is attached to another (a popup, a find bar: focus goes back to its root by itself), when the look lost several apps' windows at once (the lock screen or a native Space: they come back, and a desktop declaration would be fought on their return), when a window born in that look took focus, and when the user's latest input in the same look can't be what closed the window, which an app seen only by the periodic look makes possible: Cmd+Tab, a click that missed the closed window (the Dock, the desktop, another window), or a key press that brought up another app (a launcher). A key press within the closed window's app is Cmd+W, a key press after which that app has no windows left is Cmd+Q (macOS then keys an app of its own choosing, which is fallout, not a launcher's pick), and a click that ends an open menu is the menu's (File > Close), so all three are handed on. The other monitor's stack is left as the app left it: undoing macOS's raise there would raise over the user, and the next switch restacks it from the MRU order anyway.

Everything else the screen shows is left out: in run 49 (seq 11-23) a 27 ms flicker onto a Chrome window, ten seconds after two clicks, became the second window of every later restack of its workspace.

Key presses come from the event tap as a bare `Gesture::Key`: no key identity, so the log never holds what was typed, and at most one per 300 ms (`keys::KeyWitness`). The log does record when keys were pressed, at that 300 ms resolution: a `gesture` event per report. None is reported while Ordo is paused or rescued, when nothing reads them, and an Ordo chord never produces one; it is a command already. Unlike a click, a key press does not hand the focus slot to the OS: typing into the window Ordo declared is not reaching elsewhere. So a key-explained change arrives while a declaration stands, and it becomes the declaration, or enforcement would take the user's own move back. The exception is a change while one of Ordo's grants is in flight, which is that grant's doing or its fallout. A key press is not a fence: it splits neither hotkey coalescing nor rescan collapsing, and its look is held by the look gate like any other. Typing runs through a burst without being part of it, and what it explains is the look after it. Nor does it license following focus onto a hidden workspace: that still takes a click outside every window, Cmd+Tab or Cmd+\`, through `navigation_gesture`, which the next look spends.
