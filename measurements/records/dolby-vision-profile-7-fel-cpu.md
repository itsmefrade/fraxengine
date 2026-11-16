# Profile 7 FEL on the CPU: bit-exact reference against the hardware default

| Field | Value |
| --- | --- |
| Date | Unknown |
| Group | Dolby Vision |
| Type | Performance |
| Section | Profile 7 FEL |
| Environment | Headless Chrome with a temporary profile (test bed), local file, 30 s steady window, no link shaping. macOS, version Unknown. Machine, CPU, Chrome version and display: Unknown. Engine: Unknown for the attribution (F.1), 5130bb3 for the default change (F.2). |
| Status | Current |

## Purpose

Where does the CPU go in FEL playback on the bit-exact route? Can the two hardware decoders take FEL by default within 15 % of one renderer core, with no dropped pictures?

## Implementation

| Field | Value |
| --- | --- |
| Test asset | `rangers-fel.mkv` (Matroska, FEL: 4K base layer, 1080p enhancement layer) and `fel-v6.mp4` (MP4, FEL). Frame rate, bitrate and dimensions of `fel-v6.mp4`: Unknown. |
| Baseline | Floor: a plain element with the base layer only, renderer 2.0 to 2.2 %. Before: the bit-exact route (`q=reference`). |
| Methodology | F.1: stage counters from `web/player/js/stages.js` (ms per second of playback), per-thread CPU via libproc (`web/player/tools/perfrun.py`), 3 passes per file. Thread numbers: #1 video worker, #2 demux, #3 demux helper, #4 base layer entropy worker, #5 enhancement layer entropy worker. F.2: the bit-exact route, `q=fast` forced and the default, in interleaved batches, 3 passes each. |
| Observations | See the list below. |
| Margin of error | Spread of the passes as listed. Differences under about 2 points between batches are not findings (the later batch ran slower). No formal error margin recorded: Unknown. |

Observations:

- The premise was wrong. `auto` chose quality `reference` for every profile 7 file with an enhancement layer. FEL therefore ran in software on both layers (WASM CABAC in a worker per layer, reconstruction on WebGPU), not on the hardware decoders.
- The "unattributed 20.9 % worker" is the base layer entropy worker, not the worker that composes.
- The copies that the candidate changes aimed at add up to 2.3 ms per second of playback.
- On the hardware route the GPU process rises from 6.5 and 11 % to about 20 %, for two 4K hardware decodes and RECOVER for both layers.
- Of the 531 MB uaMemory on rangers-fel, 516 MB are the demux ring and its helper (258 MB each). They belong to the source, not to FEL.

## Results

### Floor and thread split (F intro)

| Title | Renderer, % of one core | Threads, % of one core |
| --- | --- | --- |
| floor (base layer only in the element) | 2.0 to 2.2 | |
| rangers-fel, reference | 38.1 | one worker 20.9, one 8.3, one 4.9; demux stages 4.8 ms per s |
| fel-v6, reference | 24.8 | 12.2, 4.1, 3.9; GPU main 4.8 |

### Stage counters, bit-exact route (3 passes, renderer 38.3 / 38.7 / 38.5 % for rangers-fel)

| Stage | ms per second of playback | Thread |
| --- | ---: | --- |
| `entropy.bl.wasm` (CABAC of the 4K base layer, 1.65 MB/s in, 50 MB/s of commands out) | 208 | #4, 21.7 % |
| `entropy.el.wasm` (CABAC of the 1080p enhancement layer, 0.73 MB/s in, 17 MB/s out) | 84 | #5, 8.8 % |
| `video.recon.bl` (command stream to WebGPU passes) | 22 | #1 |
| `video.recon.el` | 8 | #1 |
| `video.present` (COMPOSE and DISPLAY submit) | 3.9 | #1 |
| `video.rpu` (RPU parse and table build) | 3.2 | #1 |
| `video.feed.bl` + `video.feed.el` (copies) | 2.3 | #1 |
| `entropy.*.out` (taking the command stream out of WASM) | 6.7 | #4, #5 |

Whole video worker 5.1 % (#1). Demux 0.9 % (#2). For fel-v6 the entropy stages are 123 (base layer) and 40 (enhancement layer) ms per second.

Reading: 76 % of the renderer cost on rangers-fel (68 % on fel-v6) is CABAC in WASM.

### Bit-exact against hardware (F.1, 3 passes each)

| Run | Renderer, % | Video worker, % | GPU process, % | Chrome total, % | Page RSS, MB | Video worker heap, MB |
| --- | --- | --- | --- | --- | --- | --- |
| rangers-fel, reference | 38.3 / 38.7 / 38.5 | 5.1 | 6.5 | 47.4 | 790 | 44 |
| rangers-fel, fast | 8.8 / 9.0 / 8.7 | 4.1 to 4.3 | 20.7 to 21.8 | 34.5 to 36.1 | 544 to 552 | 14 |
| fel-v6, reference | 24.2 / 23.9 / 23.9 | 4.6 | 11.0 | 39.0 | 775 | 10 |
| fel-v6, fast | 6.9 / 7.3 / 7.2 | 3.3 to 3.4 | 20.8 to 22.0 | 32.9 to 34.7 | 508 to 532 | 2.5 to 4.6 |

Output was 0 dropped in all 12 runs.

### Default change (F.2, commit 5130bb3)

| Page renderer, % of one core, 3 passes | rangers-fel.mkv | fel-v6.mp4 |
| --- | --- | --- |
| bit exact (`q=reference`), before | 38.3 / 38.7 / 38.5 | 24.2 / 23.9 / 23.9 |
| hardware, `q=fast` forced, same batch as the baseline | 8.8 / 9.0 / 8.7 | 6.9 / 7.3 / 7.2 |
| hardware as the default, later batch | 9.9 / 9.6 / 7.4 | 7.8 / 6.2 / 6.9 |
| GPU process, bit exact, then hardware | 6.5, then 17 to 23.5 | 11.0, then 17.7 to 23.7 |
| Chrome total, bit exact, then hardware | 47.4, then 28 to 39 | 39.0, then 28 to 37 |
| Page renderer RSS, MB | 790, then 550 | 775, then 514 to 534 |
| uaMemory, MB | 562, then 531 | 529, then 523 |

0 dropped in every run, 719 or 720 shown.

## Limitations

- The hardware route is about 60 dB against the bit-exact result (figure from `web/fel/README.md`, not measured again here). It is not bit exact.
- The bit-exact route cannot reach 15 %. Its entropy decode alone is 208 + 84 ms per second on rangers-fel. Only a faster entropy decoder moves it.
- Two titles, 30 s steady window, no seeks, no link shaping.
- The later batch ran slower and less evenly than the baseline batch (the same configuration gave 9.9 and 7.4), so differences under about 2 points between batches are not findings.

## Fixes

- 5130bb3: FEL selects `fast` by default (base and enhancement layer on the WebCodecs hardware decoders, the same integer compose). `?q=reference` and the quality menu keep the bit-exact route. This reverses the earlier rule that FEL is always shown bit exact.
- The 15 % renderer budget holds on both titles with the default change.
