#!/bin/sh
# Full bit-exactness check against FFmpeg per-frame SHA-256 (Y/U/V): native (all frames of the three lab streams) and
# WebAssembly through Node (same streams). Needs dev-media/*.hevc and the *.ref lists from tools/ref-hashes.py.
cd "$(dirname "$0")/.."
export CARGO_HOME=${CARGO_HOME:-/tmp/aether-rust-toolchain/cargo} RUSTUP_HOME=${RUSTUP_HOME:-/tmp/aether-rust-toolchain/rustup}
CARGO=${CARGO:-/tmp/aether-rust-toolchain/cargo/bin/cargo}
"$CARGO" build --release --offline --bin hevcbench >/dev/null 2>&1 || exit 1
echo "--- native"; tools/verify.sh || exit 1
./build-wasm.sh >/dev/null 2>&1 || exit 1
echo "--- wasm (Node)"; js/wasm-verify.sh dist/aether_hevc_simd.wasm js/session-ref.mjs | grep -v "run 0"
