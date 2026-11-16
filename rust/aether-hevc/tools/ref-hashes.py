#!/usr/bin/env python3
"""Dev-time oracle: per-frame SHA-256 of Y/U/V (yuv420p10le, display order) decoded by FFmpeg.
Usage: ref-hashes.py <file.hevc> <width> <height> <frames> <out.txt>. Output line: "<Y> <U> <V>" like hevcbench --hash."""
import hashlib, subprocess, sys
src, w, h, n, out = sys.argv[1], int(sys.argv[2]), int(sys.argv[3]), int(sys.argv[4]), sys.argv[5]
p = subprocess.Popen(["ffmpeg", "-v", "error", "-i", src, "-frames:v", str(n), "-pix_fmt", "yuv420p10le", "-f", "rawvideo", "-"], stdout=subprocess.PIPE)
ys, cs = w * h * 2, (w // 2) * (h // 2) * 2
cnt = 0
with open(out, "w") as o:
    while True:
        buf = p.stdout.read(ys + 2 * cs)
        if len(buf) < ys + 2 * cs:
            break
        mv = memoryview(buf)
        o.write(" ".join(hashlib.sha256(x).hexdigest() for x in (mv[:ys], mv[ys:ys + cs], mv[ys + cs:])) + "\n")
        cnt += 1
print(src, cnt, "frames")
