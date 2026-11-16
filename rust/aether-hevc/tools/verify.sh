#!/bin/sh
# Native bit-exactness + speed: decodes each lab stream, compares per-frame SHA-256 with the FFmpeg oracle lists.
# Usage: tools/verify.sh [bench-binary]   (refs are made by tools/ref-hashes.py into dev-media/*.ref)
cd "$(dirname "$0")/.."
B=${1:-target/release/hevcbench}
ok=1
for spec in "awaken-raw.hevc awaken 300" "p5.hevc p5 300" "fel-p7-bl.hevc felbl 1454"; do
  set -- $spec
  $B dev-media/$1 --frames $3 --hash dev-media/$2.out | sed "s/^/$2: /"
  if cmp -s dev-media/$2.out dev-media/$2.ref; then echo "$2: BIT-EXACT ($(wc -l < dev-media/$2.out) frames)"; else echo "$2: MISMATCH"; ok=0; fi
done
# Decode plan (src/plan.rs): the p5hard segment starts at a CRA and has a second CRA 60 pictures in; with the pruned prefix and its
# output pictures dropped, the target GOP must equal the last 60 FFmpeg hashes of the full decode.
$B dev-media/p5hard.hevc --plan --hash dev-media/p5hard.out 2>/dev/null | sed "s/^/p5hard-plan: /"
if tail -60 dev-media/p5hard.ref | cmp -s - dev-media/p5hard.out; then echo "p5hard-plan: BIT-EXACT (60 frames)"; else echo "p5hard-plan: MISMATCH"; ok=0; fi
[ $ok = 1 ]
