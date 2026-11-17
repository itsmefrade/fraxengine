# Player engines compared on one Mac (aetherengine-bench)

| Field | Value |
| --- | --- |
| Date | 2026-11-15 (main run), 2026-11-16 (two fixtures re-run with the window kept visible) |
| Group | Playback performance |
| Type | Performance |
| Section | FraxEngine against native players and other browser paths |
| Environment | MacBook Pro 14", Apple M4 Pro (Mac16,8), 12 cores (8 performance, 4 efficiency), macOS 27.2, Chrome 157.0.8103.51, built-in display, FraxEngine pre-release build `abbc45f` (private history). `v0.1.0` was cut from it: same Rust and JavaScript code apart from the project rename, comments and test-file paths; the WebAssembly was rebuilt and is not byte-identical. See [Builds measured](../README.md#builds-measured); the machine was in normal use (other apps open) |
| Status | Current |

## Purpose

How much CPU and memory FraxEngine costs compared with Chrome's own `<video>`, hls.js and the native macOS players, on
the same files, the same window size and the same protocol.

## Implementation

| Field | Value |
| --- | --- |
| Test asset | The 10 fixtures of [aetherengine-bench](https://github.com/superuser404notfound/aetherengine-bench): `h264-1080p.mp4`, `hevc-4k-hdr10.mp4`, `hevc-4k-hdr10.mkv`, `hevc-4k-hdr10-90m.mp4`, `hevc-4k-hdr10-90m.mkv` (90 Mbit/s), `av1-10bit.mkv`, `vp9.webm`, `eac3-51.mp4`, `dv-p81.mp4` (Dolby Vision 8.1, 3840x1714, 24 fps, no audio), `hevc-subs.mkv` (HEVC with SRT and ASS tracks, no audio). Played from local disk. |
| Baseline | Chrome's plain `<video src>` on the same file (the fair browser baseline); the native players for scale. |
| Methodology | aetherengine-bench protocol with a fork that adds three Chrome backends (`<video>`, hls.js 1.7.4 over Media Source, FraxEngine). Per fixture block an idle power baseline, one cold throwaway run, then 2 measured repeats with rotating engine order: 10 s settle, 30 s measure, 15 s cooldown. Window 1440x810 pt (2880x1620 px) for every engine. Metrics: CPU and RSS from `ps` every second (for Chrome the sum over the whole process tree), frames delivered against the nominal frame count (a run passes at 95 %), package power from `powermetrics`. A refusal (the engine says it cannot play the file) is recorded as such. |
| Observations | The machine could not meet the bench's default quiet-machine ceiling (250 mW idle CPU power), so the ceiling was raised to 1500 mW with 250 mW stability tolerance. Power numbers are therefore indicative only, and FraxEngine's GPU power is not reported (its baseline varied between 1 and 2.9 W). VLCKit 4.0 alpha stopped on an OpenGL assertion on every launch at 1440x810 and at 1920x1080 (0 of 4 each). |
| Margin of error | 2 repeats per cell. CPU swings of several points between identical runs are normal on this machine (see METHODOLOGY). No confidence interval was computed. |

## Results

For FraxEngine against `<video>` alone, [comparison-video-element-5-repeats.md](comparison-video-element-5-repeats.md) (5 repeats, `v0.1.0`) supersedes the two-repeat numbers below.

Median of the two repeats. CPU in % of one core.

### FraxEngine per fixture

| Fixture | Route | CPU | RSS | Frames |
| --- | --- | --- | --- | --- |
| `h264-1080p.mp4` | mse | 6.8 % | 1.08 GB | pass |
| `hevc-4k-hdr10.mp4` | mse | 9.4 % | 1.35 GB | pass |
| `hevc-4k-hdr10.mkv` | mse | 10.6 % | 1.32 GB | pass |
| `hevc-4k-hdr10-90m.mp4` | mse | 13.7 % | 1.44 GB | pass |
| `hevc-4k-hdr10-90m.mkv` | mse | 12.6 % | 1.39 GB | pass |
| `av1-10bit.mkv` | mse | 6.2 % | 1.0 GB | pass |
| `eac3-51.mp4` | mse | 5.3 % | 1.13 GB | pass |
| `dv-p81.mp4` | canvas | 27.9 % | 1.16 GB | pass |
| `vp9.webm` | mse | 10.0 to 10.3 % | 1.01 GB | pass (re-run, see below) |
| `hevc-subs.mkv` | mse | 11.4 to 12.0 % | 0.98 GB | pass (re-run, see below) |

In the main run `vp9.webm` (61 to 66 of 720 frames) and `hevc-subs.mkv` (0 of 720) failed the frame gate. The cause
was Chrome's hidden-page behaviour, not the engine ([behaviour-chrome-hidden-page.md](behaviour-chrome-hidden-page.md)).
Re-run on 2026-11-16 with the window kept visible, 2 runs each: 720 of 720 frames, 0 dropped, for FraxEngine and for
the plain `<video>` (`vp9.webm` 13.9 to 14.4 %, `hevc-subs.mkv` 11.4 to 11.6 %). The re-run used the same driver
without the idle baseline and power sampling.

### All engines, ranges over the fixtures each one played

| Engine | Plays | CPU | RSS | Notes |
| --- | --- | --- | --- | --- |
| AVPlayer | MP4 only (refuses MKV, AV1, VP9) | 1 to 3.5 % | 82 to 102 MB | lightest where it plays |
| AetherEngine | all 10 | 1.6 to 3.6 % (VP9 21 %, software decode) | 120 to 580 MB | |
| KSPlayer | refuses AV1 | low | 870 MB to 1.6 GB on 4K | failed the frame gate on `hevc-subs.mkv`; launch failures |
| libmpv | all | 14 to 17 % | Unknown | about 200 to 300 mW GPU |
| VLCKit 4.0 alpha | none | n/a | n/a | OpenGL assertion at start |
| Chrome `<video>` | all 10 | 11 to 15 % | 0.78 to 1.02 GB | |
| Chrome hls.js 1.7.4 | refuses E-AC-3 | similar to `<video>` | more than `<video>` | 90 Mbit/s stalls (`bufferFullError`, Media Source quota) with the default config |
| FraxEngine | all 10 | 5.3 to 13.7 % on the mse route, 27.9 % for Dolby Vision 8.1 | 0.98 to 1.44 GB | table above |

## Limitations

- One machine, one display, one Chrome version. Native players and browser paths are different products; the
  comparison is about cost, not features.
- Chrome numbers include the whole browser tree; the native players are one process. macOS system services that do
  work for a player (for example VideoToolbox, WindowServer) are not counted for any engine.
- Power is not reliable on this run (busy machine, raised ceiling).
- Ranges for the other engines are summarised from the run; the per-fixture rows are not reproduced here.

## Fixes

None from this run. The Dolby Vision 8.1 cost was followed up in
[dolby-vision-profile-8-1-canvas-cost.md](dolby-vision-profile-8-1-canvas-cost.md).
