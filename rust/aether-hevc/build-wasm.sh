#!/bin/sh
# Builds the SIMD128 WASM module into target/wasm32-unknown-unknown/release and copies it to
# dist/aether_hevc_simd.wasm. Pass --scalar for a build without simd128 (comparison only).
set -eu
cd "$(dirname "$0")"
# The isolated toolchain in /tmp/aether-rust-toolchain is used when present (machines whose Homebrew Rust lacks
# libLLVM); otherwise cargo comes from PATH (rustup, see docs/BUILD.md).
if [ -x /tmp/aether-rust-toolchain/cargo/bin/cargo ] && [ -z "${CARGO:-}" ]; then
  export CARGO_HOME=${CARGO_HOME:-/tmp/aether-rust-toolchain/cargo}
  export RUSTUP_HOME=${RUSTUP_HOME:-/tmp/aether-rust-toolchain/rustup}
  CARGO=/tmp/aether-rust-toolchain/cargo/bin/cargo
fi
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$PWD/target}
CARGO=${CARGO:-cargo}
OUT=aether_hevc_simd.wasm
FLAGS="-C target-feature=+simd128"
MEM="-C link-arg=--max-memory=536870912"; FEATURES=""
if [ "${1:-}" = "--scalar" ]; then FLAGS=""; OUT=aether_hevc_scalar.wasm; fi
# --threads: one decoder on several Web Workers (see vendor/rust_h265/src/thr.rs). Stable Rust links a shared-memory
# module with the prebuilt std because fat LTO recompiles std's bitcode with +atomics; std's own locks stay
# single-thread stubs, so thr.rs and the allocator lock in lib.rs replace them. Own target dir: other RUSTFLAGS.
if [ "${1:-}" = "--threads" ]; then
  OUT=aether_hevc_simd_mt.wasm; FEATURES="--features threads"; CARGO_TARGET_DIR=$PWD/target/mt
  FLAGS="$FLAGS,+atomics,+bulk-memory"
  MEM="-C link-arg=--shared-memory -C link-arg=--import-memory -C link-arg=--max-memory=1073741824 -C link-arg=--export=__stack_pointer"
fi
# --recon-threads: the entropy-only decoder on host workers as with --threads (aether_recon_session_reset_threads: WPP
# rows on several threads, pictures sequential), left in dist/. The player loads it from web/exact/wasm/ for
# ?entropythreads=N; this script does not copy it there.
if [ "${1:-}" = "--recon-threads" ]; then
  OUT=aether_hevc_simd_recon_mt.wasm; FEATURES="--features threads,recon-stream"; CARGO_TARGET_DIR=$PWD/target/recon-mt
  FLAGS="$FLAGS,+atomics,+bulk-memory"
  MEM="-C link-arg=--shared-memory -C link-arg=--import-memory -C link-arg=--max-memory=1073741824 -C link-arg=--export=__stack_pointer"
fi
# --recon: the entropy-only decoder of the GPU reconstruction path (feature recon-stream), installed for the player as
# web/exact/wasm/aether_hevc_simd_recon.wasm (stripped).
RECON=""
if [ "${1:-}" = "--recon" ]; then
  OUT=aether_hevc_simd_recon.wasm; FEATURES="--features recon-stream"; CARGO_TARGET_DIR=$PWD/target/recon; RECON=1
fi
export RUSTFLAGS="$FLAGS $MEM"
if [ -n "$RECON" ]; then
  "$CARGO" build --offline --release --lib --target wasm32-unknown-unknown $FEATURES
elif [ -n "$FEATURES" ]; then
  # cdylib only: with the rlib crate type too, std is linked as plain objects (not LTO'd), which wasm-ld rejects
  # ("--shared-memory is disallowed by std-....o because it was not compiled with 'atomics'").
  "$CARGO" rustc --offline --release --lib --target wasm32-unknown-unknown $FEATURES --crate-type cdylib
else
  "$CARGO" build --offline --release --lib --target wasm32-unknown-unknown
fi
mkdir -p dist
cp "$CARGO_TARGET_DIR/wasm32-unknown-unknown/release/aether_hevc.wasm" "dist/$OUT"
python3 tools/strip-wasm.py "dist/$OUT" "dist/${OUT%.wasm}.min.wasm"
if [ -n "$RECON" ]; then mkdir -p ../../web/exact/wasm; cp "dist/${OUT%.wasm}.min.wasm" ../../web/exact/wasm/$OUT; fi
ls -l "dist/$OUT"
