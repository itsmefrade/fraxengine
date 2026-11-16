#!/usr/bin/env python3
"""Dev-time: turn an Annex B HEVC elementary stream into the pool fixtures of web/exact (`<prefix>-bl.hevc`
without NAL types >= 62, an empty `<prefix>-el.hevc`, `<prefix>-index.json` with per access unit byte ranges, IRAP
(`key`) and RASL (`rasl`) flags) plus `<prefix>-refs.json` from an FFmpeg hash list (ref-hashes.py output).
Usage: make-index.py <in.hevc> <outdir> <prefix> [<hashes.txt> <width> <height>]"""
import json, re, sys, os

src, outdir, prefix = sys.argv[1:4]
d = open(src, 'rb').read()
starts = [m.start() for m in re.finditer(b'\x00\x00\x01', d)]
# NAL i spans from the start code (including a preceding zero byte of a 4-byte code) to the next start code.
nals = []
for k, s in enumerate(starts):
    b = s - 1 if s > 0 and d[s - 1] == 0 else s
    e = (starts[k + 1] - (1 if d[starts[k + 1] - 1] == 0 else 0)) if k + 1 < len(starts) else len(d)
    h0, h1 = d[s + 3], d[s + 4]
    nals.append((b, e, (h0 >> 1) & 63, ((h0 & 1) << 5) | (h1 >> 3), d[s + 5] >> 7 if (h0 >> 1) & 63 < 32 else 0))
bl = bytearray()
aus = []
pending = None  # output offset of the first non-VCL NAL since the last VCL NAL
for b, e, t, layer, first_slice in nals:
    if t >= 62 or layer != 0:
        continue
    pos = len(bl)
    bl += d[b:e]
    if t < 32:
        if first_slice:
            start = pending if pending is not None else pos
            if aus:
                aus[-1]['bl'][1] = start - aus[-1]['bl'][0]
            aus.append({'bl': [start, 0], 'el': [0, 0], 'key': 16 <= t <= 21, 'rasl': t in (8, 9)})
        pending = None
    elif t in (36, 37, 38, 40, 41, 42, 43, 44):
        pass  # suffix-type NALs stay with the picture before
    elif pending is None:
        pending = pos
aus[-1]['bl'][1] = len(bl) - aus[-1]['bl'][0]
for a in aus:
    if not a['rasl']:
        del a['rasl']
os.makedirs(outdir, exist_ok=True)
open(f'{outdir}/{prefix}-bl.hevc', 'wb').write(bl)
open(f'{outdir}/{prefix}-el.hevc', 'wb').write(b'')
json.dump({'aus': aus}, open(f'{outdir}/{prefix}-index.json', 'w'))
print(len(aus), 'access units,', sum(a['key'] for a in aus), 'IRAP,', len(bl), 'bytes')
if len(sys.argv) > 4:
    frames = [l.split() for l in open(sys.argv[4]) if l.strip()]
    json.dump({'width': int(sys.argv[5]), 'height': int(sys.argv[6]), 'frames': frames}, open(f'{outdir}/{prefix}-refs.json', 'w'))
    print(len(frames), 'reference frames')
