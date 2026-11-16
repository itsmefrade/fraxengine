#!/usr/bin/env python3
"""Writes the PNGs posted by test.js ?mode=snap (web/results/*.json, test 'player-snapshot') as web/results/<label>.png.
usage: extract-snaps.py <label prefix>..."""
import json, glob, os, sys, base64
res = os.path.join(os.path.dirname(__file__), '..', '..', 'results')
seen = {}
for f in sorted(glob.glob(os.path.join(res, '*.json'))):
    try: d = json.load(open(f))
    except Exception: continue
    if isinstance(d, dict) and d.get('test') == 'player-snapshot' and any(str(d.get('label', '')).startswith(p) for p in sys.argv[1:]): seen[d['label']] = d
for label, d in sorted(seen.items()):
    out = os.path.join(res, label + '.png')
    open(out, 'wb').write(base64.b64decode(d['png'].split(',', 1)[1]))
    print(out, 'frame ts', d.get('frameTsUs'), d.get('headless'))
