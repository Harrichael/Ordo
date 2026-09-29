# Overlap-scoped stack ordering — design (not implemented)

## 0. The calls, most opinionated first

1. **Remember everything, enforce only overlaps.** The core keeps emitting a TOTAL priority order (MRU), unchanged in shape. Dropping the order between non-overlapping windows buys nothing (MRU is free, it's focus history) and it is exactly the thing that makes the *next* restack right after a window is dragged or laid out over another. The partial order is a plan-time view, not stored state.
2. **The shell, not the core, derives the partial order**, from the same `CGWindowListCopyWindowInfo` read that already gives the stack (`kCGWindowBounds` is in the same dict: no extra syscall). The plan must agree with the physical stack *and* the physical frames at one instant; core frames are beliefs up to a tick old and wrong mid-move/mid-unpark. Core stays deterministic; replay is unaffected because the effect payload does not change.
   **The MRU is the only stored structure.** Overlap groups and their order are rebuilt at every restack and never kept. A group's order is just the MRU filtered to the group, so storing it duplicates a fact that can drift. A stored grouping can't skip the CG read (the plan must see the live stack anyway), would need invalidation on every move, resize, birth, death, park, un-hide and display change, and would save about 2 µs per restack (§5).
3. **The minimum raise set is unique and found in closed form**: R = up-closure (in the overlap DAG) of every window that sits below something it should be above. No search. It is ≤ today's suffix-skip even for a total order.
4. **Overlap components are independent lanes.** Raises in different components may be in flight at once; sibling/top bookkeeping never crosses lanes unsafely (proof below).
5. **Overlap detection: brute-force all-pairs into a `[u64; 64]` adjacency bitset on the stack.** At N≤60 it is ~1–2 µs; the CF dict parsing around it costs more. Sweep/trees/grids/per-display buckets lose on constant factors and add a failure mode.
6. **Ignore the "Displays have separate Spaces" setting.** Geometry alone is correct in both modes; the setting could only prune ~1 µs of math, and the per-display shortcut is *wrong* when spanning is on. Don't read it. (This machine: `spans-displays = 0`, i.e. separate Spaces ON, despite ordo-emulated/lib.rs:30 recommending off.)

Counterfactual on run 29 (99 non-ghost restacks, frames from nearest prior snapshot): **421 raises issued → 182 planned (−57%)**. Restack 2228: **20 → 2** (both on the external display; zero on main). Restack 2223: 20 → 10.

## 1. Model

- `P` = priority order from the effect (index 0 = designated top). `S` = observed front-to-back stack (CG, layer 0, restricted to `P`).
- Overlap graph `O`: undirected, `i~j` iff frames intersect by > `MIN_OVERLAP` on both axes.
- Constraint DAG `E = { i→j : i~j, i<j in P }` ("i must be above j"). Acyclic because it is a subset of a total order. Not transitively closed on purpose: A~B, B~C, not A~C gives A→B, B→C only; A-vs-C order is free *unless* forced through B, which falls out of the planning, not the constraint set.
- Raise physics (from zorder.rs): background raise inserts just below the key window; a sibling (key app's window) inserts above the key window; key status never moves; raising the key window puts it on top and freezes siblings beneath it.
- Pairwise, not per-component total order: component-total order would enforce invisible A-vs-C and cost raises (the chain case above).

## 2. Building constraints

Per pass, from one CG read: `(wid, pid, bounds)` front-to-back. Index windows by P position (≤64 fast path). `adj[i]` = bitset of overlaps. Edges are implicit: `below(i) = adj[i] & !mask_upto(i)`, `above(i) = adj[i] & mask_below(i)`. Nothing allocated beyond two `[u64; 64]` + a `[Rect; 64]` SoA (x0,y0,x1,y1 as four `[f64;64]`).

Presence: the gate waits, as today, only for windows missing from the CG list (still resurfacing from an un-hide). A window that is listed but "sliver-shaped" (visible width or height on every display ≤ `MIN_OVERLAP`) is left out of the plan, not waited for. Waiting on slivers was the first design, but a window the core believes visible and the ledger has parked would then cost the full 600ms presence timeout on every switch, while an un-park still in flight at restack time is rare: the switch's moves are AX writes that return once the app has applied them, and they run before the restack is submitted.

## 3. Minimal-raise planning

Raising a set R in some order yields: R above all of U = complement (in raise order, last on top); U keeps its S order.
Valid iff (a) no edge inside U is violated in S, (b) R is up-closed: j∈R, i→j ⇒ i∈R (else j lands above i), (c) R is raised in an order consistent with E (descending P index works).

Violators `V = { i : ∃ i→j, j above i in S }` — computed as `below(i) & aboveInS(i) != 0`, where `aboveInS` is accumulated walking S top-down.

**Claim: `R* = upclose(V)` is the unique minimum valid set.** Necessity: if i∈V were kept in U, (b)'s contrapositive (i∈U, i→j ⇒ j∈U) keeps j in U, and edge i→j in U is violated. So V ⊆ R, and (b) forces upclose(V) ⊆ R. Sufficiency: U = complement of an up-closed set is down-closed; every violated edge has its top in V ⊆ R*, so (a) holds. Up-closure is one pass over indices descending (`if R∋j { R |= above(j) }`), since edges point strictly toward lower indices.

Total-order sanity check: want a,b,c,d, observed b,a,c,d. Today: suffix skip keeps c,d, raises b,a (+top). New: V={a}, raises a. The new rule never exceeds the suffix rule (the suffix rule's raised set is an up-closed superset of V).

Non-vacuous landing gates (the old "vacuous first window" bug): a minimal element of R (no R member below it in E) is in V, so it has a U-window below it; every non-minimal element has an R-predecessor that landed first. Define `landed(w)` = w observed above every *settled* window of its component (U ∪ already-landed R), excluding the designated top and the current key window. Always non-empty, always satisfiable by the physics.

**Siblings and the key window (per lane):**
- Raise order: R∖{top} by descending P index.
- A sibling raise lands above key → mark it *pending*. Pending siblings are only a problem if (i) a later background raise must be above one of them (it would land below them), or (ii) one overlaps the top.
- Before a background raise b: if `below(b) ∩ pending ≠ ∅`, raise top first (freezes all pending siblings below key), clear pending.
- At lane end: raise top if `pending ∩ adj[top] ≠ ∅` or top ∈ R.
- Consecutive siblings therefore share ONE top re-raise instead of one each. Restack 2223's main display: 7 siblings + 1 top instead of 7+7.
- A top raise is safe from any lane at any time: top is P-index 0, above everything is always consistent, and freezing another lane's pending sibling only makes it an ordinary below-key window. So lanes never conflict on the top.
- Sibling classification: by the pid of the designated top when `focus_top` (as today). When `!focus_top` (desktop key), the key app is Finder or nobody — classify against the frontmost app's pid from NSWorkspace (no AX race). Open question §12.

**Key-window handoff wait:** today waits whenever the key window is in `want`. New: wait only if some R member must land above the key window (`above(key) ∩ R ≠ ∅` in E-terms: an R window that overlaps it and outranks it). Otherwise the stale key obstructs nothing.

**Lanes:** components of O restricted to present windows (bitset BFS, O(n) word ops). Windows in different components never overlap, so relative landing order across lanes is invisible. Single worker thread runs a tiny scheduler: each lane has ≤1 raise in flight; issue the next raise of every idle lane, then one gate wait on any hint, one CG read, check every in-flight `landed`. A 900ms Chrome raise on display B no longer delays display A.
Risk: `AXUIElementPerformAction` is itself synchronous up to the app's ack; a hung app's ack blocks all lanes on one thread. v1 accepts this and measures `ax_ms` separately (§9); per-app issuer threads only if data shows ack time, not landing time, is the stall.

**Passes:** keep the two-pass structure, but pass 2 REPLANS from a fresh read (new S *and* new frames). That absorbs ghosts, late arrivals, and frames that moved mid-pass. Converged = no violated edge (and top key if `focus_top`) — never `observed == scope`, which is no longer the goal.

## 4. Where it lives; effect shape

- `ordo-core`: unchanged logic. `Effect::RestackWindows { order, focus_top }` keeps its fields (serde name `order` kept so old logs load); only the doc changes: "priority, front-most first; enforced between windows that overlap on screen."
- Shape check against plausible needs:
  - Pinned/always-on-top window, float layer: core puts it early in priority. No restructure.
  - "Core doesn't care about the order of some windows" (never-focused tail, apps with arbitrary MRU): an additive `ranked: Option<usize>` (tail unordered among itself). Additive.
  - Constraints that must hold without overlap: none plausible. Non-overlap order is invisible by definition.
  - Core-computed edges: rejected. They'd bake stale belief frames into a logged effect, and couldn't be optimized without changing the interface (the shell would have to re-derive them anyway).
- Shell, `crates/ordo/src/platform/`:
  - `zorder.rs`: one `stack_snapshot() -> SmallVec<[(WindowId, i32, Rect); 32]>` read replaces `stack_front_to_back` + `stack_with_pids` inside the reassert (one CG call per gate instead of two at pass start).
  - New pure `stack_plan.rs` (no FFI): `overlaps`, `plan(priority, snapshot, key_pid) -> Plan { lanes: Vec<Lane{ steps: Vec<(WindowId, RaiseKind)> }>, violators, untouched, edges }`. Telos: "what must move, in what order, given raise physics". It knows the physics (siblings/key) because that *is* its purpose.
  - `reassert_stack` becomes the executor: presence/handoff gates, lane scheduler, landing checks, passes. It talks to the world through a narrow port (§10).
  - `ax::raise_sequenced` → split into `collect_elements(pids)` (only pids owning R windows, not every regular app) and `raise(el)`; the lane scheduler owns sequencing. This is the switch-latency win on the fixed-cost side: today's walk asks every regular app for its `AXWindows`, one AX round trip per app, on every raise pass. The log shows where a switch's floor goes: restacks that issued zero raises take 7 ms p50 and 62 ms p95 (`restacks`, 1826 of 2383 non-ghost non-aborted rows), all of it AX focus reads and CG reads, none of it math.
  - `restack_worker.rs`: ghost watch filters to windows that were actually raised (not all of `order`).

## 5. Overlap detection

Test: `ix = min(ax1,bx1) - max(ax0,bx0) > MIN_OVERLAP && iy = ... > MIN_OVERLAP`, strict.
`MIN_OVERLAP = 2.0` pt. Reasons: the park sliver shows exactly `SLIVER = 1.0` pt (workspaces.rs `park_frame`) and a window flush to the left edge overlaps it by exactly 1pt; tiled neighbors touch or overlap by AX rounding (≤1pt, cf. `same_position`'s 1.0). Touching edges never count. (Parked windows aren't in `order` anyway since `visible_stack` filters by projection; the tolerance is belt-and-braces plus the sliver rule in §2.)

Options at N = 2–60:
- **All-pairs, SoA, branchless — chosen.** n(n−1)/2 ≤ 1770 tests; with x0/x1/y0/y1 in four `[f64; 64]` arrays the inner loop over j is 4 min/max + 2 compares + and → sets one bit; auto-vectorizes (NEON 2×f64). All data ≈ 2KB, L1-resident. ~1–2 µs, zero heap. Output directly into `adj: [u64; 64]` (symmetric fill: set `adj[i] |= 1<<j; adj[j] |= 1<<i`, or row-only for j>i and derive).
- Sort-and-sweep on x: sort is ~n log n branchy compares plus index shuffles, then the same tests for the x-overlapping pairs. Output-sensitive, but real layouts are dense (8 Preview windows at ~(4,34,1713,1037) all overlap each other; maximized windows overlap everything on their display), so it saves little and costs the sort.
- Interval tree / R-tree: allocation and pointer chasing for a problem that fits in L1. No.
- Uniform grid / spatial hash: needs cell sizing, duplicates big windows across many cells, allocates. Wins only at thousands.
- Per-display bucketing first: needs a display assignment (center-of-window), which is exactly wrong for straddlers when spanning is on; saves at most half of ~1µs.
- Context, measured (bench.c, clang -O2, live desktop with 15 layer-0 windows, 2000 iterations): one CG list read with the wid/layer/pid extraction the shell does today is 0.26 ms median (p95 0.32 ms); adding the bounds parse for every window plus the overlap bitset and components moves the median by about 2 µs, inside the read's own noise. The math alone is 0.3 µs at n=15 and 3 µs at a dense n=64. One raise is 10–900 ms. The math is 3–5 orders below anything that matters; pick the simplest correct kernel.
- N > 64: `Vec<u64>` rows of `ceil(n/64)` words (rare; keeps the same code shape via a `Bits` row type), or `SmallVec<[u64; 1]>` rows.

## 6. Spaces setting

- Key: `defaults read com.apple.spaces spans-displays` (bool). `false`/0 = "Displays have separate Spaces" ON (default); `true` = one Space spans displays. Takes effect only after logout, so it cannot change under a running daemon's session (a read at startup would be enough, if we ever needed it). Ordo does not read it today (only a doc mention, ordo-emulated/src/lib.rs:30).
- Spanning ON (separate Spaces off): a window can straddle; its CG bounds cover both displays; geometric overlap links windows on both displays into one lane. Correct by construction.
- Separate Spaces ON: a straddling window is drawn only on its main display; geometry may add a phantom edge on the other display. Cost: possibly an extra raise. Never a missed constraint.
- **Verdict: geometry alone is correct in both modes; the setting is only an optimization, and not a worthwhile one. Don't read it.**

## 7. Edge cases

- **Top overlaps nothing**: plan is focus-only (today's `focus_top` path).
- **Chain A~B~C, A≁C**: handled by E + up-closure (e.g. S = C,A,B → V={B}, R={B,A}, raise B then A).
- **Windows that start overlapping later** (user drag, an app re-applying its saved frame, a layout effect): a core rule, not an accident of the focus flow. The core emits `RestackWindows` when a visible window's frame change has settled (the frame is unchanged across a tick), not per drag tick, so a drag doesn't feed the worker a storm. The shell's plan is empty when nothing is violated, so the usual cost is one CG read plus the AX focus checks. A user drag also clicks the window, which mints a new MRU head, so the common case is right before the check runs; the rule is for the moves nobody clicked.
- **Mid-move frames**: plan uses whatever CG says; pass 2 replans with fresh frames. Residual hole: a move landing after pass 2 into a new overlap. Accept; log it (§9 `late_overlap`).
- **Mid-un-hide**: presence gate as today. **Mid-unpark**: left out of the plan while sliver-shaped (§2); if it lands after the plan, pass 2 or the next restack orders it.
- **Key window ≠ top** (handoff not landed): scoped handoff wait (§3), then exemption as today.
- **`!focus_top`** (desktop key on an empty display): background raises land below the desktop's key app window set; sibling classification per §3.
- **Shadows**: CG bounds exclude shadows; order between touching windows decides whose shadow shows. Ignored.
- **Windows outside `order`** (other layers, unmanaged apps, panels): out of scope, as today.
- **Non-overlapping sibling order drifts**, which changes Cmd-` cycling order within an app. See §12.

## 8. Failure modes

- Stale frame → missed edge → visibly wrong pair until next restack. Mitigations: plan at the latest possible read; pass-2 replan; telemetry flags it.
- Phantom edge (separate-Spaces straddler, 2pt threshold) → extra raise. Harmless.
- Sibling misclassified (key app changed mid-pass) → a "background" raise lands above key; a later raise that needs to be above it lands below. Pass 2 sees the violation and repairs.
- Landing timeout in one lane → only that lane stalls; pass-2 settle sleep only if any lane timed out.
- Ghost from a cancelled generation → ghost watch (restricted to raised windows) triggers a replan, which is minimal by construction, so a no-op ghost costs one CG read.
- Hung app blocking `AXPerformAction` ack → blocks all lanes on the single thread (v1 limitation, measured).

## 9. Telemetry (proves it works)

`restacks` new columns:
- `edges`, `components`, `violators`, `raise_set` (|R|), `untouched` (= present − |R|; replaces `skipped_suffix` semantically; keep the old column written as the would-be suffix for continuity).
- `legacy_raises`: what today's suffix-skip plan would have issued, computed by a pure fn on the same snapshot. A paired counterfactual on every restack, in production, for free.
- `violated_end`: edges still violated after the final read-back (0 ⇔ converged).
- `late_overlap`: edges present in a fresh frame read at the end that weren't in the plan (the mid-move hole, §7).
- `frames`: compact `wid:x,y,w,h` of the planned windows, so any restack can be re-planned offline (this doc's counterfactual needed a fuzzy join against snapshots).
`raises` new columns: `lane`, `ax_ms` (duration of the perform-action call itself, split from `wait_ms` landing time), `batched` (a top raise that froze >1 sibling).
Success criteria: `raise_set ≤ legacy` always; zero raises on lanes with `violators = 0`; `violated_end = 0` rate ≥ today's `converged` rate; wall `total_ms` p50/p95 down; per-lane max wait no longer summed.

## 10. Tests (Chicago, fakes)

- The planner is pure: story tests feed a snapshot + priority and assert the final state after applying the plan to a fake stack, not the plan's steps. E.g.:
  - "Two displays; main already in order, external scrambled: after the restack every overlapping pair on both displays is in priority order, and no main-display window was raised." (restack 2228 as a fixture, frames from the log.)
  - "A run of key-app siblings under the top ends ordered with the top still on top" — asserts final order, not the count of top raises (count is a telemetry concern; one number test is allowed for the 2223 fixture: raises ≤ 10).
  - "A straddling window links both displays' windows."
  - "A parked 1pt sliver never constrains a window flush to the edge."
- `FakeWindowServer`: implements the physics — key window, background inserts below key, sibling above key, per-app latency queues (seeded), in-flight raises landing at scheduled fake times, ghosts. The executor runs against it through a narrow port: `snapshot() -> [(wid,pid,rect)]`, `raise(wid)`, `wait_hint(deadline)`, `now()`. Small, stateless-at-the-interface, stable: the right place for a fake.
  - "A 900ms app on display B does not delay display A's convergence" (fake clock: lane A converged at < 50ms).
  - "A ghost from a cancelled generation is repaired by the next pass."
- Property test (seeded random layouts, n≤8): executing the plan in the fake always ends with no violated edge, and |R| equals the brute-force minimum over all 2^n subsets checked with (a)–(c). This is the proof sketch turned executable.
- Core tests: unchanged; `RestackWindows` shape unchanged.

## 11. Impact on logged cases (run 29)

Frames from world_observed seq 4109; 2 displays side by side (0..1920, 1920..3840).
Components: main = {75225, 75230, 75222, 75186, 75169, 75164, 75161, 75158 (Preview), 75109 (Chrome), 71041 (kitty)}; external = {74777 Finder, 74743 kitty, 73053, 72004 Chrome}.
- **2228** (start: main already in exact priority order): V = {74743, 74777}, R = same. Plan: external lane raises 74743, then 74777. **2 raises vs 20**; main display untouched. Expected wall ≈ 30ms (logged waits 20+7ms) vs 850ms.
- **2223**: V = seven Preview windows + 74743 + 74777; R adds 75225 (up-closure). Plan: main lane 7 siblings (75158…75222, 75230) + one top; external lane 74743, 74777 in parallel. **10 raises vs 20**; ~7 top re-raises removed by batching; external lane concurrent. Est. main lane ≈ 7×~33 + 35 ≈ 270ms vs 925ms. The seven sibling raises are genuinely needed: 75109/71041 sat above the Previews and can only be passed by raising every Preview.
- Run-wide: 99 restacks, 421 → 182 raises. Largest wins are the 14-window workspace (20 → 2 or 10) and small ones (2194: 10 → 3; 2221: 8 → 1).
- The replay scripts were ad hoc and aren't saved (read-only over the log; nearest-snapshot frames, so approximate). The `frames` telemetry column (§9) is what makes an exact replay possible without them.

## 12. Open questions

1. **Burst same-app raises?** zorder.rs says same-app raises are ordered by the app's AX queue. If a run of siblings can be issued back-to-back and gated only on the last, 2223's 7 sibling gates (~230ms) collapse to ~1. Needs a probe (does the window server apply them in queue order?). Additive to the lane scheduler.
2. **Presence gate is still global**: an un-hiding app on display B holds display A up to 600ms. Plan-without-missing plus replan-on-815 would fix it, but a missing window has no frame, so its lane is unknown until it arrives. Worth doing only if telemetry shows presence waits on the visible display.
3. **Cmd-` / app window-list order** drifts for non-overlapping siblings. Does anything in Ordo or the user's habits rely on in-app z-order? If so, siblings could be ordered by a cheap app-local pass (still only if the app is not the only thing on that display).
4. **`ranked` tail**: never-focused windows have arbitrary MRU positions; enforcing them costs raises for no user intent. Additive field if telemetry shows it.
5. **Sibling classification when `!focus_top`**: frontmost-app pid vs AX focused app. Probe needed.
6. **MIN_OVERLAP = 2pt**: confirm against logged tiled layouts (terminals snap to cell size and underfill, so real tiled overlap should be ≤1pt); `frames` telemetry answers it.
7. **Hung-app ack**: if `ax_ms` (not `wait_ms`) dominates the long tails, move issuance to per-app threads.

Sources: [macos-defaults.com: spans-displays](https://macos-defaults.com/mission-control/spans-displays.html), [nix-darwin option](https://mynixos.com/nix-darwin/option/system.defaults.spaces.spans-displays).
