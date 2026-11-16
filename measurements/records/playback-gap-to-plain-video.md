# Playback gap to a plain video element

| Field | Value |
| --- | --- |
| Date | Unknown |
| Group | Playback performance |
| Type | Performance |
| Section | Gap to a plain `<video>`, 19 local samples and remote shape |
| Environment | Apple Silicon MacBook (model, CPU count, memory, macOS version and display: Unknown), lid open, Chrome 157 (build Unknown), visible window for local runs, headless Chrome for the remote shape. Engine commit `df598c3` (baseline) and `f62b4da` (result). |
| Status | Superseded in part |

## Purpose

How far the engine's playback stands from a plain `<video>` element on the same files: start, seeks, frames, CPU and memory. The baseline was measured before any engine change, the result after the tier changes. Both cover 19 local samples and a remote, slow-link shape.

## Implementation

| Field | Value |
| --- | --- |
| Test asset | 19 local samples, one file each. Examples: `avsync-24.mov` (hevc1080-pcm), `amaze-atmos.m2ts` (h264-ac3 and truehd-atmos), `rangers-fel.mkv` (fel-rangers), `dtsx-demo.m2ts`. Remote shape: `hevc4k-flac.mkv` at 39 Mbit/s, a 50 s clip (and the same clip looped to 18 min for a memory run). |
| Baseline | A plain `<video>` on the same file. Local: `perf-floor.html`, 60 s, no seeks. Remote shape: a plain `<video>` on the same bytes over the same shaped link. |
| Methodology | `perfrun.py` starts a separate Chrome (temporary profile, visible window, 960x540 player) on `perf.html`. The session opens the file, plays 60 s sampled once a second, then does 3 near and 4 far seeks. The page posts its result to the local test server. CPU and resident memory of the Chrome processes are sampled with `ps`. Local: one pass per sample. Remote shape: headless Chrome with a temporary profile, and a local pass-through that shapes a local file (`/slow/85-75`: 75 ms to the first byte, 85 Mbit/s; gap B used `/slow/66-125`). Three passes before and after. |
| Observations | Other builds and Chrome runs shared the machine during the result pass. The CPU columns of both local tables predate a later fix (see Limitations). The two source texts do not agree on which samples changed route (see Results). |
| Margin of error | Local CPU is good to about a factor of two. Two runs of the same code gave 8.0 and 16.1 % of one core for `dv-p81-4k60`, and 1.9 and 5.7 % for `h264-eac3`. Frame counts, routes, seeks and events are not affected. Ranges in parentheses are as printed in the source. Remote: three passes before and after; no further spread is given. |

## Results

### Remote shape

| Gap | Measure | Before | After | Floor | Budget | Met |
| --- | --- | --- | --- | --- | --- | --- |
| A | `hevc4k-flac.mkv`, 39 Mbit/s: first frame ms | 1241 / 1253 / 1186 | 611 / 616 / 616 | 421 / 446 / 416 | floor + 300 | yes (+190) |
| A | same: requests in 20 s | 227 | 2 | 1 | 2 to first frame | yes |
| A | same: 1 MiB range requests | all of them | 0 | 0 | 0 | yes |
| B | `amaze-atmos.m2ts` on `/slow/66-125`: route changes without a decoder error | 1 (reopen at 2.5 to 6.1 s) | 0 / 0 / 0 | 0 | 0 | yes |
| B | same: clock after 30 s | 17.5 s | 30.07 / 30.06 / 30.07 s | 30 s | | |
| B | same: page renderer CPU (% of one core) | 17.9 to 34.6 | 17.1 / 18.5 / 19.1 | no floor (TrueHD) | | |
| C | `dtsx-demo.m2ts` (local): route at the end | WebCodecs after -12909, 20 fps | MSE element, 673 / 673 / 673 shown | element, video only | | yes |
| C | same: page renderer CPU (% of one core) | 37 to 43 | 9.8 / 11.7 / 12.4 | 1.4 (no DTS decode) | | |
| D | `hevc4k-flac.mkv`, 50 s: page renderer RSS MB | 808 / 807 / 800 | 594 / 616 / 619 | 283 to 287 | floor + 150 | no (+310 to 335) |
| D | same, looped to 18 min, 10 min played: physical footprint MB at 1 and 8.5 min | not measured | 316 / 301 | 201 / 252 | floor + 150, no growth | yes (+115 / +48, no growth) |
| D | same: page renderer RSS MB, minute 3.5 to 10 | not measured | 584 to 693 | 371 to 376 | flat within 20 | no (110 MB band, no trend) |
| D | same: dropped frames of 14374 | Unknown | 21 | 18 | | |

### Gap to the floor, local disk (result, 8 samples)

