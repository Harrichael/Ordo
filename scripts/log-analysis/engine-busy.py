#!/usr/bin/env python3
"""Rescans by trigger, and which rescans ran while a hotkey waited for the engine. Compares runs 39
and 45; edit the tuple for others.

Reads ~/Library/Application Support/Ordo/log.db, read-only. See docs/switch-speed.md.
"""
import sqlite3, os, json
def J(t):
    try: return json.loads(t)
    except Exception: return t
from collections import Counter
db = sqlite3.connect(os.path.expanduser("~/Library/Application Support/Ordo/log.db"))
def pct(v, p):
    v = sorted(v); return v[min(len(v)-1, int(p/100*len(v)))] if v else float('nan')
for R in (39, 45):
    ev = db.execute("select seq, wall_ms, kind, json_extract(payload,'$.WorldObserved.trigger') from events where run_id=? order by seq", (R,)).fetchall()
    snaps = {s: t for s, t in db.execute("select seq, total_ms from snapshots where run_id=?", (R,))}
    trig = Counter()
    for s, w, k, t in ev:
        if k == 'world_observed' and t:
            name = J(t)
            key = list(name)[0] if isinstance(name, dict) else name
            if key == 'AxHint': key = 'AxHint:' + (name['AxHint']['kind'] if isinstance(name['AxHint']['kind'], str) else name['AxHint']['kind']['Other'])
            trig[key] += 1
    dur = db.execute("select (max(wall_ms)-min(wall_ms))/60000.0 from events where run_id=?", (R,)).fetchone()[0]
    hk = sum(1 for e in ev if e[2] == 'hotkey')
    print(f"run {R}: {dur:.0f} min, {hk} hotkeys, rescans by trigger:")
    for k, n in trig.most_common(8): print(f"   {n:5}  {k}")
    # what the engine did in the 400ms before each queued hotkey
    hb = db.execute("select wall_ms, oldest_wait_ms, seq from hotkey_batches where run_id=? and oldest_wait_ms > 50", (R,)).fetchall()
    before = Counter()
    for w, wait, seq in hb:
        for s, ew, k, t in ev:
            if w - wait <= ew <= w and k == 'world_observed':
                name = J(t) if t else None
                key = list(name)[0] if isinstance(name, dict) else name
                if key == 'AxHint': key = 'AxHint:' + (name['AxHint']['kind'] if isinstance(name['AxHint']['kind'], str) else name['AxHint']['kind']['Other'])
                before[key] += 1
    print(f"   hotkeys that waited >50ms: {len(hb)}; rescans the engine ran while they waited:", dict(before.most_common(6)))
