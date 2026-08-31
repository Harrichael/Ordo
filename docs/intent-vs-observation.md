# Intent vs. observation: the data-ownership audit

Status: **being implemented in reviewed steps** (2026-08-31; originally an
exploration plan). Michael approved the program after the Opus design review
(docs/intent-vs-observation-review.md): hotfixes (park sliver guard,
S-after-R merge) → ordo-emulated crate extraction → snapshot seam split
(observation vs workspace-word channels, believed frames) → CG-probe death
evidence + (id,pid) identity → enforcement-as-assertion → declared-focus
command context. The reconciler (docs/desired-state-reconciler.md) stays
PAUSED; re-evaluate it against the finished ownership model.

## The principle (Michael's formulation)

- **Workspace navigation is intent — declarative.** "I am on workspace 2" and
  "this is a workspace-3 window" are declarations. Only the user (or an
  explicitly named policy acting for the user) may write them.
- **Window placement within a workspace is observational.** Where a visible
  window sits on its own workspace is whatever the world says it is; belief
  follows reality there, freely.
- **An observation that contradicts a declaration is NOT a valid observation
  of that declared fact.** A ws3 window visibly standing on ws2 does not mean
  "it's a ws2 window now" and does not mean "windows just live there" — it is
  a *violation* to be either corrected (assert the declaration) or explicitly
  adopted by a named policy. Never silently absorbed.

Everything that went wrong this week is a category violation — observation
writing into intent-owned data:

| Incident (dated, log refs) | Violation |
|---|---|
| Weekend flatten (run 39, wake 08-31 09:06): empty sleep scans erased every assignment; all 12 windows re-adopted onto ws3 | scan **absence** wrote assignments (fixed for the fully-empty case: "an empty observation is not an observation of emptiness") |
| Silent reassignment (run 39 seqs 25659/25661, 09:52:16→:19): Chrome 35858 missed ONE scan (AX timeout), was dropped, re-adopted 3s later onto current ws2; the 10:26 state seed then persisted the corruption; every ws2 visit now stands it up "deterministically" | **partial**-scan absence wrote an assignment — the documented deferred gap ("N consecutive absences"), and the source of Michael's reproducible phantom |
| Enforcement war (run 41 ~11:10, ping-pong at 1s cadence; nohup "re-parking phantom" attempts 2,3): stale queued restores vs the 3-attempt band-aid; last stale write wins after damping gives up | damping treats **our own stale writes** as "an app fighting back" and surrenders a declared fact (parked-ness) to them |
| Focus snap-back (run 38 seq 1410, fixed via FOCUS_SETTLE_NS) | a self-caused focus echo was classified as user navigation — observation misattributed as intent |
| Carry mis-target (run 38 seq 20447: carried 35858 instead of visible 38113) | a **command** read observation-lagged focus as if it were the user's intent |
| MRU cross-monitor leaks | `monitor` is derived from the observed frame, which is garbage while parked (sliver corner flips displays) — a derivation anchored on the wrong category |
| F6b strandings (fixed by state.json) | declared facts (assignments, restore frames) lived only in RAM — intent wasn't durable |

## The audit: every datum, its writer today, its rightful owner

Categories: **D** declared (only commands/named policies write), **O** observed
(only observation writes), **V** derived (recomputed, never stored authority),
**B** bookkeeping (ours, internal).

### Core `State` (ordo-core/src/state.rs)

