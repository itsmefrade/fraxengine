# Blu-ray M2TS on the element: route policy and H.264 renumbering

| Field | Value |
| --- | --- |
| Date | Unknown |
| Group | Playback performance |
| Type | Compatibility |
| Section | Route policy and Blu-ray H.264 |
| Environment | Headless Chrome with a temporary profile (test bed). macOS, version Unknown. Machine, CPU, Chrome version and display: Unknown. Engine: e36787e for the renumbering (section C), Unknown for the route run (section B). |
| Status | Current |

## Purpose

Does a Blu-ray M2TS stay on its playback route when its data arrives slowly? Does the PPS renumbering let Blu-ray H.264 play on the `<video>` element (MSE route), and how far does it cover whole discs?

## Implementation

| Field | Value |
| --- | --- |
| Test asset | Blu-ray M2TS files: `amaze-atmos.m2ts` (Dolby Amaze, Atmos, 63.5 s), `dtsx-demo.m2ts` (DTS:X demo, 33.2 s), `truehd71.m2ts` (interlaced, MBAFF). For the coverage count only: a TrueHD 7.1 channel check (94.5 s) and a DTS-HD MA 7.1 sound check (87.8 s). A 4K sample for the route check (file not named in the source). Resolution, frame rate and bitrate: Unknown. |
| Baseline | Section C: a plain `<video>` on an ffmpeg stream copy of the same file, video only, no DTS (the floor). Section B: none stated. |
| Methodology | Section B: `amaze-atmos.m2ts` on the shaped link (66 Mbit/s cap, 125 ms to first byte, slow ramp). The route and the reopen come from the engine's events. Section C: `dtsx-demo.m2ts` on the element, before and after the fix, with per-thread and per-process CPU from `web/player/tools/perfrun.py` (% of one core). Coverage: a whole-file count of distinct PPS contents under one id, demux only, on each Blu-ray M2TS available locally. |
| Observations | See the list below. |
| Margin of error | Section C after: spread of the three runs as listed in Results. No error margin recorded: Unknown. Repeat count of the section B run and of the floor and before runs: Unknown (section B reports one event). |

Observations:

- The amaze run had no decoder error. The element was waiting for data.
- Blu-ray H.264 redefines PPS id 0 for every picture (2 to 4 contents on the samples). Chrome moves the parameter sets into the format description, and VideoToolbox keeps the first content of an id. A plain `<video>` on an ffmpeg stream copy fails with -12909 the same way, so the floor itself was broken. With one id per content the same element plays.
- `truehd71.m2ts` is interlaced (MBAFF). Chrome has no hardware path for it.

## Results

### Section C: `dtsx-demo.m2ts`

| Run | Route at the end | Shown | Renderer, % of one core | Chrome total, % of one core |
| --- | --- | --- | --- | --- |
| before | WebCodecs, after -12909 | 20 fps | 37 to 43 | 44 to 50 |
| after (3 runs) | MSE element | 673 / 673 / 673 | 9.8 / 11.7 / 12.4 | 22.0 / 25.0 / 26.4 |
| floor (video only, no DTS) | plain element | 600 of 600 | 1.4 | 10.5 |

WASM audio decode alone:

| Stream | ms per second of playback |
| --- | --- |
| DTS:X | 23 to 30 |
| TrueHD Atmos | 17 |
| FLAC | 0.7 |
| PCM to FLAC for the element | 0.9 to 1.5 |

`truehd71.m2ts`: its own software decoder costs 37 % (two decoder threads 9.4 + 7.2, media thread 7.3). There is no floor for it.

### Section B: `amaze-atmos.m2ts` on the shaped link

| Event | Result |
| --- | --- |
| MSE path failed: no progress for 3 s with 2.1 s buffered. Reopened at 6.1 s on the WebCodecs path. | Playback clock 17.5 s after 30 s of wall time. The reopen costs 9 to 19 s of playback. |

The budget (zero route changes without a decoder error) and the planned route test (amaze on the shaped link, and the 4K sample on a 20 Mbit/s link) have no result in the source.

### Coverage of the renumbering (demux only, whole file)

| File | Length | Contents under id 0 | Last new content at | Contents first seen after the 2 MiB start scan |
| --- | ---: | ---: | --- | ---: |
| Dolby Amaze (Atmos) | 63.5 s | 4 | 0.1 MB, 0.5 s | 0 |
| DTS:X demo | 33.2 s | 2 | 1.3 MB, 2.09 s | 0 |
| TrueHD 7.1 channel check | 94.5 s | 1 | start | 0 |
| DTS-HD MA 7.1 sound check | 87.8 s | 1 | start | 0 |

The 673 / 673 / 673 run covers only the case where every content is inside the start scan and so in the avcC.

Late case: the start scan was shortened to 512 KiB for one run of the DTS:X demo, whose second content first appears at 1.3 MB. Route MSE element, 673 shown, 0 dropped, no error, and all seeks played (near, far, and back to the start). The scan length was restored afterwards.

## Limitations

- No feature-length (2 hour) disc M2TS was available on the test machine. The count over a whole film is not measured.
- The late case was exercised once.
- The 18-content limit per original id (the id itself, sixteen 9-bit ids, id 255) is not measured beyond the count above. Above it, the renumbering gives up only when more than 18 contents are in use inside one GOP.
- `truehd71.m2ts` has no hardware path; nothing in the engine can remove its software decode cost.
- Section B's route rule is a planned change; the source does not say it is in the code.

## Fixes

- e36787e: one PPS id per content for Blu-ray H.264, so the element plays these files. The fallback route is no longer entered for them.
- Renumbering limit: when no id is free, the id of a content that no slice has named since the last IDR picture and that no original id maps to is reused (`recycled` counter). When more than 18 contents are in use inside one GOP, the demux worker posts one `pps-renumber-overflow` log line per track. Commit: Unknown.
- Section B (route watchdog): the source names the "no progress" trigger of `#mseWatchdog` as a deletion target. It does not say the change is done.