| Sample | First frame ms (floor / engine) | Page renderer CPU (% of one core, floor / engine) | GPU process CPU (floor / engine) | Browser CPU (floor / engine) | Chrome total CPU (floor / engine) | Page renderer RSS MB (floor / engine) |
| --- | --- | --- | --- | --- | --- | --- |
| h264-aac | 153 / 249 | 2.7 / 4.3 | 6.3 / 11.5 | 1.2 / 6.3 | 11.4 / 22.4 | 155 / 376 |
| h264-eac3 | 78 / 217 | 1.7 / 5.7 | 6.2 / 11.7 | 1.2 / 6.4 | 9.6 / 24.1 | 153 / 431 |
| hdr10-4k60 | 83 / 120 | 1.6 / 7.1 | 6.2 / 16.9 | 1.2 / 5.5 | 9.6 / 29.6 | 221 / 473 |
| hdr10-90m | 162 / 239 | 4.2 / 6.8 | 8.1 / 12.8 | 1.2 / 6.0 | 15.2 / 26.1 | 452 / 790 |
| hdr10-eac3joc | 83 / 236 | 2.2 / 9.7 | 6.5 / 12.2 | 1.3 / 5.9 | 10.6 / 27.9 | 234 / 500 |
| hdr10-mkv | 166 / 218 | 3.2 / 5.9 | 7.8 / 12.4 | 1.3 / 6.1 | 13.7 / 24.8 | 311 / 606 |
| hdr10-mp4 | 218 / 237 | 3.2 / 5.6 | 7.7 / 12.7 | 1.3 / 6.2 | 13.6 / 24.9 | 329 / 686 |
| hevc1080-pcm | 169 / 233 | 2.4 / 4.3 | 7.7 / 12.4 | 1.2 / 5.8 | 12.4 / 22.7 | 144 / 393 |

The floor plays no E-AC-3, so for h264-eac3 and hdr10-eac3joc it is video only.

### Local samples, baseline and result (19 samples, one pass each)

| Sample | Route after the change | Shown frames (before / after) | Seek far ms (before / after) | Page renderer CPU, % of one core (before / after) | Chrome total CPU, % of one core (before / after) |
| --- | --- | --- | --- | --- | --- |
| dtsx-demo | MSE element | 673 / 673 | 97 / 102 | 8.0 / 7.4 | 23.0 / 25.5 |
| dtsx-gravity | MSE element | 1442 / 1442 | 78 / 88 | 7.2 / 10.2 | 19.6 / 28.2 |
| dv-p5-4k60 | canvas compositor (high) | 1440 / 1440 | 234 / 244 | 93.0 / 93.7 | 111.0 / 109.9 |
| dv-p81-4k24 | canvas compositor (fast) | 1440 / 1446 | 161 / 170 | 6.6 / 9.0 | 26.5 / 39.2 |
| dv-p81-4k60 | canvas compositor (fast) | 1440 / 1440 | 72 / 110 | 10.3 / 16.1 | 38.6 / 62.6 |
| fel-rangers | canvas compositor (reference) | 1438 / 1444 | 108 / 115 | 33.8 / 30.3 | 46.0 / 41.6 |
| fel-v6 | canvas compositor (reference) | 1445 / 1444 | 191 / 193 | 22.8 / 21.0 | 37.0 / 34.4 |
| h264-aac | MSE element | 1442 / 1443 | 48 / 54 | 4.2 / 4.3 | 21.8 / 22.4 |
| h264-ac3 | MSE element | 1175 / 1396 | 362 / 68 | 37.2 / 6.9 | 45.7 / 25.0 |
| h264-eac3 | MSE element | 1442 / 1443 | 64 / 72 | 5.1 / 5.7 | 21.7 / 24.1 |
| hdr10-4k60 | MSE element | 1441 / 1441 | 28 / 24 | 7.5 / 7.1 | 28.4 / 29.6 |
| hdr10-90m | MSE element | 1442 / 1442 | 346 / 390 | 6.0 / 6.8 | 23.0 / 26.1 |
| hdr10-eac3joc | MSE element | 1033 / 1034 | 146 / 163 | 9.7 / 9.7 | 25.8 / 27.9 |
| hdr10-mkv | MSE element | 1443 / 1443 | 160 / 175 | 5.6 / 5.9 | 22.7 / 24.8 |
| hdr10-mp4 | MSE element | 1442 / 1442 | 144 / 163 | 5.0 / 5.6 | 22.4 / 24.9 |
| hdr10plus | canvas compositor (fast) | 1055 / 1055 | 61 / 122 | 6.1 / 9.1 | 22.9 / 35.2 |
| hevc1080-pcm | MSE element | 1443 / 1441 | 26 / 26 | 4.0 / 4.3 | 20.9 / 22.7 |
| truehd-atmos | MSE element | 1174 / 1395 | 383 / 132 | 42.6 / 13.1 | 50.5 / 29.8 |
| truehd71 | WebCodecs to video element | 1796 / 1805 | 330 / 324 | 37.6 / 41.6 | 44.2 / 48.2 |

