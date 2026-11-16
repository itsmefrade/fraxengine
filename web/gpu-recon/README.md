# GPU reconstruction for the bit exact HEVC decoder

CABAC and syntax stay in the WASM decoder (`rust/aether-hevc`, vendored
`rust_h265`); reconstruction (dequant + inverse transform, motion compensation, intra prediction, deblocking, SAO) runs in
WebGPU compute shaders. Everything must be **bit exact** against `rust_h265` (which is bit exact against FFmpeg).

## Shared conventions (all kernels)

- Target content: HEVC Main 10 (8-bit Main must also work through the same code with bitDepth 8), 4:2:0, up to 3840x2160.
  `bitDepth` is a uniform. Samples are integers in [0, (1 << bitDepth) - 1].
- **Picture buffer**: one `array<u32>` storage buffer per picture, three planes back to back (Y, then Cb, then Cr), each plane
  row major with **two 16-bit samples per u32** (sample at even x in the low half). Plane strides in samples are multiples of
  64 (`strideY = align64(width)`, `strideC = align64(width / 2)`). Offsets in u32 words: `offY = 0`,
  `offCb = strideY * height / 2`, `offCr = offCb + strideC * (height / 2) / 2`. There is no shared helper module; each kernel
  reads and writes the words itself (`intra.wgsl.mjs` has its own `loadSample`).
  Writes of a single 16-bit half need care: a workgroup must own whole u32 words, or use `atomicAnd`/`atomicOr` on an
  `array<atomic<u32>>` view; document which you use.
- **Residual / intermediate buffers**: `array<i32>` unless a kernel documents otherwise.
- **Command records**: each kernel defines the per-frame list it consumes (TUs, PUs, edges, CTB SAO params) as a packed
  `array<u32>` and documents the exact layout in its own `FORMAT-<kernel>.md` in this folder. Keep records fixed size and
  aligned to 4 bytes; the CPU side (G0) will produce exactly that layout.
- WGSL: integer arithmetic only (`i32`/`u32`), no floats in any sample path. Mirror `rust_h265`'s clipping and rounding
  exactly; cite the source function (file:line) each kernel mirrors.
- One kernel file per stage, exported as an ES module string: `idct.wgsl.mjs`, `mc.wgsl.mjs`, `intra.wgsl.mjs`,
  `bs.wgsl.mjs` (deblocking boundary strengths from edge codes), `deblock.wgsl.mjs`, `sao.wgsl.mjs`, plus a small JS
  driver per stage (`*.mjs`) that creates pipelines and dispatches.
- Tests: each stage has a reference test-vector generator under `rust/gpu-recon-ref/` (its own crate per stage or one shared
  crate with a binary per stage, depending on `rust/aether-hevc/vendor/rust_h265` read-only by path, or copying the reference
  function verbatim when it is private) that writes inputs and expected outputs to files, and a headless Chrome page under
  `web/gpu-recon/test/` that runs the WGSL and compares bit for bit. Report match counts.
- Never edit `rust/aether-hevc/vendor/rust_h265` from a kernel task: that tree belongs to G0.
