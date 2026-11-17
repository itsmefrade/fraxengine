# FraxEngine against Chrome `<video>`, five repeats

| Field | Value |
| --- | --- |
| Date | 2026-11-17 |
| Group | Playback performance |
| Type | Performance |
| Section | Comparison with the plain `<video>` |
| Environment | MacBook Pro 14", Apple M4 Pro (Mac16,8), macOS 27.2, Chrome 157.0.8103.51, built-in display, window 1440x810 pt. FraxEngine `v0.1.0` (tag, commit `1a14478`), built from a clean checkout of the tag with `web/dist-tools/build.sh` as fixed after the tag (the script did not build three HEVC modules; the engine code is unchanged). The machine was in normal use (other apps open). |
| Status | Current |

## Purpose

The headline comparison in the README had 2 repeats per cell, with spreads up to 7 points, so most gaps to `<video>`
were inside the noise. This record repeats the headline cells 5 times each, against the public `v0.1.0` build.

## Implementation

| Field | Value |
| --- | --- |
| Test asset | From [aetherengine-bench](https://github.com/superuser404notfound/aetherengine-bench): `h264-1080p.mp4`, `hevc-4k-hdr10.mp4`, `hevc-4k-hdr10-90m.mp4` (90 Mbit/s), `vp9.webm`, `dv-p81.mp4` (Dolby Vision 8.1, 3840x1714, 24 fps, no audio). 30 s of each played. |
| Baseline | Chrome's plain `<video src>` on the same file, same window, same Chrome switches. |
| Methodology | One Chrome per run, 10 s settle, 30 s measure, CPU and RSS of the whole Chrome process tree from `ps` once per second over 26 s inside the measure window. Chrome switches on every run, both engines: `--disable-backgrounding-occluded-windows --disable-renderer-backgrounding --disable-background-timer-throttling` (see [behaviour-chrome-hidden-page.md](behaviour-chrome-hidden-page.md)). 5 rounds; each round runs every fixture with FraxEngine then `<video>`, then Dolby Vision 8.1 with `?dvbase=1`, so drift over time hits both engines alike. 55 runs, all completed. Reported: median, and min to max. |
| Observations | Every run delivered 719 to 721 of 720 expected frames with 0 dropped. The first round was lower than the other four for both engines on most fixtures (for example HDR10: FraxEngine 9.3 against 11.6 to 12.8 %, `<video>` 11.1 against 14.1 to 15.2 %). Dolby Vision 8.1 ran on the canvas route by default and on the element route with `dvbase=1`. |
| Margin of error | Standard deviation across the 5 repeats 0.2 to 1.7 points. The min-to-max ranges of the two engines overlap on both HDR10 files, so those gaps are smaller than the run-to-run spread. No confidence interval was computed. |

## Results

CPU in % of one core, whole Chrome process tree. RSS is the median, whole tree.

| File | FraxEngine CPU, median (min to max) | `<video>` CPU, median (min to max) | FraxEngine RSS | `<video>` RSS |
| --- | --- | --- | --- | --- |
| `h264-1080p.mp4` | 9.1 % (8.2 to 9.4) | 12.2 % (10.3 to 12.6) | 1.07 GB | 0.83 GB |
| `hevc-4k-hdr10.mp4` | 12.4 % (9.3 to 12.8) | 14.8 % (11.1 to 15.2) | 1.34 GB | 0.95 GB |
| `hevc-4k-hdr10-90m.mp4` | 12.0 % (10.0 to 12.9) | 16.0 % (12.6 to 16.4) | 1.45 GB | 1.01 GB |
| `vp9.webm` | 10.0 % (8.9 to 10.2) | 12.4 % (10.4 to 12.5) | 1.02 GB | 0.82 GB |
| `dv-p81.mp4`, Dolby Vision processing (default) | 28.4 % (25.8 to 28.8) | 13.1 % (12.6 to 13.1), base layer only | 1.16 GB | 0.87 GB |
| `dv-p81.mp4`, `?dvbase=1` (base layer only) | 13.6 % (13.1 to 13.8) | same as above | 1.26 GB | same as above |

Raw values per run (CPU %, in round order):

| File | FraxEngine | `<video>` |
| --- | --- | --- |
| `h264-1080p.mp4` | 8.2, 9.1, 9.1, 9.4, 9.3 | 10.3, 12.1, 12.6, 12.2, 12.3 |
| `hevc-4k-hdr10.mp4` | 9.3, 12.8, 11.6, 12.4, 12.5 | 11.1, 15.2, 14.1, 14.8, 15.1 |
| `hevc-4k-hdr10-90m.mp4` | 10.0, 12.0, 12.2, 12.9, 11.7 | 12.6, 16.0, 16.2, 16.4, 16.0 |
| `vp9.webm` | 8.9, 10.2, 10.0, 10.2, 9.8 | 10.4, 12.4, 12.4, 12.2, 12.5 |
| `dv-p81.mp4` default | 25.8, 28.7, 28.8, 28.4, 27.4 | 12.6, 13.1, 13.1, 13.1, 12.9 |
| `dv-p81.mp4` `dvbase=1` | 13.6, 13.1, 13.8, 13.6, 13.2 | |

### Reading

- On H.264, VP9 and the 90 Mbit/s HEVC file, FraxEngine's highest run is at or below `<video>`'s median, and
  4 of 5 FraxEngine runs are below `<video>`'s lowest run on H.264 and VP9.
- On the 4K HDR10 file the medians differ by 2.4 points but the ranges overlap; call it no worse than `<video>`.
- Dolby Vision 8.1 with processing costs about twice `<video>`, which plays only the base layer. The cost is the
  WebGPU canvas presentation ([dolby-vision-profile-8-1-canvas-cost.md](dolby-vision-profile-8-1-canvas-cost.md)).
  With `dvbase=1` FraxEngine is within 0.5 points of `<video>`.
- FraxEngine uses 0.2 to 0.45 GB more RSS than `<video>`.

## Limitations

- One machine, one Chrome version, a machine in normal use. CPU of the Chrome tree only; WindowServer and other
  system processes are not counted ([hdr-windowserver-cost.md](hdr-windowserver-cost.md)).
- The Chrome background-throttling switches were on for both engines. Without them a page that macOS reports as
  hidden is paused or stops presenting, which makes CPU numbers meaningless for both.
- These numbers are higher than the 2-repeat runs of
  [comparison-player-engines-m4-pro.md](comparison-player-engines-m4-pro.md) for both engines (different day,
  different machine load, the switches above). Compare engines within one record, not across records.
- Dolby Vision 7 FEL, AV1, MKV variants and audio-only files were not repeated here.

## Fixes

- `web/dist-tools/build.sh` now builds all four HEVC WebAssembly modules when they are missing. Before, a clean checkout
  of `v0.1.0` failed the static scan (`aether_hevc_simd_mt.wasm`, `aether_hevc_simd_recon.wasm`,
  `aether_hevc_simd_recon_mt.wasm` missing).
- Dolby Vision processing stays the default for Profile 8.1; `?dvbase=1` is documented as a battery-saving option.
