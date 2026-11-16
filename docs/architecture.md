# Architecture

FraxEngine is plain ES modules and WebAssembly, no bundler. The work is split across workers that share memory
(`SharedArrayBuffer`), which is why the page must be cross-origin isolated.

```
 File / HTTP Range source
          |
   demux worker  ------ aether_wasm.wasm: MKV / MP4 / TS / raw demux, verified keyframes,
          |             Dolby Vision RPU split and parse, HDR10+ metadata, subtitles
          |
          +--> video worker --- WebCodecs (H.264, HEVC, AV1, VP9; hardware where Chrome has it),
          |        |            VC-1 in WebAssembly; Dolby Vision base and enhancement layers
          |        v
          |   WebGPU composition (web/fel): RPU reshaping (polynomial / MMR), FEL residual add,
          |   display mapping, PQ / HLG output on an HDR canvas
          |        ^
          |        +-- bit-exact path (web/exact, web/gpu-recon): aether_hevc_simd*.wasm decodes HEVC
          |            exactly on the CPU, or decodes only the entropy layer and reconstructs on the GPU
          |
          +--> MSE worker ----- remux to fragmented MP4 for a <video> element when Chrome can play
          |                     the stream natively (hardware decode, HDR10 signalling kept)
          |
          +--> audio worker --- aether_wasm.wasm: AC-3, E-AC-3 + JOC, TrueHD + Atmos objects,
                   |            DTS core / HD MA / HRA / DTS:X, LPCM; WebCodecs for AAC, Opus, FLAC
                   v
              AudioWorklet ring buffer: speaker layout or binaural render, the clock the video follows
```

## Routes

A session picks one video route when it opens and keeps it, unless the element or a SourceBuffer reports an error.

| Route | Used for | How the picture reaches the screen |
| --- | --- | --- |
| `mse` (element) | everything Chrome can decode natively, HDR10 and HLG included | the demuxed stream is remuxed to fragmented MP4 and appended to a `<video>` through Media Source; Chrome composites it like any video |
| `canvas` | Dolby Vision (processed), HDR10+, VC-1, interlaced H.264, streams the element refuses | WebCodecs decodes to `VideoFrame`s, WebGPU draws them on a canvas (an `rgba16float` HDR canvas where the display supports it) |

The `canvas` route costs more CPU than the `mse` route, mostly in Chrome's WebGPU presentation, not in the shaders
([measurement](../measurements/records/dolby-vision-profile-8-1-canvas-cost.md)).

## Dolby Vision

Browsers do not support Dolby Vision. The engine handles it itself:

1. **Demux.** The Rust demuxer splits the base layer (BL), the enhancement layer (EL) and the RPU. The RPU becomes a
   per-picture table for the GPU (reshaping, NLQ and display metadata).
2. **Decode.** The SPS of each layer is relabelled so Chrome's hardware decoder returns frames with only the YCbCr to
   RGB matrix applied. BL and EL each get a `VideoDecoder`; frames are imported as external textures without a copy.
3. **GPU.** Three stages in one command buffer per picture: RECOVER (back to 10-bit codes), COMPOSE (polynomial / MMR
   reshaping and the NLQ residual, integer-only WGSL ported from `web/fel/reference/fel-reference16.wgsl`), DISPLAY
   (RPU colour decode and tone mapping).

The hardware path is about 60 dB from the bit-exact result. Optional modes decode one or both layers in WebAssembly
for a bit-exact picture at a higher CPU cost. Details: [web/fel/README.md](../web/fel/README.md) and the
[display management notes](../web/fel/DM-THEORY.md).

## Repository layout

| Directory | Contents |
| --- | --- |
| `rust/` | Cargo workspace: `aether-demux`, `aether-dovi`, `aether-audio`, `aether-ac3`, `aether-joc`, `aether-vc1`, `aether-subs`, `aether-player`, `aether-core`, and `aether-wasm` (the C ABI linked into `aether_wasm.wasm`). Stand-alone: `aether-hevc` (bit-exact HEVC Main / Main10), `aether-dtsuhd` (not shipped), `gpu-recon-ref*` (CPU references for the GPU reconstruction shaders). See [rust/README.md](../rust/README.md). |
| `web/player/` | the reference player page, the session logic and the workers |
| `web/engine/` | JavaScript loaders over the `aether_wasm.wasm` ABI (demuxer, Dolby Vision, HDR10+, audio, VC-1, sources) |
| `web/fel/` | WebGPU Dolby Vision compositor and display mapping |
| `web/exact/` | worker pools for the bit-exact HEVC decoder |
| `web/gpu-recon/` | WGSL HEVC reconstruction fed by the entropy-only decoder |
| `web/subs/` | subtitle renderer (PGS, ASS / SSA, SRT, WebVTT) |
| `web/app/` | a small Jellyfin client built on the player ([jellyfin.md](jellyfin.md)) |
| `web/dist-tools/` | assembles the deployable static site `web/dist` ([deploy.md](deploy.md)) |
| `scripts/` | `build.sh`, `test.sh` |
| `docs/` | guides |
| `measurements/` | measurement records and the methodology |