| Datum | Today | Rightful | Notes / violations |
|---|---|---|---|
| `mode` (Active/Rescued) | user chords | **D** | sound |
| `workspace_count` | config/backend | **D** | sound |
| `monitor_ws` (active ws per monitor) | snapshot (fed by ledger under emulation) | **D** under emulation; **O** under native (user can swipe externally → named adoption) | per-backend ownership must be explicit in the snapshot contract |
| `windows[w].workspace` | snapshot (ledger's word) | **D** — mirror of the declaration | sound iff the ledger is sound; the ledger is where the violations live |
| `windows[w].frame` | snapshot | **O** for visible-on-current; **V** while parked (a consequence of the declaration) | phantom = the V case contradicting D |
| `windows[w].monitor` | derived from observed frame | **V**, but should derive from the SAVED frame while parked | concrete fix for the MRU monitor leaks |
| `windows[w].title/bundle_id` | snapshot | **O** | metadata; sound. NOTE: Chrome titles drift per tab — identity is the CG id, never the title |
| window existence (in `windows`) | present-in-scan | **O**, but absence needs EVIDENCE | one missed AX answer ≠ closed. Candidates: N consecutive absences, and/or a CG-window-list existence check (CG answers even when an app's AX is slow) |
| `focused` | snapshot | **O** with echo attribution (settle cap shipped) | sound now; same horizon idea may be needed elsewhere |
| `focus_history` (MRU) | observed focus | **O/V** | collateral damage when existence flickers (dropped window loses its MRU slot) — fixing existence-evidence fixes this too |
| `pending`, corrections, `tear_corrections`, `last_switch_mono_ns`, `next_op` | core | **B** | sound |

### Emulated backend (crates/ordo/src/{ledger,platform/emulated_backend}.rs)

| Datum | Today | Rightful | Notes / violations |
|---|---|---|---|
| `ledger.current` | commands (+R blanking) | **D** | sound |
| `ledger.assign` (window→ws) | commands + `note_seen` (adopt-on-first-sight) + `forget_missing` (drop-on-absence) | **D** | THE core defect: `forget_missing`+`note_seen` together let observation rewrite declarations. Adoption of a genuinely NEW window is a legitimate named policy; re-adoption of a momentarily-unscanned window is not — and today the two are indistinguishable |
| `saved` (restore frames) | captured at park | **D** (a promise; capture-at-park is the freshness rule) | sound; pruning must follow assignment ownership, not scan presence |
| `parked` set | park/restore ops | **V** of (assign, current) in principle; stored today | drift between it and the ledger armed the band-aid against the user (fixed in lockstep-prune, but "derived, recomputed" would make the drift unrepresentable) |
| `enforce_attempts` | band-aid | **B** | damping bug: budget is per-window-until-clean; stale SELF-writes drain it. Fix direction: per-episode budgets (reset on each fresh perturbation), and/or don't count fights against writes we ourselves issued recently (self-write horizon — the frame twin of FOCUS_SETTLE_NS) |
| `suspended`, `state_path`, `boot_time` | R/S/O model | **B** | sound |
| `state.json` | write-through of ledger promises | durable **D** | validates the schema choice: the file contains exactly the declared set (assignments, restore frames, current ws) and nothing observed. Keep it that way |

### Shell / worker

| Datum | Today | Rightful | Notes |
|---|---|---|---|
| `intercepting` | chords | **D** | sound |
| restack generation, ghost watch, ws_events ring | worker | **B** | sound |
| z-order | derived from MRU each restack | **V** | sound (never stored) |

## The redesign to explore (candidate tweaks, in dependency order)

1. **Assignment ownership** (kills the deterministic phantom-maker):
   - `forget_missing` requires evidence of death: N consecutive absences
     (start N=3) and/or a CG-list existence probe before forgetting. Keep the
     assignment (and `saved`) for an absent-but-not-dead window.
   - `note_seen` adopts ONLY genuinely new ids — an id that was recently
     declared keeps its declaration on reappearance. Mechanically: absence
     counters on ledger entries instead of instant removal.
   - Consequence: the weekend/partial-scan class becomes unrepresentable, not
     guarded against.
2. **Violation handling for parked frames** (kills the sitting phantom):
   - Reframe `enforce_placement` as *asserting a declaration*, with damping
     that only counts genuine external opposition: per-episode budgets (reset
     per fresh perturbation), possibly plus a self-write horizon so our own
     stale restores never drain the budget.
3. **Derivations anchored on the right category**: window `monitor` derives
   from the saved (declared) frame while parked → fixes MRU scope leaks.
4. **Commands read declared context** (carry mis-target) — DONE 2026-08-31:
   `State::declared_focus()` (most recent pending focus grant, falling back
   to observation) feeds the carry, the MRU anchor, demote, and
   move-to-monitor; a landed grant retires older pending grants so the
   declaration can never point backward. Carries also became assignment-only
   (`AssignWindowToWorkspace`), ending the park/restore double-write race.
5. Only after 1–4 are settled: **re-evaluate the reconciler** against this
   table. Its Desired/Believed split ≈ D/O here; its fight-or-adopt table
   should be regenerated from this audit rather than trusted as written.

## Open questions for Michael (decide before implementing anything)

- Death evidence: N consecutive absences (how many? scan cadence varies) vs a
  CG existence probe (authoritative but a new platform call) vs both?
- Should an absent-but-declared window survive a daemon restart too (persist
  absence counters?), or is state.json's current behavior (persist all
  declared entries; prune on first sighted scan) already right once pruning
  requires death evidence?
- The adoption policy for genuinely new windows: current workspace (today's
  rule) — confirm, and confirm what counts as "genuinely new" (never-seen id?
  id + creation hint?).
- Fight-or-adopt per datum: e.g. user deliberately drags a sliver out — adopt
  (reassign to current ws) or fight (re-park)? Today: fight with damping.
- Native backend: which of these ownerships flip (assignment is observable
  there; `monitor_ws` is OS-co-owned)?

## Context for resumption (post-compaction)

- **Daemon is STALE**: running run 41 (started 08-31 10:38) = pre-revert
  binary — still has the old double-press Ctrl+Alt+Cmd+R (destructive reset).
  Michael's later refresh silently didn't happen (no run 42). The built
  binary on disk has: R/S/O bring-up model (R=fresh single press, S=resume+
  save, O=reload file) and the conditional settle sleep. Do not restart
  without Michael's go; warn him R is dangerous until refresh.
- **Uncommitted**: conditional settle sleep (zorder.rs), issues.txt overlap
  verdict, this doc. Committed+pushed: 8b1bac9 (persistence, amnesia fixes,
  R/S/O), 1a6ba30 (event hints, settle cap, phantom band-aid).
- **Dangling**: nohup.out is git-tracked by accident (commit 8b1bac9) and
  keeps growing — Michael hasn't answered whether to gitignore+untrack it.
- Useful queries live in the incident rows above; the log DB is
  ~/Library/Application Support/Ordo/log.db (WAL, live), state file
  state.json alongside it.
- Standing agreements: implement exactly Michael's spec, ask about gaps
  before coding (see memory feedback-implement-spec-exactly); no git without
  explicit permission; no daemon restarts without a go; reconciler PAUSED.
