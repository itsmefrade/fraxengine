#!/bin/sh
# Runs every test suite that works without a GPU:
#   - cargo test for the rust workspace and the stand-alone crates (aether-hevc, aether-dtsuhd, gpu-recon-ref*)
#   - node --test for web/player/test, web/engine/test, web/subs/test
#
#   sh scripts/test.sh            everything
#   sh scripts/test.sh --rust     cargo only
#   sh scripts/test.sh --js       node only (needs the WASM from scripts/build.sh; built on demand)
#
# Tests that need media skip with a message when it is absent: the generated fixtures
# (rust/aether-demux/tests/fixtures/generate*.sh, rust/aether-ac3/tests/gen_fixtures.sh, ffmpeg) and the
# local lab media (AETHER_LAB / the paths in web/engine/test/_helpers.mjs). ffmpeg/ffprobe on PATH enable
# the reference-decode comparisons.
# Not run here: browser pages (*.test.html, web/gpu-recon/test/*.html need WebGPU in Chrome).
set -eu
root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
PATH="$HOME/.cargo/bin:$PATH"; export PATH
# Same selection as scripts/build.sh.
if [ -n "${AETHER_RUST_TOOLCHAIN:-}" ]; then
  PATH="$AETHER_RUST_TOOLCHAIN/cargo/bin:$PATH"
  RUSTUP_HOME="$AETHER_RUST_TOOLCHAIN/rustup"; CARGO_HOME="$AETHER_RUST_TOOLCHAIN/cargo"
  export PATH RUSTUP_HOME CARGO_HOME
fi
rust=1; js=1
for a in "$@"; do
  case $a in
    --rust) js=0 ;;
    --js) rust=0 ;;
    -h|--help) sed -n '2,15p' "$0"; exit 0 ;;
    *) echo "unknown option $a" >&2; exit 2 ;;
  esac
done
offline=""
[ "${AETHER_OFFLINE:-}" = 1 ] && offline="--offline"
failed=""
run() {
  label=$1; shift
  printf '\n== %s\n' "$label"
  if "$@"; then echo "-- ok: $label"; else echo "-- FAILED: $label"; failed="$failed
  $label"; fi
}

if [ "$rust" = 1 ]; then
  run "cargo test rust/ (workspace)" sh -c "cd '$root/rust' && cargo test --workspace --locked $offline"
  # Own workspaces. aether-hevc's single test decodes real streams, so it runs optimised.
  run "cargo test rust/aether-hevc" sh -c "cd '$root/rust/aether-hevc' && cargo test --release --locked --offline"
  for c in aether-dtsuhd gpu-recon-ref gpu-recon-ref-filters gpu-recon-ref-intra gpu-recon-ref-mc; do
    run "cargo test rust/$c" sh -c "cd '$root/rust/$c' && cargo test --release --locked $offline"
  done
fi

if [ "$js" = 1 ]; then
  command -v node >/dev/null 2>&1 || { echo "missing tool: node (22 or newer)" >&2; exit 1; }
  if [ ! -f "$root/web/wasm/aether_wasm.wasm" ]; then
    echo "web/wasm/aether_wasm.wasm missing, running scripts/build.sh"
    sh "$root/scripts/build.sh"
  fi
  command -v ffmpeg >/dev/null 2>&1 || echo "note: no ffmpeg on PATH, the reference-decode tests skip"
  for d in web/player/test web/engine/test web/subs/test; do
    # shellcheck disable=SC2046
    run "node --test $d" sh -c "cd '$root' && node --test $d/*.test.mjs"
  done
fi

if [ -n "$failed" ]; then
  printf '\nFAILED:%s\n' "$failed"
  exit 1
fi
printf '\nall suites passed\n'
