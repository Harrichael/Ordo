# Intent vs. observation: the data-ownership audit

Status: **implemented; principle locked** (2026-09-01; originally an
exploration plan, then implemented in reviewed steps). Michael approved the
program after the Opus design review (docs/intent-vs-observation-review.md):
hotfixes (park sliver guard, S-after-R merge) → ordo-emulated crate
extraction → snapshot seam split (observation vs workspace-word channels,
believed frames) → CG-probe death evidence + (id,pid) identity →
enforcement-as-assertion → declared-focus command context. One piece of the
review was later REVERSED: adopt-at-the-limit (see the principle below).
The reconciler (docs/desired-state-reconciler.md) stays PAUSED; re-evaluate
it against the finished ownership model.

2026-09-02: focus joined the declared set. This document had filed it as
observation-only; the section "Focus: two fields" below is the design record
for the correction and supersedes the `focused` row and candidate 4.

## The principle (Michael's formulation, final form)

- **Workspace navigation is intent — declarative.** "I am on workspace 2" and
  "this is a workspace-3 window" are declarations. The ONLY writers are
  Ordo's own commands: an explicit user command (switch, carry/move), rescue,
  and a window's BIRTH (a brand-new window has no prior intent, so it files
  onto the current workspace — the one case with nothing to preserve).
  Nothing else: not an app raising, moving, or un-hiding a window, not a
  notification, not an observation of any kind.
- **Window placement within a workspace is observational.** Where a visible
  window sits on its own workspace is whatever the world says it is; belief
  follows reality there, freely. Ordo remembers a parked window's frame only
  as a promise to restore it.
- **Which window should be key is declared; which window IS key is observed.**
  Two fields, side by side, exactly as `windows[w].workspace` sits beside
  `windows[w].frame`. Every switch, carry, MRU chord, demote and birth DECIDES
  the key window; that decision is `State::focus_intent`. What AX reports is
  `State::focused`. See "Focus: two fields".
- **An observation that contradicts a declaration is NOT a valid observation
  of that declared fact.** A ws3 window visibly standing on ws2 does not mean
  "it's a ws2 window now" and does not mean "windows just live there" — it is
  a *violation* to be corrected on screen (assert the declaration) or
  surfaced to the user. Never absorbed into the declaration. Equivalently:
  a declaration must never travel through the observation channel.
