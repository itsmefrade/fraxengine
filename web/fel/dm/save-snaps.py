#!/usr/bin/env python3
"""Moves the PNG snapshots posted by dm-verify.mjs ('dm-snap' records in web/results/*.json) to web/results/<file>.png and deletes those records.
Usage: python3 -I web/fel/dm/save-snaps.py"""
import base64, json, pathlib
results = pathlib.Path(__file__).resolve().parents[2] / 'results'
n = 0
for p in sorted(results.glob('*.json')):
    try:
        d = json.loads(p.read_text())
    except Exception:
        continue
    if isinstance(d, dict) and d.get('test') == 'dm-snap' and 'png' in d:
        (results / d['file']).write_bytes(base64.b64decode(d['png'].split(',', 1)[1]))
        p.unlink(); n += 1
print(f'wrote {n} PNGs to {results}')
