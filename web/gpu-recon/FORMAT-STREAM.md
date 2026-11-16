# FORMAT-STREAM: the per-picture reconstruction command stream (G0)

Producer: `rust_h265` built with the cargo feature `recon-stream` (off by default; without it nothing of this is compiled).
`Decoder::with_recon_stream(entropy_only)` records every picture while the normal decoder runs; with `entropy_only = true`
the decoder parses and records but reconstructs nothing (no prediction, residual, loop filter or output copy: the CPU
work that remains once reconstruction runs on the GPU). `Decoder::take_recon_events()` drains the events,
`rust_h265::recon::encode_event` packs one into the `u32` layout below, `decode_event` parses it back, and
`rust_h265::recon::Replayer` rebuilds the pictures from parsed events alone with the decoder's own reconstruction
functions. Recording needs display order and one decoder thread. Code: `rust/aether-hevc/vendor/rust_h265/src/recon.rs`;
tool: `rust/aether-hevc/src/bin/reconstream` (`verify`, `dump`, `bench`).

The kernel records inside a picture are exactly the stage formats, ready to upload: TU records and coefficient entries
= FORMAT-idct.md, PU records and weight entries = FORMAT-mc.md, intra records, intra map and the `list` schedule =
FORMAT-intra.md, block map and deblocking CTB records = FORMAT-deblock.md, SAO CTB records = FORMAT-sao.md. The host
only adds each kernel's `Params` (from the picture header) and the DPB arena.

## Stream file (`stream.rcs`)

Little-endian `u32` words: `0x31534352` ("RCS1"), version `2`, then events back to back. Version 2: the block map
(section 7) carries deblocking edge codes instead of boundary strengths, flagged per picture by header flag bit 6;
readers accept version 1 files (flag clear) unchanged. Every event is
`[type, n, payload (n words)]`, so a reader can skip unknown types.

