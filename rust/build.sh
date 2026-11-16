#!/bin/sh
# Tests the whole workspace, then builds aether-wasm for the browser into web/wasm/.
# AETHER_RUST_TOOLCHAIN=/tmp/aether-rust-toolchain selects an isolated rustup install
# (needed where the Homebrew Rust is missing libLLVM). AETHER_OFFLINE=1 adds --offline.
set -eu
rust_root=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
project_root=$(dirname -- "$rust_root")
toolchain=${AETHER_RUST_TOOLCHAIN:-}
if [ -z "$toolchain" ] && [ -d /tmp/aether-rust-toolchain ]; then
  toolchain=/tmp/aether-rust-toolchain
fi
if [ -n "$toolchain" ]; then
  PATH="$toolchain/cargo/bin:$PATH"
  RUSTUP_HOME="$toolchain/rustup"
  CARGO_HOME="$toolchain/cargo"
  export PATH RUSTUP_HOME CARGO_HOME
fi
offline=""
[ "${AETHER_OFFLINE:-}" = 1 ] && offline="--offline"
cd "$rust_root"
cargo test --workspace --exclude mp4parse --locked $offline
cargo build -p aether-wasm --release --locked $offline --target wasm32-unknown-unknown
mkdir -p "$project_root/web/wasm"
cp target/wasm32-unknown-unknown/release/aether_wasm.wasm "$project_root/web/wasm/aether_wasm.wasm"
ls -l "$project_root/web/wasm/aether_wasm.wasm"