- **Enforcement therefore never adopts.** This reverses the review's
  adopt-at-the-limit recommendation (2026-09-01): at its write limit,
  enforcement stands down loudly (stderr + a Standoff trace) and leaves the
  window visibly misplaced until the next user command. Losing where the
  user filed a window is silent and permanent; a misplaced window is obvious
  and self-heals on the next switch.

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
| Unlanded grants (run 51: 36 of 255 switch grants never landed; op 172's target lost to a Chrome sibling for 166 snapshots, every op reporting Ok) | focus had no declaration to enforce against — a grant was a one-shot expectation, never retried |
| Carry restacks omitted the carried window (run 51, all 7); seq 22484 carry produced zero effects | the restack head became the key window (AppKit keys a raised sibling), and the carry guard read that flung OBSERVATION as the user's intent |
| Empty-workspace fling (run 51 seq 12750-12769: Cmd+N, birth, fling to a hidden kitty window 32ms later, followed) | with nothing visible able to hold focus, every OS-handed focus read as navigation — inference from observation, guarded by timing (`close_fallout`, `FOCUS_SETTLE_NS`), each new cause costing a new guard |
| MRU reordered by flings (`touch` on raw observed focus) | an app's focus fling wrote the user's Alt+Tab order |

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
| `focused` | snapshot | **O** — pure; mirrored every snapshot, never damped or corrected | its declared twin is `focus_intent` (2026-09-02; the old "echo attribution / settle cap" guards are gone) |
| `focus_intent` | commands only (`State::declare_focus`) | **D** | `Window(w)` is enforced like a parked frame; `Deferred` = the OS owns the slot. See "Focus: two fields" |
| `focus_history` (MRU) | `declare_focus`, plus observed focus only while `Deferred` and only for visible-workspace windows | **D**-driven | a fling can no longer reorder Alt+Tab; still collateral damage when existence flickers |
| `navigation_gesture` | the tap's witnessed gestures | **B** | consumed by the next observation — "since the last observation", not a time window |
| `pending`, corrections, `tear_corrections`, `focus_corrections`, `next_op` | core | **B** | sound |

### Emulated backend (crates/ordo/src/{ledger,platform/emulated_backend}.rs)

| Datum | Today | Rightful | Notes / violations |
|---|---|---|---|
| `ledger.current` | commands (+R blanking) | **D** | sound |
| `ledger.assign` (window→ws) | commands + `note_seen` (adopt-on-first-sight) + `forget_missing` (drop-on-absence) | **D** | THE core defect: `forget_missing`+`note_seen` together let observation rewrite declarations. Adoption of a genuinely NEW window is a legitimate named policy; re-adoption of a momentarily-unscanned window is not — and today the two are indistinguishable |
| `saved` (restore frames) | captured at park | **D** (a promise; capture-at-park is the freshness rule) | sound; pruning must follow assignment ownership, not scan presence |
| `parked` set | park/restore ops | **V** of (assign, current) in principle; stored today | drift between it and the ledger armed the band-aid against the user (fixed in lockstep-prune, but "derived, recomputed" would make the drift unrepresentable) |
| `enforce_attempts` | band-aid | **B** | damping bug: budget is per-window-until-clean; stale SELF-writes drain it. Fix direction: per-episode budgets (reset on each fresh perturbation), and/or don't count fights against writes we ourselves issued recently (a self-write horizon) |
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
4. **Commands read declared context** (carry mis-target) — DONE 2026-08-31,
   REBUILT 2026-09-02: `State::declared_focus()` first read the most recent
   pending focus grant (a peek at `pending`, self-expiring with it); it now
   reads the `focus_intent` field, falling back to observation only under
   `Deferred`. Carries also became assignment-only
   (`AssignWindowToWorkspace`), ending the park/restore double-write race.
5. Only after 1–4 are settled: **re-evaluate the reconciler** against this
   table. Its Desired/Believed split ≈ D/O here; its fight-or-adopt table
   should be regenerated from this audit rather than trusted as written.

## Focus: two fields (2026-09-02)

The `focused` row above originally read "**O** with echo attribution (settle
cap shipped) — sound now". It was not sound. Under emulation "which window
should be key" is DECIDED by every switch, carry, MRU chord, demote and
birth — a declaration — but it had no field, no persistence and no correction
loop: a `FocusWindow` grant was a one-shot `Expectation::Focused` that expired
after three rescans and, unlike placement, was never retried. Because there
was nothing to enforce against, follow-the-focus INFERRED intent from focus
observations, and each discovered non-navigation cause of a focus change
(our own echo, a close, a late grant landing, an app re-keying a sibling)
cost a subtractive guard: the pending-expectation guard, `explains` echo
attribution, same-snapshot `close_fallout`, the 2s `FOCUS_SETTLE_NS` window.
The verified empty-workspace incident (run 51 seq 12750-12769) slipped past
all four. That accumulation was the architectural failure.

### The two fields

```
focused:      Option<WindowId>   // OBSERVATION. Mirrors the world. Never damped, never corrected.
focus_intent: FocusIntent        // DECLARATION. Written only by commands, through State::declare_focus.

enum FocusIntent { Window(WindowId), Deferred }
```

`Deferred` is deliberately not `Option<WindowId>`: `None` invites a reader to
treat it as "not set yet" and supply a default. `Deferred` is a positive
assertion — the OS owns the slot, there is nothing to enforce — and it STANDS
until the next command. It is a state, not a pending item.

**Why `Deferred` and not "copy the OS's choice"**: an earlier draft handled a
gesture whose target Ordo cannot name (a click, Cmd+Tab) by watching the next
focus observation and copying it into the declaration. That is a declaration
travelling through the observation channel — the exact edge this document
forbids — and, being a copy, it needs a settle window (which of several focus
changes in a batch is "the" one?), reintroducing the race. `Deferred` copies
nothing; enforcement reads it as "nothing to enforce".

### The writer set (closed; every writer supplies a constant or an id it already holds)

