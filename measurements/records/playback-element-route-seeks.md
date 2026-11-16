# Element route seeks against the floor

| Field | Value |
| --- | --- |
| Date | Unknown |
| Group | Playback performance |
| Type | `Performance` |
| Section | Seeks |
| Environment | macOS (version Unknown), headless Chrome, Chrome version Unknown, machine and CPU Unknown, display Unknown. Before: commit a9ef3bc. After: the commits listed under Fixes (the commit measured in H.4 is not named). |
| Status | `Current` |

## Purpose

On the remote shape, a seek outside the buffer showed its picture after 0.7 to 3.7 s, and a seek back to the start after 6.8 s, although the first packet arrived 25 to 135 ms after the seek (section A finding). This record measures how far the element route closes the gap to a plain `<video>`.

## Implementation

| Field | Value |
| --- | --- |
| Test asset | `hevc4k-flac.mkv` (4K HEVC, 39 Mbit/s, FLAC, keyframes 1 to 10 s apart), Matroska. `zz-hevc-4k-hdr10.mp4` (the same pictures with AAC, 38 Mbit/s, `moov` at the end), MP4. |
| Baseline | The floor: a plain `<video>` on the same bytes over the same link. The floor takes 1.1 to 1.5 s for the far forward seeks and 3.1 s for the seek to 6 s, because it also downloads from the keyframe. |
| Methodology | Headless Chrome with an own pass-through on `/slow/85-75` (75 ms to the first byte, 85 Mbit/s). `perf.html` and `perf-floor.html` run one plan: three near seeks from 38 s (`seekat=38`), five far ones, and a seek to 0. Time runs from the seek call to the first presented picture at the target (`requestVideoFrameCallback`, media time within one picture of the target). Three passes per row. Each run held the shared measurement lock. Other jobs ran Chrome and builds meanwhile (load 6 to 26). The pass-through does not penalise a new request the way the real edge does. |
| Observations | The three passes of a row lie within 20 ms of each other in most rows. Seek latency followed the load little. The H.2 attribution used marks in the engine, from one pass. |
| Margin of error | Unknown. No formal margin is stated. The source says the three passes of a row lie within 20 ms of each other in most rows. |

## Results

### H.1 The floor is not fast where the GOP is long

Keyframes up to 10 s apart at 39 Mbit/s mean up to 49 MB to download for a far seek. The plain element takes 160 ms or less only where Chrome still holds the bytes: a seek back to a place already played, and the seek to 0 on the MP4 (17 to 28 ms), need no request. The target is this floor, not zero.

### H.2 and H.3 Attribution and steps (summary)

- Attribution (before, Matroska): the video lane, not the link, was the bottleneck. The download ran at 85 Mbit/s, but video reached the SourceBuffer at about half that rate. The loop yielded every 16 packets, and 176 packets (38 MB) waited in the lane. Every seek flushed, even inside the buffer. The demux worker was blocked in a read when the seek came. The element was told its target only after 300 ms past it was appended. On MP4, a target within 3 s after a keyframe started a whole GOP early.
- Steps (one pass each, Matroska): the far back seek to 6 s went 3764 ms, then 3579, 3276 and 3183 ms as the commits below were added. The near back 5 s seek went 3916, 3730, 3725 and 3635 ms. The seek to 0 went 564 ms, then 197 ms.

### H.4 Result, three passes, ms from the seek call to the picture at the target

Matroska, 4K HEVC 39 Mbit/s, FLAC:

| seek | target s | before (a9ef3bc) | after | floor |
| --- | --- | --- | --- | --- |
| near forward 10 s | 48 | 5397 / 5395 / 5396 | 349 / 331 / 321 | 386 / 380 / 383 |
| near back 5 s | 44.9 / 45 | 6132 / 6030 / 5770 | 210 / 207 / 207 | 205 / 208 / 208 |
| near forward 30 s | 76.9 | 3094 / 3113 / 3391 | 1536 / 1541 / 1532 | 1230 / 1258 / 1229 |
| far forward | 96 | 2606 / 2489 / 3288 | 1376 / 1391 / 1379 | 1223 / 1131 / 1224 |
| far back | 18 | 790 / 727 / 756 | 278 / 274 / 271 | 160 / 162 / 162 |
| far forward | 66 | 3584 / 3567 / 3063 | 1218 / 1179 / 1203 | 1472 / 1471 / 1470 |
| far back | 6 | 6806 / 6839 / 5805 | 3179 / 3180 / 3186 | 3083 / 3082 / 3088 |
| far forward | 84 | 3146 / 3151 / 3078 | 1483 / 1485 / 1487 | 1244 / 1240 / 1242 |
| start back to 0 | 0 | 527 / 534 / 534 | 195 / 198 / 198 | 527 / 523 / 523 |
| first frame | | 646 / 716 / 704 | 618 / 554 / 662 | 442 / 375 / 374 |
| steady dropped | | 0 of 240 / 0 of 241 / 0 of 240 | 0 of 240 / 0 of 240 / 1 of 240 | 0 of 239 / 0 of 239 / 0 of 239 |

