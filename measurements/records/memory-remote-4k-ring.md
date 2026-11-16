# Remote 4K memory with the read-ahead ring

| Field | Value |
| --- | --- |
| Date | Unknown |
| Group | Memory |
| Type | `Performance` |
| Section | Remote 4K, read-ahead ring |
| Environment | macOS (version Unknown), headless Chrome with a temporary profile, Chrome version Unknown, machine and CPU Unknown, display Unknown. Engine commit Unknown. |
| Status | `Current` |

## Purpose

How much memory the 4K remote path costs in the page renderer, and whether the fixed 256 MiB read-ahead ring can be replaced by a smaller ring without losing the network cushion.

## Implementation

| Field | Value |
| --- | --- |
| Test asset | `hevc4k-flac.mkv`, 4K HEVC with FLAC. |
| Baseline | The floor: a plain `<video>` on the same file over the same link. |
| Methodology | Result: `/slow/85-75`, 50 s, no seeks, headless, three passes. Ten-minute run: the same file looped to 18 minutes by stream copy, `/slow/85-75`, 600 s of playback, one pass each. Memory is the page renderer's RSS plus `measureUserAgentSpecificMemory`, and `vmmap -summary` physical footprint for what the process owns. CPU is the page renderer's % of one core. |
| Observations | RSS is the wrong ruler for the budget. About 240 MB of the renderer's RSS is resident shared library pages, and freed PartitionAlloc pages stay resident until the system wants them. At 8.5 minutes the PartitionAlloc region held 334 MB resident, of which 184 MB was dirty; the other 150 MB was freed and reclaimable. The script and WASM region (ring included) was 127 MB resident, 89 MB dirty, against 3 MB on the floor. The ring is resident once written. |
| Margin of error | Unknown. No error margin is stated. Passes are listed one by one in the tables. The engine ran at load 8 and the floor at load 2 to 5 in the ten-minute run. |

## Results

### Starting point (section D table)

| | page renderer RSS | of which |
| --- | --- | --- |
| floor | 283 MB | |
| engine before | 703 to 715 MB (800 to 808 MB once the one-request transport fills the ring) | 256 MiB ring (one SharedArrayBuffer, resident once written), up to 120 MiB of SourceBuffer video, script heaps 6 MB |

### Result, 50 s, `/slow/85-75`, three passes

| | page renderer RSS MB | page renderer CPU % | first frame ms |
| --- | --- | --- | --- |
| before (256 MiB ring) | 808 / 807 / 800 | 10.3 / 9.7 / 8.6 | 896 / 863 / 851 |
| after (64 MiB ring) | 594 / 616 / 619 | 6.3 / 7.5 / 7.7 | 872 / 837 / 838 |
| floor | 283 to 287 | 5.0 | 402 / 389 / 423 |

The "before" passes predate the measurement lock.

### Ten minutes, `/slow/85-75`, 600 s of playback, one pass each

| | footprint at 1 min | footprint at 8.5 min | peak footprint | RSS minute 2 | RSS minute 3.5 to 10 | dropped of 14374 |
| --- | --- | --- | --- | --- | --- | --- |
| engine | 316 MB | 301 MB | 422 MB | 509 MB | 584 to 693 MB | 21 |
| floor | 201 MB | 252 MB | 266 MB | 386 MB | 371 to 376 MB | 18 |

- Footprint: 115 MB above the floor after a minute, 48 MB above it after 8.5 minutes, no growth (316 to 301 MB). The budget of floor plus 150 MB holds on this ruler.
- RSS: 210 to 320 MB above the floor and not flat within 20 MB. It climbs until about minute 3.5 and then moves inside a 110 MB band without a trend (633, 660, 665, 633, 630, 667, 693, 676, 584, 619 MB every 32 s from minute 5).
- Where the 48 to 115 MB of footprint sits: the ring (64 MiB once it has wrapped), WASM memory and packets in flight between workers, and SourceBuffer bytes beyond what a plain element keeps.

## Limitations

- The budget asked for SourceBuffer 20 s or 48 MiB ahead and 10 s or 24 MiB behind, and a page renderer at most 150 MB above the floor and flat within 20 MB between minutes 2 and 10. The RSS part of that budget is not met. The footprint part is met on this run.
- The ten-minute run is one pass each, at unequal load.
- Open decision (owner): a 48 MiB ring with 16 MiB behind would save another 16 MB. It was not taken. The canvas routes (FEL, HDR10+, profile 5) have no SourceBuffer, and the ring is their only network cushion. 48 MiB ahead is about 5 s at 80 Mbit/s (it was up to 160 MiB). A smaller ring only on the element route is possible, at the price of a second sizing rule.

## Fixes

- The fixed 256 MiB ring was reduced to 64 MiB, with a quarter (16 MiB) kept behind the read position, so 48 MiB ahead. Commit Unknown.