Notes on this table:

- All 19 samples: 7 of 7 seeks ok, no route change, no error event after the change. `truehd71` logs two known decoder warnings (interlaced H.264 goes to Chrome's software decoder; one recovered decode error after a seek).
- The source calls the start, seeks, frames and drops of the other samples unchanged within the spread. Each sample ran once, so single seek values carry that noise.
- The result text names `dtsx-demo` as one of three M2TS samples that moved from WebCodecs to the MSE element, at 37 to 43 % of one core before. The baseline table lists `dtsx-demo` on the MSE element at 8.0 % of one core, with seeks near 100 ms. The 37 to 43 % range matches `h264-ac3`, `truehd-atmos` and `truehd71` in that table. `truehd71` stays at 41.6 % of one core after the change, and the text does not mention it.
- Renderer RSS on local disk did not go down: `hdr10-mkv` 606 MB at the end (575 before), `hdr10-90m` 790 MB (785 before). Local files do not use the 256 MiB ring, so the saving is on remote media only.

## Limitations

- **Superseded in part.** Both local tables (baseline and result) and the gap-to-floor table predate the on-demand element fix (`CORE-V2.md`, section G). On these routes the session's hidden MediaStream `<video>` never got a frame and held the page's load event, so Chrome repainted its loading spinner about 30 times a second. With the fix, the GPU and browser processes are at the floor. The CPU columns of the engine in those tables still carry the spinner, so the GPU and browser gap they show is overstated. What remains of the gap is the page renderer's own work.
- Gaps E (Dolby Vision profile 5) and F (FEL) were not changed in the result. Their rows are a second baseline pass.
- No floor exists for AC-3, TrueHD and DTS audio, Blu-ray M2TS, HDR10+ and Dolby Vision. A plain element has no decoder for the audio, takes no transport stream, and shows only the base layer.
- Not measurable with local samples: no AC-3-only file next to 1080p video; no 1080p HEVC with compressed audio; the Dolby Vision profile 5 and 8.1 4K60 samples have no audio and last 30 s; no feature-length Blu-ray M2TS (the count of PPS contents per id was measured on four clips of 33 to 95 s, giving 4, 2, 1 and 1); no local 4K file longer than two minutes (the 10 minute memory run looped a 2 minute file nine times by stream copy); A/V drift on the MSE route reports 0 with 0 samples. The floor page has no seek pass.
- The remote link is a local stand-in. The real edge penalises small ranges (43 against 83 to 90 Mbit/s, measured on the real path before the change). The after figure on the real site still needs a run there.
- Remote seeks outside the buffer take 0.7 to 3.7 s, and 6.8 s back to the start of the file, although the first packet arrives within 25 to 135 ms. The time passes between that packet and the first frame. This is consistent with downloading from the keyframe to the target at link speed, but not measured. Open.
- Budget not met for D on RSS: 584 to 693 MB against 371 to 376 MB for the floor. Physical footprint is within budget (+48 to +115 MB). About 150 MB of the RSS difference is freed allocator memory that stays resident.
- Ring size: a 64 MiB ring with 16 behind was taken. A 48 MiB ring would save about 16 MB, but the canvas routes have no SourceBuffer, so the ring is their whole network cushion. The result also refers to the 256 MiB ring as the one used on remote media, so the source does not say which ring each D figure used.
- The reorder-depth look-ahead covers HEVC in Matroska. H.264 in Matroska keeps the 16 packet window, because its SPS is not parsed for the depth.

## Fixes

- **Remote media transport (gaps A, B and D).** Wherever SharedArrayBuffer exists there is one transport: one open-ended request, and a ring with backpressure. The rule that sent media from a different origin than the page to 1 MiB range requests, two at a time, is deleted. Commit: Unknown.
- **Blu-ray M2TS stay on the MSE element route (gap C).** `h264-ac3`, `truehd-atmos` and `dtsx-demo` stay on the element. Commit: Unknown.
- **On-demand element for the hidden MediaStream video (section G of `CORE-V2.md`).** The element is made only when needed, which brings the GPU and browser processes to the floor. Commit: Unknown.
- **Seek regression.** The first version of D (commit `4d47e08`) broke a seek back to the start of a file once more than 16 MiB had been read. Fixed in `f62b4da` with a unit test. The tables above are from after this fix.
