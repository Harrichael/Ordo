# Overlapped raises — data-gated plan

Status: **collecting data, do not implement yet.** The `restacks`/`raises`
tables (added 2026-08-27) must accumulate a few weeks of real daily use
first; the whole point is to design the heuristic from measured
distributions, not guesses.

## The idea (Michael's)

The z-order reassert serializes raises today: each `AXRaise` is confirmed
landed (CG read-back) before the next is issued, because unconfirmed raises
land in arbitrary relative order. That costs the true AX latency of every
misordered window's app, serially — typically ~50–150ms per workspace
switch.

Instead: issue raises **optimistically overlapped**, pacing them from
per-app latency statistics so that they land in the desired order with
p ≥ 99.9%. Detect the rare misorder on the final read-back and fall back to
the serialized pass. Target: ~20ms for the common switch.

## Physics constraints (all probed on Tahoe 26, probes in crates/ordo/examples/)

- `AXRaise` goes to the target APP, not WindowServer. The app acks fast;
  the actual reorder happens when its main thread services the request
  (Chrome measured past 400ms — ack ≠ apply). The latency is the app's
  scheduling, compounded by macOS throttling occluded/background apps —
  exactly the apps a reassert raises. Unfixable from our side; only
  schedulable-around.
- **Same-app raises are FIFO** in the app's AX queue. They can ALWAYS be
  overlapped safely — no statistics needed. Only cross-app boundaries need
  the heuristic.
- No WindowServer-side bulk reorder exists SIP-on (`SLSOrderWindow` refused
  error 1000; SLPS bursts coalesce to the last call). AXRaise is the only
  per-window lever.
- A background app's raise lands directly BELOW the key window; a sibling
  of the key window lands ABOVE it, so every sibling raise is followed by
  re-raising the designated top (see `raise_pass` in
  crates/ordo/src/platform/zorder.rs).
- The reassert's contract: `desired[0]` is the designated top and the core
  focuses it before restacking. A key window inside the rest of the order
  makes every landing gate unsatisfiable (this was once a 13s engine
  freeze — see the round-trip regression test in ordo-core).

## What already exists

The detect-and-retry fallback is **already built**: the second/absorb pass
in `reassert_stack` sleeps 150ms, re-reads the stack, and re-raises only
displaced windows. An overlapped first pass slots into the same structure —
the change is only "issue optimistically, keep the read-back verdict."

## The data being collected

Every daemon reassert writes to `~/Library/Application Support/Ordo/log.db`
(probes print the same stats but don't write):

- `restacks` — one row per reassert: `total_ms`, `presence_wait_ms` (time
  waiting for un-hidden windows to resurface BEFORE any raise),
  `handoff_wait_ms` (focus handoff), `desired`, `missing`,
  `skipped_suffix`, `second_pass` (safety net actually ran), `converged`.
- `raises` — one row per issued raise: `window`, `pid`, `kind`
  (`background`/`sibling`/`top` — different physics, expect different
  distributions), `pass`, `above_scope`/`above_all` (burial depth at pass
  start), `wait_ms` (issue → landing confirmed), `timed_out`.

## Analysis to run (after a few weeks)

```sql
-- Where does the time actually go? If unhide dominates, overlapping
-- raises optimizes the wrong phase — pursue faster resurfacing instead.
SELECT r.total_ms,
       r.presence_wait_ms AS unhide,
       r.handoff_wait_ms  AS focus_handoff,
       (SELECT COALESCE(SUM(wait_ms),0)
          FROM raises x WHERE x.restack_id = r.restack_id) AS raising
FROM restacks r;

-- Per-app latency distributions (the heuristic's core input).
SELECT pid, kind, COUNT(*), AVG(wait_ms), MAX(wait_ms) FROM raises
GROUP BY pid, kind;

-- Does burial depth predict latency (deeper = longer throttled)?
SELECT above_all, AVG(wait_ms), COUNT(*) FROM raises
WHERE kind='background' GROUP BY above_all;

-- How often does the safety net fire today?
SELECT second_pass, converged, COUNT(*) FROM restacks GROUP BY 1, 2;
```

Compute percentiles (p50/p99/p99.9) per pid with a script — SQLite has no
built-in stddev/percentile. Group by pid within a run; across runs pids
change, so join through a window's owning app if longitudinal per-app
stats are needed (CG gives pid only; bundle id would need capturing at
log time — add it to `raises` if this bites).

## Decision criteria

Implement overlap only if the data shows:

1. Raise landings (not presence wait) dominate `total_ms`; and
2. per-app latency is predictable enough that a pacing rule (e.g. "wait
   app A's p99.9 before issuing to app B") beats serialization; and
3. the misorder fallback cost (150ms settle + re-pass) × predicted failure
   rate stays well under the saved time.

Sketch when green-lit: within one app, fire the whole run of consecutive
raises back-to-back (FIFO guarantee). At each cross-app boundary, wait
max(observed landing, per-app pacing quantile) instead of full landing
confirmation. Keep the final read-back + absorb pass untouched as the
correctness backstop. Persist per-app quantiles from the `raises` table
(or compute at daemon startup from recent runs).

## Related

- issues.txt "Smaller open items" has the one-paragraph version.
- SLSRequestNotificationsForWindows (event ids 804/1325/1326, alt-tab's
  WindowServerEvents.swift) could replace landing-gate polling with push
  acks — orthogonal, but would sharpen `wait_ms` measurements and reduce
  poll cost.
