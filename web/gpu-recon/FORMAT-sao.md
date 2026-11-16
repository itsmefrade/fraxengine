# FORMAT-sao: HEVC sample adaptive offset (stage `sao`)

Kernel `sao.wgsl.mjs`, driver `sao.mjs`, test `test/sao.html`, vectors from `rust/gpu-recon-ref-filters` (its own crate,
`src/sao_ref.rs` is the filter half of `rust_h265/src/sao.rs` copied verbatim). Bit exact with `rust_h265` (`sao.rs`
`sao_band_filter`, `sao_edge_filter`, `edge_offset_px`, `sao_ctb_rows_at`, `restore_no_filter`; line numbers below at
commit 9515fd59).

Covered: band offset (4 bands from `sao_band_position`, offsets after `log2_sao_offset_scale`), edge offset in all four
classes, clipping to the component's bit depth, per-sample neighbour availability across CTB boundaries (picture border,
slice boundaries with `slice_loop_filter_across_slices_enabled_flag` of the later slice in decoding order, tile boundaries
with `loop_filter_across_tiles_enabled_flag`), PCM / transquant-bypass samples keeping their deblocked value, input taken
from the fully deblocked picture. Bit depths 8 to 12, luma and chroma depths may differ. 4:2:0 only.

## Decision: availability and parameters come from the CPU

The CPU sends one 16-byte record per CTB: the three planes' SAO type, class and offsets exactly as `decode_sao_param`
leaves them in `sao_params` (merges resolved, `slice_sao_luma_flag` / `slice_sao_chroma_flag` = 0 giving type 0), plus a
9-bit availability mask of the 3x3 neighbouring CTBs that `sao_ctb_rows_at` (`sao.rs:642`) computes from
`sao_skip_slice_boundary` (`sao.rs:437`: tile ids, slice addresses, tile-scan order, `filter_slice_edges`). The GPU never
sees slice or tile ids. Packing: `rust/gpu-recon-ref-filters/src/pack.rs` `pack_sao_ctbs`.

## Buffers (bind group 0, one dispatch per picture)

| binding | content | type |
| --- | --- | --- |
| 0 | `Params`, the same 16 x u32 block as deblocking (FORMAT-deblock.md) | uniform |
| 1 | `src`: the deblocked picture (README layout) | storage `array<u32>`, read |
| 2 | `dst`: the output picture (README layout), a **different** buffer | storage `array<u32>`, read_write |
| 3 | the deblocking block map (FORMAT-deblock.md); SAO reads only bits 8..11 (no-filter) and only when `anyNoFilter` | storage `array<u32>`, read |
| 4 | SAO CTB records, 4 x u32 per CTB, raster order | storage `array<vec4<u32>>`, read |

`createSao(device).encode(pass, bindGroup, params)` dispatches entry `saoMain`, one invocation per u32 word of every plane
row (`saoThreads(params)` invocations, workgroup size 64, 2D past 65535 workgroups).

**Second buffer, no atomics.** SAO reads deblocked neighbours (edge offset reads the rows above and below), so it cannot
write in place. `dst` receives every word of every row of the three planes: samples of CTBs without SAO, no-filter
samples and the stride padding are copied from `src`, so `dst` is the finished picture and needs no initialisation, and
`src` is free afterwards (the two can alternate between DPB slots). An invocation owns one whole output word. If the
result must land in the deblocked buffer itself, copy it to a scratch buffer first (`copyBufferToBuffer`, about 0.22 ms for
4K Main 10) and run SAO from the scratch. When `sample_adaptive_offset_enabled_flag` is 0, or no CTB uses SAO
(`any_sao`), skip the dispatch and use the deblocked buffer as the picture.

## SAO CTB record (4 x u32)

Word 0:

| bits | meaning |
| --- | --- |
| 0..8 | availability: bit `dy * 3 + dx` (dx, dy in 0..2 for the CTB at offset (dx - 1, dy - 1)) set when that CTB exists and `!sao_skip_slice_boundary(state, rs, neighbour_rs)`; bit 4 (the CTB itself) is always 1 |
| 9..10 / 11..12 / 13..14 | SaoTypeIdx of Y / Cb / Cr: 0 not applied, 1 band, 2 edge (Cr repeats Cb's type) |
| 15..19 / 20..24 / 25..29 | class of Y / Cb / Cr: `sao_band_position` (0..31) for band, `SaoEoClass` (0 horizontal, 1 vertical, 2 135 degrees, 3 45 degrees) for edge (Cr repeats Cb's class) |
| 30..31 | reserved, 0 |

Words 1 / 2 / 3 (Y / Cb / Cr): four signed bytes, byte k (bits `8k..8k+7`) = `SaoOffsetVal[k + 1]` = `offset_val[c][k + 1]`,
already shifted by `log2_sao_offset_scale` (at most 31 << 2 = 124 at 12 bits, so i8 holds every legal value). For band
offset byte k belongs to band `(band_position + k) & 31`; for edge offset byte k to edge category k + 1 (category 1 local
minimum, 2 and 3 edges, 4 local maximum; the decoder's sign convention is already applied).

## Semantics the kernel mirrors

- Band: `band = sample >> (bitDepth - 5)`, `k = (band - band_position) & 31`, offset byte k when k < 4, clip.
- Edge: neighbours a = (x + dx, y + dy), b = (x - dx, y - dy) with (dx, dy) = (-1, 0), (0, -1), (-1, -1), (1, -1) for
  classes 0..3. A sample is left unchanged unless both neighbours lie inside the picture plane and the CTBs holding them
  (at most one CTB away) have their availability bit set: this is `sao_edge_filter`'s per-sample `edge_ok` (`sao.rs:330`),
  so a CTB corner sample uses the diagonal CTB's bit. `sum = sign(s - a) + sign(s - b)`; sums -2, -1, 1, 2 select
  categories 1, 2, 3, 4 (`EDGE_IDX`), 0 leaves the sample.
- Chroma CTBs are `ctb / 2` on the chroma grid and share the luma CTB's record; the last CTB row / column is clipped to
  the plane, as in `sao_ctb_rows_at`.
- No-filter (`restore_no_filter`): a sample whose luma 4x4 block (chroma: the block of luma `(2x, 2y)`) is flagged keeps
  its deblocked value. Both samples of one word always share the CTB and the 4x4 block.

## Reference and test

Same generator and cases as deblocking (FORMAT-deblock.md): every case carries the deblocked picture rust_h265 produced
and the final picture after `apply_sao_picture` (the rolling single-thread path without no-filter blocks, the snapshot +
`restore_no_filter` path with them). SAO parameters are random per CTB (30% none, 30% band, 40% edge per component, all
classes and band positions, offset magnitudes up to cMax with every legal `log2_sao_offset_scale`). `test/sao.html` checks
SAO alone (rust's deblocked picture in) and the whole GPU chain (unfiltered picture, deblock, SAO into a second buffer).

Result (Chrome 157, Apple M-series, headless): **0 mismatches over 21,489,024 samples** for SAO alone and **0 over
21,489,024** for the chain, 0 in the stride padding, in 50 cases; SAO changes 4,416,010 of those samples.

GPU time, 3840x2160 Main 10: **about 0.58 ms** per picture (100 back-to-back SAO passes in one submit; a 25 MB buffer
copy takes 0.22 ms the same way, so SAO runs at about 2.6 copies). Deblock + SAO together: about 0.8 ms per 4K picture.
A first version (one invocation per word, every neighbour sample loaded separately) measured the same 0.58 to 0.61 ms in
alternating runs, so the separate neighbour loads were not the cost; the current one fetches the CTB record and the
neighbour words once per word.
