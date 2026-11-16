#!/bin/bash
# Build the wasm32-unknown-unknown cdylib (raw C ABI, no wasm-bindgen).
#   tools/build-wasm.sh          -> target/wasm32-unknown-unknown/release/aether_vc1.wasm
#   tools/build-wasm.sh simd     -> target-simd/wasm32-unknown-unknown/release/aether_vc1.wasm (+simd128)
# Honours CARGO_HOME / RUSTUP_HOME from the environment.
set -eu
HERE="$(cd "$(dirname "$0")/.." && pwd)"
cd "$HERE"
CARGO="${CARGO:-cargo}"
if [ "${1:-}" = "simd" ]; then
  CARGO_TARGET_DIR="$HERE/target-simd" RUSTFLAGS="-C target-feature=+simd128" "$CARGO" build --release --target wasm32-unknown-unknown
  echo "$HERE/target-simd/wasm32-unknown-unknown/release/aether_vc1.wasm"
else
  CARGO_TARGET_DIR="$HERE/target" "$CARGO" build --release --target wasm32-unknown-unknown
  echo "$HERE/target/wasm32-unknown-unknown/release/aether_vc1.wasm"
fi
