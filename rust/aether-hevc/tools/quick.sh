#!/bin/sh
# Quick native bit-exactness: first N frames of each lab stream vs FFmpeg hashes. Usage: tools/quick.sh [N awaken] [N p5] [N fel]
cd "$(dirname "$0")/.."
B=${BENCH:-target/release/hevcbench}
ok=0
chk() { # file ref frames
  $B dev-media/$1 --frames $3 --hash dev-media/$2.out | tail -1
  if head -$3 dev-media/$2.ref | cmp -s - dev-media/$2.out; then echo "$2 EXACT ($3 frames)"; else echo "$2 MISMATCH"; ok=1; fi
}
chk awaken-raw.hevc awaken ${1:-80}
chk p5.hevc p5 ${2:-150}
chk fel-p7-bl.hevc felbl ${3:-250}
exit $ok
