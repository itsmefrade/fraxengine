#!/bin/bash
# Differential test against FFmpeg on random-payload streams.
# usage: tools/difftest.sh FIRST_SEED LAST_SEED [W H FRAMES INTERLACE PAYLOAD]
# Requires ffmpeg on PATH and a release build of the examples:
#   cargo build --release --examples
# ffmpeg runs with -cpuflags 0 so that the canonical C DSP paths are the oracle (the NEON/x86 IDCTs
# use 16 bit lanes and legitimately diverge on out-of-range coefficients that random data produces).
set -u
HERE="$(cd "$(dirname "$0")/.." && pwd)"
EX="${CARGO_TARGET_DIR:-$HERE/target}/release/examples"
OUT="${DIFF_OUT:-$HERE/fixtures/rnd}"
mkdir -p "$OUT"
first=$1; last=$2; shift 2
pass=0; fail=0; skip=0; ovf=0
for seed in $(seq "$first" "$last"); do
  f="$OUT/s$seed.vc1"
  "$EX/randgen" "$seed" "$f" "$@" 2>/dev/null || { skip=$((skip+1)); continue; }
  ffmpeg -nostdin -cpuflags 0 -v error -y -f vc1 -i "$f" -f framemd5 "$OUT/s$seed.ref" 2>"$OUT/s$seed.err"
  if grep -v -E "Failed to open codec|low score|Overriding invalid mquant|Last message repeated" "$OUT/s$seed.err" | grep -q .; then skip=$((skip+1)); continue; fi
  grep -v '^#' "$OUT/s$seed.ref" | awk '{print $NF}' > "$OUT/s$seed.refh"
  "$EX/vc1md5" "$f" 2>"$OUT/s$seed.mine.err" | awk '{h=$3; sub(/[IPB]$/,"",h); print h}' > "$OUT/s$seed.mine"
  if cmp -s "$OUT/s$seed.refh" "$OUT/s$seed.mine"; then pass=$((pass+1)); rm -f "$OUT/s$seed".{err,ref,refh,mine,vc1,mine.err}; else fail=$((fail+1)); echo "MISMATCH seed $seed ($(wc -l < "$OUT/s$seed.refh") ref frames, $(wc -l < "$OUT/s$seed.mine") mine)"; fi
done
echo "pass=$pass fail=$fail skip=$skip overflow-skipped=$ovf"
