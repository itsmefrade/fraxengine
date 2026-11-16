# FORMAT-deblock: HEVC deblocking filter (stage `deblock`)

Kernel `deblock.wgsl.mjs`, driver `deblock.mjs`, test `test/deblock.html`, vectors from `rust/gpu-recon-ref-filters`
(its own crate, `src/deblock_ref.rs` is `rust_h265/src/deblock.rs` copied verbatim). Bit exact with `rust_h265`
(`deblock.rs`, line numbers below at commit 9515fd59; its SIMD path `deblock_simd.rs` is tested bit identical to the scalar one, which the kernel mirrors).

Covered: luma strong / normal filter decisions (lines 0 and 3 of each 4-line segment), tc / beta tables scaled by bit
depth, chroma filtering (bS 2 only, chroma QP through `chroma_qp_table` with `pps_cb_qp_offset` / `pps_cr_qp_offset`), all vertical
edges before any horizontal edge, PCM / transquant-bypass samples left unchanged per side, slice and tile boundaries
(`slice_loop_filter_across_slices_enabled_flag` of the Q side's slice, `loop_filter_across_tiles_enabled_flag`), per-slice
`slice_beta_offset_div2` / `slice_tc_offset_div2` (deblocking override), slices with deblocking disabled. Bit depths 8 to 12,
luma and chroma depths may differ. 4:2:0 only (4:4:4 and monochrome are not covered: route such streams to the CPU).
Picture width and height are multiples of 8 (they are multiples of MinCbSizeY), width at least 16.

## Boundary strengths: edge codes from the CPU, strengths on the GPU

The first version computed every boundary strength on the CPU (`cu_tree.rs` `compute_deblocking_boundary_strengths`) and
sent them in the block map. On 4K FEL that was about 5 % of the entropy-only decoder's time, mostly motion comparisons
that need the MVs and reference POCs of both sides. Since stream version 2 (FORMAT-STREAM.md, picture header flag bit 6)
the CPU only records **where** edges are and of which kind, 2 bits per 4-sample segment in the same block map bits
(below), and the kernel `bs.wgsl.mjs` derives the strengths from the PU records (motion, reference slots; FORMAT-mc.md)
and the TU records (cbf_luma; FORMAT-idct.md) the GPU already has. The block map keeps its size and layout; the stream
does not grow. Slice and tile rules, QP and the no-filter flags stay on the CPU (one linear packing pass).

A version 1 stream (flag bit 6 clear) carries strengths in bits 0..7 and skips the bS kernel.

### Edge codes (bits 0..7 of the block map, stream version 2)

The recorder writes a code for each edge segment exactly where the decoder writes a strength (`recon.rs`
`PicRecorder::deblock_edges`, called from `compute_deblocking_boundary_strengths` and `mark_intra_tu_boundaries`):

| code | written for | bS |
| --- | --- | --- |
| 0 | no edge, the picture's left / top edge, or a CU in a slice with `slice_deblocking_filter_disabled_flag` | 0 |
| 1 (transform) | top and left edge of an inter transform block, or of an inter CU without residual (skip, `rqt_root_cbf` 0) | 2 if either side is intra, else 1 if either side's luma transform block has coded residual, else motion |
| 2 (prediction) | every 8x8 grid line inside an inter transform block (or residual-free CU) of a CU with more than one PU | 2 if either side is intra (never happens), else motion |
| 3 (intra) | top and left edge of an intra transform block or a PCM CU | 2 |

"Intra" for a side means no PU record covers its 4x4 block. "Motion" is spec 8.7.2.4 as `inter_boundary_strength_full`
computes it (1 when the two sides use different reference pictures or a different number of MVs, or any MV component
differs by 4 or more quarter samples; for two bi-predicted sides with the same pair of pictures both pairings are
tried), comparing reference pictures by **DPB slot** instead of POC and MV differences in i32. The recorder marks a
picture unsupported (FORMAT-STREAM.md) when two slots it references hold the same POC or a reference index lies past
its list, so slot equality is POC equality wherever the GPU runs. A line inside one PU compares a PU with itself: bS 0.

### Kernel `bs.wgsl.mjs` (driver `bs.mjs`, one bind group, three dispatches before deblocking)

| binding | content | type |
| --- | --- | --- |
| 0 | `BsParams`: width, height, `width / 4` (cell stride), `width / 8` (map stride), PU count, TU count, `(width / 8) * (height / 8)`, noCells | uniform |
| 1 | cells: one u32 per 4x4 luma block, `width / 4` per row: PU index + 1 (0: none), bit 31 cbf_luma | storage, read_write |
| 2 | PU records (FORMAT-mc.md, stream section 2 as uploaded) | storage, read |
| 3 | TU records (FORMAT-idct.md, stream section 0) | storage, read |
| 4 | block map, edge codes replaced by strengths **in place** | storage, read_write |

The host clears the cells (`clearBuffer`, before the pass), then `bsPu` (one invocation per PU writes its index + 1
into the cells it covers), `bsTu` (one per TU record: luma TUs set bit 31 of their cells) and `bsMain` (one per block map
word) run in the reconstruction pass ahead of everything that reads the map. PUs do not overlap and neither do the
luma TUs, so no cell has two writers in a dispatch. A picture without PUs skips the clear and the two scatter
dispatches (`noCells`: every side is intra). Workgroup size 64, dispatches folded into 2D past 65535 groups.

The CPU mirror is `recon.rs` `resolve_boundary_strengths` (`bs_cells`, `edge_bs`, `motion_bs`): the replay builds the
strengths the same way before deblocking, and a full decode with `Recorder::check_bs` compares them with the decoder's
own for every picture (`reconstream verify` without `--entropy-only`, the fixture test, `reconstream dump`).

## Buffers (bind group 0, two dispatches per picture)

| binding | content | type |
| --- | --- | --- |
| 0 | `Params` (16 x u32, below) | uniform |
| 1 | picture (README layout), filtered **in place** | storage `array<u32>`, read_write |
| 2 | block map, one u32 per 8x8 luma block | storage `array<u32>`, read |
| 3 | CTB records, one u32 per CTB | storage `array<u32>`, read |

Dispatch order (`createDeblock(device).encode(pass, bindGroup, width, height)`): entry `deblockV` (every vertical luma and
chroma edge), then entry `deblockH` (every horizontal edge), both in the caller's compute pass. WebGPU makes the storage
writes of one dispatch visible to the next, which gives exactly the order of `deblock_picture` (`deblock.rs:526`):
horizontal edges read vertically filtered samples. Workgroup size 64; the driver dispatches `ceil(n / 64)` workgroups,
folded into 2D past 65535 (`groupsFor`), with `n` from `deblockThreads(width, height)`.

**No atomics.** One invocation filters one 4-line segment of one edge. A luma edge at x (multiple of 8) reads x-4..x+3 and
writes x-3..x+2, a chroma edge at xc reads xc-2..xc+1 (rows for horizontal edges), so the u32 words an invocation touches
are its own within a dispatch, and it writes whole words (unchanged halves rewritten with their input value).

## Params (16 x u32, shared with SAO)

`makeParams({width, height, bitDepthY, bitDepthC, log2Ctb, cbQpOffset, crQpOffset, anyNoFilter})` builds it.

| word | name | value |
| --- | --- | --- |
| 0, 1 | width, height | luma picture size |
| 2, 3 | strideY, strideC | plane strides in samples: `align64(width)`, `align64(width / 2)` |
| 4, 5 | offCb, offCr | plane offsets in u32 words (README) |
| 6, 7 | bitDepthY, bitDepthC | 8 to 12 |
| 8 | log2Ctb | CtbLog2SizeY (4 to 6) |
| 9 | widthInCtbs | `ceil(width / ctb)` |
| 10, 11 | cbQpOffset, crQpOffset | `pps_cb_qp_offset`, `pps_cr_qp_offset` as i32 (deblocking uses the PPS offsets only) |
| 12 | anyNoFilter | `PictureState::any_no_filter`; 0 makes both kernels ignore the no-filter bits |
| 13 | heightInCtbs | `ceil(height / ctb)` |
| 14 | mapStride | `width / 8`, block map words per row |
| 15 | reserved | 0 |

## Block map (one u32 per 8x8 luma block, row major, `width / 8` per row, `height / 8` rows)

For the block whose top-left luma sample is `(x, y)`:

| bits | meaning | source (`PictureState`) |
| --- | --- | --- |
| 0..1 | bS of the vertical edge at x, rows y..y+3 | `bs_vertical[(y >> 2) * (width >> 2) + (x >> 2)]` |
| 2..3 | bS of the vertical edge at x, rows y+4..y+7 | same at `y + 4` |
| 4..5 | bS of the horizontal edge at y, columns x..x+3 | `bs_horizontal[(y >> 2) * (width >> 2) + (x >> 2)]` |
| 6..7 | bS of the horizontal edge at y, columns x+4..x+7 | same at `x + 4` |

In a version 2 stream bits 0..7 arrive as the edge codes above, in the same positions, and the deblocking kernel reads
them after `bsMain` has turned them into strengths.
| 8 / 9 / 10 / 11 | no-filter flag of the 4x4 block at (x, y) / (x+4, y) / (x, y+4) / (x+4, y+4) | `tab_no_filter[(y >> 2) * min_pu_width + (x >> 2)] != 0` |
| 12..15 | reserved, 0 | |
| 16..23 | QpY (i8, two's complement) | `get_qp_y(x, y)` = `tab_qp_y[(y >> log2_min_cb) * min_cb_width + (x >> log2_min_cb)]` |
| 24..31 | reserved, 0 | |

The bS tables are indexed with row width `pic_width >> 2` (`deblock.rs:443` `read_bs`, and `bs_w = pic_w >> 2` where
`cu_tree.rs` writes them), **not** `y_stride / 4`. Note for G0: `recon.rs` `finish_tables` currently exports
`bs_stride = y_stride / 4` (CTB-aligned width), which differs from what the decoder uses whenever the width is not a
multiple of the CTB size (1440, 1280 with CTB 64, ...); the table itself is right, the stride is not.

The kernel only reads strengths on the 8x8 grid, skips the vertical edge at x = 0 and the horizontal edge at y = 0 (as
`luma_vertical_edges` / `luma_horizontal_edges` / `chroma_edges` do), and filters chroma only where bS is 2. Chroma edges
lie on the 16-luma grid; each 4-row (4-column) chroma segment takes bS, QP and the no-filter flags from the luma position
of its first sample, as `chroma_edges` probes them (`deblock.rs:747`, `edge_no_filter`).

## CTB record (one u32 per CTB, raster order, `widthInCtbs * heightInCtbs`)

| bits | meaning |
| --- | --- |
| 0 | skip the vertical edge on the CTB's left side: `cx > 0 && skip_vertical_slice_boundary(state, cx << log2Ctb, cy << log2Ctb)` |
| 1 | skip the horizontal edge on the CTB's top side: `cy > 0 && skip_horizontal_slice_boundary(state, cx << log2Ctb, cy << log2Ctb)` |
| 2..7 | reserved, 0 |
| 8..15 | beta_offset (i8) = `tab_deblock_offsets[rs].0` (`slice_beta_offset_div2 * 2` of the CTB's slice) |
| 16..23 | tc_offset (i8) = `tab_deblock_offsets[rs].1` |
| 24..31 | reserved, 0 |

`skip_*_slice_boundary` (`deblock.rs:462`, `487`) depend only on the two CTBs on either side of a CTB boundary (tile ids
with `filter_across_tiles`, slice addresses, `filter_slice_edges` of the Q side), so bits 0 and 1 carry them exactly. The
offsets of an edge are those of the CTB holding its Q sample, which is the CTB the kernel reads.

## Reference and test

```
cd rust/gpu-recon-ref-filters
ffmpeg -ss 5 -i ../../web/player/fixtures/zz-hevc-4k-hdr10.mp4 -frames:v 1 -pix_fmt yuv420p10le -f rawvideo /tmp/f4k.yuv
CARGO_HOME=... RUSTUP_HOME=... cargo run --release --offline -- ../../web/results/gpu-recon-filters /tmp/f4k.yuv 3840x1714
open http://127.0.0.1:8790/gpu-recon/test/deblock.html          # ?cases=<filter>&iters=<timing runs>; result in window.result
```

The generator writes 50 cases to `web/results/gpu-recon-filters` (git ignored): 48 random geometries (32x32 to 720x480,
CTB 16 / 32 / 64, MinCb 8 to 32, bit depths 8 / 10 / 12 with differing luma / chroma depths, up to 4x3 tiles and 6
slices with random across-flags and offsets, random no-filter blocks), half with independent random tables (every bS
value everywhere, also off the 8x8 grid and at x = 0 / y = 0) and half with a CU quadtree (bS on CU / TU edges, QP per
CU, coding-error steps), on random, synthetic smooth and real (crop of a 4K HDR10 frame) content; a 1920x1080 8-bit case
with 4x3 tiles, 9 slices and no-filter CUs; and a 3840x2160 Main 10 case (CTB 64, one slice) for timing.

Result (Chrome 157, Apple M-series, headless): **0 mismatches over 21,489,024 samples** in 50 cases, 3,717,760 of them
changed by the filter. Negative controls (each one deliberate one-line kernel fault, reverted) all fail: strong filter
rounding 12,328 mismatches, no-filter ignored 13,823, slice / tile skip ignored 8,298, normal filter p1 condition
ignored 285,331 (on the 1080p case alone).

GPU time, 3840x2160 Main 10: **about 0.25 ms** per picture (100 back-to-back copy + deblock passes in one submit, minus
the 25 MB copy alone: 0.45 to 0.47 ms against 0.22 ms); timestamp queries around a single pass give a 0.2 ms median
(Chrome quantises them to 65 us steps).
