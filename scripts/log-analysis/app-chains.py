#!/usr/bin/env python3
"""What each app's queue did (AppChain rows), and when each switch landed. Needs a run with the
app queues (docs/app-queue-design.md). Usage: app-chains.py RUN_ID

A switch queues all its jobs within a few ms, so chains whose first job was queued close together
are taken as one switch. Its landing time is when its last chain finished; its focus time is when
its focus job ran. Both are from the moment the switch queued them, which is after the frame read
and the save (the `cost` on the Switch row), not from the key press.

Reads ~/Library/Application Support/Ordo/log.db, read-only. See docs/switch-speed.md.
"""
import sqlite3, os, json, sys
from collections import defaultdict
db = sqlite3.connect(os.path.expanduser("~/Library/Application Support/Ordo/log.db"))
R = int(sys.argv[1])
def pct(v, p):
    v = sorted(x for x in v if x is not None)
    return v[min(len(v)-1, int(p/100*len(v)))] if v else float('nan')
def row(label, v):
    v = [x for x in v if x is not None]
    if not v: print(f"{label:44} (none)"); return
    print(f"{label:44} {pct(v,50):7.1f} {pct(v,90):7.1f} {pct(v,95):7.1f} {max(v):7.1f}  n {len(v)}")
names = {}
for (p,) in db.execute("select payload from events where run_id=? and kind='world_observed' order by seq desc limit 50", (R,)):
    for w in json.loads(p)['WorldObserved']['snap']['windows']:
        names.setdefault(w['app'], (w.get('bundle_id') or '?').split('.')[-1])
chains = []
for pid, p in db.execute("select json_extract(payload,'$.pid'), json_extract(payload,'$.chain') from park_trace where run_id=? and kind='AppChain' order by rowid", (R,)):
    c = json.loads(p); c['pid'] = pid; chains.append(c)
if not chains: sys.exit(f"run {R}: no AppChain rows")
print(f"run {R}: {len(chains)} chains\n{'':44} {'p50':>7} {'p90':>7} {'p95':>7} {'max':>7}")
row("chain: waited before starting", [c['wait_ms'] for c in chains])
row("chain: done", [c['done_ms'] for c in chains])
row("chain: time inside position writes", [c['moves_ms'] for c in chains if c['moves']])
row("one write (ax_ms)", [w[1]['ax_ms'] for c in chains for w in c['writes']])
row("un-hide/hold elapsed", [c['show']['elapsed_ms'] for c in chains if c.get('show')])
row("focus ran at", [c['focus']['done_ms'] for c in chains if c.get('focus') and not c['focus']['skipped']])
print(f"moves replaced before sending: {sum(c['replaced'] for c in chains)}; focuses skipped: "
      f"{sum(1 for c in chains if c.get('focus') and c['focus']['skipped'])}; "
      f"holds that left windows out: {sum(1 for c in chains if c.get('show') and not c['show']['converged'])}")
print("\nper app:")
per = defaultdict(list)
for c in chains: per[c['pid']].append(c)
for pid, cs in sorted(per.items(), key=lambda kv: -len(kv[1])):
    print(f"  {names.get(pid, pid):12} chains {len(cs):4}  done p50 {pct([c['done_ms'] for c in cs],50):6.1f} p90 {pct([c['done_ms'] for c in cs],90):6.1f}"
          f"  moves/chain {sum(c['moves'] for c in cs)/len(cs):4.1f}  un-hides {sum(1 for c in cs if c.get('show'))}")
# Group chains into switches by when they were queued.
chains.sort(key=lambda c: c['queued_wall_ms'])
groups, cur = [], []
for c in chains:
    if cur and c['queued_wall_ms'] - cur[0]['queued_wall_ms'] > 20:
        groups.append(cur); cur = []
    cur.append(c)
if cur: groups.append(cur)
def landed(g):
    t0 = g[0]['queued_wall_ms']
    return max(c['queued_wall_ms'] - t0 + c['done_ms'] for c in g)
def focus_at(g):
    t0 = g[0]['queued_wall_ms']
    f = [c['queued_wall_ms'] - t0 + c['focus']['done_ms'] for c in g if c.get('focus') and not c['focus']['skipped']]
    return f[0] if f else None
print(f"\nper switch (chains queued within 20 ms of each other): {len(groups)}")
row("switch landed (last chain done)", [landed(g) for g in groups])
row("switch focus ran", [focus_at(g) for g in groups])
row("apps per switch", [len(g) for g in groups])
rs = db.execute("select landing_wait_ms from restacks where run_id=? and ghost_pass=0", (R,)).fetchall()
row("\nrestack: waited for its apps (landing_wait_ms)", [r[0] for r in rs])
