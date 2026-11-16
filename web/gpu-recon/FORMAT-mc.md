# Motion compensation (inter prediction) on the GPU: record format

Kernel `mc.wgsl.mjs`, driver `mc.mjs`, reference generator `rust/gpu-recon-ref-mc`, test page `test/mc.html`.
Bit exact with `rust/aether-hevc/vendor/rust_h265/src/inter_pred.rs` (`motion_compensation_pu`, `mc_luma`,
`mc_luma_i16`, `mc_chroma`, `mc_chroma_i16`, the copy / average shortcuts, explicit weighted uni and bi
prediction, edge clamping) and the narrowing of its SIMD kernels in `mc_simd.rs`.

## What the stage does

For every inter PU it writes the final, clipped prediction samples (Y, Cb, Cr) into the current picture's DPB
slot, exactly what `motion_compensation_pu` leaves in `PictureState`'s planes. It writes nothing else: samples
outside the PUs (intra blocks, picture padding) and the reference slots are untouched (the test checks both).

The residual is **not** added here. The decoder adds it afterwards with `clip(pred + res)` on the already clipped
prediction, so the add-residual pass (IDCT stage) reads the destination slot, adds and clips in place, and must run
after this dispatch (a later dispatch in the same compute pass is enough; WebGPU orders storage writes between
dispatches). Fusing it here would only save one read of the prediction and would tie this stage to the IDCT
stage's residual layout.

## Supported

4:2:0, BitDepthY and BitDepthC 8 to 12 (independently, as in the range extensions), PU sizes 4x8 / 8x4 up to 64x64
including AMP (widths and heights in {4, 8, 12, 16, 24, 32, 48, 64}), uni and bi prediction, default and explicit
weighted prediction (with or without `high_precision_offsets_enabled_flag`), MVs over the whole i16 range (blocks
fully outside the picture read the clamped edge). Pictures up to 65535 x 65535, up to 65536 DPB slots.

Not supported: 4:2:2 and 4:4:4 (the kernel hard-codes the 4:2:0 subsampling shift and `chroma_mv`), 4:0:0, bit
depths above 12. Streams with those fall back to the CPU path.

## Buffers and bindings (group 0)

| binding | type | contents |
|---|---|---|
| 0 | uniform | parameter block (below) |
| 1 | storage, read | PU records |
| 2 | storage, read | weight entries |
| 3 | storage, read_write | **DPB arena**: every slot back to back, slot `s` at word `s * slotWords`, each slot one picture in the README layout |
| 4 | storage, read_write | only with `separateDst`: the destination picture (binding 3 is then read only) |

Why an arena and not one buffer per picture: a PU can reference any of up to 16 pictures and WGSL cannot index
an array of buffers; Chrome on Apple exposes 10 storage buffers per stage and `maxStorageBufferBindingSize` of
4 GiB. A 4K 10-bit slot is 24.9 MB, so the device must request the adapter's `maxStorageBufferBindingSize` and
`maxBufferSize` (the default 128 MiB holds five slots). In the default mode the current picture is one of the
arena's slots (`dstBase = slot * slotWords`); the references and the destination are one binding, which keeps one
bind group per arena for the whole sequence. `separateDst: true` writes to another buffer instead (for example a
staging picture); a buffer cannot be bound both read only and writable in one dispatch, so the destination must
not be inside the arena then.

Every slot has the same geometry (the SPS). Picture layout as in `README.md`: planes Y, Cb, Cr, two 16-bit samples
per u32 (even x in the low half), `strideY = align64(width)`, `strideC = align64(width / 2)`,
`offCb = strideY * height / 2`, `offCr = offCb + strideC * (height / 2) / 2` (words). `slotWords` is at least
`offCr + strideC * (height / 2) / 2`; `pictureLayout(width, height).words` is the minimum.

Writes: a PU starts on an even x and has an even width in every plane (luma x0 and width are multiples of 4), so
the kernel writes whole u32 words of its own PU only. No atomics; PUs of one picture never overlap.

## Parameter block (80 bytes, 20 u32, written by `GpuMc.encode`)

| word | field | |
|---|---|---|
| 0, 1 | width, height | luma samples (`pic_width_in_luma_samples`, not the stride) |
| 2, 3 | strideY, strideC | samples |
| 4, 5 | offCb, offCr | words |
| 6 | slotWords | words per arena slot |
| 7 | dstBase | word offset of the destination picture (in the arena, or in binding 4) |
| 8, 9 | bitDepthY, bitDepthC | |
| 10 | flags | bit 0: u16 samples (`max(bitDepthY, bitDepthC) > 8`, `Sps::needs_u16_samples`); bit 1: scalar build narrowing (below) |
| 11 | puCount | records in the general range |
| 12 | groupsX | x size of the general dispatch grid |
| 13 | puFirst | first record of the general range |
| 14 | smallFirst | first record of the small range |
| 15 | smallCount | records in the small range |
| 16 | groupsXSmall | x size of the small dispatch grid |
| 17..19 | 0 | |

