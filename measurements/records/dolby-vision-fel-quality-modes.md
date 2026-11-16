# Dolby Vision Profile 7 FEL: quality modes (hybrid input)

| Field | Value |
| --- | --- |
| Date | Unknown |
| Group | Dolby Vision |
| Type | Quality |
| Section | Profile 7 FEL, hw / exact-bl / exact-el / exact-both |
| Environment | macOS, Apple GPU (adapter `apple/metal-3`), 12 CPU cores, Chrome 156 and 157 with `--headless=new`, 1920 x 1080 canvas. Engine commit: Unknown. Display: Unknown |
| Status | Current |

## Purpose

Which hybrid input mode gives the best composed quality and sustained speed for Profile 7 FEL, measured against the exact gold reference.

## Implementation

| Field | Value |
| --- | --- |
| Test asset | Two sets. (1) The 1454 picture pair fixture, about 3.5 Mbps (non-retag streams for WASM, retag streams for hardware). (2) Real media, from the local test media, demuxed with `web/engine`: `DV FEL All Layers Test (Woman at 80s).mkv` (1440 pictures, 9.5 Mbps, 4K BL + 1080p FEL, P7 FEL with MMR), `DV FEL Power Rangers Credits Test.mkv` (1447 pictures, 18.1 Mbps, open GOP, 4K BL + 1080p FEL), `awaken-girl.4K.HDR.DV.mkv` (all 264 pictures, 89.5 Mbps, P7 MEL, 4K BL + 1080p MEL) |
| Baseline | `exact-both`, the bit exact composition. Quality is measured against the exact gold: FFmpeg decode of the same layers composed by `composeExact` at 12 checkpoint pictures (fixture) or 8 (real media) |
| Methodology | Quality: composed 12 bit result (vdr12) against the gold, PSNR and max error in codes of 4095, plus BL and EL code max error. Sustained fps: first to last presented pair, no readback, at most 3 frames in flight. Fixture at N = 3 / 4 / 5 / 6 workers, real media at N = 4 / 5 / 6 over 1440+ pictures. Startup: time to first composed frame. Peak RSS: sum over the Chrome process tree, sampled each second (it double counts shared pages). Repeats: Unknown |
| Observations | The machine was shared with other agents the whole time. Load average was 8 to 15 on 12 cores for the fixture and 5 to 20 for real media, so fps is noisy and conservative. `VideoDecoder` prefer-hardware is supported, software is not. Headless and visible window runs agree within load noise. Vsync paced presentation was not measured |
| Margin of error | Unknown. Per run means are kept in `web/results`; the spread between repeats is not recorded |

## Results

### Fixture: 1454 picture pair, Chrome 156 and 157

| Mode | vdr12 PSNR min / mean (dB) | vdr12 max err (of 4095) | BL / EL code max err | fps N=3 / 4 / 5 / 6 | Peak RSS (N=4) | Startup to first frame |
| --- | --- | --- | --- | --- | --- | --- |
| `hw` | 59.5 / 60.8 | up to 129 | 32 / 3 | 210 to 249 (hardware path, see [dolby-vision-fel-compose-exactness.md](dolby-vision-fel-compose-exactness.md)) | 1.3 GB | 70 to 100 ms |
| `exact-bl` | 75.8 / 75.8 | 10 | 0 / 12 | 111 / 103 / 119 / 120 | 4.6 GB | 190 to 260 ms |
| `exact-el` | 59.5 / 61.4 | 128 | 32 / 0 | 199 / 189 / 211 / 189 | 3.6 GB | 90 to 150 ms |
| `exact-both` | exact (inf) | 0 (12 of 12 identical) | 0 / 0 | 81 / 91 / 93 / 80 | 6.1 GB | 175 to 330 ms |

Realtime is 23.976 fps. `exact-bl` runs at 4.3 to 5 times realtime and `exact-both` at 3.4 to 3.9 times. Per plane for `exact-bl`: Y 80.9, U 72.9, V 71.4 dB. Fewer than 5 gaps over 41.7 ms in every sustained run. Peak RSS is dominated by the reorder buffer (default `budgetMB` 1536, peak buffered 1.1 to 2.0 GB) and about 320 MB of WASM memory per BL worker.

Display detail: 3840 x 2160 canvas, `exact-bl` N=4: 101 fps headless, 96 fps visible window (`hw`: 151 headless, 164 visible). Visible 1920 x 1080 `exact-bl` N=4: 108 fps.

### Real media, Chrome 157

