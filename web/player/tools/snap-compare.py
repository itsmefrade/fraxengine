#!/usr/bin/env python3
"""Extracts player snapshots (web/results/*.json posted by test.js ?mode=snap) as PNG files and compares VC-1 ones against FFmpeg.
usage: snap-compare.py <media file> <label prefix> [outdir=web/results] [--fps 24000/1001]
Reference 1 (the acceptance one): ffmpeg decodes the frame the snapshot shows (index from frameTsUs), converts BT.709 limited -> full range rgb24
(scale=in_color_matrix=bt709:out_color_matrix=bt709, default swscale chroma filter). Neighbouring frames are compared too, to prove the alignment.
Reference 2 ("chromaMatchedReference"): the same with bilinear chroma and the siting the stream declares (in_h_chr_pos=0, in_v_chr_pos=128): the default scaler
interpolates 4:2:0 chroma as if it were centred and with a bicubic filter, which is most of the difference to reference 1.
"""
import json, glob, os, subprocess, sys, base64, tempfile
import numpy as np
from PIL import Image

media, prefix = sys.argv[1], sys.argv[2]
outdir = sys.argv[3] if len(sys.argv) > 3 and not sys.argv[3].startswith('--') else os.path.join(os.path.dirname(__file__), '..', '..', 'results')
fps = 24000 / 1001
res = []
# display order frame times of the file (the nominal frame rate drifts against the container's millisecond timestamps). FFmpeg's best effort
# times of this VC-1 start two frames late (0.083 s); the player shows the first picture at the first container timestamp, so the list is rebased.
pts_list = [float(x) for x in subprocess.run(['ffprobe', '-v', 'error', '-select_streams', 'v:0', '-show_entries', 'frame=best_effort_timestamp_time', '-of', 'csv=p=0', media], capture_output=True, text=True, check=True).stdout.split() if x not in ('N/A', '')]
files = sorted(glob.glob(os.path.join(os.path.dirname(__file__), '..', '..', 'results', '*.json')))
snaps = {}
for f in files:
    try: d = json.load(open(f))
    except Exception: continue
    if isinstance(d, dict) and d.get('test') == 'player-snapshot' and str(d.get('label', '')).startswith(prefix): snaps[d['label']] = d  # newest wins
for label, d in sorted(snaps.items()):
    png = os.path.join(outdir, f'{label}.png')
    open(png, 'wb').write(base64.b64decode(d['png'].split(',', 1)[1]))
    idx0 = min(range(len(pts_list)), key=lambda i: abs(pts_list[i] - pts_list[0] - d['frameTsUs'] / 1e6))
    a = np.asarray(Image.open(png).convert('RGB')).astype(np.int32)
    def ref_png(i, flt):
        ref = tempfile.mktemp(suffix='.png')
        subprocess.run(['ffmpeg', '-v', 'error', '-y', '-i', media, '-vf', f'select=eq(n\\,{i}),{flt},format=rgb24', '-frames:v', '1', '-fps_mode', 'passthrough', ref], check=True)
        b = np.asarray(Image.open(ref).convert('RGB')).astype(np.int32); os.remove(ref)
        return b
    def stat(b):
        e = a - b; mse = float(np.square(e).mean())   # np.square, not e * e: numpy 2.2 on Python 3.14 squares `e` in place for e * e
        return {'psnrDb': round(10 * np.log10(255 * 255 / mse), 2) if mse else 'inf', 'maxErr': int(np.abs(e).max()), 'meanAbsErr': round(float(np.abs(e).mean()), 3), 'pctWithin1': round(float((np.abs(e).max(axis=2) <= 1).mean() * 100), 2), 'pctWithin2': round(float((np.abs(e).max(axis=2) <= 2).mean() * 100), 2)}
    DEFAULT = 'scale=in_color_matrix=bt709:in_range=limited:out_color_matrix=bt709:out_range=full'
    MATCHED = DEFAULT + ':in_h_chr_pos=0:in_v_chr_pos=128:flags=bilinear+accurate_rnd+full_chroma_int'
    # which picture does the snapshot show? (the time list of FFmpeg is approximate) the one the chroma matched reference fits best
    fits = {off: stat(ref_png(idx0 + off, MATCHED)) for off in (-2, -1, 0, 1, 2)}
    best = max(fits, key=lambda o: fits[o]['psnrDb'] if fits[o]['psnrDb'] != 'inf' else 999)
    idx = idx0 + best
    row = {'label': label, 'png': os.path.relpath(png), 'frameTsUs': d['frameTsUs'], 'frameIndex': idx, 'indexFromTimeList': idx0, 'headless': d.get('headless'), 'size': list(a.shape[1::-1])}
    for off in (-1, 0, 1):
        row[f'offset{off:+d}'] = stat(ref_png(idx + off, DEFAULT))
    row['chromaMatchedReference'] = fits[best]
    res.append(row)
print(json.dumps(res, indent=1))
json.dump(res, open(os.path.join(outdir, f'{prefix}-compare.json'), 'w'), indent=1)
