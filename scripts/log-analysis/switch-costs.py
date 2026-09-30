#!/usr/bin/env python3
"""Where a switch's own time goes (the `cost` on Switch/View rows), per-app move times from
AppMoved rows, and single-write times. Needs a run from 2f37f98 on. Usage: switch-costs.py RUN_ID
Runs with the app queues have no AppMoved rows; see app-chains.py for those.

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
    if not v: print(f"{label:48} (none)"); return
    print(f"{label:48} {pct(v,50):7.1f} {pct(v,90):7.1f} {pct(v,95):7.1f} {max(v):7.1f}  mean {sum(v)/len(v):6.1f}  n {len(v)}")
names = {}
for (p,) in db.execute("select payload from events where run_id=? and kind='world_observed' order by seq desc limit 50", (R,)):
    for w in json.loads(p)['WorldObserved']['snap']['windows']:
        names.setdefault(w['app'], (w.get('bundle_id') or '?').split('.')[-1])
trace = db.execute("select rowid, kind, payload from park_trace where run_id=? order by rowid", (R,)).fetchall()
switches = []; cur = None
for rid, kind, p in trace:
    t = json.loads(p)
    if kind in ('Switch', 'View'):
        cur = dict(kind=kind, cost=t.get('cost'), detail=t.get('detail'), apps=[], writes=[], shown=[])
        switches.append(cur)
    elif cur is None: continue
    elif kind == 'AppMoved': cur['apps'].append((t['pid'], t['moves']))
    elif kind == 'AppShown' and t.get('hold'): cur['shown'].append((t['pid'], t['hold']))
    elif t.get('write') and kind in ('Park','Restore','Rehost','Rehome'): cur['writes'].append((t['window'], t['write']))
sw = [s for s in switches if s['cost']]
print(f"run {R}: {len(sw)} switches/views with costs\n{'':48} {'p50':>7} {'p90':>7} {'p95':>7} {'max':>7}")
keys = [k for k in ('read_ms','persist_ms','moves_ms','visibility_ms','queue_ms') if any(k in s['cost'] for s in sw)]
for k in keys:
    row(f"switch: {k}", [s['cost'].get(k) for s in sw])
row("switch: engine-thread total", [sum(s['cost'].values()) for s in sw])
row("windows moved per switch", [len(s['writes']) for s in sw])
row("apps moved per switch", [len(s['apps']) for s in sw])
row("one write (ax_ms)", [w['ax_ms'] for s in sw for _, w in s['writes']])
row("slowest app's share (total_ms)", [max((m['total_ms'] for _, m in s['apps']), default=None) for s in sw])
print("\nper app (all switches):")
per = defaultdict(lambda: dict(list=[], total=[], eui=0, n=0, win=[]))
for s in sw:
    for pid, m in s['apps']:
        a = per[pid]; a['list'].append(m['list_ms']); a['total'].append(m['total_ms']); a['eui'] += m['enhanced_ui']; a['n'] += 1; a['win'].append(m['windows'])
for pid, a in sorted(per.items(), key=lambda kv: -sum(kv[1]['total'])):
    print(f"  {names.get(pid, pid):12} batches {a['n']:4} windows/batch {sum(a['win'])/a['n']:4.1f}  list p50 {pct(a['list'],50):6.1f} p95 {pct(a['list'],95):6.1f}  total p50 {pct(a['total'],50):6.1f} p90 {pct(a['total'],90):6.1f} p95 {pct(a['total'],95):6.1f} max {max(a['total']):7.1f}  eui toggled {a['eui']}")
row("\nun-hide/hold: slowest app per switch", [max((h['elapsed_ms'] for _, h in s['shown']), default=0) for s in sw])
