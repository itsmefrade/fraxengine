#!/bin/sh
# Builds every WASM module the player loads and installs it where web/ expects it.
#
#   sh scripts/build.sh            player modules (see the table in docs/BUILD.md)
#   sh scripts/build.sh --dist     then assemble the deployable site in web/dist (web/dist-tools/build.sh --skip-rust)
#
# Toolchain: rustup stable with the wasm32-unknown-unknown target, python3 (wasm stripping). No wasm-bindgen,
# no wasm-opt: every module is a raw C ABI cdylib, stripped by rust/aether-hevc/tools/strip-wasm.py.
# ~/.cargo/bin goes first in PATH so a rustup toolchain wins over a Homebrew rustc.
# AETHER_RUST_TOOLCHAIN=<dir> selects an isolated rustup install (<dir>/cargo, <dir>/rustup) instead.
# AETHER_OFFLINE=1 passes --offline to cargo (needs a warm registry cache).
set -eu
root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
dist=0
for a in "$@"; do
  case $a in
    --dist) dist=1 ;;
    -h|--help) sed -n '2,12p' "$0"; exit 0 ;;
    *) echo "unknown option $a" >&2; exit 2 ;;
  esac
done

PATH="$HOME/.cargo/bin:$PATH"; export PATH
if [ -n "${AETHER_RUST_TOOLCHAIN:-}" ]; then
  PATH="$AETHER_RUST_TOOLCHAIN/cargo/bin:$PATH"
  RUSTUP_HOME="$AETHER_RUST_TOOLCHAIN/rustup"; CARGO_HOME="$AETHER_RUST_TOOLCHAIN/cargo"
  CARGO="$AETHER_RUST_TOOLCHAIN/cargo/bin/cargo"
  export PATH RUSTUP_HOME CARGO_HOME CARGO
fi
# One cargo for every step (rust/aether-hevc/build-wasm.sh would otherwise prefer /tmp/aether-rust-toolchain).
CARGO=${CARGO:-$(command -v cargo || true)}; export CARGO
offline=""
[ "${AETHER_OFFLINE:-}" = 1 ] && offline="--offline"

need() { command -v "$1" >/dev/null 2>&1 || { echo "missing tool: $1 ($2)" >&2; exit 1; }; }
need cargo "install rustup, then: rustup target add wasm32-unknown-unknown"
need python3 "used to strip DWARF from the HEVC modules"
if command -v rustup >/dev/null 2>&1 && ! rustup target list --installed 2>/dev/null | grep -qx wasm32-unknown-unknown; then
  echo "missing rustup target: rustup target add wasm32-unknown-unknown" >&2; exit 1
fi
# 1.92.0 is the compiler the player was developed and measured with: it reproduces the shipped recon modules byte for byte.
# Others build, but generate different code (1.99.1: modules 3 to 7 % smaller, not measured for speed).
rustc_version=$(rustc --version)
echo "rustc: $rustc_version"
case $rustc_version in
  "rustc 1.92.0 "*) ;;
  *) echo "warning: reference toolchain is rustc 1.92.0, see docs/BUILD.md" >&2 ;;
esac

step() { printf '\n== %s\n' "$*"; }

# 1. Engine: demux, Dolby Vision RPU, audio codecs, VC-1, subtitles, session logic.
step "aether_wasm.wasm (rust workspace, crate aether-wasm)"
(cd "$root/rust" && cargo build -p aether-wasm --release --locked $offline --target wasm32-unknown-unknown)
mkdir -p "$root/web/wasm"
cp "$root/rust/target/wasm32-unknown-unknown/release/aether_wasm.wasm" "$root/web/wasm/aether_wasm.wasm"

# 2. Bit-exact HEVC decoder (rust/aether-hevc, its own workspace). build-wasm.sh writes rust/aether-hevc/dist/.
hevc="$root/rust/aether-hevc"
install_hevc() { mkdir -p "$root/web/exact/wasm"; cp "$hevc/dist/$1.min.wasm" "$root/web/exact/wasm/$1.wasm"; }
step "aether_hevc_simd.wasm (CPU FEL/MEL reconstruction)"
sh "$hevc/build-wasm.sh"; install_hevc aether_hevc_simd
step "aether_hevc_simd_recon.wasm (entropy-only decoder of the GPU reconstruction path)"
sh "$hevc/build-wasm.sh" --recon; install_hevc aether_hevc_simd_recon
step "aether_hevc_simd_recon_mt.wasm (same, WPP rows on worker threads, ?entropythreads=N)"
sh "$hevc/build-wasm.sh" --recon-threads; install_hevc aether_hevc_simd_recon_mt
# Default module URL of web/exact/session-mt.mjs, which the recon entropy worker imports (for its futex imports), so
# the site assembler requires it; also the threaded decoder of web/exact/mt-bench.html.
step "aether_hevc_simd_mt.wasm (one decoder on several workers)"
sh "$hevc/build-wasm.sh" --threads; install_hevc aether_hevc_simd_mt

step "installed"
(cd "$root/web" && ls -l wasm/*.wasm exact/wasm/*.wasm)

if [ "$dist" = 1 ]; then
  step "web/dist"
  sh "$root/web/dist-tools/build.sh" --skip-rust
fi
