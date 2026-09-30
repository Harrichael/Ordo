# One queue per app: design

## 0. The calls, most opinionated first

1. **The engine stops waiting for apps.** Every write to an app goes onto that app's queue, and a long-lived thread per app works through it. A switch returns once its writes are queued; the apps land them in parallel, each at its own pace. Before, a switch was a row of steps (focus, moves, un-hides), and each step waited for the slowest app before the next began. Now each app runs its own chain, and a light app's windows land in a few ms whatever Chrome is doing.
2. **Frames stay observations; the queues say what to distrust.** Once writes land after the engine moves on, a read can catch a window mid-flight. The queues keep a record of every write still on its way, and `Desktop::in_flight(window)` answers from it. The model uses that in exactly two places (§3). Frames are never rewritten to where a window is going: the model's saved positions must be things the screen showed, and its enforcement budgets (`pending_repark`, `enforce_attempts`) depend on seeing the truth.
3. **A queue is ours, so we can cancel on it.** Nothing can take back a message once it reaches an app, but a job still on our queue can be dropped. A newer move for a window replaces a queued older one and takes the window out of any queued un-hide's hold. A newer focus anywhere makes a queued older focus a no-op. Rescue drops everything.
4. **Focus is emitted after the reveal.** The core now emits `FocusWindow` after the `SwitchWorkspace` or `ViewMonitor` that reveals its window. On the head window's app, that puts the focus after its moves and its un-hide, on one thread. Fronting a hidden app un-hides it with nothing holding its parked windows; that can no longer happen for a switch's own focus. The price is that keyboard focus waits for that app's chain (§5).
5. **A restack waits for its own apps.** A restack carries a marker that completes when the apps owning its windows have worked through what was queued before it. The worker waits on it, up to 600 ms and cancellable by a newer order, before planning. The outgoing apps' parks aren't waited on.

## 1. What a switch looks like now

On the engine thread:
1. The core decides. Effects: `SwitchWorkspace`, then `FocusWindow(head)` (or `FocusDesktop`), then `RestackWindows`, then `RequestRescan`.
2. `SwitchWorkspace`: frames from the window server (1 ms), the plan, `state.json` saved, then moves and un-hides queued per app. Returns.
3. `FocusWindow`: the owner's pid from the window server, and the focus queued on that app. Returns.
4. `RestackWindows`: a marker for the order's apps, handed to the restack worker with the order. Returns.
5. `RequestRescan`: the AX walk (about 30 ms). Its reads reach each app between two of our writes, not after the whole chain.

On each app's thread, in parallel: that app's moves, then its un-hide with the hold, then the focus if it owns the head window, then the marker.

On the restack worker: wait for the marker, then plan and raise as before.

## 2. The queues (`crates/ordo/src/app_queue.rs`)

Platform-free, behind an `AppSession` trait per app, so the tests drive it with a fake app. The Accessibility session is `ax::AxApp`.

One thread per pid, started on first use and exiting after 60 s idle.

Jobs:
- `Move { window, to }`: an `AXPosition` write. Consecutive moves run as one batch under one enhanced-UI bracket.
- `Frame { window, to }`: the position → size → position write of `SetWindowFrame`.
- `Show { hold }`: un-hide, holding the listed windows until the window server shows them at their spots (`show_app_holding`, unchanged in what it does).
- `Hide`: the Cmd+H hide.
- `Focus { window, generation }`: front the app, make the window key, raise it. Skipped if a newer focus was queued anywhere since.
- `Marker`: counts a latch down.

Queuing a `Move` or `Frame` for a window drops any unsent `Move` or `Frame` for it: the newest decision about a window is the only one left standing. An unsent `Show` for the same app follows it too. Each move says whether it parks (the model knows), and a park adds the window to the hold at its park spot, while a restore or a `Frame` takes it out. A burst can park a window after its app's un-hide was planned, and revealing the app drags every window it doesn't hold onto a display.

`AxApp` keeps the app element and its window elements between jobs. An element is used only while it still names its window (ids are recycled). It re-reads them when a window is missing or a write through one fails, then tries once more, but never re-reads twice for one write: against a hung app each re-read costs a messaging timeout.

A job that panics costs only itself: its windows' writes are treated as refused, and the app gets a fresh session. A dead thread would otherwise leave its queue growing, its windows in flight and every marker naming it waiting out its bound.

`abandon()` drops every unsent job, counts down their markers, stops a running hold and a running batch of moves at the next write, and forgets the in-flight record. An un-hide already sent still lands. It's called in two places:
- the periodic thread, the moment it sees the rescue signal, since the rescue CLI's gather is already running in its own process;
- the effector, on `SetIntercepting { enabled: false }`, which pause emits too.

Every write for a window must go to the queue of the same pid: the window server's owner, as every caller uses today. Two queues writing one window would land its writes in either order.

## 3. The in-flight record

Per window: which write it is (a sequence number) and its stage:
- **queued**: on our queue;
- **sent**: the thread is making the write;
- **written at t**: the write returned success at time t.

