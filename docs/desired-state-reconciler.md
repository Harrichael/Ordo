# Desired-state reconciler

Status: **design plan, not started** (written 2026-08-27; revised same day
after adversarial review — see "Review outcomes" at the end). This
restructures the core's command handling from delta-emission to
goal-convergence.

Staging recommendation (changed by review): ship P1 (hotkey coalescing at
dequeue) and the generation-checked restack worker (P2/P4) **first**, under
the current architecture. They are cheap, reversible, cover the measured 80%
(the 1.9s restack tail and stale-replay — and RestackWindows is already
fire-and-forget with no op/expectation/belief, so a restack worker touches no
attribution or replay contract), and they double as the instrumented
experiment that tells us whether the residual ~150ms engine-blocking justifies
this rewrite. This doc is the destination if the answer is yes.

## Why

Measured (run 34, `restacks`/`events` tables): a workspace switch blocks the
engine thread 350–600ms typically (focus 20–50ms, park/restore + visibility
115–150ms, landing-gated restack 180–410ms), and ~1.9s when a straggler raise
ghosts (restack 82: one kitty raise took 1496ms, forced the second pass, never
converged). Hotkeys pressed during the stall queue in the channel and replay
stale — the log shows a hotkey stamped 0ms after a 1724ms-late rescan, firing a
whole extra switch. Rapid back-and-forth is the worst case: each press pays the
previous press's convergence cost, serially, and then replays.

The structural claim (stated honestly): effects-as-deltas queue and go stale;
expectations-as-memory can fight a world that moved on. The drag-yank and
clamp-fight bugs were fixed under the current model (`hands_on`,
monitor-arrival satisfaction), so the current model CAN express the fixes —
one guard at a time. The reconciler's bet is that goal-diffing makes the whole
class unrepresentable and makes rapid-command coalescing free, at the price of
a real rewrite of reconcile.rs, the best-tested module in the core.

## The idea

Split the core's state and put a pure planner between the halves:

- **Believed** — what observation says the world is. Updated ONLY by
  `WorldObserved`, exactly as today. Belief-follows-observation survives
  intact; the rule was never "no intent in state," it was "never confuse
  intent with belief."
- **Desired** — what the goal is. Mutated by commands in microseconds, and by
  two other explicitly-named writer categories (below). Rapid commands
  coalesce for free: there is no stored delta to replay, only the current
  goal.
- **Planner** (pure, ordo-core): `plan(&Desired, &Believed) -> Vec<Action>`.
  Recomputed from scratch every pass — the goal-vs-belief diff is derived,
  never remembered, so it cannot go stale or leak.
- **Converger** (shell worker thread): executes the plan in bounded steps,
  checking a generation counter between steps and inside every poll loop. A
  new generation abandons at the next stopping point and re-plans from the
  latest state. Work the old and new goals share drops out of the recomputed
  plan.