| Command | Writes |
|---|---|
| switch workspace | `Window(destination MRU head)`; `Deferred` if the destination is empty (nothing there can hold focus) |
| carry window to workspace | `Window(the carried window)` — and it heads the restack |
| MRU chord / demote | `Window(target)` |
| move to other monitor | `Window(the moved window)` |
| a window's birth that took focus | `Window(that window)` — NOT gated on the `AXWindowCreated` hint (unlike corralling): Slack and System Settings announced none of their births in run 51 (8 seen only by the periodic scan vs 2 announced), and a standing declaration would have yanked focus back from every new window |
| follow (a witnessed gesture landed on a hidden workspace) | `Window(that window)` |
| the invariant (focus on a hidden window, no gesture, OS owned the slot) | `Window(visible MRU head)` |
| user gesture: any mouse-down, Cmd+Tab, Cmd+` | `Deferred` |
| rescue; daemon start; `--paused`; `--observe` | `Deferred` (the initial value — no special case) |
| enforcement standoff | `Deferred` — see below |

`declare_focus` is the single write path (the field is private). It also
records `Window(w)` in the MRU history, resets the damping episode, and spends
any gesture still waiting for its observation. Structurally: `handle_hotkey`
splits into `resolve(HotkeyAction) -> Option<Command>` and
`execute(Command) -> FocusIntent`; both matches are exhaustive and `execute`
returns a bare `FocusIntent`, so a new chord without a focus decision is
E0004 at compile time, not a stale declaration at runtime.

### Two concerns that were conflated

1. **`focus_intent` governs WHICH visible window should be key** — sometimes
   Ordo's call, sometimes the OS's.
2. **"The key window must be on the VISIBLE workspace" is an INVARIANT**, not
   a declaration. It holds whoever owns the slot and needs no intent to
   evaluate. Observed focus on a window whose workspace is not current, with
   no gesture explaining it, is unusable state (the user would type into an
   invisible window): declare the visible MRU head and pull focus there. This
   is the honest home of what `close_fallout` was groping toward, and it is
   what closes the hole `Deferred` would otherwise open.

### Gestures — three grades; the default is what makes this scale

The event tap masked `KeyDown` only and forwarded only recognized chords, so a
click or a Cmd+Tab left no trace but the focus change it caused — which is why
focus LOOKED observational. The tap now also witnesses every mouse-down (with
its point) and Cmd+Tab (reported on Cmd release, when the switcher acts) and
Cmd+`, passing all of them through untouched as `Event::Gesture`. Ordinary
keystrokes are deliberately not forwarded: Cmd+N preceded the verified fling
by 500ms, so any "recent input" rule would have blessed it.

1. A gesture that explains focus landing on a hidden workspace's window is
   NAVIGATION: follow the workspace there (the README's Cmd+Tab / Dock-click
   feature, now a consequence of a witnessed command rather than an
   inference).
2. A gesture with no nameable target writes `Deferred`. A mouse-down INTO a
   window on the visible workspace keys that window (or a sheet or child Ordo
   does not model), so it does NOT license a follow — a hidden landing after
   it is a fling. A mouse-down outside every visible window (Dock, menu bar,
   a notification) can be aimed at anything and does. This is the only
   point-to-window question asked, and it needs no z-order.
3. NO gesture: the declaration stands. A focus change contradicting
   `Window(w)` is a violation, re-asserted under `DAMPING_LIMIT` exactly like
   a parked frame. Because this is the default, a fling cause nobody has
   catalogued yet costs zero new code.

