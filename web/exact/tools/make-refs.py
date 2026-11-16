#!/usr/bin/env python3
"""Dev-time: per-frame SHA-256 of Y/U/V planes from ffmpeg (display order, yuv420p10le)."""
import hashlib, json, subprocess, sys
layer, w, h = sys.argv[1], int(sys.argv[2]), int(sys.argv[3])
src = f"web/fixtures/fel-p7-{layer}.hevc"
p = subprocess.Popen(["ffmpeg","-v","error","-i",src,"-pix_fmt","yuv420p10le","-f","rawvideo","-"],stdout=subprocess.PIPE)
ys, cs = w*h*2, (w//2)*(h//2)*2
out = []
while True:
    buf = p.stdout.read(ys+2*cs)
    if len(buf) < ys+2*cs: break
    mv = memoryview(buf)
    out.append([hashlib.sha256(mv[:ys]).hexdigest(), hashlib.sha256(mv[ys:ys+cs]).hexdigest(), hashlib.sha256(mv[ys+cs:]).hexdigest()])
json.dump({"width":w,"height":h,"frames":out}, open(f"web/exact/refs/{layer}.json","w"))
print(layer, len(out), "frames")