The lived scenario: on ws2, converging toward ws3, user presses "next" →
`desired.workspace` becomes 4 (computed from the GOAL, see "Command read
context") → worker abandons at its next stopping point → plans toward 4 from
wherever reality actually is. No queue, no stale replay, no wasted work, and
the engine thread was never blocked.

### Three writers of Desired

Naming them prevents the categories from blurring (each has different rules):

1. **Commands** — hotkeys. Decide targets at command time (Alt+Tab picks its
   MRU window when pressed); the planner decides HOW, never WHAT.
2. **Adoption** — external world changes we choose to accept (the
   fight-or-adopt table below).
3. **Policy** — observation-triggered goal-setting that is neither: the
   new-window corral, follow-the-focus, tear realignment. These exist today
   as effect-emitting reconcile arms; here they become Desired mutations.
   Their anchor is the GOAL context, not believed: a window created
   mid-convergence corrals to `desired.workspace`, not to the workspace being
   left (the synchronous cascade used to guarantee this by blocking; the
   reconciler must guarantee it by rule).

## Command read context

Commands compute their targets against **Desired overlaid on Believed**: goal
attributes (`workspace`, `focused`) read the goal; history and geometry read
belief. Without this rule the flagship scenario fails — `WorkspaceNext`
computed from believed ws2 mid-convergence yields 3 twice instead of 3 then 4,
and Alt+Tab pressed mid-switch would pick its MRU candidate relative to the
workspace being LEFT, writing a `desired.focused` that contradicts
`desired.workspace`. Consequences to encode as tests, not prose:

- Double WorkspaceNext mid-convergence → targets 3 then 4.
- Alt+Tab mid-switch → candidate filtered by `desired.workspace`'s MRU.
- Double Alt+Tab before the first focus is observed → the second press
  toggles from `desired.focused`, not from stale believed focus (today the
  cascade's PostEffect rescan usually hides this; the reconciler must not
  regress it).

## State schemas

`Desired` stores decisions, never derivations:

```rust
struct Desired {
    workspace: WorkspaceId,                      // the goal workspace (spans monitors)
    window_ws: BTreeMap<WindowId, WorkspaceId>,  // assignment goal
    frames: BTreeMap<WindowId, Rect>,            // restore target — PARKED /
                                                 // placement-intent windows only
    focused: Option<WindowId>,
}
```

- `frames` is deliberately partial: a visible window under no active placement
  intent has NO goal frame, so no attribution error can ever make the planner
  yank a visible window — the invariant is in the data shape, not planner
  discipline. Entries are written by placement commands (move-to-monitor
  translates), by corral policy, and by **park receipts**: the
  `Place{visible:false}` executor reads the actual frame immediately before
  parking and reports it back, preserving today's capture-at-park-transition
  freshness (goal-maintained-by-2s-polled-adoption would restore stale frames
  after a drag-then-switch within one scan interval). An entry is dropped when
  its window is visible and converged.
- Derived at plan time, NOT stored: z-order (from `focus_history` — raising IS
  focusing, one fact), park geometry, app dimming.
- `focus_history` stays believed-side with one carve-out: MruDemote reorders
  it at command time (it is a command ON the history). The unconditional
  MRU-touch on observed focus is suppressed while an unsatisfied
  `desired.focused` is in flight and the observed focus is the window being
  demoted/left — otherwise any rescan mid-handoff un-demotes and re-derives a
  restack whose top contradicts the goal (the unsatisfiable top≠key
  configuration behind the old 13s freeze). This uses in-flight knowledge —
  see attribution.
- **Convergence bookkeeping** is a third core-state category, neither goal nor
  belief: per-(window, axis) damping counters and last-action outcomes. The
  worker reports per-action results (`ActionResult { generation, action,
  outcome }`) back through the engine — this replaces, rather than deletes,
  today's `EffectResult`/op-outcome plumbing, and keeps "the AX write failed —
  window gone" distinguishable from "slow app." Damping must NOT live in the
  worker: generation bumps would reset it, and a stubborn app's re-placements
  generate the very observations that would bump generations — the eternal
  fight `DAMPING_LIMIT` exists to prevent.

## The planner

```rust
enum Action {
    Focus(WindowId),
    SetAppVisibility { app: Pid, visible: bool },
    Place { window: WindowId, visible: bool, frame: Rect },
    Restack { order: Vec<WindowId> },
    SwitchSpaces { target: WorkspaceId },  // native backend only
}
```

Plan ordering is perceived-latency ordering:

1. `Focus` — first; physics wants the key window settled before raises.
2. Unhides + `Place{visible:true}` for the incoming workspace (reveal first —
   AeroSpace does the same "to reduce flicker").
3. `Restack` — MRU order for the goal workspace.
4. `Place{visible:false}` for the outgoing windows.
5. Housekeeping: cosmetic hides (Dock dimming), corrections of externally
   perturbed parked windows.

The tail is the interruptible-without-consequence part: under rapid switching,
generations preempt before step 5, so dimming and tidying retreat to idle time.
On a fast bounce back, outgoing apps were never hidden — no unhide wait, no
re-render stall.

## The converger

One dedicated thread owning all **window placement writes** (frames, raises,
app visibility). Explicitly NOT worker-owned: `WarpMouse` (latency-critical CG
call, stays engine-side) and `SetIntercepting` (an atomic flag flip). The
contract:

- Inputs: `(generation, Desired, Believed)` snapshots from the engine — always
  taken atomically from one core state, so one seq identifies both halves.
- A **step** is one bounded unit: one app's frame batch, one raise + its
  landing gate, one visibility write. Generation is checked between steps and
  at every 5ms poll tick. Worst-case deafness to a new command: one poll tick
  plus one in-flight AX write (~0.2s messaging-timeout bound).
- Landing gates do not disappear. Sequenced, landing-confirmed raises remain
  the only deterministic cross-app ordering with SIP on (probed); the gates
  just become interruptible. The raise-overlap plan (docs/raise-overlap.md)
  later lives entirely inside the executor as "how many raises per step" — no
  interface change, which is the test that this interface is the right shape.
- **In-flight tracking (expectations, demoted).** The worker keeps, for the
  current generation, the set of issued-but-unconfirmed actions with
  satisfaction predicates (today's expectation predicates: monitor-arrival for
  cross-monitor places, sliver-arrival for parks, key-status for focus). This
  memory is executor-internal and dies with the generation — it never re-enters
  core state — but it is NOT optional: pure toward/away attribution cannot
  classify our own transients (a park write moves a frame AWAY from its
  restore goal by design; hiding an app can fling focus arbitrarily — adopting
  that as user intent would corrupt the goal). The engine's attribution
  consults it (the worker shares a read view) as category "in-flight, ours."
- **Ghost absorption is a cross-generation responsibility.** Abandoning
  mid-restack routinely leaves a raise in flight that lands during a LATER
  generation (rapid bouncing makes this the common case, and z-order is in
  neither state half — a post-convergence ghost would otherwise be silently
  permanent until the next switch). The worker's quiescent state therefore
  ends with a CG stack read-back against the last plan's restack order,
  re-running the absorb pass if broken — today's second pass, promoted from
  one call's internal detail to the worker's settle contract.
- After finishing or abandoning a generation the worker sends
  `Msg::Rescan(Converged { generation })`; the loop closes through
  observation, off the engine's hot path.
- Rescue: bumps generation to a poisoned value ("stop and park the thread");
  the gather already runs on its own thread and trusts only the log + raw OS
  reads, unchanged.

## Attribution and fight-or-adopt

Deltas are classified into FOUR categories: toward-goal (echo/benign),
**in-flight ours** (matches a worker in-flight predicate — ignore, it's us,
mid-move), away-goal external (consult the table), and belief-only facts
(created/destroyed):

| Attribute | External change | Response |
|---|---|---|
| Frame of a visible window | drag/resize | **Adopt** — and since visible windows have no goal frame, there is usually nothing to even update. Kills the drag-yank class structurally. |
| Frame/monitor of a parked window | app or OS moved it | **Fight** — re-park, damped. |
| Window workspace assignment | e.g. native: user drags window to another Space in Mission Control | **Adopt** `desired.window_ws`. (Today this is absorbed silently; fighting it would be a regression.) |
| Focus | user clicked / Cmd+Tab | **Adopt** `desired.focused`; follow-the-focus policy decides whether `desired.workspace` adopts too (close-fallout rule carries over verbatim). |
| Workspace visibility (native) | external space switch | **Adopt** `desired.workspace` — the FOCUSED monitor's workspace under a tear, with today's tear damping and the reachability guard carried over (never adopt a workspace no display can reach, or the plan emits unsatisfiable `SwitchSpaces` forever). |
| Z-order | anything | **Fight** — always ours, derived fresh each plan. |
| Window created/destroyed | app | Belief-side fact; corral policy (a Desired writer, category 3) decides placement for new windows against the GOAL context. |

Monitor-arrival satisfaction carries over as the `Place` predicate — and on a
clamp, **adopt** the clamped frame into `desired.frames` so the plan converges
instead of eternally diffing.

**Believed workspace under emulation** (circularity trap): today
`WindowSnap.workspace` is fed by the backend ledger — after the ledger is
subsumed, feeding it from `desired.window_ws` would make belief an echo of the
goal and toward/away vacuous for this attribute. Under emulation, believed
assignment must be DERIVED from physical observation (frame at sliver geometry
+ app hidden ⇒ parked), with `desired.window_ws` as the only assignment
authority. Under native, spaces are genuinely observable. State this per
backend in the snapshot contract.

## Logging and replay

The planner is pure, so planning replay stays exact:

- Commands are logged with the resulting `Desired`.
- `WorldObserved` as today.
- Each plan invocation logs `{generation, state_seq, actions}`; replay
  recomputes `plan()` and asserts the action list byte-for-byte.
- Worker EXECUTION (where it abandoned, which steps ran) is deliberately NOT
  reproducible from plan inputs — so per-step outcome/timing logs (the
  `restacks`/`raises` schema generalized to all actions) are load-bearing for
  debugging, not optional telemetry.
- `replay.rs` is a rewrite landing with the worker milestone, not a growth:
  its whole comparator is "update() reproduces the logged effect stream,"
  which stops being the contract for placement.

What is lost: the single-cascade "one event fully settles before the next"
property. The corral-anchor rule, the MRU-touch suppression, and the command
read context are the three places this doc re-derives guarantees that
property used to give for free. Any other logic discovered to lean on it
during migration must get the same treatment — write the rule down, test it.

## Backend fit

Emulated: `Ledger` + `saved` + `parked` are subsumed by `Desired` +
convergence bookkeeping; the backend demotes to mechanism — granular
idempotent ops (`place(window, visible, frame)` = park/restore + sliver
geometry + EUI bracket, `set_app_hidden`), no bookkeeping. The monolithic
`switch_workspace()` disappears from the emulated path; preemption requires
the granularity anyway.

Native: a space switch is one Mission Control lever press per display — not
granular, not preemptible mid-animation. It keeps the coarse `SwitchSpaces`
action, emitted only under `Capabilities { atomic_switch: true }`. The trait
keeps both shapes honestly rather than pretending one abstraction covers them.

## Migration milestones

Re-sliced after review: Desired must be maintained by the NON-command writers
(follow-the-focus, tear realign, close-fallout all switch workspaces without a
command) before anything reads it authoritatively, so attribution-lite comes
before the ledger lift. Riskiest semantic change lands while execution is
still synchronous; concurrency lands last, smallest.

- **R1 — Desired exists.** Command arms mutate `Desired` and still emit
  today's effects (R1's "planner" is a refactor of the hotkey arms into
  goal-mutation + derivation, using today's effect vocabulary and expectation
  registration — the real granular Action set can't exist until the backend
  is granular). Shell ledger asserts agreement in shadow mode; the assert must
  account for Rescued-mode no-ops and out-of-range targets. Tests: per-command
  desired-mutation, including the command-read-context traces above.
