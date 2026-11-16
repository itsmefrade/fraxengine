#!/bin/bash
# Native and WASM (Node) decode speed on a fixture stream, plus an MD5 check of the first frames.
# usage: tools/bench.sh [fixture-name]   (default lpcm71; needs tools/regen-fixtures.sh first)
set -eu
HERE="$(cd "$(dirname "$0")/.." && pwd)"
cd "$HERE"
NAME="${1:-lpcm71}"
CARGO="${CARGO:-cargo}"
"$CARGO" build --release --examples >/dev/null 2>&1
echo "== native"
"${CARGO_TARGET_DIR:-$HERE/target}/release/examples/vc1md5" "fixtures/$NAME.vc1" --time 2>&1 | tail -1
head -300 "fixtures/$NAME.md5" > "fixtures/$NAME.300.md5"
for v in "" simd; do
  W="$(tools/build-wasm.sh $v | tail -1)"
  echo "== wasm ${v:-scalar} (node)"
  node tests/wasm/run.mjs "$W" "fixtures/$NAME.vc1" --frames 300 --ref "fixtures/$NAME.300.md5"
  node tests/wasm/run.mjs "$W" "fixtures/$NAME.vc1" --frames 600
done