A refused write, or a window that escaped a hold, clears the entry. A window counts as in flight while queued or sent, or for `LAND_WINDOW` (500 ms) after its write returned: the window server's copy of a frame trails the app's.

The model asks in two places:
- **`park()`**, during a switch. A window whose restore is still on its way reads as parked. Skipping its park on that reading would let the restore land afterwards and leave the window on the wrong workspace. So an in-flight window keeps the promise its write was made from, and its park is written whatever the frame says.
- **`enforce_placement`**, on every scan. A hidden-declared window with a write on its way is skipped: no write, and nothing charged against it. It's judged once its write has had the chance to land, by the existing rules, `pending_repark` included.

## 4. Changes by crate

**ordo-core**: `FocusWindow` after `SwitchWorkspace` (switch) and after `ViewMonitor` (view, focus on a hidden monitor, demote), with the three tests that pinned the old order updated. `FocusDesktop` follows the switch too.

**ordo-emulated**:
- `Desktop::move_windows`, `show_apps` and `hide_app` return nothing and may land later; `Desktop::in_flight` is new; `Desktop::window_frames` is gone.
- The model no longer patches `WriteStat` into its rows or reads `HoldStat` back. `AppShown` rows say what was asked; the outcome is on the app's `AppChain` row.
- `SwitchCost` is now `read_ms`, `persist_ms`, `queue_ms`.
- The post-hide read-back is gone. It found nothing in runs 29–33, and with hides queued it would read before the hide landed.
- The fake desktop can hold moves back (`queueing`, `land_queued`), which is how the two in-flight rules are tested.

**ordo (shell)**:
- `app_queue.rs` and `ax::AxApp`, as above.
- `AxDesktop`: writes go to the queues.
- `MacEffector`: `FocusWindow` and `SetWindowFrame` are queued on the owner's queue (the pid comes from the window server), and they report `Ok` at once, or `Failed` if the window has no owner. `RestackWindows` passes a marker.
- The restack worker waits on the marker; `landing_wait_ms` is logged (schema v8).
- Trace: one `AppChain` row per app per chain, from its first job queued to its queue running empty. The row has the moves (each write's time and when it finished), moves replaced, the un-hide's hold stats, the focus (and whether it was skipped), and when each part finished. It replaces `AppMoved`.

## 5. Costs and risks, and what shows them

- **Keyboard focus waits for the head app's chain.** Chrome-headed switches that also un-hide Chrome wait its moves plus its un-hide (about 100 ms typical) before the focus lands, and keys typed meanwhile go to the old window. That's the price of call 4. `AppChain.focus.done_ms` measures it. If it hurts, the alternative is to focus first and hold in the model, as before.
- **Focus confirmation runs closer to the core's 1 s window.** A chain plus the app's own grant can push a switch's `Focused` expectation past 1 s, which re-grants focus. The note `FocusReasserted` counts those.
- **The restack starts later.** It starts once the destination's apps land, where before it started once everything had landed. `landing_wait_ms` shows the wait.
- **Faster engine, more intermediate switches.** The engine takes the next press after the rescan (about 30 ms) rather than after the whole switch, so a fast burst executes more of its presses. Replaced moves cut the cost on the apps; each intermediate un-hide still runs.
- **The 1a race is fixed for windows, not for the desktop.** The logged case of loose ends 1a is `FocusDesktop` activating Finder, whose parked window nothing holds. This design doesn't touch that path. The fix belongs in the model: add the desktop owner to the shows, with its hold.
- **Three focus writers**: the queues, the core's re-grants (which go through the queues too), and the restack worker's take-back, which goes straight to the app after its marker.

What's not changed:
- the post-switch rescan, which stays on the engine thread;
- rescue's own gather, which stays direct: the kill switch must not depend on the machinery it may be rescuing us from.

## 6. Tests

- `crates/ordo/tests/app_queue_test.rs`, against fake apps with a gate that holds an app mid-job:
  - one app's jobs reach it in order;
  - a slow app doesn't hold up another;
  - a newer move replaces a queued one and frees the window from a queued hold;
  - an unsent un-hide holds what is parked by the time it runs;
  - a frame and a move replace each other;
  - an overtaken focus never runs;
  - abandoning drops everything not yet sent, and stops a batch of moves at its next write;
  - a marker waits for its own apps, and for a job already under way;
  - a write is in flight until it has had time to land;
  - a write reporting late leaves the newer one in flight (checked to fail without the sequence check);
  - a panicking job costs that job only.
- The model, with the fake holding moves back:
  - a window whose restore is still on its way is parked when the user turns back;
  - a window whose park is still on its way comes back when the user turns back;
  - a scan older than our own park neither re-parks nor charges the window.

  Both were checked to fail with the in-flight rules taken out.
- The core: the three order tests.
- Live, from the log:
  - the `AppChain` rows (per-app finish times, focus done, replaced moves);
  - `landing_wait_ms` on restacks;
  - `FocusReasserted` notes;
  - the `Suppressed` and `Reassert` counts, and `Moved` rows at the park corner on the current workspace, against run 46.
