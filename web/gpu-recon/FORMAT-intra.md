# FORMAT-intra: HEVC intra prediction on the GPU

Kernel `intra.wgsl.mjs`, driver `intra.mjs`, reference `rust/gpu-recon-ref-intra`, test `test/intra.html`.

The stage predicts every intra TU of a picture and adds its residual with clipping, in place in the picture buffer
(README layout), bit exact with `rust_h265`. Mirrored code (`rust/aether-hevc/vendor/rust_h265/src`):
`intra_pred.rs` `build_reference_samples` (fast and general / constrained intra paths), `filter_reference_samples`
([1,2,1] and strong 32x32 smoothing), `predict_planar`, `predict_dc` (luma edge filter), `predict_angular` (mode 10 / 26
luma boundary filters), `add_residual`; `cu_tree.rs` `predict_intra_luma` / `predict_intra_chroma` (which filters run,
the implicit RDPCM edge filter switch) and `cip_masks`. The SIMD kernels of `intra_simd.rs` equal the scalar code (their
own tests), so the scalar code is the reference.

Covered: 4:2:0, 8, 10 and 12 bit (tested), TU 4..32, all 35 modes, picture / slice / tile / z-scan availability
(as bits, see below), constrained intra prediction, strong intra smoothing, `intra_smoothing_disabled_flag`,
disableIntraBoundaryFilter (implicit RDPCM with transquant bypass). Not covered: 4:4:4 / 4:2:2 (chroma filtering and the
4:2:2 mode table), which G0 marks unsupported anyway.

## Order in a picture

Before this stage the picture holds every sample that does not depend on intra prediction: motion compensation plus
residual of inter blocks, and PCM samples. Intra TUs are then predicted in dependency order (waves, below), each TU's
residual added right after its prediction because later TUs read the result. Deblocking and SAO run after.

## Buffers

| binding | buffer | content |
| --- | --- | --- |
| 0 | uniform `Params` (64 bytes) | picture size and layout, bit depths, flags, table offsets (`IntraStage.prepare` fills it) |
| 1 | storage `array<u32>`, read_write | the picture (README layout: Y, Cb, Cr, two 16-bit samples per word) |
| 2 | storage `array<u32>`, read | schedule (groups, steps, scheduled TU records; built by `buildSchedule`) |
| 3 | storage `array<i32>`, read | residual: per TU `size * size` i32 row major at `resOffset` (FORMAT-idct.md layout) |
| 4 | storage `array<u32>`, read | intra map for constrained intra pred: bit `i = (y / 4) * (width / 4) + x / 4` (luma 4x4 block) at word `i >> 5`, bit `i & 31`, 1 = intra (rust_h265 `pred_flag == 0`; PCM blocks are intra). Only read for TUs with the cip flag; bind any small buffer otherwise |
| 5 | uniform, dynamic offset | per dispatch `groupBase, groupCount` (256-byte slots) |

Writes: each invocation stores one whole u32 (two horizontally adjacent samples of one TU). TUs start at x multiples
of 4 in every plane and are at least 4 wide, so a TU owns whole words: no atomics, no read-modify-write.

## TU record (input, 4 x u32, decoding order)

| word | bits | content |
| --- | --- | --- |
| 0 | 0..15, 16..31 | `x`, `y`: TU origin in samples of its plane (chroma units for Cb / Cr) |
| 1 | 0..2 | `log2Size` 2..5 in samples of its plane |
|   | 3..4 | plane: 0 Y, 1 Cb, 2 Cr (Cb and Cr are separate records with the same mode and availability) |
|   | 5..10 | final intra mode 0..34 (chroma: table 8-3 with the mode 34 substitution, DM = luma mode of PU 0 for 4:2:0) |
|   | 11..15 | availability: bit 11 bottom_left, 12 left, 13 up_left, 14 up, 15 up_right, exactly rust_h265 `ReferenceAvailability` from `compute_luma_avail_inner(x_luma, y_luma, size_luma)` (chroma: the luma position and twice the size) |
|   | 16 | edge filters off: `implicit_rdpcm_enabled_flag && cu_transquant_bypass_flag` (no DC edge filter, no mode 10 / 26 boundary filter) |
|   | 17 | constrained_intra_pred_flag (of the slice's PPS) |
| 2 | | `resOffset` into the residual buffer, `0xffffffff` = no residual (cbf 0) |
| 3 | | wave (>= 1), optional: `analyze()` computes it; a producer that has it (G0) may write it and the test checks both agree |

Order: decoding order, i.e. a luma TU, then its Cb and Cr TUs (the 4:2:0 4x4 chroma TU of an 8x8 luma split comes
after the fourth luma 4x4 and sits at the parent position). The picture-level inputs are `bitDepthY`, `bitDepthC`,
`strong_intra_smoothing_enabled_flag`, `intra_smoothing_disabled_flag` (`IntraStage.prepare(pic, ...)`).

`fromStreamIntra(intraSection, tuSection)` converts G0's stream (FORMAT-STREAM.md section 4, 6-word records, one
record for Cb + Cr, different availability bit order, residual by TU index) into these records. It is under test: the
`stream-*` cases feed the stream sections unchanged.

