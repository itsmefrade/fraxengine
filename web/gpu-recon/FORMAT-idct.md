# FORMAT-idct: dequantisation + inverse transform (stage `idct`)

Kernel `idct.wgsl.mjs`, driver `idct.mjs`, test `test/idct.html`, vectors from `rust/gpu-recon-ref` (`src/bin/idct.rs`).
The CPU (G0) sends **raw coefficient levels** (what CABAC decodes, after sign hiding, before dequantisation) as a compact
list per TU. The GPU dequantises, applies the transform, and writes one `n x n` i32 residual block per TU. Everything is
bit exact with `rust_h265` (`residual_coding.rs` dequant and transform skip, `inverse_transform.rs`).

Covered: 4x4 DST (intra luma), DCT 4/8/16/32, transform skip (every size; the decoder only allows 4x4 outside RExt),
transquant bypass, scaling lists (flat, default, explicit, 16x16/32x32 DC), bit depths 8 to 12 (luma and chroma may
differ, it is per TU), the 16 bit clipping after both passes (`coeffMin/Max`), QP up to 51 + 6 * (bitDepth - 8).
Not covered (the flag bits are reserved and must be 0, route such TUs to the CPU): extended_precision_processing,
transform_skip_rotation, implicit/explicit RDPCM, cross-component prediction, 4:2:2.

## Buffers (one dispatch per picture)

| binding | content | type |
| --- | --- | --- |
| 0 | params `{tuBase, tuCount, wgPerRow, 0}` | uniform, written by `IdctPipeline.dispatch` |
| 1 | DCT matrix (1024) + DST matrix (16) | uniform, owned by `IdctPipeline` |
| 2 | TU records | storage `array<u32>`, 6 words per TU |
| 3 | coefficient entries | storage `array<u32>`, 1 word per nonzero coefficient |
| 4 | scaling lists | storage `array<u32>`, 24 * 17 words, rebuilt only when the active SPS/PPS lists change |
| 5 | residual | storage `array<i32>`, read_write |

The TU order inside the buffer is free (one workgroup per TU, no inter-TU dependency). `tuBase`/`tuCount` select a
sub-range, so a host may also dispatch per bucket (e.g. only the TUs one intra wavefront needs).

## TU record (6 x u32, little endian)

| word | bits | meaning |
| --- | --- | --- |
| 0 | 0..15, 16..31 | `x`, `y`: TU origin in samples of its plane (chroma units for Cb/Cr). Ignored by this kernel, kept for the stages that add the residual to the prediction |
| 1 | 0..1 | `log2Size - 2` (4x4 .. 32x32) |
|   | 2..3 | plane: 0 Y, 1 Cb, 2 Cr (ignored here) |
|   | 4 | DST: 4x4 **intra luma** only (inter 4x4 luma uses the DCT) |
|   | 5 | transform skip (`transform_skip_flag`) |
|   | 6 | transquant bypass (`cu_transquant_bypass_flag`) |
|   | 7 | reserved, 0 (transform skip rotation) |
|   | 8..10 | `bitDepth - 8` of this TU's component (8 to 12) |
|   | 11..15 | scaling list index `sizeId * 6 + matrixId` (0..23), or 31 for flat 16 |
|   | 16..22 | `qP'` = qP + QpBdOffset of the component, i.e. the `qp + qp_bd_offset` that `compute_dequant_scale` receives (0..75) |
|   | 23..31 | reserved, 0 (RDPCM, extended precision) |
| 2 | | `coefOffset`: index of the TU's first entry in the coefficient buffer |
| 3 | | `numCoefs`: entry count (1 to n*n) |
| 4 | | `resOffset`: index (in i32 elements) of the TU's residual block in the residual buffer |
| 5 | | reserved for other stages (ignored) |

Residual blocks are row major, stride `n`, `n * n` elements, i32 holding values in the int16 range. Every element of the
block is written (zeros included), so the residual buffer needs no clearing. `resOffset` can simply be a running sum of
`n * n` over the TUs in record order.

Scaling list index: `matrixId = 3 * isInter + cIdx`, `sizeId = log2 - 2`, except a 32x32 **chroma** TU (4:4:4 only) which
uses `sizeId = 2` (this is `resolve_scaling_matrix`, `residual_coding.rs:1528`). Use 31 when `scaling_list_enabled_flag`
is 0. The kernel itself forces flat 16 for transform skip blocks larger than 4x4 (`residual_coding.rs:1276`), so G0 does not
have to special case that, and ignores the list for bypass TUs.

## Coefficient entry (1 x u32)

`entry = pos | (level & 0xffff) << 16`

* `pos` (bits 0..9) = `y * n + x`, the raster position inside the TU (`n <= 32`, so at most 1023).
* `level` (bits 16..31) = `TransCoeffLevel` as int16, final sign applied (after sign data hiding), **not** dequantised.
  Conformant streams keep it inside [-32768, 32767]; the kernel is exact over that whole range (the dequantised value
  saturates where `rust_h265` clamps it). Bits 10..15 are reserved, 0.
* Entries of one TU can come in any order (scan order is fine) and each position at most once. Positions with level 0 are not sent.

Where G0 produces them: in `decode_residual_simple` / `decode_residual_coding_generic` the place that today computes
`pos = y_c * size + x_c` and stores `coeffs[pos] = dequantised` instead pushes `pos | (level as i16 as u16) << 16`
(the `level` before `scale`/`scale_m`). The dequantisation, the transform skip shift and the 8..32 zero bounds
(`nz_cols`/`nz_rows`) are then all done on the GPU: the kernel derives the bounds itself from the entries (an
`atomicMax` over x and y, rounded up to groups of 4), so G0 does not supply them. A DC only TU (one entry, `pos = 0`,
not DST, not skip, not bypass) takes the `idct_dc` equivalent path (one dequantisation, constant fill).