"Explains" means "arrived since the previous observation": the flag is
consumed by the next snapshot, and by any declaration in between (a Dock
click followed by Cmd+Right before any snapshot must not read the switch's
own parked-old-focus snapshot as the click's navigation). No time window.

Evidence: every gesture leaves `Note::GestureClassified { gesture, armed,
within }` in the log — the verdict, not just the event. An unarmed follow is
otherwise invisible (the gesture row is there, the hidden landing is held,
nothing says why), `within` names the visible window that swallowed a click
(a Dock click over an auto-hidden Dock lands inside the window beneath it and
will show up here), and a `SystemSwitch` row is the proof that the Cmd-release
path fires on real hardware at all. `Note::HeldFocus { window, from, from_app }`
records the hidden window the invariant pulled away from.

### Enforcement and standoff

Every snapshot: if `focus_intent` is `Window(w)`, `w` is in the model and on
the visible workspace, `focused != w`, and no `Focused(w)` grant is still
pending, re-issue the grant — up to `DAMPING_LIMIT` times per declaration
(the command's own grant is not counted), then `Note::FocusDiverged` once
(naming the declared window and the `winner`/`winner_app` that kept the slot)
and **retire the declaration to `Deferred`**. This is the one place focus
deliberately differs from frames. A parked frame's declaration is the user's
filing: losing it is silent and permanent, so a standoff keeps it. A focus
declaration is a claim about now, and a lost one only misdirects the next
carry or MRU chord toward a window the user is visibly not in (run 51: with
44267 key against a grant to 41105, every same-monitor Alt+Shift+Tab resolved
against 41105). Retiring is not adopting — nothing is copied; the OS simply
owns the slot again, the invariant still holds, and the next command declares
afresh.

Retiring alone does not end the standoff when the window that won is on a
HIDDEN workspace (the Outlook-toast shape): `Deferred` resets the budget, the
next snapshot still shows focus on a hidden window, the invariant re-declares
the visible MRU head with a fresh budget, and three grants later it stands
down again — a rate-limited loop, forever, each grant raising the parked
window. So the stand-down also **concedes the slot to the app that kept it**
(`State::conceded`, the pid of the observed key window at that moment), and
the invariant skips while that app still holds focus. The unit is the app,
not the window: AppKit key-window ownership is per-application, and which of
its windows an app keys is incidental — Chrome's key window wandered among
its windows through run 51's standoff, and Cmd+H churn hops focus among an
app's hidden windows routinely. A per-window concession reads each hop as
the world moving on, and the loop returns at full rate. The concession is
evidence, not a timer or a blacklist: it is spent when key belongs to a
DIFFERENT app (the world moved — a later fling back is a fresh violation) or
a command arrives (`declare_focus` clears it — the user asked again, so it is
fought for again with a full budget). Two things do NOT spend it. A focus
vacuum (`focused == None`; reconcile filters `focused` to windows in the
model, so a scan blip is the same case): nobody else took the slot, so it says
nothing about whether the app relented, and the invariant cannot fire on it
anyway. And the app hopping to a VISIBLE window of its own: harmless to the
invariant, and clearing there would let a visible/hidden alternation re-arm
the loop. The Chrome-sibling case never needed this (the sibling is visible,
so `Deferred` simply stands), and is unchanged.

A declaration about a window absent from the model (closed, or dropped by one
flaky scan) is vacuous rather than wrong: nothing is enforced, commands read
the observation, and it resumes untouched if the window reappears.

### MRU

`touch` no longer runs on raw observed focus. Under `Window(w)` the
declaration is recorded (at command time, before any echo). Under `Deferred`
the OS's choice is the only record of where the user went, so observed focus
is recorded — but only when it lies on the visible workspace: a hidden
landing is either navigation (the follow declares it) or a fling (recorded
nowhere).

### Persistence

`focus_intent` rides in the core `State` and therefore in replay checkpoints
(`serde(default)` = `Deferred` for older checkpoints). It is deliberately NOT
in state.json: that file holds the emulated ledger's durable promises, and a
restart should start `Deferred` — the OS owned the desktop meanwhile, and
re-asserting a pre-restart focus would be enforcing a claim about a different
now.

### Known residuals

- Between a refused grant and its standoff (`DAMPING_LIMIT` re-assertions,
  each waiting `EXPECTATION_RESCANS` snapshots — seconds when AX hints are
  firing, up to ~25s on the periodic scan alone) commands read the declared
  window while the user may visibly be in the one the app kept key.
- Under `Deferred`, a fling to a VISIBLE window is not corrected (the OS owns
  the slot) and is recorded in MRU. The invariant still catches hidden
  landings.
- A gesture consumed by an uneventful snapshot before the OS lands its focus
  change (a periodic scan racing a slow app's activation) turns a real
  Cmd+Tab to a hidden workspace into a held landing; the user re-presses.
- After a standoff against a hidden window the keyboard stays in that
  invisible window until the user acts: the concession suppresses the
  invariant for that app's hidden windows, so the pull-back it would
  otherwise perform is the one thing Ordo no longer does for them. Any chord,
  click or Cmd+Tab ends the concession.

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
  RESOLVED 2026-09-01: fight foreign writes with the damped budget, then
  stand down and keep the declaration — never adopt (see the principle).
- Native backend: which of these ownerships flip (assignment is observable
  there; `monitor_ws` is OS-co-owned)?

## Context for resumption (historical; superseded 2026-09-01)

The operational notes that lived here (stale run-41 daemon, uncommitted
files, nohup.out) described the 2026-08-31 working session and are long
stale — trust the git log and the running daemon, not this section. Still
true and worth keeping:

- Useful queries live in the incident rows above; the log DB is
  ~/Library/Application Support/Ordo/log.db (WAL, live), state file
  state.json alongside it.
- Standing agreements: implement exactly Michael's spec, ask about gaps
  before coding (see memory feedback-implement-spec-exactly); no git without
  explicit permission; no daemon restarts without a go; reconciler PAUSED.