MP4, 4K HEVC 38 Mbit/s, AAC:

| seek | target s | before (a9ef3bc) | after | floor |
| --- | --- | --- | --- | --- |
| near forward 10 s | 48 | 5351 / 5330 / 5330 | 328 / 313 / 336 | 301 / 300 / 301 |
| near back 5 s | 44.9 / 45 | 5871 / 5817 / 5606 | 212 / 206 / 206 | 206 / 206 / 206 |
| near forward 30 s | 76.9 | 6653 / 6656 / 7565 | 1371 / 1132 / 989 | 1215 / 1215 / 1213 |
| far forward | 96 | 2769 / 2772 / 2923 | 1194 / 1187 / 1190 | 1018 / 1018 / 1023 |
| far back | 18 | 4820 / 4826 / 4833 | 206 / 205 / 199 | 158 / 158 / 161 |
| far forward | 66 | 3356 / 3364 / 3371 | 1065 / 1058 / 1061 | 977 / 978 / 979 |
| far back | 6 | 6519 / 6496 / 6501 | 2921 / 2919 / 2931 | 2908 / 2907 / 2908 |
| far forward | 84 | 5920 / 5930 / 5929 | 1291 / 1288 / 1296 | 1253 / 1207 / 1206 |
| start back to 0 | 0 | 603 / 602 / 600 | 106 / 104 / 109 | 17 / 28 / 25 |
| first frame | | 661 / 687 / 673 | 607 / 689 / 673 | 656 / 549 / 526 |
| steady dropped | | 0 of 241 / 0 of 240 / 0 of 240 | 0 of 240 / 0 of 239 / 0 of 241 | 0 of 238 / 0 of 239 / 0 of 239 |

Against the budget (floor + 300 ms): met in every row of both files. The widest gaps are the Matroska near forward 30 s seek (+274 to +312 ms) and the far forward seeks to 84 s (+241 to +245) and 96 s (+153 to +260). The Matroska seek to 0 and the far forward seek to 66 s are faster than the floor.

Checked in every after pass: the session returned to playing, the counted picture is the target's, and no picture was dropped in the 2 s after any seek (before: up to 15 of 48 after far seeks). Steady play, 60 s on the same link: 0 of 1442 pictures dropped after, 0 of 1442 before.

### H.5 What remains against the floor

- The decode after the download (90 to 220 ms per far seek): Chrome's MSE seek waits until the target is buffered and only then decodes from the keyframe, while a plain element decodes as the bytes arrive. Starting the element at the keyframe would show the wrong picture first, so this stays.
- Audio through decode and FLAC (Matroska file, up to 130 ms): the first PCM at the target has to fill a FLAC block before it is appended. The MP4 (AAC passed through) does not pay this, and is 90 to 180 ms from the floor on far seeks.
- Bytes the floor still holds. The engine holds 16 MiB behind the reader. A far seek back to a place played before costs a request (270 ms against 160 ms on the Matroska file, 200 against 160 on the MP4). The seek to 0 on the MP4 takes 104 ms against 17 to 28.
- After a seek that flushed, nothing behind the target is in the buffer. The audio before the target is dropped, so a step back right after a far seek flushes again (3.6 s in the step table's second row). Keeping the audio from the keyframe on would make that an in-buffer seek back to the keyframe.
- A second request on the seek to 6 s: the demuxer steps back more than the 128 KiB margin after the first restart (two requests 82 ms apart). About 85 ms of 3.2 s. Not changed, because the margin was sized on the real edge.
- Not on WebKit: the fragment cut, the early element seek and the in-buffer seek are Chrome only, and were not measured there.

## Limitations

- Not measured: the real edge, a visible window, files with short GOPs (where the far seeks have little to download), and the canvas routes, which the lane and interrupt changes also reach.
- The pass-through does not penalise a new request the way the real edge does (slow first megabyte), so nothing here says how often to restart a download.
- The commit measured as "after" in H.4 is not named in the source.

## Fixes

- 1639c64: the loop gives its queue a turn after every read that waited and while a lane is held.
- ddd5c1e: the video fragment closes at the first packet whose decode time is past the target, the audio fragment at the first frame that sounds there, then both every 80 ms until playback starts.
- 29a9042: the element is pointed at the target when the worker reports the buffers flushed. The worker declares a live seekable range so Chrome does not drop the seek.
- 291b5b9: a seek interrupts the loop's blocked read through a shared count (seek call to seek start 0 to 3 ms).
- b6bdb7f: a target inside the element's buffered range, or up to 5 s past the end of the range being played when that much is downloaded, is the element's own seek.
- 91266e7: the MP4 seek starts at the audio stored in front of the keyframe, and the 3 s lookback goes for MP4 and MOV.
