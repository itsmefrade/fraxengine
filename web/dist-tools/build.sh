#!/bin/sh
# Builds the static site in web/dist.
#   sh web/dist-tools/build.sh [--skip-rust] [--no-source] [--no-verify] [--media FILE]
# 1. sh rust/build.sh (workspace tests + aether_wasm.wasm; AETHER_RUST_TOOLCHAIN / AETHER_OFFLINE as there)
#    --skip-rust (or AETHER_SKIP_RUST=1) uses the wasm already in web/wasm.
# 2. web/exact/wasm/aether_hevc_simd.wasm is built from rust/aether-hevc when missing.
# 3. assemble.py: static import scan (fails on any unresolved import / worker URL), copy, manifest, size report.
# 4. verify-dist.py --mode smoke: loads the built site in headless Chrome and fails on any 404 or load error.
set -eu
here=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
web=$(dirname -- "$here")
root=$(dirname -- "$web")
skip_rust=${AETHER_SKIP_RUST:-0}; src=""; verify=1; media=""
while [ $# -gt 0 ]; do
  case $1 in
    --skip-rust) skip_rust=1 ;;
    --no-source) src="--no-source" ;;
    --no-verify) verify=0 ;;
    --media) shift; media=$1 ;;
    *) echo "unknown option $1" >&2; exit 2 ;;
  esac
  shift
done

# Same toolchain selection as rust/build.sh (assemble.py needs cargo metadata for the licence files).
toolchain=${AETHER_RUST_TOOLCHAIN:-}
if [ -z "$toolchain" ] && [ -d /tmp/aether-rust-toolchain ]; then toolchain=/tmp/aether-rust-toolchain; fi
if [ -n "$toolchain" ]; then
  PATH="$toolchain/cargo/bin:$PATH"; RUSTUP_HOME="$toolchain/rustup"; CARGO_HOME="$toolchain/cargo"
  export PATH RUSTUP_HOME CARGO_HOME AETHER_RUST_TOOLCHAIN="$toolchain"
fi

if [ "$skip_rust" != 1 ]; then
  sh "$root/rust/build.sh"
fi

if [ ! -f "$web/exact/wasm/aether_hevc_simd.wasm" ]; then
  echo "web/exact/wasm/aether_hevc_simd.wasm missing, building rust/aether-hevc"
  sh "$root/rust/aether-hevc/build-wasm.sh"
  mkdir -p "$web/exact/wasm"
  cp "$root/rust/aether-hevc/dist/aether_hevc_simd.min.wasm" "$web/exact/wasm/aether_hevc_simd.wasm"
fi

# shellcheck disable=SC2086
python3 -I "$here/assemble.py" $src

if [ "$verify" = 1 ]; then
  if [ -n "$media" ]; then
    python3 "$here/verify-dist.py" --mode smoke --media "$media"
  else
    python3 "$here/verify-dist.py" --mode smoke
  fi
fi
echo "done: $web/dist"
