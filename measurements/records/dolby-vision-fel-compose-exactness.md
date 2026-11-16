# Dolby Vision Profile 7 FEL: COMPOSE bit exactness and hardware path accuracy

| Field | Value |
| --- | --- |
| Date | Unknown |
| Group | Dolby Vision |
| Type | Quality |
| Section | COMPOSE stage bit-exactness and hardware path accuracy |
| Environment | macOS, Apple GPU / Metal 3, Chrome 156 (user's own profile) and Chrome 157 (fresh profile). Headless: Unknown. Engine commit, display: Unknown |
| Status | Current |

## Purpose

Whether the COMPOSE stage reproduces the exact integer reference bit for bit, how far the hardware decode path lands from that reference, and whether the MMR and P5 colour checks agree with independent references.

## Implementation

| Field | Value |
| --- | --- |
| Test asset | `web/fixtures/fel-p7-retag-*`: 1454 frame pairs, 4K BL + 1080p EL, 12 checkpoint frames for the `exact` and `hw` steps. MMR: `DV FEL All Layers Test (Woman at 80s).mkv` (1440 pictures) and `DV FEL Power Rangers Credits Test.mkv` (1447 pictures), 8 pictures each. P5 colour: `Test Jellyfin 4K DV P5.mp4` (4K, 60 fps, 44 Mbps, IPTPQc2), 5 frames |
| Baseline | Exact gold: FFmpeg planes with the exact RPU NAL and the gold manifest hashes (`raw16Sha256`, `vdr12Sha256`), produced by independent Python / NumPy integer maths, not by this shader. MMR: a BigInt port of `predict_reference16`. P5: an independent float64 implementation of the IPTPQc2 equations |
| Methodology | `exact` step: sha256 of `raw16` and `vdr12` against the gold, all 12 frames. `hw` step: WebCodecs hardware decode, RECOVER, COMPOSE, compared with the exact composed signal (PSNR and max absolute error). `fps` step: decode, RECOVER, COMPOSE, DISPLAY and present, no readback, at most 3 frames in flight. MMR: `mmr.test.html` composes FFmpeg's exact planes with a zero residual EL and compares WGSL with the BigInt port. P5: the DISPLAY chain on exact planes input, 4000 pixels per picture. Repeats: Unknown |
| Observations | All 12 RPUs of the fixture clip use identity polynomial luma and chroma curves, so MMR is ported but not exercised by a stream here. Only 12 RPU NALs exist for the clip, so non-checkpoint frames reuse the previous checkpoint RPU. This does not change compute cost |
| Margin of error | Unknown |

## Results

### COMPOSE, exact step

| Check | Result |
| --- | --- |
| `raw16` and `vdr12` sha256 against the gold | Match for all 12 checkpoint frames |

### Hardware path against the exact result (`hw` step, same 12 frames)

| Signal | PSNR (dB) | Max abs error |
| --- | --- | --- |
| Composed 12 bit (vdr12), all planes | 59.1 to 61.2 (mean 60.8) | 16 to 129 codes of 4095 |
| Composed Y / U / V | 61.1 to 61.5 / 57.6 to 60.9 / 56.0 to 60.1 | Unknown |
| Composed raw16 | 59.1 to 61.2 | 256 to 2064 of 65535 |
| Recovered BL codes (10 bit) | 58.9 to 61.7 | 3 to 32 of 1023 |

Not bit exact, and below 60 dB on 3 of 12 frames (360, 720 and 1080). The error is in the base layer. The identity mapping scales BL error by 4 into 12 bit, and the EL residual adds almost nothing. For frame 360, 8.25 M of 8.29 M luma samples are within 2 codes. The large errors sit at Y code below 128 (near black, 26 k pixels, up to 26 codes) and in saturated chroma (up to 32 codes).

### MMR (`mmr.test.html`)

| Check | Result |
| --- | --- |
| Samples compared | 144000 per file (6000 Cb, 6000 Cr and 6000 luma at each of 8 pictures), including the 8 piece luma polynomial |
| Mismatches against the BigInt reference | 0 |
| Chroma samples moved by MMR by more than one 10 bit code | 60 to 90 % (woman), 1 to 10 % (rangers) |

### P5 colour check (DISPLAY chain)

| Check | Result |
| --- | --- |
| DISPLAY P5 chain against the f64 IPTPQc2 model | Matches to 4.9e-4 PQ, 4000 pixels per picture (exact planes input) |

### Sustained throughput (`fps` step)

| Canvas | fps | x realtime | Pair interval p50 / p95 / p99 / max (ms) | Gaps over 41.7 ms |
| --- | --- | --- | --- | --- |
| 1920 x 1080 (2 x 2 supersample), Chrome 156 | 248.8 | 10.4 | 4.1 / 6.1 / 9.3 / 22.5 | 0 |
| 1920 x 1080, fresh Chrome 157 | 209.7 | 8.75 | 0.3 / 19.4 / 21.0 / 26.4 | 0 |
| 3840 x 2160, fresh Chrome 157 | 210.5 | 8.78 | 0.3 / 18.6 / 19.9 / 58.6 | 1 |

## Limitations

- The fixture uses identity polynomial curves on all 12 RPUs. The MMR and the 8 piece luma curve are covered only by the real media runs in the separate MMR check above.
- The hardware path is not bit exact. The cause is the import: it returns clamped RGB, so near black or saturated pixels with a negative component lose information and the inverse matrix cannot recover them. This is not fixable from the JS side. A software decode of those pixels, or of the whole BL, is the exact fallback (see the quality modes record).
- The DISPLAY stage matches the f64 model to under 0.001 relative error in 4 synthetic cases: no tone mapping, BT.2390 4000 to 1000 and 4000 to 600 nit, and SDR white 100.
- Not measured: 50 to 80 Mbps UHD remux, a 10 minute soak, and a real RPU per frame.

## Fixes

None. The source says the hardware path cause is not fixable from the JS side.
