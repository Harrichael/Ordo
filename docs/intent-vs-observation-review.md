# Opus design review of docs/intent-vs-observation.md

Status: independent subagent review (Opus, 2026-08-31), reproduced verbatim.
Michael has since decided what stands (2026-09-01): most of it was adopted,
but the review's adopt-at-the-limit recommendation (end of candidate 2, and
recommended answer 4) is REVERSED — enforcement never rewrites a declaration;
at the limit it stands down loudly and leaves the window visibly misplaced
(see docs/intent-vs-observation.md, "The principle", and
`enforce_placement`'s doc in ordo-emulated). The body below is unedited
history; where it argues for adoption, the principle overrides it.

---

## Verdict

The central thesis is right and the diagnosis of the phantom-maker is right: `forget_missing`/`note_seen` (`crates/ordo/src/ledger.rs:89`, `:82`) really do let a single missed AX answer rewrite a declaration, and every incident row maps to a real code path I could locate. But the audit understates the blast radius by one critical step — absence doesn't just drop the *assignment*, it deletes the *restore frame* (`emulated_backend.rs:231-233`) and then `park()` re-captures the **sliver position as the window's saved frame** (`emulated_backend.rs:163-165`), which is what makes the corruption permanent and durable rather than a 3-second blip. Two of the four "rightful category" verdicts (`parked` as V, `enforce_attempts` as B) are wrong in ways that would misdirect the redesign, three load-bearing data are missing from the table entirely, and the plan's proposed death-evidence mechanism ("N consecutive absences") is unsound against this codebase's actual scan cadence.

---

## Audit corrections

**Right, in one line each:** `mode`, `workspace_count` writer, `windows[w].workspace` as a D-mirror, `title/bundle_id` as O with CG-id identity, `focused` needing echo attribution, `focus_history` as collateral damage of existence flicker, `ledger.current` as D, `saved` as a D promise with capture-at-park as the freshness rule, `state.json` as durable D containing only declarations, `z-order` as never-stored V, and the whole "an observation contradicting a declaration is not an observation of that fact" principle. The `monitor_ws` row's note ("per-backend ownership must be explicit in the snapshot contract") is the single most valuable line in the document and is underdeveloped — see §6.

### 1. `parked` is not V of (assign, current)

The plan says `parked` is "V of (assign, current) in principle; stored today", and that making it derived "would make the drift unrepresentable". Both halves are wrong.

It is not derivable from (assign, current): `assign[w] != current` says the window *should* be parked; `parked` says "we have issued the sliver write **and** captured the pre-park frame". Those diverge legitimately. `park()` returns `None` without any bookkeeping when the window isn't in the AX frame map (`emulated_backend.rs:163`), so a window can be declared-hidden with no park write ever issued and no saved frame. Deriving `parked` would assert a park that never happened.

What `parked` actually is: a redundant encoding of `saved.contains_key(w)`. The persistence layer already states this as an invariant — "Present iff the window is parked… the schema makes that state unrepresentable" (`statefile.rs:46-49`) — and `load_state` reconstructs one from the other (`emulated_backend.rs:92-97`). The correct fix is not "derive it", it's **collapse the two fields into one**: `saved: HashMap<WindowId, Rect>`, presence = parked. That makes the drift unrepresentable *today*, with a smaller diff than either the current lockstep-prune or the plan's derivation.

And the drift is *not* fixed today, contrary to the row's parenthetical. `load_state` sets the ledger via `Ledger::restore`, which **drops** out-of-range assignments (`ledger.rs:64`), then populates `saved`/`parked` from `ps.windows` unconditionally (`emulated_backend.rs:92-97`). Restart with `--workspaces 3` after a session at 9 and every window assigned to ws 4-9 loses its assignment while staying in `parked` — the exact "parked ⊃ assign" state the doc says lockstep-prune eliminated, now armed against the user on the next `enforce_placement` (`emulated_backend.rs:325` won't skip them; they have no assignment).

