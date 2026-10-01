#!/usr/bin/env python3
"""Per-switch timeline for one run: hotkey queue wait, engine time (hotkey -> post-switch rescan),
its parts, and the restack on the worker; then how long looks were held back for busy apps (schema
v9 on; zero before). Usage: switch-breakdown.py RUN_ID
For runs before the app queues: later runs carry no un-hide stats on AppShown rows, and their
writes happen after the engine moves on; use app-chains.py for those.

Reads ~/Library/Application Support/Ordo/log.db, read-only. See docs/switch-speed.md.
"""
import sqlite3, os, json, sys
db = sqlite3.connect(os.path.expanduser("~/Library/Application Support/Ordo/log.db"))
R = int(sys.argv[1]) if len(sys.argv) > 1 else 37
def pct(v, p):
    v = sorted(x for x in v if x is not None)
    return v[min(len(v)-1, int(p/100*len(v)))] if v else None
# hotkey event seq -> processing time; queue wait by batch first_seq
hot = {s: w for s, w in db.execute("select seq, wall_ms from events where run_id=? and kind='hotkey'", (R,))}
qwait = {s: w for s, w in db.execute("select seq, oldest_wait_ms from hotkey_batches where run_id=? and seq is not null", (R,))}
sw_op = {s: op for s, op in db.execute("select seq, op_id from effects where run_id=? and kind='switch_workspace'", (R,))}
post = {}
for seq, w, trig in db.execute("select seq, wall_ms, json_extract(payload,'$.WorldObserved.trigger') from events where run_id=? and kind='world_observed' and payload like '%PostEffect%'", (R,)):
    op = json.loads(trig)['PostEffect']['op']
    post.setdefault(op, (seq, w))
has_held = any(c[1] == 'held_ms' for c in db.execute("pragma table_info(snapshots)"))
held_col = "held_ms" if has_held else "0"
snap = {s: (t, wk, en, h) for s, t, wk, en, h in db.execute(f"select seq, total_ms, walk_ms, enforce_ms, {held_col} from snapshots where run_id=?", (R,))}
# un-hide/hold time per switch: AppShown rows between consecutive Switch rows, by rowid
trace = db.execute("select rowid, kind, json_extract(payload,'$.hold') from park_trace where run_id=? and kind in ('Switch','AppShown') order by rowid", (R,)).fetchall()
switch_shows = []; cur = None
for rid, k, hold in trace:
    if k == 'Switch':
        cur = []; switch_shows.append(cur)
    elif cur is not None and hold:
        cur.append(json.loads(hold)['elapsed_ms'])
restacks = db.execute("select wall_ms, total_ms, presence_wait_ms, handoff_wait_ms, refocused, raise_set from restacks where run_id=? and ghost_pass=0 and aborted=0 order by restack_id", (R,)).fetchall()
rows = []
seqs = sorted(s for s in hot if s in sw_op)
for i, s in enumerate(seqs):
    op = sw_op[s]
    if op not in post: continue
    pseq, pwall = post[op]
    eng = pwall - hot[s]
    sn = snap.get(pseq)
    walk = sn[0] if sn else None
    shows = switch_shows[i] if i < len(switch_shows) else []
    unhide = max(shows) if shows else 0
    held = sn[3] if sn else 0
    rows.append(dict(queue=qwait.get(s, 0), engine=eng, walk=walk, unhide=unhide, held=held,
                     moves=eng - (walk or 0) - unhide - held, enforce=sn[2] if sn else None, t=hot[s]))
# attach restacks: first restack whose start >= hotkey time
ri = 0
for r in rows:
    while ri < len(restacks) and restacks[ri][0] - restacks[ri][1] < r['t']:
        ri += 1
    if ri < len(restacks):
        w, tot, pres, hand, refoc, rs = restacks[ri]
        r.update(restack=tot, presence=pres, refocus=refoc, raised=rs)
print(f"run {R}: {len(rows)} switches")
if not rows:
    sys.exit()
cols = [("queue", "hotkey waiting for the engine"), ("engine", "focus + switch + rescan, on the engine"),
        ("moves", "  of which: focus + window moves"), ("unhide", "  of which: slowest app un-hide/hold"),
        ("held", "  of which: its look held back for the apps"),
        ("walk", "  of which: post-switch rescan walk"), ("enforce", "then: core's placement check (hides)"),
        ("restack", "restack on the worker, in parallel"), ("presence", "  of which: waiting for windows to reappear")]
print(f"{'':44} {'p50':>5} {'p90':>5} {'p95':>5} {'max':>5}  {'mean':>6}")
for k, label in cols:
    v = [r.get(k) for r in rows]; v2 = [x for x in v if x is not None]
    print(f"{label:44} {pct(v,50):>5.0f} {pct(v,90):>5.0f} {pct(v,95):>5.0f} {max(v2):>5.0f}  {sum(v2)/len(v2):6.1f}")
rf = [r for r in rows if r.get('refocus')]; nr = [r for r in rows if 'refocus' in r and not r['refocus']]
print(f"restack with a focus take-back: n {len(rf)} p50 {pct([r['restack'] for r in rf],50)}; without: n {len(nr)} p50 {pct([r['restack'] for r in nr],50)}")
import re
details = [d for d, in db.execute("select json_extract(payload,'$.detail') from park_trace where run_id=? and kind='Switch' order by rowid", (R,))]
moved = []
for d in details:
    m = re.match(r"parking (\d+), restoring (\d+), rehosting (\d+)", d or "")
    moved.append(sum(map(int, m.groups())) if m else None)
per = [r['moves'] / moved[i] for i, r in enumerate(rows) if i < len(moved) and moved[i]]
print(f"windows moved per switch p50 {pct(moved,50)} mean {sum(x for x in moved if x)/len(moved):.1f}; focus+moves per window moved p50 {pct(per,50):.1f} p90 {pct(per,90):.1f} ms")
held = [h for h, in db.execute(f"select {held_col} from snapshots where run_id=?", (R,))]
waited = [h for h in held if h > 0]
if waited:
    print(f"looks held back for busy apps: {len(waited)} of {len(held)}; wait p50 {pct(waited,50):.0f} p90 {pct(waited,90):.0f} max {max(waited):.0f} ms")
else:
    print(f"looks held back for busy apps: 0 of {len(held)}" + ("" if has_held else " (log predates held_ms)"))
