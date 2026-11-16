#!/usr/bin/env python3
"""Dev tool: exact reference planes for 8 checkpoint frames of media/<name> (decoded by FFmpeg from the Annex B layers written by prep-media.mjs).
usage: gen-gold.py <name> -> media/<name>-checkpoints.json, <name>-gold-bl.yuv, <name>-gold-el.yuv (yuv420p10le, frames concatenated in checkpoint order)"""
import json, subprocess, sys, os
name = sys.argv[1]
d = os.path.join(os.path.dirname(os.path.abspath(__file__)), '..', 'media')
n = len(json.load(open(f'{d}/{name}-index.json'))['aus'])
ks = sorted({round(i * (n - 1) / 7) for i in range(8)})
json.dump({'frames': ks}, open(f'{d}/{name}-checkpoints.json', 'w'))
sel = '+'.join(f'eq(n\\,{k})' for k in ks)
for layer in ('bl', 'el'):
    if os.path.getsize(f'{d}/{name}-{layer}.hevc') == 0: continue
    subprocess.run(['ffmpeg', '-v', 'error', '-y', '-i', f'{d}/{name}-{layer}.hevc', '-vf', f"select='{sel}'", '-fps_mode', 'passthrough', '-pix_fmt', 'yuv420p10le', '-f', 'rawvideo', f'{d}/{name}-gold-{layer}.yuv'], check=True)
    sz = os.path.getsize(f'{d}/{name}-gold-{layer}.yuv')
    print(name, layer, ks, sz)