## Dependencies and schedules

A TU reads the reconstructed samples of the regions its availability bits allow (left column and its bottom-left
extension, the corner, the above row and its above-right extension, clamped to the picture). Every 4x4 block of those
regions that an earlier intra TU wrote is a dependency (inter / PCM samples are wave 0). `analyze()` does this in one
pass in decoding order with a 4x4 grid per plane: global wave = 1 + max wave read. It equals G0's recorded wave on
every TU of the stream cases (0 mismatches).

The kernel runs one workgroup per (dispatch, group); a group is a list of steps, a step a set of independent TUs.
WebGPU orders storage writes between dispatches of one pass; inside a workgroup the steps are separated by barriers.
`buildSchedule(tus, w, h, log2Ctb, mode)`:

- `list` (default): one workgroup per CTU and dispatch. Each TU gets the earliest slot (dispatch, round): after the TUs
  it reads, in a later round of the same dispatch when they are in its CTU, in a later dispatch otherwise, at most
  `LIST_ROUNDS = 8` rounds per dispatch. Rounds are steps.
- `ctu`: CTU wavefront. CTU level = 1 + max level of the other CTUs it reads (one dispatch per level, about
  `widthInCtbs + 2 * heightInCtbs`), steps = local waves inside the CTU. Fewest dispatches, but a CTU waits for the whole
  of every CTU it reads, which doubles the serial steps against the global wave count.
- `wave`: one dispatch per global wave, TUs packed into independent workgroups. Fewest serial steps, one dispatch each.

Rounds per dispatch for `list` (synthetic 4K I frame g-4k-i, GPU ms): 4: 16.4 (1134 dispatches), 8: 14.5 (607),
16: 15.3 (346), 32: 16.1 (219); `ctu` 19.6 (126), `wave` 52.7 (4352). Persistent kernels with cross-workgroup ready
flags would reach the global wave count with one dispatch, but WebGPU gives no forward progress guarantee between
workgroups (a stalled producer hangs the device), so they were not used.

Schedule buffer (binding 2, `Uint32Array` from `buildSchedule`): groups at word 0 (3 words: first step, step count,
first scheduled TU), steps at `stepBase` (4 words: first TU, TU count, reference samples, sample pairs), scheduled TU
records at `tuBase` (words 0..2 of the input record, word 3 = `refOffset | pairOffset << 16` inside the step). A step
holds at most 64 TUs and 1536 reference samples (`4 * size + 1` per TU); workgroup memory is about 9.3 KB (default
16 KB limit). The CPU side (JS here; G0 can produce the slots while decoding) is O(TU perimeter / 4).

## Measurements (Apple M-series GPU, headless Chrome, timestamp queries around the pass, median of 5)

4K wave and step counts of real streams (TUs logged from an instrumented copy of rust_h265, first 60 pictures each):

| stream (CTB) | I frame intra TUs | I global waves | `list` dispatches / serial steps | P/B frames: waves median (max) |
| --- | --- | --- | --- | --- |
| HDR10 3840x1720 (64) | 352035 | 3998 | 560 / 4475 | 0 in B, up to 1069 in P |
| p5 DV (32) | 81192 | 2497 to 2761 | 475 / 3617 | 25 (389) |
| awaken MEL (64) | 176298 to 216393 | 3225 to 3546 | 467 / 3729 | 129 (1286) |
| fel-p7-bl (64) | 265101 to 282786 | 3036 to 3115 | 443 / 3542 | 0 (no intra in P/B) |
| jellyfin 40 Mbit/s (32) | 74886 to 83265 | 2410 to 2697 | | 23 (107) |

GPU time of the stage (real geometry; the `real-*` cases use the logged TUs with random residuals, `stream-*` the real
residuals end to end):

| picture | TUs | `list` | `ctu` | `wave` |
| --- | --- | --- | --- | --- |
| HDR10 I (3840x1720) | 352035 | 15.3 ms | 20.3 ms | 54.2 ms |
| fel-p7-bl I | 265101 | 11.9 to 12.5 ms | 13.7 to 14.5 ms | 39.2 ms |
| awaken I | 176298 | 11.1 ms | 15.7 ms | |
| p5 I | 81192 | 8.8 to 9.5 ms | 11.1 to 12.3 ms | 24.4 ms |
| Rangers BL I | 60939 | 4.3 ms | 6.0 ms | |
| awaken P, 44 % intra (worst seen) | 96162 | 5.1 to 5.4 ms | 8.1 ms | 16.2 ms |
| awaken P, median | 33534 | 1.1 ms | 2.4 ms | 1.8 ms |
| p5 P / B (stream pic 1, 2) | 11241, 5703 | 0.79, 0.26 ms | 0.98, 0.33 ms | |
| Rangers BL P | 12357 | 0.59 ms | 0.92 ms | |

