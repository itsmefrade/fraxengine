#!/bin/sh
# A/B: js/ab.sh <file> <frames> <rounds> <wasmA> [<wasmB> ...]  (baseline_simd.wasm uses web/exact/session.mjs, others the session-ref API)
cd "$(dirname "$0")/.."
F=$1; N=$2; R=$3; shift 3
for r in $(seq 1 $R); do
  for w in "$@"; do
    if [ "$(basename $w)" = baseline_simd.wasm ]; then S=../../web/exact/session.mjs; else S=js/session-ref.mjs; fi
    echo "$(basename $w) $(node js/node-bench.mjs $w dev-media/$F --frames $N --session $S | grep BEST | sed 's/BEST \([0-9.]*\) fps.*/\1/')"
  done
done | sort | awk '{a[$1]=(a[$1]==""||$2>a[$1])?$2:a[$1]} END{for(k in a)print k, "best", a[k], "fps"}' | sort