| type | event | payload |
| --- | --- | --- |
| 1 | Picture | header, section directory, sections (below). Reconstruct into DPB slot `header.slot`. |
| 2 | Generated | `picId, slot, poc, width, height, bitDepthY \| bitDepthC << 8 \| chromaFormatIdc << 16`: fill `slot` with mid-grey (`1 << (bitDepth - 1)`), a stand-in for a missing reference (spec 8.3.3.2). Only damaged streams produce it. |
| 3 | Output | `picId, slot`: display the picture in `slot` now (display order, cropped by the picture's conformance window). |
| 4 | Discard | `picId, slot`: the picture in `slot` will never be displayed (NoOutputOfPriorPicsFlag). |

**Slots.** `slot` is a DPB buffer index chosen by the recorder: a picture gets the lowest slot that holds neither a
reference picture nor a picture still waiting for its Output / Discard event. So a host may overwrite a slot as soon as
the Picture event naming it arrives: everything that referenced or displayed the old content came earlier in the stream.
Measured maxima: 7 slots (awaken), 6 (fel-p7-bl), 5 (p5), 3 (Rangers BL); the bound is `sps_max_dec_pic_buffering + 1`.
`picId` counts pictures (and generated stand-ins) in decoding order from 0.

## Picture payload

Header (24 words), then a directory of 15 `[offset, count]` pairs (offset in words from the start of the payload,
count in records of that section), then the sections.

| word | content |
| --- | --- |
| 0, 1 | picId, slot |
| 2 | POC (i32) |
| 3, 4 | width, height (luma, coded size; multiples of MinCbSizeY) |
| 5 | bitDepthY \| bitDepthC << 8 \| chroma_format_idc << 16 |
| 6 | log2Ctb \| log2MinCb << 8 \| log2MinTb << 16 |
| 7 | flags: bit 0 sample_adaptive_offset_enabled_flag, 1 strong_intra_smoothing_enabled_flag, 2 intra_smoothing_disabled_flag, 3 pic_output_flag, 4 anyNoFilter, 5 loop_filter_across_tiles_enabled_flag, 6 block map bits 0..7 are edge codes (FORMAT-deblock.md "Edge codes"; the GPU resolves them with `bs.wgsl.mjs`), 31 unsupported (see below) |
| 8, 9 | pps_cb_qp_offset, pps_cr_qp_offset (i32; the deblocking Params words 10 and 11) |
| 10..13 | conformance window left, right, top, bottom (chroma sample units) |
| 14 | number of intra waves (highest `wave` of the intra records) |
| 15, 16 | widthInCtbs, heightInCtbs |
| 17 | block map stride (`width / 8`) |
| 18 | intra map stride (`ceil(width / 4)`, the intra kernel's `imapStride`) |
| 19 | number of PUs in the small range (the first records of section 2, FORMAT-mc.md `smallCount`) |
| 20, 21 | intra schedule `stepBase`, `tuBase` (FORMAT-intra.md schedule buffer, the intra `Params` words 12 and 13) |
| 22, 23 | 0 |

Everything else a kernel's `Params` needs (strides, plane offsets of the README picture layout) follows from these.

| # | section | record | count is |
| --- | --- | --- | --- |
| 0 | TU | 6 words, FORMAT-idct.md TU record | TUs |
| 1 | coefficient entries | 1 word, FORMAT-idct.md `pos \| level << 16` | entries |
| 2 | PU | 6 words (below) | PUs |
| 3 | weight entries | 4 words (below) | entries |
| 4 | intra | 4 words, FORMAT-intra record (below) | records (one per plane) |
| 5 | PCM | 3 words: `x \| y << 16`, `log2Size`, sample offset | PCM CUs |
| 6 | PCM samples | two u16 per word (low half first) | samples |
| 7 | block map | 1 word per 8x8 luma block, FORMAT-deblock.md (edge codes in bits 0..7 when flag bit 6 is set) | blocks (`width / 8 * height / 8`) |
| 8 | deblocking CTB records | 1 word per CTB, FORMAT-deblock.md | CTBs |
| 9 | SAO CTB records | 4 words per CTB, FORMAT-sao.md | CTBs |
| 10 | CTB info (replay only) | 3 words per CTB: slice address (i32), tile-scan address, tile id \| filter_slice_edges << 31 | CTBs |
| 11 | scaling lists | 24 * 17 words, FORMAT-idct.md table (`packScalingLists` layout); absent (count 0) when scaling lists are off | words |
| 12 | intra map | 1 bit per 4x4 luma block, set for intra (bit `i & 31` of word `i >> 5`, `i = (y / 4) * stride + x / 4`); only when a block uses constrained_intra_pred_flag, else count 0 | words |
| 13 | intra schedule | the `buildSchedule(..., 'list')` buffer of FORMAT-intra.md (groups at 0, steps at `stepBase`, scheduled records at `tuBase`) | words |
| 14 | intra dispatches | 2 words per dispatch: `groupBase, groupCount` (the intra stage's per-dispatch uniform) | dispatches |

### TU (section 0) and coefficients (section 1)

Exactly FORMAT-idct.md. Word 4 (`resOffset`) is the running sum of `n * n` over the picture's TUs in record order, so
one residual buffer of `sum(n * n)` i32 holds every TU. Word 5 (reserved there) is the index of the intra *block* the
TU belongs to (counting a Cb + Cr record pair as one block), or `0xffffffff` for an inter TU, which the host adds to
the MC prediction before the intra waves (an add-residual pass over the TUs with word 5 = `0xffffffff`). TUs are in decoding order. Entries are pushed where
`decode_residual_simple` (`residual_coding.rs`) computes `pos`, before dequantisation, with the final sign.

### PU (section 2) and weight entries (section 3)

Exactly FORMAT-mc.md. Two contiguous ranges: the PUs of at most 8x8 first (header word 19 = their count, the
kernel's small range `smallFirst = 0`), then every other PU (`puFirst = smallCount`); decoding order inside each.

| word | content |
| --- | --- |
| 0 | `x \| y << 16` (luma) |
| 1 | `w \| h << 8 \| flags << 16`: flag bit 0 list 0 used, bit 1 list 1 used, bit 2 explicit weighted prediction |
| 2 | `slot0 \| slot1 << 16`: reference DPB slots (0 for an unused list) |
| 3, 4 | MV of list 0 / 1: `mvx (i16) \| mvy (i16) << 16`, quarter luma samples (0 for an unused list) |
| 5 | `wt0 \| wt1 << 16`: weight entry of each used list when weighted, else 0 |

Weight entry (4 words): `weight \| offset << 16` for Y, Cb, Cr (i16 halves; offsets already multiplied by
`1 << (BitDepth - 8)` unless high_precision_offsets_enabled_flag), then `luma_log2_weight_denom \|
chroma_log2_weight_denom << 8`. One entry per distinct weight set in the picture (deduplicated), so a list index of a
slice maps to one entry.

### Intra (section 4)

The intra kernel's record (`intra.wgsl.mjs`, `rust/gpu-recon-ref-intra`), one per plane: a luma block, or a chroma block
as a Cb record immediately followed by its Cr record (same position, size, mode, availability and wave). Decoding order.

| word | content |
| --- | --- |
| 0 | `x \| y << 16` in the plane's samples |
| 1 | `log2Size \| plane << 3 \| mode << 5 \| avail << 11 \| boundaryFiltersOff << 16 \| cip << 17`; avail bits 0 bottom-left, 1 left, 2 up-left, 3 up, 4 up-right (the decoder's `ReferenceAvailability`: picture, slice, tile and z-scan rules applied) |
| 2 | `resOffset` of the block's residual in the IDCT output (= word 4 of its TU record), `0xffffffff` when no residual is coded |
| 3 | wave (>= 1) |

`mode` is 0 planar, 1 DC, 2..34 angular; chroma mode 4 (DM) is already resolved. `boundaryFiltersOff` is
disableIntraBoundaryFilter (implicit RDPCM with bypass, RExt only). `cip` is constrained_intra_pred_flag: reference
samples in inter 4x4 luma blocks are then unavailable, read from the intra map (section 12).

**Waves and schedule.** A record's wave is 1 + the highest wave of the intra records whose samples it reads (its
availability regions clamped to the plane, 4x4 cells per plane; inter and PCM samples are wave 0), exactly
`intra.mjs` `analyze`. Sections 13 and 14 are `buildSchedule(..., 'list')` (one workgroup per CTU and dispatch, at most
`LIST_ROUNDS = 8` rounds per dispatch, steps of at most 64 records and 1536 reference samples), ported to Rust
(`recon.rs` `schedule_intra`) and computed while decoding, so the host does neither. The replay checks every picture's
schedule: each record scheduled once, and every record it reads in an earlier dispatch or in the same workgroup at an
earlier step (`check_intra_schedule`).
Order a GPU runs a picture in: boundary strengths from the edge codes (`bs.wgsl.mjs`: PU and TU records scattered into
a 4x4 cell map, then the block map resolved in place), IDCT of every TU (one dispatch), MC of every PU (small range, general range), PCM
samples, every inter TU (word 5 = `0xffffffff`) added to its prediction with clipping, the intra dispatches, deblocking
(vertical, horizontal), SAO into the next buffer. The replay runs the same order with the intra blocks by wave.

### Unsupported pictures

Header flag bit 31 marks a picture that uses something the stream does not carry: range extension tools (any non-default
SPS range extension flag or cross-component prediction), extended precision coefficients, chroma formats other than
4:2:0, SAO offsets outside i8, a reference without a slot, a reference index past its list, two referenced slots
holding the same POC (boundary strengths compare references by slot), a CTB no slice decoded (a lost slice segment: the
decoder fills it grey, the stream has no record of it). Such a picture must take the CPU path (FORMAT-idct.md's
"route to the CPU" cases are all among them). None of the four test streams has one.

## Proof of completeness (replay)

`reconstream verify <file>` decodes with the recorder, packs every event, parses it back, rebuilds every picture from
the parsed stream only (`Replayer`, GPU order above: dequantisation mirrored from `decode_residual_simple`, the
decoder's inverse transform, intra prediction with the recorded availability, motion compensation from the recorded PUs
and slots, the boundary strengths resolved from the edge codes with the PU and TU records exactly as `bs.wgsl.mjs`
does (`resolve_boundary_strengths`), the loop filter tables rebuilt from the resolved block map / CTB / SAO records,
which must repack to the same words) and compares each output frame with the decoder's. Without `--entropy-only` it
also compares the resolved strengths of every picture with the decoder's own (`Recorder::check_bs`). `--entropy-only` records with reconstruction off. Per-frame
SHA-256 against the FFmpeg references of `tools/verify-all.sh`:

| stream | frames | replay of the packed stream (decoder entropy-only) | stream per picture (avg) | slots | max intra waves |
| --- | --- | --- | --- | --- | --- |
| fel-p7-bl (dev-media) | 1454 | bit exact, 1454 / 1454 | 1.43 MB | 6 | 3686 |
| p5 | 300 | bit exact, 300 / 300 | 2.20 MB | 5 | 3468 |
| awaken | 264 | bit exact, 264 / 264 | 8.15 MB | 7 | 3568 |
| Rangers BL (10 s extract, `ffmpeg ... -t 10 -map 0:v:0 -c copy -bsf:v hevc_mp4toannexb -f hevc`) | 241 | bit exact, 241 / 241 (reference made with `tools/ref-hashes.py`) | 1.59 MB | 3 | 1462 |

The same four streams also replay bit exact with full reconstruction in the decoder (frames compared with the
decoder's too). `rust_h265`'s test `recon::tests::replay_matches_decoder_on_fixtures` replays 51 of its fixtures (PCM,
constrained intra, scaling lists, transform skip, transquant bypass, tiles, slices, dependent slices, WPP, open GOP CRA,
non CTB aligned sizes; the 4:4:4 / RExt fixtures stop at their first, unsupported, picture) frame for frame against the
decoder, and checks that an entropy-only decoder records the same events. Explicit weighted prediction: an x265
`weightp=1:weightb=1` 640x360 10-bit fade (60 pictures, 20224 weighted PUs) replays bit exact against FFmpeg.

## CPU cost: entropy decoding alone against the full decode

`reconstream bench --mode full|record|entropy|entropy-nopack` (native) and `node js/recon-bench.mjs <wasm> <file>`
(WebAssembly, SIMD build with `--features recon-stream`, one thread, Node 25), Rangers BL 10 s extract (241 pictures,
3840x2160 Main 10), best of 3, reference output in every mode (no output copy, as the player's session):

| | native (M-series) | WASM (Node) | share of full |
| --- | --- | --- | --- |
| full decode | 7.9 ms / picture (126 fps) | 10.8 ms (92 fps) | 100 % |
| full decode + recording + packing | 9.2 ms | 12.4 ms | |
| **entropy only + recording + waves + intra schedule + packing (what stays on the CPU)** | **4.5 ms** (223 fps) | **5.8 ms** (172 fps) | **56 % / 54 %** |
| entropy only, records not packed | 4.4 ms | 5.9 ms | |

Recording costs about 1.5 ms per 4K picture in WASM (full + recording minus full), of which the intra waves and
`list` schedule are about 0.7 ms and the block map / CTB packing about 0.3 ms; packing to words is under 0.1 ms. So
parsing alone is about 4.3 ms, 40 % of the full decode, in the plan's 35 to 45 % range; with the records the CPU keeps
54 %. Packed stream per picture, average: Rangers BL 1.6 MB, p5 2.2 MB, awaken 8.2 MB; 4K I pictures 4 MB (Rangers) to
16 MB (fel-p7-bl), most of it coefficient entries, TU and intra records and the intra schedule (which repeats the
intra records).

The feature costs nothing when off: `tools/verify-all.sh` stays bit exact (native and WASM) and the feature-off decoder
runs at the speed of the previous commit (p5 600 pictures 64.2 to 65.5 fps against 65.0 to 65.2, Rangers BL 115 to 118
fps against 117 to 119, alternating runs).

## Test data for the kernels (dumps)

`reconstream dump <file> <dir> --pictures N` writes the first N pictures in decoding order (from the first IRAP, so every
reference is in the dump) to `<dir>`:

- `stream.rcs`: the stream file above (magic, version, the Picture events and the Output events emitted until then).
- `picNNN.bs.u32`: the decoder's own boundary strengths of the picture as a block map (strengths in bits 0..7, the
  rest as in section 7), the expected output of `bs.wgsl.mjs` (`test/bs.html`). `--no-stages` writes the stream, the
  index and these maps only (no stage pictures).
- `index.json`: one entry per event: `word` (where the event starts in `stream.rcs`, in words), picId, slot, POC,
  size, bit depths, counts (TUs, coefficient entries, PUs, intra blocks, PCM CUs), waves, the slots its PUs reference,
  and `stages`, the file prefix of its stage pictures.
- `picNNN.{pred.u16, residual.i16, pre_deblock.u16, post_deblock.u16, post_sao.u16}` (NNN = picId): Y, Cb, Cr back to
  back, tight rows (stride = plane width), coded size (before the conformance crop), little-endian 16-bit. `pred` is the
  prediction of every block (MC, intra as predicted during reconstruction, PCM samples), `residual` the residual added to
  each block after the inverse transform (signed, 0 where none is coded), `pre_deblock` the reconstruction before the
  loop filters (= `pred + residual` clipped, the intra stage's expected output), `post_deblock` after deblocking (the
  SAO input), `post_sao` the final picture (what DPB slot `slot` holds for later MC). A 4K 10-bit stage file is 24.9 MB.

Data (a local dump directory):

| dir | source | pictures (decoding order) |
| --- | --- | --- |
| `fel-p7-bl/` | `rust/aether-hevc/dev-media/fel-p7-bl.hevc` | 3: I (POC 0), then P / B |
| `rangers-bl/` | Rangers BL 10 s extract (`rangers-bl.hevc` next to the data dir) | 3: I, P, B |
| `p5/` | `dev-media/p5.hevc` | 2: I, P |
| `awaken/` | `dev-media/awaken-raw.hevc` | 2: I, P |

Regenerate (disk is tight; 1.2 GB for the set above):

```
cd rust/aether-hevc
CARGO_HOME=/tmp/aether-rust-toolchain/cargo RUSTUP_HOME=/tmp/aether-rust-toolchain/rustup \
  /tmp/aether-rust-toolchain/cargo/bin/cargo build --offline --release --features recon-stream --bin reconstream
target/release/reconstream dump dev-media/p5.hevc <dir> --pictures 2
```
