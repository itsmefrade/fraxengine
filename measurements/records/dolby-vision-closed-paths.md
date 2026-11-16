# Dolby Vision: approaches that do not work in Chrome

| Field | Value |
| --- | --- |
| Date | 2026-11-07 |
| Group | Dolby Vision |
| Type | `Behaviour` |
| Section | Approaches not taken |
| Environment | Chrome 157. OS, machine model, display and engine commit Unknown. |
| Status | `Historical (approach not taken)` |

## Purpose

Which ways of playing the full FEL (the Dolby Vision enhancement layer) at full quality, cheaper than now, were tried in
Chrome and failed? These are recorded so they are not tried again.

## Implementation

| Field | Value |
| --- | --- |
| Test asset | Unknown. No file is named in this section. |
| Baseline | Exact reference decode for the recovery path. A single-threaded WASM decoder for the threaded path. |
| Methodology | Each row gives the path, the result and the evidence, as the source states them. The recovery path was modelled. The threaded WASM path was timed against the single-threaded decoder. The worker count for the threaded row is not stated. |
| Observations | The `VideoFrame.copyTo` path fails with `format: null` for 10-bit formats. Chromium lists it as a known gap (w3c/webcodecs issue #631). No timeline for a fix was found. Threaded decode runs faster at 4K but uses more CPU at 1080p. Single-threaded tuning gave a 1.31x gain in its first round, then 1 to 3 % per round. |
| Margin of error | Unknown. |

## Results

| Path | Result | Evidence |
| --- | --- | --- |
| Hardware decode, then recover the codes | Not possible | Every web-exposed path in Chrome 157 (WebGPU import and copy, WebGL RGBA32F and RGB10_A2, Canvas float16, `<video>`) passes through an 8-bit RGB intermediate. Conversion was modelled at 99.98 to 99.999 % exact. A Y code moves RGB by 0.29 steps. Even with full information, the best Y accuracy is 63 %. |
| `VideoFrame.copyTo` with 10-bit formats | Not available | `format: null`. I420P10, NV12 and I420 are rejected. Chromium lists this as a known gap (w3c/webcodecs #631), with no timeline. |
| Threaded WASM decoder | Works, but does not reduce CPU | At 4K, 4 times faster for the same CPU. At 1080p, 62 % more CPU. Kept for later, for the real-time margin. |
| Single-threaded micro-optimisation | Plateau | Three rounds: 1.31x first, then 1 to 3 %. The rest is memory traffic and CABAC. |

## Limitations

- Chrome 157 only. The web-exposed path list and the `copyTo` status can change in later Chrome versions and in the W3C issue.
- This covers web paths only. Native OS paths are not covered by this record.
- The recovery result is a model, not a hardware run.
- The threaded row does not state the worker count. The current FEL plan uses 2 WASM workers.
- The OS, machine and display are not recorded, so the CPU figures cannot be compared with other records.

## Fixes

None. No code changed because of these results.
