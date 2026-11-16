# Building FraxEngine

All browser code is plain JavaScript served as is; the only build products are WebAssembly modules, which are
git-ignored and produced by one script:

```sh
sh scripts/build.sh            # the modules the player loads
sh scripts/build.sh --dist     # then assemble the deployable site in web/dist (see DEPLOY.md)
sh scripts/test.sh             # cargo and node test suites (see below)
```

## Toolchain

| Tool | Version | Why |
| --- | --- | --- |
| Rust via rustup | **1.92.0** is the reference (CI pins it); newer stable builds too (checked: 1.99.1), with different code generation | every module, `edition = "2024"` |
| rustup target `wasm32-unknown-unknown` | same toolchain | `rustup target add wasm32-unknown-unknown` |
| python3 | standard library only (tested with 3.14) | strips DWARF from the HEVC modules, assembles `web/dist` |
| node | 22 or newer | JavaScript tests only |
| ffmpeg / ffprobe | any recent | optional: reference decodes in the tests, fixture generation |

Not needed: `wasm-bindgen`, `wasm-pack`, `wasm-opt`, Emscripten. Every module exposes a raw C ABI and is
loaded with `WebAssembly.instantiate` / `compileStreaming` by hand-written loaders.

`scripts/build.sh` puts `~/.cargo/bin` first in `PATH`, so a rustup toolchain wins over a Homebrew `rustc`
(which on some machines lacks `libLLVM`). `AETHER_RUST_TOOLCHAIN=<dir>` selects an isolated rustup install
(`<dir>/cargo`, `<dir>/rustup`); `AETHER_OFFLINE=1` passes `--offline` to cargo once the registry cache is warm.
The threaded modules use `-C target-feature=+atomics,+bulk-memory` on stable Rust (cargo prints a warning that
`atomics` is unstable; it is expected), see the comments in `rust/aether-hevc/build-wasm.sh`.

## What gets built and where it goes

| Module | Source | Installed as | Loaded by |
| --- | --- | --- | --- |
| `aether_wasm.wasm` | `rust/` workspace, crate `aether-wasm` | `web/wasm/` | `web/engine/aether-wasm.mjs` (demux, Dolby Vision RPU, audio, VC-1, subtitles, FEL tables) |
| `aether_hevc_simd.wasm` | `rust/aether-hevc` (SIMD128) | `web/exact/wasm/` | `web/player/js/exact-live.js` (bit exact CPU decode: the High and Reference qualities, the profile 5 fallback without a hardware decoder) |
| `aether_hevc_simd_recon.wasm` | `rust/aether-hevc --recon` | `web/exact/wasm/` | `web/player/js/dv-path.js` (entropy-only decoder for GPU reconstruction) |
| `aether_hevc_simd_recon_mt.wasm` | `rust/aether-hevc --recon-threads` | `web/exact/wasm/` | `web/player/js/dv-path.js` with entropy threads (3 above 40 Mbit/s on 6 or more cores, or `?entropythreads=N`) |
| `aether_hevc_simd_mt.wasm` | `rust/aether-hevc --threads` | `web/exact/wasm/` | default URL of `web/exact/session-mt.mjs` (imported by the recon entropy worker, so `web/dist` needs it), `web/exact/mt-bench.html` |

The HEVC modules are the `*.min.wasm` outputs of `rust/aether-hevc/build-wasm.sh` (DWARF stripped by
`rust/aether-hevc/tools/strip-wasm.py`, the `name` section kept). `web/exact/wasm/aether_video_simd.wasm`, an
older decoder used by a few pages under `web/exact/` for benchmarks, is not built any more
(its crate was an early prototype and is not in this repository); those benchmark pages need a copy you build yourself.

### Reproducibility against the development checkout

Fresh clone, `AETHER_RUST_TOOLCHAIN=<rustc 1.92.0> sh scripts/build.sh`, compared with the modules the player was
last run with:

| Module | Result with 1.92.0 |
| --- | --- |
| `aether_hevc_simd_recon.wasm`, `aether_hevc_simd_recon_mt.wasm` | byte-identical |
| `aether_wasm.wasm` | same exports and imports, 212 bytes larger, not byte-identical |
| `aether_hevc_simd.wasm`, `aether_hevc_simd_mt.wasm` | same exports and imports, about 3 % larger: the old files predate the latest decoder commits |

Building with 1.99.1 gives the same exports and imports except for the linker globals `__data_end` and
`__heap_base` (nothing reads them), and modules 3 to 7 % smaller. Its speed has not been measured, so use 1.92.0
for anything shipped: `rustup toolchain install 1.92.0 --target wasm32-unknown-unknown`, then
`RUSTUP_TOOLCHAIN=1.92.0 sh scripts/build.sh`. `scripts/build.sh` warns on any other version.

## Tests

See [testing.md](testing.md).
