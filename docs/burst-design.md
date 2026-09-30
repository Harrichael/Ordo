# Keeping up with a burst: design

Built on top of `02456b1`, which fixed the un-hide hold (call 3). The code: `State::declared_workspace` (ordo-core), `app_queue.rs` (cancellation, focus placement, the fronting lock, the desktop, `focus_if_current`, `idle`), `platform/look_gate.rs` and `Engine::with_look_gate`.

The goal: spam Cmd+→ through nine workspaces, and have the apps go from wherever they are to workspace 9, doing nothing for the workspaces passed on the way. Today the engine is busy 90 ms per switch (74 of it the post-switch scan), so fast presses pile up. The piled-up presses are folded into one switch, which is why bursts sometimes work now, but only by accident of the engine being slow.

## 0. The calls, most opinionated first

1. **The engine never looks at the screen while the apps are busy with our writes.** An AX read waits behind our own writes in the app's queue: the post-switch scan went from 29 ms to 74 ms once the writes moved onto the apps' queues. Every full scan is held until the queues are idle, up to 250 ms. A thread outside the engine holds the request, and the engine checks again that the queues are idle before it scans, since a press can start new writes in between.
2. **The core resolves presses against the workspace it declared, not the one it last saw.** Today "next" resolves against the workspace the last scan confirmed, so without a scan between presses a burst would switch to workspace 2 over and over. The core already has this pattern for focus: `declared_focus()` reads Ordo's declaration while one stands, else the observation. `declared_workspace()` does the same: the target of the newest pending switch, else the observed workspace. Every field of `State` keeps a single source, confirmation stays on the look, and no new event is needed.
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

- `State::declared_workspace()`: the target of the newest pending `AllMonitorsOn`, else `current_workspace()`.
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