### 2. `enforce_attempts` is not B

Classifying it bookkeeping is what lets the plan reach for "per-episode budgets" as the fix. It is an **observation** — "how much opposition has the world offered" — and the defect is that it observes the wrong thing (counting our own writes as opposition). Once you call it O, the correct fix follows mechanically: fix the observation, don't tune the budget. See candidate 2.

### 3. `workspace_count` needs the same per-backend annotation as `monitor_ws`

The row says "config/backend | **D**". Under emulation it is D (CLI `--workspaces`, `main.rs:46` → `Ledger::count`). Under native it is pure O: `reconcile::apply_snapshot` takes the min over monitors' reported space counts (`reconcile.rs:136-138`), which the user changes in Mission Control. Same split as `monitor_ws`, same reason.

Worse, under emulation it is a D that **destroys other declarations when it shrinks**: `Ledger::restore` drops rather than clamps (`ledger.rs:64`). A config edit silently deletes assignments and restore frames.

### 4. `intercepting` has three writers, not one

The row says "chords | **D** | sound". Three independent writers, no arbitration: the tap's fast path, the rescue CLI's SIGUSR1 handler (`main.rs:234-237`), and the core's `SetIntercepting` effect (`effector.rs:76-78`). `State::mode` is a fourth representation of the same declaration with its own writer (`update.rs:96-110`). Since `MacWorldSource::snapshot` gates `enforce_placement` on the atomic (`platform/mod.rs:95`) while the core gates effects on `mode`, the two can disagree for a window of time — enforcement running while the core believes it is inert. Not a bug I can pin to an incident, but it is not "sound", and it is the same category error at one level up: one declaration, several homes.

### 5. Data missing from the audit entirely

- **`WindowRecord.app` (Pid)** — O, and decisions key off it (`state.rs:29` says so explicitly): dock dimming (`emulated_backend.rs:196`), `MruApp` (`update.rs:282`). Its omission matters because the pid is exactly what candidate 1 needs to survive window-id reuse (see below).
- **`State.monitors` (frame, `is_main`)** — absent from every table, yet it is the anchor for `park_frame`, which recomputes the sliver corner from *whatever display is currently main* on every enforce pass (`emulated_backend.rs:139-147`, called at `:331`). Unplug a monitor and every parked window's target position moves, so every parked window reads as in-violation simultaneously and each burns enforce budget for a reason that has nothing to do with an app fighting back. Symmetrically, `saved` frames can name a display that no longer exists, and nothing clamps them on restore (only `rescue_gather` clamps, `rescue_gather.rs:71`). This is a V-anchored-on-O problem of exactly the kind candidate 3 addresses, on a datum the audit never lists.
- **`WindowSnap.workspace`'s fabricated default.** `platform/mod.rs:151` does `topo.window_ws.get(&w.id).copied().unwrap_or(WorkspaceId(1))`. `skylight.rs:112` documents the opposite contract: "the caller treats an omitted window as 'workspace unknown' and leaves belief be." The caller does not. Under emulation this is dead code (`note_seen` guarantees an entry), but under native it silently teleports a window into belief on ws1 whenever `SLSCopySpacesForWindows` doesn't resolve it — an *unknown* laundered as a declaration. The audit has no row for "workspace unknown", and the snapshot type has no way to express it. That is the taxonomy's real gap: there is no fourth value **U/unknown**, and every place one is needed today fabricates a declaration instead.

### 6. A missing incident row: the core also misattributes our own frame writes

The table blames the *backend's* damping for treating self-writes as opposition. The same misattribution exists in the core and is unlisted. `Expectation::AllMonitorsOn` explains `MonitorWorkspaceChanged` and `FocusChanged` only (`reconcile.rs:214-215`); it does **not** explain `WindowFrameChanged` or `WindowMonitorChanged`. So every park and every restore emits unexplained frame deltas → `Note::External` for each parked window on every switch (`update.rs:494-497`), and marks each of them `hands_on` (`update.rs:510-519`), which suppresses legitimate frame-correction retries for that snapshot (`update.rs:551`). On multi-monitor it also emits a spurious `WindowMonitorChanged` per parked window, because the sliver is always on the main display.

