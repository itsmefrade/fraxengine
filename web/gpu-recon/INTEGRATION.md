# G4: wiring GPU reconstruction into the player

Owner: coordinator. Inputs: the stage kernels (`FORMAT-*.md`) and G0's command stream (`FORMAT-STREAM.md`).

## Shape

```
demux worker ──BL packets──> entropy worker (WASM, rust_h265 with recon-stream, one per layer)
             └─EL packets──> entropy worker (EL)
entropy worker ──per picture: command stream (transferable ArrayBuffer)──> video worker
video worker (owns the WebGPU device, already used by FelCompositor):
  GpuRecon.decodePicture(stream):
    upload stream (one writeBuffer, or a mapped staging slot as in fel-compositor #stagePlanes)
    MC (inter PUs, reads DPB slots) -> prediction in the target slot
    IDCT -> residual buffer
    inter add-residual + clip
    intra waves (prediction + residual per TU, in dependency order)
    deblock (vertical, then horizontal) -> SAO -> final picture in its DPB slot
  output order: the entropy worker reports output (bumping) events; the video worker hands DPB slots
  to FelCompositor in display order, BL and EL paired by pts as today.
```

- One command encoder and one submit per picture (WebGPU command cost in the GPU process was measured: bind group creation
  per frame cost about 4 points; reuse bind groups per DPB slot).
- No GOP pool and no open-GOP prefix: entropy decoding is sequential per layer and cheap (G0 measures it). Seeks reset the
  entropy worker at a random access point exactly as the single-thread session does today.
- DPB on the GPU: one arena storage buffer per layer with `max_dec_pic_buffering + 1` slots back to back (slot `s` at
  `s * slotWords`; WGSL cannot index an array of buffers and a stage gets at most 10 storage buffers). The current picture is a
  slot in the same arena (MC `dstBase`). A 4K 10-bit slot is 24.9 MB: request the adapter's `maxStorageBufferBindingSize` and
  `maxBufferSize`.
- FelCompositor input: today it reads 16-bit codes from `blBuf`/`elBuf` (planes packed two per u32, stride = width). Either
  the compositor gets a stride-aware read (preferred, zero copy straight from the DPB slot) or one copy pass per picture.

## Verification

- End to end: GPU pictures hashed (readback, test mode only) against the CPU decoder per frame on fel-p7-bl, p5, awaken, the
  Rangers BL and EL, FEL_TEST_V6; FEL snapshots against today's exact path at equal timestamps.
- Fallback: any GPU stage error or a stream feature the kernels do not cover (documented per FORMAT file) falls back to the
  existing exact pool for the session, never to a non exact picture.

## Measurement

CPU (renderer + GPU process) of Rangers FEL and FEL_TEST_V6 against today's exact path (~0.95 core), GPU time per picture,
memory (DPB on GPU instead of 2 to 3 WASM instances).