- **R2 — Attribution-lite.** The policy/adoption writers keep `Desired`
  truthful: follow-the-focus, tear realign, close-fallout, external
  focus/space changes, and window_ws adoption all write Desired alongside
  their current effects. Small, testable, and a prerequisite for anything
  reading Desired as authority. Expectations still fully in place.
- **R3 — Ledger lifts into core.** Emulated backend becomes dumb granular
  ops; `saved`/`parked`/`Ledger` deleted; `plan()` becomes the only author of
  park/restore decisions; park receipts land here. Believed-workspace
  derivation from physical parkedness lands here (the circularity trap).
- **R4 — Attribution rewrite.** Four-category classification; adoption table;
  expectations leave CORE state (`PendingOp`, `EXPECTATION_RESCANS`, op echo
  matching) and their predicates move to the (still-synchronous) executor's
  in-flight set; damping relocates to convergence bookkeeping; ActionResult
  replaces EffectResult. The update_test echo/expiry/damping suite is
  rewritten as adoption-table cases, written FIRST against the table above.
  This is the milestone with real regression risk — budget soak time.
- **R5 — The worker.** Plan execution moves to the converger thread with the
  generation contract, in-flight view sharing, ghost-absorption settle, and
  the replay rewrite. Acceptance test against run-34 numbers: no stale switch
  replay; bounce-back presence wait ~0; engine-thread stalls bounded by
  snapshot cost only (snapshots still walk every app with 0.2s AX timeouts —
  moving THEM off-thread is explicitly out of scope here and would be the
  next investigation).