This matters more than its severity suggests: the entire plan is built on reading run logs, and this floods the `External` channel with our own writes on every single switch. Fixing it is the same one-line-of-architecture fix as candidate 3 (see below).

---

## Candidate-by-candidate assessment

### Candidate 1 — assignment ownership

**Does it kill the documented phantom?** Yes, but only if it covers more than the plan says, and the plan is missing the mechanism that makes the phantom *permanent*.

The full trace, with line numbers. Chrome 35858 is assigned ws3, parked, `saved` holds its real frame; current is ws2. One scan misses it (AX timeout, `ax.rs:28`, `ax.rs:73` — a `copy_attr("AXWindows")` failure drops that app's **entire** window set, not one window):

1. `forget_missing` drops the assignment (`ledger.rs:90`, called `emulated_backend.rs:225`).
2. `saved.retain` / `parked.retain` delete the restore frame and the parked flag (`emulated_backend.rs:231-232`). **The promise is destroyed, not just the assignment.**
3. `persist()` writes the loss to disk (`:235`).
4. Next scan re-sights it → `note_seen` adopts it to `self.current` = ws2 (`ledger.rs:84`).
5. It is now declared ws2 (visible) while physically sitting at the 1px corner, and `enforce_placement` iterates `self.parked` (`:319`) — which no longer contains it — so nothing ever corrects it.
6. Next switch away from ws2, `park()` runs: not in `parked`, so it captures `frames.get(&window)` — **which is the sliver** — as the window's saved frame (`:163-165`).

Step 6 is the whole ballgame and the plan never mentions it. From there the window is 1px in the corner forever, durably, and every "deterministic stand-up on ws2 visits" the plan observed is that.

**Therefore: the highest-value single change in this entire document is not candidate 1. It is a guard in `park()`:** refuse to capture a frame that is already (approximately) a park frame; treat the window as already parked and keep whatever `saved` you have. Four lines at `emulated_backend.rs:160-167`. It defends against the partial-scan path, the restart path, the R+S path (§Risks), the post-rescue path, and every future path nobody has thought of, because it is an invariant on the *promise* rather than a guard on one of the ways to break it. Do it first, independently, before any of candidates 1-4.

**Edge cases the plan asks about:**

- **Window closed while parked on a hidden workspace.** Absent from AX forever → correctly forgotten after evidence. But note that dock dimming has `Cmd+H`-hidden its app (`emulated_backend.rs:191-208`), which is why the next point is critical.
- **The CG existence probe must not use the on-screen list.** `zorder.rs:35-78` already declares and uses `CGWindowListCopyWindowInfo` with `ON_SCREEN_ONLY | EXCLUDE_DESKTOP` (`zorder.rs:25-26, 38`). A hidden app's windows are *not* on-screen, so probing with those flags would report exactly the parked-and-dimmed windows as dead — a strictly worse phantom-maker than the one being fixed. Use option `0` (all windows) with the layer-0 filter. Good news: this is **not a new platform call** as the plan's open question assumes; the binding and the CF walk exist, and reading `kCGWindowNumber`/`kCGWindowOwnerPID` needs no Screen Recording permission (only `kCGWindowName` does), which the restack worker's landing gates already prove in production.
- **App quit.** All windows vanish in one scan. Fine — stale ledger entries are inert (`switch()`'s plan entries no-op through `park`/`restore`'s `frames.get(&window)?`).
- **Window id reuse — the plan's biggest unaddressed risk, and one candidate 1 *introduces*.** CGWindowIDs are recycled within a WindowServer life; `statefile.rs:13-15` already reasons about this across boots but not within one. Today an id is forgotten instantly, so reuse is harmless. Under candidate 1, a dead window's assignment **and its `saved` frame** linger for the grace period; a newly-opened window that draws the recycled id inherits both and gets teleported to a stranger's old frame on the next switch. Mitigation is cheap and the data is already in hand: record the owning `Pid` alongside each assignment and treat a reappearing id with a different pid as genuinely new. This also *defines* "genuinely new" (see open questions).
- **Absence counters vs scan cadence — "N consecutive absences" is unsound as specified.** The cadence is not 2s. It is: periodic at `--interval` (default 2.0s, `main.rs:28`), *plus* `PostEffect` rescans inside one engine cascade (`engine.rs:227-233`, up to `MAX_CASCADE` = 64), *plus* one per AX focus/creation hint (`observer.rs:167`), *plus* space-change hints at up to 150ms (`observer.rs:74`) and the WindowServer stream's 150ms-debounced hints (`ws_events.rs:238`). A single burst of workspace switching can produce five or ten snapshots in under a second. N=3 "consecutive absences" can therefore elapse in ~200ms — against Chrome, the app that actually times out. The counter would fire *fastest* exactly when the system is busiest and AX is least reliable. If you keep counters at all, the bound must be wall-clock ("absent continuously ≥ T seconds **and** ≥ N scans"), not scan-count.
- **Interaction with state.json pruning.** Today's documented behavior ("pruned by the first non-empty scan", `statefile.rs:15-16`) is a *startup-specific* corruption path the plan doesn't call out: the very first scan after `load_state` is the one most likely to be partial, since every app is being AX-enumerated for the first time. Fixing the pruning rule fixes that too.
- **`note_seen` needs no change.** `or_insert` already only writes when absent (`ledger.rs:84`). Given a fixed `forget_missing` plus the pid check, adoption is already "genuinely new ids only". The plan's phrasing implies two edits; it's one. Worth knowing — it halves the work.

**Also fix, in the same change:** the retains at `emulated_backend.rs:231-233` must follow the assignment's fate, and `enforce_attempts` should be keyed off the declaration too (see candidate 2), not off `parked`.

### Candidate 2 — enforcement as declaration-assertion

**Sound in direction, and it fixes a hole the plan doesn't claim.** Reframing enforcement to iterate the *declaration* (`assign[w] != current`) rather than `self.parked` (`emulated_backend.rs:319`) closes a permanent blind spot: a window whose park write never happened (because `park()` early-returned at `:163` on a missing frame) is declared hidden, sits visibly on the current workspace, and is **invisible to enforcement forever**. The plan presents candidate 2 as being about budgets; say explicitly that the iteration domain is the substantive change.

**The two proposed mechanisms are in tension, and the plan doesn't notice.** "Per-episode budgets (reset on each fresh perturbation)" is unsafe *without* the self-write horizon, because our own stale write is itself a fresh perturbation — the budget would reset forever and you'd have an unbounded fight, precisely what damping exists to prevent. So the self-write horizon is not the "and/or" alternative; it is a precondition. Order matters.

**A cheaper mechanism than either, with no clock and no horizon.** The discriminator you need is free and already in the struct. A stale restore lands the window at *exactly* the frame we asked for, because `restore()` writes `self.saved.get(&window)` verbatim (`emulated_backend.rs:178-179`). So per enforce pass, classify the observed frame three ways:

- ≈ `park_frame(f)` → compliant; clear the counter (already done, `:333-335`).
- ≈ `saved[w]` → **our own stale restore landed late.** Re-park; do *not* count it.
- neither → a foreign write. Count it.

Exact, deterministic, testable in the pure `Ledger`/backend without a clock — which matters, because the alternative (an `Instant`-based horizon) makes the backend time-dependent and hurts replay; `boot_time` (`emulated_backend.rs:64`) is the existing precedent for that and it's the ugliest thing in the file. If you do end up needing wall-clock, pass `now` into `enforce_placement` from the shell rather than reading the clock inside the backend.

The one misclassification: an app restoring its own autosaved frame after a minimize/restore also lands ≈ `saved[w]`. Benign — the desired response (re-park without counting) is identical.

**Races with the app's serial AX queue.** Better than the plan implies, on one axis and worse on another. Better: all `ax::set_frames` calls originate on the engine thread (`engine.rs:222-247`, `platform/mod.rs:105`), and `set_frames` joins its per-app threads before returning (`ax.rs:403-407`), so two Ordo batches never overlap in issue time. Worse: within a batch, each app gets a fresh `AXUIElement` on its own thread (`ax.rs:411`), and `AXSetAttributeValue` is acknowledged long before the app relayouts — Chromium is documented at 100ms+ in this very repo (`zorder.rs:86`). And `MacWorldSource::snapshot` reads the world *before* issuing the enforce write (`platform/mod.rs:96-99` says so), so attempt 1 is structurally guaranteed to still look like a violation on the next snapshot. With `ENFORCE_LIMIT = 3` (`emulated_backend.rs:34`), the effective budget against a genuine opponent is roughly **one**. That alone explains the enforcement war without needing the stale-restore story.

**One more structural point.** The plan's fix keeps the shape "count opposition, then surrender". Surrender leaves the two models in permanent disagreement, which *is* the state that produced the war. Michael's own principle says the resolution is "correct, or explicitly adopt by a named policy" — never "stand down and diverge". So: at the limit, **adopt** (reassign to the current workspace, log a named `AdoptedEscapee` note), don't just stop writing. An episode should never end in disagreement.

### Candidate 3 — monitor derived from the saved frame while parked

**Right instinct, wrong lever, and the stated mechanism doesn't hold.** The plan blames MRU cross-monitor leaks on parked windows' `monitor` being derived from the sliver. But every MRU predicate filters `r.workspace != cur_ws` *first* (`update.rs:271`, `:324`), and `mru_stack` does the same (`update.rs:127`), so parked windows are already excluded from MRU scoping. The leak cannot come from parked-ness per se.

Where it does come from is the **restore lag**: the ledger flips the window to the current workspace immediately (`ledger.rs:101`), but belief still carries the sliver frame until a snapshot after the restore write lands. In that window the window *is* in MRU scope with `monitor` = main display and `frame` = the corner. That also poisons `WarpMouse { to: s.windows[&target].frame.center() }` (`update.rs:292`, `:331`) — pointer to the corner — and `MoveFocusedToOtherMonitor`'s `translate_between` (`update.rs:370`).

Deriving `monitor` from `saved` *while parked* does not cover the restore-lag case, because by then the window isn't parked. The rule should key on the observation, not on the flag: **whenever the observed frame is the sliver, the believed frame is `saved[w]`.** (`saved` conveniently survives `restore()` — `emulated_backend.rs:172` clears `parked` but never `saved` — so the data is there.)

**Cheaper alternative, and strictly better: substitute the frame at the snapshot seam, not the derivation in the core.** Have the backend hand the snapshot the *believed real frame* (saved, when the observed frame is the sliver) instead of the raw AX frame. One site, in `MacWorldSource::snapshot` / `BackendTopology`. Then:

- `monitor` derives correctly for free, with no special case in `reconcile::derive_monitor`.
- `WarpMouse` and `MoveFocusedToOtherMonitor` are fixed as a side effect.
- The `External`-note flood and the spurious `hands_on` from §Audit-6 disappear, because the core never sees a frame change to the sliver at all.
- The core stops knowing that slivers exist — which is the correct Telos boundary. Parking is the emulated backend's private mechanism; today it leaks into core belief, and every fix downstream of that leak is a patch.

`enforce_placement` keeps working: it already receives the raw frames on a separate path (`platform/mod.rs:100-105`).

### Candidate 4 — commands read declared focus

**Sound, and the cheaper of the two options named is not the better one.** "Defer while a `Focused` expectation is unconfirmed" drops the keypress; a swallowed Cmd+Ctrl+arrow feels broken. The right shape costs about the same: read the *declared* focus, defined as the most recent pending `Expectation::Focused(w)`, falling back to `s.focused`. Six lines of pure code on `State`, next to `focused_monitor()` (`state.rs:113`). It self-expires via `rescans_left` and falls back correctly when the op fails (`update.rs:829-831`). Fixes the same class at `update.rs:202` (carry), `:259` (Mru scoping anchor), `:315` (demote), `:351` (move-to-monitor), and `state.rs:114`.

Do **not** build this and the reconciler's "command read context" — agreed with the plan; this is the version to build.

**A second cause of carry breakage the plan doesn't mention.** A carry emits `MoveWindowToWorkspace` then `SwitchWorkspace` to the same target (`update.rs:221-245`). The move parks the window — captures `saved`, writes the sliver (`emulated_backend.rs:281`) — and the switch immediately restores it, writing `saved` back (`:270`). Two frame writes for one window, microseconds apart, applied on the app's own schedule. If they land out of order the carried window arrives at the destination as a 1px sliver, and thereafter `park()` may canonicalize it (see candidate 1, step 6). This is a coin flip on every carry against a slow app.

The D/O framing actually predicts the fix, which is a point in the taxonomy's favor: **"reassign" (write the declaration) and "move" (write the declaration *and* enforce it visually) are two different operations**, and a carry wants the first. Splitting `Effect::MoveWindowToWorkspace` into an assignment-only variant for the carry path removes both writes' race and one full AX round trip from every carry.

### Candidate 5 — re-evaluate the reconciler afterward

Agreed, and see §6: after candidates 1-4, ask whether the reconciler has anything left to do that isn't emulation-specific.

---

## Recommended answers to the open questions

**1. Death evidence.** CG existence probe as the *sole* authority; no absence counters. Concretely: after `forget_missing` finds ledger entries absent from the AX scan, batch-probe them against `CGWindowListCopyWindowInfo(0 | EXCLUDE_DESKTOP)` (option `0`, **not** `ON_SCREEN_ONLY`), layer-0 filter, one WindowServer round trip, at most once per periodic rescan. Present in CG → keep the declaration untouched. Absent from CG → dead, forget. Reasons to prefer this over counters: (a) it is authoritative rather than statistical; (b) it removes the cadence problem entirely, which "N consecutive" cannot solve because the cadence is event-driven and bursty; (c) the binding already exists (`zorder.rs:30`) and needs no new permission; (d) it answers question 2 for free. If you want belt-and-braces, add a counter as a *fallback for when the CG read itself returns empty* (same "an empty read is not a read of emptiness" rule as `engine.rs:97` and `emulated_backend.rs:223`) — never as the primary.

**2. Persisting absence counters across restart.** Moot under the answer above: no counters to persist. And it is the better outcome anyway, because on restart the first scan is the *most* likely to be partial, so a grace-period design has to be at its most conservative exactly where it has the least information. A CG probe on the first scan is simply correct. Change `statefile.rs:15-16`'s documented contract from "pruned by the first non-empty scan" to "pruned only on CG-confirmed death".

**3. Adoption policy for genuinely new windows.** Keep "current workspace" — it is right, it matches where macOS actually puts new windows, and the core's new-window corralling already anchors on the pre-observation workspace for the focus-stealing case (`update.rs:451`, `:579-599`). Define **genuinely new** as: *the (id, pid) pair is not in the ledger.* Not "never-seen id" (id reuse), and not requiring the `AXWindowCreated` hint — that hint is per-app and permanently unavailable for apps whose observer attach failed once, since `attach_new` marks failures as attached forever (`observer.rs:113-118`). The pid is already in every `AxWindow` (`ax.rs:32`), so this costs one field on the ledger entry.

**4. Fight-or-adopt when the user drags a sliver out.** Fight, but only against genuinely foreign writes, and end by adopting rather than by surrendering:
- observed ≈ park frame → compliant.
- observed ≈ `saved[w]` → our own stale write (or an app's autosave); re-park, don't count.
- observed ≈ neither → foreign; count. At the limit, **adopt**: reassign to the current workspace, clear `saved`, log a named policy note.

Rationale: a 1px sliver is not a thing a user drags by accident, and a window that visibly refuses to hide *is* on the visible workspace — adopting makes belief true, whereas today's "stop correcting" leaves a permanent lie in the ledger, which is what the enforcement war was made of. The user also already has explicit levers (`rescue_window`, the R/O chords), so auto-adoption isn't the only escape hatch — which is why the limit can stay small.

**5. Which ownerships flip under native.**
- **Flip D→O:** `ledger.assign` (SkyLight reports window→space, `skylight.rs:113`), `monitor_ws` (already annotated), `workspace_count` (`reconcile.rs:136`), `windows[w].frame` (fully O — no slivers, so candidate 3's whole problem evaporates).
- **Cease to exist:** `saved`, `parked`, `enforce_attempts`, `state.json`, the entire `enforce_placement` path, `bring_up`/`suspended`.
- **Don't flip:** `mode`, `intercepting`, focus intent (candidate 4), MRU, z-order.

The consequential observation, which the plan should state plainly: **under native there is no standing assignment declaration at all** — only per-command intent with a short expectation lifetime, which `PendingOp` already models (`state.rs:52-57`). So "an observation contradicting a declaration" is meaningful for assignment *only under emulation*. The whole D/O apparatus, and the reconciler behind it, is emulation-specific machinery. That makes one question strategically prior to all five: is emulated the long-term backend? Signals say yes — it is the CLI default (`main.rs:41`) and native's `move_window_to_workspace` is documented as possibly restricted on current macOS (`native_backend.rs:174-183`). If so, say it out loud and stop hedging the design across two backends. If not, don't build D/O into the core.

---

## Risks the plan misses

1. **`park()` canonicalizes the sliver as the saved frame** (`emulated_backend.rs:163-165`). The permanence mechanism behind every phantom report; reachable from partial scans, restarts, workspace-count shrinks, R+S, and post-rescue. Highest-value fix in the document, and independent of all four candidates.

2. **Absence deletes `saved`, not just `assign`** (`:231-233`), and `persist()` makes the loss durable (`:235`). Candidate 1 mentions keeping `saved`; the audit's incident row doesn't, and it's the difference between a 3-second blip and a permanently 1px window.

3. **`R` then `S` destroys every restore frame while the windows are still physically parked.** `bring_up(false)` blanks the ledger and clears `saved`/`parked` (`:301-306`) with `suspended = true`, relying on the file to still hold the promises. `resume_persistence` then sets `suspended = false` and immediately `persist()`s the *blank* model (`:310-313`), overwriting the file with `windows: []`. Ctrl+Alt+Cmd+R followed by Ctrl+Alt+Cmd+S (`keys.rs:88-93`) permanently strands every hidden workspace. In the plan's own terms: `S` is a command that destroys declarations it has never seen. `S` after `R` should either be refused or should merge, never blind-overwrite.

4. **`Ledger::restore` drops rather than clamps out-of-range assignments** (`ledger.rs:64`) while `load_state` populates `parked` unconditionally (`:92-97`) — recreating the parked ⊃ assign drift the doc believes is fixed, and stranding those windows via mechanism (1).

5. **CG probe flags.** `ON_SCREEN_ONLY` (`zorder.rs:25, 38`) excludes `Cmd+H`-hidden apps' windows — i.e. exactly the parked, dock-dimmed set (`emulated_backend.rs:191-208`). A probe built by copying the existing helper would be a worse phantom-maker than the bug it fixes.

6. **Window id reuse within a boot** — a *new* failure mode introduced by candidate 1's grace period. Needs the (id, pid) binding.

7. **Scan cadence is bursty and event-driven**, not the 2s the counter design assumes (`main.rs:28`, `engine.rs:227`, `observer.rs:74`, `ws_events.rs:238`). N-consecutive-scans fires fastest under exactly the load that makes AX least reliable.

8. **`enforce_placement` iterates `parked`, not the declaration** (`:319`): a park write that never happened is unenforceable forever.

9. **`park_frame` re-anchors on the live main display** (`:139-147`, called `:331`): a display change puts every parked window in violation at once and drains every budget for a non-adversarial reason. And `saved` frames referencing a departed display restore windows off-screen, unclamped.

10. **The core classifies our own park/restore writes as external world changes** (`reconcile.rs:212-229` + `update.rs:494-519`): `External` notes for every parked window on every switch, spurious `WindowMonitorChanged` on multi-monitor, and `hands_on` suppression of legitimate frame retries. This degrades the very log channel the plan is being written from.

11. **A carry issues park-then-restore for the same window in one turn** (`update.rs:221-245` → `emulated_backend.rs:281` then `:270`), two racing frame writes per carry.

12. **`focused = None` on a partial AX scan** is the same absence-is-not-evidence hole in a datum the audit calls sound. `ax::focused_window()` returns `None` if no app answers `AXFrontmost` in 0.2s (`ax.rs:121-153`); belief then blanks focus (`reconcile.rs:179`), which makes `MruMonitor`/`MruApp`/`MruOtherMonitor` silently no-op (`update.rs:262`) and moves the `focused_monitor()` anchor to the main display (`state.rs:113-119`). Same fix shape as the window case: a missing answer is not an answer of "nothing".

13. **`WorkspaceId(1)` fabricated for unresolved windows** (`platform/mod.rs:151`), contradicting `skylight.rs:112`'s stated contract. Harmless under emulation, a silent belief-teleport under native. The general lesson: the snapshot type has **no representation for "unknown"**, so every unknown becomes a fabricated declaration. That is the one gap in the D/O/V/B taxonomy itself — it needs a **U**.

---

## On the taxonomy (§6)

The four categories are a good analysis aid and a poor enforcement mechanism. The simpler mechanical rule that achieves the same guarantee, and that I'd recommend stating as the actual invariant:

> **A declaration must never travel through the observation channel.**

Ordo already implements half of the discipline: `reconcile::apply_snapshot` rebuilds `s.windows` wholesale from the snapshot and explicitly carries forward only "what the snapshot cannot know" (`reconcile.rs:115-119`). That is exactly the D/O split, enforced by construction. The reason the bug class exists anyway is that **`window.workspace` — a declaration — is smuggled inside `WindowSnap`**, so it arrives on the observation channel, gets rebuilt wholesale like an observation, and produces `Delta::WindowWorkspaceChanged` deltas for facts nobody observed. Every one of the plan's incidents is downstream of that single laundering.

So the structural change that makes the class unrepresentable, rather than guarded against, is at the seam and not in the ledger: `WorldSnapshot` carries what AX and CG saw (id, pid, title, frame, focused, monitors); assignment and current-workspace arrive as a **separate declarations input**. Then under emulation a workspace "change" without a command is a type error, and under native the same field arrives on the observation channel because that's where the truth lives — which is the per-backend ownership the `monitor_ws` row asks for, expressed as a type rather than a footnote. It also subsumes candidate 3 (substitute the frame at the same seam) and gives "unknown" somewhere to live.

Two smaller notes. `V` as "never stored authority" fights an existing, well-argued decision: `WindowRecord.monitor` is deliberately stored-derived for hot-path reasons (`state.rs:34-36`). The plan wants to change its *anchor*, which is right; the "never stored" framing points the wrong way. And `B` is doing no work — no bug in the audit traces to a B datum except `enforce_attempts`, which isn't B. Three categories plus U, and one rule about channels, would carry the whole argument.