The I frame cost is latency, not throughput: about 2.7 us per serial step and 2.5 us per dispatch with the GPU mostly
idle (15 to 40 workgroups per dispatch). Inherent depth is the global wave count (2400 to 4000 in 4K I frames); `list`
gets within 12 to 45 % of it. JS `analyze` + `buildSchedule` cost 15 to 55 ms for a 4K I frame and 1 to 12 ms for a P
frame: this belongs in G0 (WASM, while decoding).

## Tests

`test/intra.html?cases=a,b&modes=list,ctu,wave&reps=5` compares every sample of every plane (picture padding excluded)
with the reference and checks `analyze()`'s waves against the producer's. Extra query: `rounds=K`, `patch=[[from,to]]`
(shader string replacements), `badsched=1` (all TUs in one step).

Result (all three schedules): 15 generated / real-geometry cases, 319,919,904 samples compared (189,684,576 of them intra
predicted, 3.79 M TU reconstructions), 0 mismatches, 0 wave mismatches; 9 `stream-*` cases (p5, awaken, fel-p7-bl,
Rangers BL pictures from `reconstream dump`, real residuals, expected = the decoder's pre-deblocking picture), 223,948,800
samples, 0 mismatches, G0 waves reproduced exactly. Negative controls, each caught: strong smoothing rounding (1440
samples), constrained intra ignored (896222), mode 10 filter off (433609), up-right clamp off (862), no dependency order
(`badsched`, 9763514 on the HDR10 I frame).

Generated cases (`rust/gpu-recon-ref-intra`, data in `test/data-intra/`, `*.bin` ignored by git):

```
G=rust/gpu-recon-ref-intra/target/release/gpu-recon-ref-intra; D=web/gpu-recon/test/data-intra
$G --out $D --name t1 --w 416 --h 240 --ctb 4 --slices 3 --tiles 2x2 --cip --rdpcm --intra 0.6
$G --out $D --name a-1080i-10b --w 1920 --h 1080 --bd 10 --ctb 6 --seed 11 --slices 4 --tiles 2x2 --smooth 0.7
$G --out $D --name b-1928p-8b-cip --w 1928 --h 1096 --bd 8 --ctb 5 --seed 12 --intra 0.3 --cip --slices 3
$G --out $D --name c-832i-smoothoff --w 832 --h 480 --bd 10 --ctb 4 --seed 13 --smooth-off --rdpcm --tiles 3x2 --slices 2
$G --out $D --name d-720-nostrong-cip --w 1280 --h 720 --bd 10 --ctb 6 --seed 14 --no-strong --detail 0.9 --cip --intra 0.7 --slices 2
$G --out $D --name e-1024-12b --w 1024 --h 576 --bd 12 --ctb 6 --seed 15 --detail 0.2 --smooth 0.8 --rdpcm --intra 0.8
$G --out $D --name f-640-8b-flat --w 640 --h 360 --bd 8 --ctb 6 --seed 16 --detail 0.1 --smooth 1.0 --cbf 0.2
$G --out $D --name g-4k-i --w 3840 --h 2160 --bd 10 --ctb 6 --seed 21 --detail 0.5 --smooth 0.6
$G --out $D --name h-4k-p --w 3840 --h 2160 --bd 10 --ctb 6 --seed 22 --detail 0.5 --smooth 0.6 --intra 0.08
# real geometry: --tus <records of one picture> (r-*.tus kept in data-intra), e.g.
$G --out $D --name real-hdr10-I --w 3840 --h 1720 --ctb 6 --seed 31 --tus $D/r-hdr10-I.tus
# end to end from G0's stream:
reconstream dump rust/aether-hevc/dev-media/p5.hevc /tmp/dump-p5 --pictures 3
rust/gpu-recon-ref-intra/target/release/from_dump /tmp/dump-p5 0 $D stream-p5-pic0
```

The generator draws CTU / CU / TU trees (tiles, slices, intra and inter CUs, NxN, deferred 4x4 chroma, implicit RDPCM
CUs), modes (planar and DC weighted, every angular mode), residuals (small, large, beyond the sample range for the
clip, cbf 0), and a starting picture mixing flat, gradient (strong smoothing) and noise regions, then reconstructs with
verbatim copies of the rust_h265 functions (`intra_pred.rs`, `compute_luma_avail_inner`, `cip_masks`; the SIMD switch
is off). The `r-*.tus` files were logged by a scratch copy of rust_h265 with one hook in `predict_intra_luma` /
`predict_intra_chroma` (after `compute_luma_avail_inner`) and a picture marker after `decode.finish` in
`finish_picture`; the vendored tree was not touched.
