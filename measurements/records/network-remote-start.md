# Remote start: first frame and requests on a shaped link

| Field | Value |
| --- | --- |
| Date | Unknown |
| Group | Network |
| Type | `Performance` |
| Section | Remote start |
| Environment | macOS (version Unknown), headless Chrome with a temporary profile, Chrome version Unknown, machine and CPU Unknown, display Unknown. Engine commit e9af353 plus the reorder-depth look-ahead (commit Unknown). |
| Status | `Current` |

## Purpose

How fast the engine shows the first frame of a 4K HEVC file streamed over a slow remote link, and how many requests it makes before playback, compared with a plain `<video>` on the same bytes.

## Implementation

| Field | Value |
| --- | --- |
| Test asset | 4K HEVC, 39 Mbit/s, FLAC. Container and file name are not given in section A; the Result says "same file". |
| Baseline | The floor: a plain `<video>` on the same bytes over the same link. The floor was measured in the section A table and in the Result. |
| Methodology | A pass-through shapes a local file like the remote link. Section A shape: 66 Mbit/s cap, 125 ms to first byte, slow ramp, 30 to 40 s per run, load 1.3 to 2.2. Result shape: `/slow/85-75` (85 Mbit/s, 75 ms to first byte, the shape measured on the real path), 20 s, three passes. Request count and kbit/s come from the pass-through log. Start marks come from the engine, in ms after open. |
| Observations | Before the Result, the site ran on 1 MiB ranges, two at a time. The origin check classed the site's own media as a local file. Corrected in 5aebd53. The start scan always read 2 MiB (757 ms on the 66 Mbit/s link). The demux worker was blocked on the ring 98 % of the time while the link ran at 42 of 66 Mbit/s. Seeks on the same link, one pass: the first packet arrived 25 to 135 ms after a seek, but a seek outside the buffer showed its frame after 2.1 to 3.1 s (0.75 s for one far seek, 116 ms inside the buffer). See [playback-element-route-seeks.md](playback-element-route-seeks.md). |
| Margin of error | Unknown. No error margin is stated. The spread between passes is in the tables. |

## Results

### Section A table (66 Mbit/s, 125 ms to first byte)

| | first frame | requests before the first packet | link use while filling |
| --- | --- | --- | --- |
| floor | 671 ms | 1 | not measured yet |
| engine at the time of section A | 1685 to 1781 ms (3 runs) | 3 (0-0 probe, body, tail) | 42 of 66 Mbit/s |

Start split of the engine in that table (ms after open):

| Mark | ms |
| --- | --- |
| demux worker booted | 76 |
| opening request sent | 214 (the nested helper worker boots in series, 137) |
| first byte | 347 |
| header parsed | 733 (waits for the 1 MiB tail, which shares the link with the body) |
| start scan done | 1490 |
| first packet | 1602 |
| element ready | 1721 |

### Result (e9af353 and the reorder-depth look-ahead), `/slow/85-75`, 20 s, three passes

| | first frame ms | requests | requests before play |
| --- | --- | --- | --- |
| before | 1241 / 1253 / 1186 | 227 | 12 to 13 |
| one transport (HttpStreamSource everywhere) | 941 / 928 / 933 | 2 | 2 |
| lazy Cues, tail after the first packet | 855 / 839 / 855 | 2 | 2 |
| look-ahead sized by the SPS reorder depth | 611 / 616 / 616 (same session before: 859 / 846 / 866) | 2 | 2 |
| floor | 421 / 446 / 416 (earlier session 402 / 389 / 423) | 1 | 1 |

Gap to the floor: 190 ms, inside the budget of 300 ms.

The look-ahead: the Matroska demuxer held 16 packets of a track to rebuild decode timestamps (0.67 s of media, about 330 ms of download at 39 Mbit/s over 85). HEVC states its reorder depth in the SPS (`sps_max_num_reorder_pics`), so the demuxer now holds depth + 1 packets (2 to 5 on the local files). H.264, tracks without DefaultDuration, a shift over 500 ms, and any stream whose decode timestamp comes after its presentation timestamp keep the 16 packet window.

Startup marks after the change (ms after open):

| Mark | ms |
| --- | --- |
| open request | 27 |
| response | 107 |
| header parsed | 112 |
| start scan done | 297 (was 518) |
| workers inited | 388 |
| first packet | 390 |
| element ready and playing | 550 |

What is left of the gap: the scan still needs the first keyframe and depth + 1 packets (185 ms), audio set-up and worker init run after the scan instead of beside it (91 ms), and the element takes 160 ms from the first packet to its first frame.

## Limitations

- Measured on the pass-through, not on the real remote edge. The Result uses the `/slow/85-75` shape. The planned runs on the real title are not in the Result.
- The link-use budget (90 % of the link while the buffer is below target) is not measured in the Result.
- The floor has no seek pass to compare with. The seek figures above are one pass.
- The reorder-depth change does not apply to H.264 or to tracks without DefaultDuration.

## Fixes

- e9af353 and the reorder-depth look-ahead (commit Unknown). The Result measures one transport, lazy Cues with the tail after the first packet, and the look-ahead sized by the SPS reorder depth.
- 5aebd53: the origin check that sent the site's own media down the local file path was corrected.