Size: 24 bytes per TU + 4 bytes per nonzero coefficient. A typical 4K inter frame (40k TUs, 146k coefficients) is 1.5 MB.

## Scaling list table

24 lists, list `l = sizeId * 6 + matrixId`, 17 words each at `l * 17`: words 0..15 hold the 64 list bytes (`sl[sizeId][matrixId][j]`,
byte `j` in word `j / 4`, bits `8 * (j % 4)`; 4x4 lists use the first 16 bytes), word 16 the DC value (`sl_dc[sizeId - 2][matrixId]`
for sizeId 2 and 3, `sl[sizeId][matrixId][0]` otherwise). `packScalingLists(sl, slDc)` in `idct.mjs` builds it from the
decoder's `ScalingList` (`sl[4][6][64]`, `sl_dc[2][6]`). Matrix position for a coefficient `(x, y)`: 4x4 `(y << 2) + x`,
8x8 `(y << 3) + x`, 16x16 `((y >> 1) << 3) + (x >> 1)`, 32x32 `((y >> 2) << 3) + (x >> 2)`; DC (x = y = 0) of 16x16 and
32x32 uses word 16. When no TU uses a list (flat 16 everywhere) the buffer can stay a 1-element dummy.

## Kernel structure

One workgroup of 64 threads per TU (2D grid past 32768 TUs, `wgPerRow` in the params). Workgroup memory: `blk` and `tmp`,
1056 i32 each (row stride n + 1, no bank conflicts), plus two atomics. Phases, all barriers at top level (uniform):

1. zero `blk`; 2. dequantise and scatter the entries into `blk`, `atomicMax` x/y; 3. pass 1 (vertical, `(sum + 64) >> 7`,
clipped to int16) over columns `< nzCols`, taps `< nzRows`, written transposed into `tmp`; 4. pass 2 (horizontal,
`(sum + 2^(19 - bd)) >> (20 - bd)`, clipped) over all rows, taps `< nzCols`, into `blk`; 5. coalesced copy to the residual buffer.
DCT sizes use the even/odd split (`out[i] = e + o`, `out[n-1-i] = e - o`), the same exact integer products as
`idct_*_skip`; the DST is a direct 4 tap matrix product (algebraically the `tr_4x4_luma` butterfly). Transform skip and
bypass skip phases 3 and 4. The dequantisation never leaves 32 bits: `scale * scaleM` is split at bit 14 and a product at
or above `2^(15 + shift)` is clamped directly (the result would clamp anyway).

## Test and measurements

Regenerate vectors (the `.bin` files are about 16 MB and not meant to be committed):

```
cd rust/gpu-recon-ref
CARGO_HOME=/tmp/aether-rust-toolchain/cargo RUSTUP_HOME=/tmp/aether-rust-toolchain/rustup CARGO_TARGET_DIR=<outside the repo> \
  /tmp/aether-rust-toolchain/cargo/bin/cargo run --release --offline --bin idct -- ../../web/gpu-recon/test/vectors
```

Open `web/gpu-recon/test/idct.html?mode=verify` (or `mode=perf`, default `all`) from the dev server; the page leaves its result in
`window.__idct` (`ok`, `mismatches`, per kind/size/bit depth stats, `perf`). The generator has 6.19 M residual samples in
50 808 TUs: 30k/12k/4k/1k random TUs of 4/8/16/32 (all four kinds, bit depths 8 to 12, QP 0 to max, flat, default and random
scaling lists, sparse, low frequency box, single row or column, dense, full range and +-32767/-32768 levels, DC only) plus
one coefficient at every position with the extreme levels. The generator also asserts the butterfly kernels against the
dispatch it uses.

Notes on the reference: `inverse_transform` is a private module of `rust_h265`, so `rust/gpu-recon-ref/src/ref_inverse_transform.rs` is a
marked copy (script `regen_copy.py`); its only edits drop the SIMD branch (`tx_simd`, bit identical to the scalar
kernels by the vendor tests) and un-gate the butterfly kernels from `cfg(test)`. The dequantisation formula is copied
from `decode_residual_simple` (the i64 branch). The vendored tree is not touched.

Result (headless Chrome 157, Apple M4 Pro / Metal, `timestamp-query` GPU time, average of 20 back to back dispatches, two runs agree within 3%):

| synthetic 4K frame | TUs | coefficients | samples | GPU time |
| --- | --- | --- | --- | --- |
| light (4x4 : 8x8 : 16x16 : 32x32 = 55 : 30 : 12 : 3 %) | 15 000 | 54 k | 1.34 M | 0.77 to 0.87 ms |
| typical (same mix) | 40 000 | 146 k | 3.58 M | 0.82 ms |
| heavy | 100 000 | 365 k | 8.94 M | 1.87 ms |
| typical count, every coefficient present | 40 000 | 3.58 M | 3.58 M | 1.44 ms |
| all 4x4 | 100 000 | 197 k | 1.60 M | 1.5 to 1.7 ms |
| all 32x32, dense | 3 000 | 3.07 M | 3.07 M | 0.93 to 1.03 ms |

CPU cost per picture (create encoder, begin pass, `queue.writeBuffer` of the 16 byte params, one `dispatchWorkgroups`,
end, finish, submit) measured in the page's JS: 2 to 3 microseconds (the GPU process side of the same calls is not included).
Tiny TUs are the expensive case per sample (a 64 thread workgroup for 16 samples); packing several 4x4 TUs per workgroup
is the obvious next step if this stage ever shows up in a profile.
