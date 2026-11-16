# Dolby Vision profile 5 at 4K60: hardware base layer against the WASM decode

| Field | Value |
| --- | --- |
| Date | Unknown |
| Group | Dolby Vision |
| Type | Performance |
| Section | Profile 5, 4K60 |
| Environment | Headless Chrome, own server, local file without link shaping. Machine shared with two other measurement jobs. macOS, version Unknown. Machine, CPU, Chrome version, display and engine commit: Unknown. |
| Status | Current |

## Purpose

Can the profile 5 base layer be decoded in hardware, with the reshape in one WebGPU pass, while the renderer stays at 15 % of one core or less and drops no pictures? Do the pixels match the exact WASM route?

## Implementation

| Field | Value |
| --- | --- |
| Test asset | `fixtures/dv-p5.mp4`, Dolby Vision profile 5, 4K60 (section title), MP4. The clip has 1797 pictures. Bitrate and bit depth of the file: Unknown (base layer is Main 10). |
| Baseline | Floor: a plain element on the same file (renderer 4.3 %, wrong colours). Route for the pixel reference: the exact WASM route (`p5=wasm`) and FFmpeg's planes through COMPOSE and DISPLAY. |
| Methodology | Query `src=fixtures/dv-p5.mp4&secs=30&seeks=0`, the same query before and after. Steady window 24 s (1440 pictures). Renderer CPU and GPU process CPU as % of one core, per-thread split. Passes: WASM route 4, hardware one pass 9, hardware three stages (`lean=0`) 9. Pixel comparison (`p5-route-compare.html`): frames 0, 449, 898, 1347 and 1790, PQ R'G'B' output, tone mapping off, readback of an rgba16float target, 10-bit codes. |
| Observations | See the list below. |
| Margin of error | Spread across passes as listed in Results. Hardware one-pass median 12.8 % (range 7.1 to 15.7). No formal error margin recorded: Unknown. |

Budget from the source: renderer at most 15 %, 0 dropped of 1800. Target for the pixel comparison: 2 codes.

Observations:

- The machine was shared. WindowServer ranged from 2 to 29 %, and the same route from 7 to 16 % of the renderer. The spread is part of the finding.
- The base layer SPS of this clip says full range with matrix 2 (unspecified). The first relabelling kept the matrix, the range flag was then not found (`hvccFullRange` null), and the hardware route inverted a limited range conversion: mean 26.5, p99 64 codes away from the exact route. Profile 5 is now relabelled with matrix 9 (`P5_CARRIER`).
- The hardware route imports the picture, and the codes RECOVER reads back differ from the decoded ones on about half of the pixels. Where the exact Y, Cb, Cr is not a displayable RGB of the full range matrix (0.3 to 0.7 % of the pixels), the import clips, up to 50 luma codes. Inside the cube the recovered luma is off by -1 to +3 codes, mean +1.0. The step of Chrome's conversion that causes this was not found.

## Results

### Renderer and GPU process

| Route | Passes | Renderer, % of one core | GPU process, % | Dropped in the window | Renderer threads, % |
| --- | --- | --- | --- | --- | --- |
| WASM base layer (`p5=wasm`) | 4 | 92.7, 94.9, 93.2, 95.6 | 9.6 to 9.9 | 0, 1, 2, 1 | decode workers 35.0 + 21.8 + 21.6, demux and post 9.4 |
| Hardware, one pass | 9 | 14.2, 12.5, 12.8, 12.0, 12.8, 13.6, 15.7, 10.4, 7.1 (median 12.8) | 14.4 to 34.8 | 0, 1, 2, 4, 1, 0, 0, 2, 1 | video worker 5.1 to 7.9, demux 1.9 to 3.2, IO 1.5 to 2.1, Media 0.7 |
| Hardware, three stages (`lean=0`) | 9 | 12.3, 13.1, 13.5, 15.1, 13.7, 14.9, 15.6, 7.0, 15.2 | 16.2 to 36.2 | 2, 5, 0, 16, 3, 70, 8, 0, 13 | same split |

The 15 % renderer budget holds in 8 of 9 hardware one-pass passes. No route kept 0 dropped in every pass on this day, the WASM route included. The GPU process pays part of the gain.

The one-pass path removes the WASM base layer decode and, on pictures with no residual and no MMR, RECOVER, COMPOSE and the two code buffers. The mapping is a 3 x 1024 LUT. One pass costs about the same CPU as three stages; it is kept because it drops fewer pictures.

### Pixel comparison (PQ R'G'B', 10-bit codes, frames 0, 449, 898, 1347, 1790)

| Comparison | Max | Mean | p99 | p99.9 |
| --- | --- | --- | --- | --- |
| one pass against exact | 48.9 | 1.04 | 7.5 | 21.5 |
| three stages against exact | 56.8 | 1.03 | 7.5 | 22.75 |
| one pass against three stages | 65.2 | 0.16 | 3 | 13.5 |

Target not met. The source says the error is above the 2 codes asked for (10 to 13 % of the samples) and that it is not fixed. The cause is the import, not the pass count. The route that is exact in the codes is `p5=wasm`.

## Limitations

- Local file only, 30 s, no seeks, no link shaping. This is not a network test.
- The machine has one hardware HEVC decoder. The fallback to the WASM route when no hardware decoder accepts the stream is not exercised.
- The pixel comparison covers 5 frames.
- The machine was shared, so the renderer numbers move with other load.

## Fixes

- Profile 5 base layer decoded in hardware (WebCodecs), with the reshape done in one WGSL pass. The WASM base layer decode is deleted from the profile 5 route. `p5=wasm` or `q=high` keeps the WASM route, and it is taken by itself when no hardware HEVC decoder accepts the stream.
- Profile 5 relabelled with matrix 9 (`P5_CARRIER`) to fix the range inversion. Commit: Unknown.