## PU record: 6 u32 (24 bytes), little endian

| word | bits | field |
|---|---|---|
| 0 | 0..15 | x0: luma x of the PU's top-left sample |
| 0 | 16..31 | y0: luma y |
| 1 | 0..7 | nPbW: luma width (4..64) |
| 1 | 8..15 | nPbH: luma height (4..64) |
| 1 | 16 | predFlagL0 |
| 1 | 17 | predFlagL1 |
| 1 | 18 | weighted: explicit weighted prediction for this PU's slice |
| 1 | 19..31 | 0 |
| 2 | 0..15 | DPB arena slot of the L0 reference (`RefPicList0[refIdxL0]`), 0 if unused |
| 2 | 16..31 | slot of the L1 reference, 0 if unused |
| 3 | 0..15 | mvL0.x as i16 (quarter luma samples, the raw `MvField.mv[0].x`) |
| 3 | 16..31 | mvL0.y as i16 |
| 4 | 0..31 | mvL1 the same way |
| 5 | 0..15 | weight entry index used with L0 (any valid index if unused) |
| 5 | 16..31 | weight entry index used with L1 |

Only inter PUs get a record (`pred_flag != 0`; an intra CU in a P/B slice gets none, as in the decoder). Chroma MVs
are derived in the kernel (`chroma_mv`, 4:2:0: the luma MV read as eighth chroma samples).

`weighted` is the value the decoder passes as `weighted_pred_flag` to `motion_compensation_pu`:
`((pps.weighted_pred_flag && P slice) || (pps.weighted_bipred_flag && B slice)) && !pred_weight_table.all_default`
(decoder.rs:2249). When it is clear the weight entries are not read.

### Record ranges

The records of a picture are two contiguous ranges of one buffer, each dispatched separately:

- **small range** (`smallFirst`, `smallCount`): only PUs with both dimensions <= 8 (8x8, 8x4, 4x8). Entry point
  `mainSmall`, four PUs per workgroup. A larger PU here would be predicted wrongly.
- **general range** (`puFirst`, `puCount`): any PU, including small ones (correct, only slower). Entry point
  `main`, two workgroups per record.

Order inside a range is free. G0 should write the two ranges directly (for example small records from the start
of the buffer, the rest from a second cursor); `partitionRecords()` in `mc.mjs` reorders a mixed list on the CPU
for producers that do not.

## Weight entry: 4 u32 (16 bytes)

One entry per (slice, list, refIdx) that weighted PUs use; the test uses `list * 16 + refIdx` for its single
slice.

| word | low i16 | high i16 |
|---|---|---|
| 0 | LumaWeightLX[refIdx] | luma offset, **scaled**: `luma_offset_lX * (high_precision ? 1 : 1 << (BitDepthY - 8))` |
| 1 | ChromaWeightLX[refIdx][0] (Cb) | Cb offset, scaled by `BitDepthC` the same way |
| 2 | ChromaWeightLX[refIdx][1] (Cr) | Cr offset, scaled |
| 3 | bits 0..7: luma_log2_weight_denom, bits 8..15: ChromaLog2WeightDenom | |

These are the `PredWeightTable` fields after the decoder's parsing (`luma_weight_l0[i]` is the full weight
`(1 << denom) + delta`, not the delta). The denominators are read from the L0 entry when L0 is used, else from
the L1 entry (they are per slice).

## Arithmetic (mirrors inter_pred.rs)

- 14-bit intermediates `predSamplesLX` as in `mc_luma_i16` / `mc_chroma_i16`: integer position `p << (14 - bd)`;
  one fractional axis `sum >> (bd - 8)`; both axes: horizontal pass `>> (bd - 8)`, vertical pass `>> 6`, then
  narrowed to i16 (below). Every reference read is clamped to `[0, width - 1] x [0, height - 1]` of its plane.
