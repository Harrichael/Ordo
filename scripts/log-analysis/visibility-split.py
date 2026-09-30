#!/usr/bin/env python3
"""The un-hide step's time, split by whether the switch really un-hid an app. Compares runs 39 and 45;
edit the tuple for others. Runs before the app queues only: later runs have no `visibility_ms` and
no hold stats on AppShown rows; app-chains.py reports their un-hides.

Reads ~/Library/Application Support/Ordo/log.db, read-only. See docs/switch-speed.md.
"""
import sqlite3, os, json
db = sqlite3.connect(os.path.expanduser("~/Library/Application Support/Ordo/log.db"))
def pct(v, p):
    v = sorted(v); return v[min(len(v)-1, int(p/100*len(v)))] if v else float('nan')
for R in (39, 45):
    rows = db.execute("select kind, payload from park_trace where run_id=? order by rowid", (R,)).fetchall()
    sw = []; cur = None
    for kind, p in rows:
        t = json.loads(p)
        if kind in ('Switch', 'View'):
            cur = dict(cost=t.get('cost'), unhid=0, asked=0, shown=0); sw.append(cur)
        elif cur and kind == 'AppShown':
            cur['shown'] += 1
            h = t.get('hold')
            if h: cur['asked'] += 1
            if h and h.get('unhid'): cur['unhid'] += 1
    sw = [s for s in sw if s['cost']]
    for label, sel in (("no real un-hide", lambda s: s['unhid'] == 0), ("with a real un-hide", lambda s: s['unhid'] > 0)):
        v = [s['cost']['visibility_ms'] for s in sw if sel(s)]
        a = [s['asked'] for s in sw if sel(s)]
        print(f"run {R} {label:20} n {len(v):3}  visibility p50 {pct(v,50):6.1f} p90 {pct(v,90):6.1f} mean {sum(v)/max(len(v),1):6.1f}   apps asked per switch mean {sum(a)/max(len(a),1):.1f}")
