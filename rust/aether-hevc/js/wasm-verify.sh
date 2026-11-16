#!/bin/sh
# WASM bit-exactness + single-thread speed through Node. Usage: js/wasm-verify.sh <wasm> [session.mjs]
cd "$(dirname "$0")/.."
W=${1:-dist/aether_hevc_simd.wasm}; S=${2:-}
ok=1
for spec in "awaken-raw.hevc awaken 264" "p5.hevc p5 300" "fel-p7-bl.hevc felbl 1454"; do
  set -- $spec
  if [ -n "$S" ]; then node js/node-bench.mjs "$W" dev-media/$1 --frames $3 --hash dev-media/$2.wout --session "$S" | sed "s/^/$2: /"; else node js/node-bench.mjs "$W" dev-media/$1 --frames $3 --hash dev-media/$2.wout | sed "s/^/$2: /"; fi
  if cmp -s dev-media/$2.wout dev-media/$2.ref; then echo "$2: BIT-EXACT"; else echo "$2: MISMATCH"; ok=0; fi
done
[ $ok = 1 ]