- uni, not weighted: `clip((v + (1 << (13 - bd))) >> (14 - bd))` (equals the decoder's copy for integer MVs);
- uni, weighted: `clip(((v * w + (1 << (sh - 1))) >> sh) + o)`, `sh = 14 - bd + denom`;
- bi, not weighted: `clip(sat16(sat16(a + b) + (1 << (14 - bd))) >> (15 - bd))` with the decoder's saturating
  i16 adds (equals `avg_block` for integer MVs);
- bi, weighted: `clip((a * w0 + b * w1 + ((o0 + o1 + 1) << log2Wd)) >> (log2Wd + 1))`, `log2Wd = denom + 14 - bd`.

### Narrowing

The decoder stores the result of a 2-D (both axes fractional) filter as i16. A luma half/half phase over an
extreme pattern (maximum where the filter taps are positive, 0 elsewhere) reaches about 33,250, past `i16::MAX`.
What happens then depends on the build:

- u16 samples (`flags` bit 0) with the simd128 / NEON kernels (`HAVE_SIMD`, the production WASM build and native
  aarch64): `narrow32` **saturates**, except for blocks narrower than 4 samples, which take the scalar tail and
  **wrap** (only 2-wide chroma, which cannot overflow);
- u8 samples (every plane 8-bit) or the `scalar` feature build: `(a >> 6) as i16` **wraps**.

The kernel saturates when `flags` bit 0 is set, bit 1 is clear and the plane's PU width is >= 4, and wraps
otherwise. Set bit 1 only to match a decoder built with `--features scalar`. Real video essentially never reaches
this range; the test vectors include it on purpose (one reference slot holds that pattern): the SIMD and scalar
reference builds differ on 2990 / 2968 / 2942 / 1085 luma samples of the bd10 / bd10hp / bd12 / bd8c10 vectors
(0 on bd8), and running either set with the other narrowing mismatches exactly on those samples.

## Tests

```sh
cd rust/gpu-recon-ref-mc
CARGO_HOME=/tmp/aether-rust-toolchain/cargo RUSTUP_HOME=/tmp/aether-rust-toolchain/rustup \
  /tmp/aether-rust-toolchain/cargo/bin/cargo run --release --offline -- ../../web/gpu-recon/test/data-mc
# the scalar decoder build, for the narrowing negative control:
... cargo run --release --offline --features scalar --target-dir target/scalar -- ../../web/gpu-recon/test/data-mc
node <scratch>/cdp.mjs "http://127.0.0.1:8790/gpu-recon/test/mc.html" 2 "...window.mcResult..."
```

The generator compiles the vendored `inter_pred.rs`, `mc_simd.rs`, `simd.rs` and `pixel.rs` unmodified (`#[path]`,
read only) with stubs for the decoder types they touch, and runs every CU through the real
`motion_compensation_cu` / `motion_compensation_pu`. Each vector file (`data-mc/mc-<case>.bin`, git ignored, about
18 MB) holds four random reference slots (uniform noise, a 0 / max / noise mix, the narrowing pattern), the current
slot's initial content, the records, the weight entries and the expected current slot. Cases: bd 8, 10, 10 with
high precision offsets, 12, and luma 8 / chroma 10, at 1288x728 (648x360 for the mixed case, widths not a
multiple of 64 so the stride padding holds a marker that must never be read).

Coverage per 1288x728 file: 3600 to 3900 PUs of all 24 sizes from a random coding quadtree with every partition
mode including AMP, 6% intra holes, 40% bi, 30% weighted, all 16 luma and 64 chroma phases, a third of the
predictions with a filter window past a picture edge (including MVs of -32768 and 32767), 14% of the predictions
whole-sample in luma and chroma (the copy / average shortcuts) and more whole-sample in luma only, 11% of the PUs reading the
narrowing pattern.

Result (M4 Pro, Chrome 157 headless): 10 vector files (5 cases x SIMD / scalar reference) x 3 modes (arena with
small + general ranges, arena with every record through `main`, `separateDst`) = 30 runs, **38,132,736 samples
compared (33,510,816 of them predicted), 0 mismatches**, reference slots unchanged. `?flipNarrowing=1` (negative
control) gives exactly the 59,910 expected mismatches.

## Performance

`test/mc.html?bench=1&nocheck=1`: synthetic 3840x2160 10-bit pictures (CTB quadtree, small MVs with every phase,
5% long MVs, 4 reference slots of noise), GPU timestamps, M4 Pro, Chrome 157. Back to back = 40 pictures in one
submit, first begin to last end; isolated = one submit per picture.

| picture | PUs (small range) | back to back | isolated |
|---|---|---|---|
| B, 60% bi | 5441 (355) | 1.49 ms | 1.44 ms |
| P, uni | 5492 (517) | 1.12 ms | 1.11 ms |
| B, 60% bi | 16860 (9577) | 1.96 ms | 1.97 ms |
| B, 60% bi, dense | 83444 (68108) | 4.45 ms | 4.39 ms |
| static, 64x64 integer MVs | 3299 (0) | 0.49 ms | 0.52 ms |

Without the small range (every record through `main`): 1.64 / 1.11 / 2.37 / 7.81 / 0.50 ms. Isolated wall time
(submit to `onSubmittedWorkDone`) adds about 0.3 ms. Timestamps in Chrome are quantized to about 65 us per
query, hence the back-to-back span.

The kernel is latency bound: doubling its workgroup memory (fewer resident workgroups) made it 1.7x slower, while
skipping the reference loads changed nothing and skipping the filter arithmetic saved about 40%. Ideas not tried: f32 filter arithmetic (exact
here, every sum stays below 2^24, but the README rules out floats in sample paths), 16x16 luma tiles with 2x2
blocks per lane (less workgroup memory, more window overhead).
