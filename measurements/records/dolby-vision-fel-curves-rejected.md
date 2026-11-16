# Profile 7 FEL: enhancement layer as a luma curve (built, measured, reverted)

| Field | Value |
| --- | --- |
| Date | Unknown |
| Group | Dolby Vision |
| Type | Performance |
| Section | Profile 7 FEL, curve approximation |
| Environment | Headless Chrome with a temporary profile (test bed), local files, interleaved passes against the hardware FEL default. Machine, CPU, macOS version, Chrome version and display: Unknown. Engine: 3e21260 (built), reverted by 779ca43. |
| Status | Historical (approach not taken) |

## Purpose

Can the enhancement layer be approximated by decoding only its IRAP pictures and applying a per-code luma curve to the other pictures, for less CPU than the hardware FEL default?

## Implementation

| Field | Value |
| --- | --- |
| Test asset | `rangers-fel.mkv` and `fel-v6.mp4` (see the FEL record). |
| Baseline | Hardware FEL, the default, interleaved with the curve run. For the picture comparison: full FEL composed output on 5 non-IRAP pictures. |
| Methodology | `?felcurves=1` on the hardware route. Only the IRAP pictures of the enhancement layer are decoded (2 per second instead of 24). Each is composed in full. A small compute pass returns its luma residual per base layer code (one sample per 8 x 8 block). The mean per code is added to the luma prediction table of the pictures up to the next IRAP. Chroma gets the mapping only. Three interleaved passes for CPU. For the picture comparison (`felcurves=compare`): composed buffer read back, 12-bit luma, PSNR. |
| Observations | On `fel-v6` the residual does not follow the luma code, so the curve gives no gain there. |
| Margin of error | "Inside the spread of the passes" (source). No formal error margin recorded: Unknown. |

## Results

### CPU, interleaved with the default (3 passes)

| Route | rangers page / GPU process, % | fel-v6 page / GPU process, % |
| --- | --- | --- |
| hardware FEL (default) | 9.9, 9.6, 7.4 / 23.5, 22.3, 17.1 | 7.8, 6.2, 6.9 / 23.7, 17.7, 19.6 |
| FelCurves | 8.9, 6.8, 7.9 / 20.6, 14.8, 16.8 | 7.5, 6.5, 6.1 / 21.7, 17.7, 16.9 |

About 1 point of renderer and 1.5 to 3.5 points of GPU process in the mean, inside the spread of the passes. Memory unchanged (uaMemory 531 / 523 MB, page RSS 544 to 562 / 523 to 540 MB). 0 dropped.

### Picture against full FEL (5 non-IRAP pictures)

| Title | Curve, PSNR dB (mean error in codes, max) | Mapping only, PSNR dB |
| --- | --- | --- |
| rangers | 45.2, 43.4, 42.8, 42.7, 39.4 (6 to 15 codes, max 583 to 710) | 30.4, 28.5, 27.9, 27.8, 24.3 |
| fel-v6 | 26.0, 25.9, 26.0, 26.2, 25.7 (73 to 96 codes, max about 1800) | 26.0, 25.6, 26.0, 26.0, 25.6 |

Chroma without its residual: 60 dB on rangers, 27.6 dB on fel-v6.

## Limitations

- Five pictures, two titles.
- On rangers the curve recovers about 15 dB over the mapping alone. On fel-v6 it recovers nothing and has a larger mean error than the mapping.
- The CPU gain is inside the noise.

## Fixes

None kept. The curve code was built in 3e21260 and reverted in 779ca43, because there was no gain outside the noise and the picture cost was not worth keeping it. 3e21260 holds the code if a title ever needs it.