Each milestone is independently shippable and lived-with before the next.

## Review outcomes (2026-08-27)

An adversarial review of the first draft produced 14 findings; the material
ones and their resolutions, now folded into the text above:

1. Pure toward/away attribution cannot classify our own transients →
   in-flight tracking kept, demoted to executor-internal (BLOCKER, fixed).
2. Commands computed targets from stale belief → command read context section
   (BLOCKER, fixed).
3. Damping/outcome feedback had no home → convergence bookkeeping category +
   ActionResult path (fixed).
4. `desired.frames` was a freshness regression vs capture-at-park → park
   receipts + Option-shaped frames (fixed).
5. Preemption makes ghost raises routine and post-convergence ghosts were
   permanent → worker settle contract with read-back + absorb (fixed).
6. Missing window_ws adoption row + believed-workspace circularity under
   emulation → table row + physical derivation (fixed).
7. Corral anchored on believed mid-convergence → policy writers anchor on
   goal context (fixed).
8. MRU-touch on rescan un-demotes mid-handoff → touch suppression rule
   (fixed).
9. R2-before-R3 dependency inversion → milestones re-sliced (fixed).
10–13. Tear adoption guard, replay single-seq + per-step logs, snapshot-cost
   honesty in the acceptance criterion, WarpMouse/SetIntercepting homes — all
   folded in.
14. Complexity honesty: the measured 80% is coverable by P1 + a restack
   worker under the CURRENT model → staging recommendation at the top.
15. (2026-08-27, from the run-38 follow-focus snap-back:) "in-flight, ours"
   dying with its generation cannot classify a grant that LANDED and was then
   superseded — kitty delivered a duplicate activation 520ms after its focus
   grant had already confirmed, and every landing signal was long gone. The
   in-flight set must retain satisfied/superseded focus entries for a
   landing-tail horizon (~2s, the FOCUS_SETTLE_NS bound in update.rs) before
   dropping them. This is the focus-domain twin of finding 5's ghost raises:
   fallout has no complete ledger, so quiescence is time-bounded, never
   counted out.