Quality, composed vdr12 against the exact gold at 8 checkpoints:

| File | Mode | PSNR min / mean of non-exact pictures (dB) | max err (of 4095) | BL codes max err | EL codes max err |
| --- | --- | --- | --- | --- | --- |
| woman | `hw` | 59.5 / 64.5 (one near black picture at 88.7) | 37 | 12 | 8 |
| woman | `exact-bl` | 72.4 / 75.6 (1 of 8 exact) | 7 | 0 | 8 |
| woman | `exact-el` | 60.5 / 64.9 | 37 | 12 | 0 |
| woman | `exact-both` | exact | 0 | 0 | 0 |
| rangers | `hw` | 61.3 / 62.1 | 100 | 25 | 3 |
| rangers | `exact-bl` | 61.6 / 61.8 | 12 | 0 | 3 |
| rangers | `exact-el` | 61.8 / 61.9 | 100 | 25 | 0 |
| rangers | `exact-both` | exact | 0 | 0 | 0 |
| awaken (MEL) | `hw` | 61.2 / 61.3 | 76 | 19 | n/a |
| awaken (MEL) | `exact-bl` | exact (EL not used) | 0 | 0 | n/a |

Sustained fps (1440+ pictures, no readback). The `hw` column does not depend on N. The exact columns are N = 4 / 5 / 6.

| File | `hw` | `exact-bl` | `exact-el` | `exact-both` |
| --- | --- | --- | --- | --- |
| woman (9.5 Mbps) | 186 | 92 / 95 / 100 | 114 / 115 / 119 | 57 / 53 / 56 |
| rangers (18 Mbps, open GOP) | 145 | 40 / 43 / 47 | 99 / 109 / 112 | 38 / 43 / 51 |
| awaken (90 Mbps, MEL) | 341 | 26 / 26 / 26 | not needed | not needed |

Startup and memory at N=4: first composed picture in 58 to 70 ms for `hw` and 84 to 260 ms for the exact modes (awaken `exact-bl` 650 to 740 ms). Peak RSS of the Chrome process tree: `hw` 1.3 to 1.6 GB, `exact-el` 2.1 to 3.6 GB, `exact-bl` 3.6 to 4.7 GB, `exact-both` 2.9 to 4.0 GB. Minimum prebuffer for glitch free 23.976 playout is 63 to 413 ms for every case except awaken `exact-bl`, which needs about 3 s (26 fps against 24 needed).

### Findings

- Which layer carries the error depends on the content. On woman the BL dominates, and exact BL gives 75 dB. On rangers the EL residual is as large an error source as the BL. `exact-bl` and `exact-el` each gain only about 0.2 to 0.5 dB there, and only `exact-both` is exact. `exact-bl` is a conditional gain: large for BL dominated content, small otherwise.
- MEL (awaken): with the residual off the EL is never decoded, so `exact-bl` is already bit exact and costs only the BL decode.
- Open GOP (rangers): every RASL picture after a CRA is lost without a prefix. The GOP parallel pool decodes the previous GOP first and drops its output, so a GOP is decoded about 1.3 to 2 times. The EL has its own IRAP positions, so the pool splits each layer on its own flags.
- Decode cost is bitrate bound: 9.5 Mbps 92 to 100 fps, 18 Mbps (open GOP) 40 to 47 fps, 90 Mbps 26 fps (BL only). `exact-both` at 38 to 57 fps is 1.6 to 2.4 times realtime on the two 10 to 20 Mbps FEL files, under heavy machine load, with startup under 0.3 s and no stalls once a 0.2 s prebuffer is held. It does not scale to a 90 Mbps class FEL, where about 25 fps is expected.

## Limitations

- The fixture clip is about 3.5 Mbps. WASM decode cost per frame is far below a 50 to 80 Mbps UHD remux, so `exact-bl` is expected to be bitrate bound there. That case was not measured.
- The decode and the GPU share the machine. No 10 minute soak was run.
- The remaining error of `exact-bl` is the hardware EL (max 10 to 12 codes, Chrome RGB clamping near black and on saturated colours). It enters the composition scaled down, so vdr12 error is at most 10 of 4095 (about 75.8 dB).
- Real media results cover the tested segments only (60 s of each file, see Implementation).
- The `hw` figures in the fixture table come from the hardware path measurement in the compose record.

## Fixes

- Open GOP prefix: `skip` in `web/exact/pool.js` (`loadFixtures`) and `web/exact/stream.js`. Commit: Unknown.
