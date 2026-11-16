# Dolby Vision 8.1: where the extra CPU of the canvas route goes

| Field | Value |
| --- | --- |
| Date | 2026-11-16 |
| Group | Dolby Vision |
| Type | Performance |
| Section | Profile 8.1, canvas route |
| Environment | MacBook Pro 14", Apple M4 Pro, macOS 27.2, Chrome 157.0.8103.51, window 1440x810 pt, FraxEngine `abbc45f` |
| Status | Current |

## Purpose

Dolby Vision 8.1 used about 28 % of a core against 9 to 14 % for HDR10 files on the element route
([comparison-player-engines-m4-pro.md](comparison-player-engines-m4-pro.md)). Find which part of the pipeline costs it.

## Implementation

| Field | Value |
| --- | --- |
| Test asset | `dv-p81.mp4` (HEVC Dolby Vision 8.1, 3840x1714, 24 fps, no audio); `hevc-4k-hdr10.mp4` (HEVC HDR10, 24 fps, with audio) |
| Baseline | the same HDR10 file on the element (`mse`) route |
| Methodology | A/B with the engine's debug parameters, each run 10 s settle and 30 s measure, CPU and RSS of the Chrome process tree per process type: default; `maxw=1920` (canvas capped at 1920 wide); `nodraw` (decode and metadata, no drawing); `dvsleep=0` (wake on every vsync); `q=native`; HDR10 file forced to the canvas (`mse=0`); `dvbase=1` (the base layer through the element). 1 to 2 runs per variant. Then a Chrome trace (`--trace-startup`, JSON, categories `gpu`, `viz`, `cc`, `media`, `webgpu`, `gpu.dawn`, `blink`, `v8`) of four variants, analysed as self time per thread and event over the last 6 s, with thread CPU time (`tdur`) where available. |
| Observations | The engine's open-frame count stayed at about 10 and there were no decode errors or decoder recoveries, so frames are not held too long. The fixture has no audio track, so audio is not a factor. |
| Margin of error | Default runs ranged from 22.3 % to 29.0 % on the same day, so differences under about 5 points are not findings. Tracing itself raised CPU (Chrome total about 35 % instead of about 23 %). |

## Results

### A/B (Chrome total CPU, % of one core)

| Variant | Total | GPU process | Renderer |
| --- | --- | --- | --- |
| Dolby Vision 8.1, default (canvas) | 22.3 to 29.0 % | 14 to 18.5 % | 7.4 to 9.9 % |
| canvas capped at 1920 wide | 24.3 % | 15.6 % | 8.2 % |
| no drawing (`nodraw`) | **8.7 to 9.7 %** | **3.5 to 3.8 %** | 4.6 to 5.3 % |
| wake on every vsync (`dvsleep=0`) | 22.3 % | 14.0 % | 7.4 % |
| HDR10 file, element route | 9.7 % | 5.7 % | 3.5 % |
| HDR10 file forced to the canvas (`mse=0`) | **24.4 %** | **14.8 %** | 9.0 % |
| Dolby Vision 8.1, base layer through the element (`dvbase=1`) | **15.6 to 15.8 %** | 8.3 to 8.6 % | 6.6 to 6.8 % |

### Trace (ms per second of wall time)

| Event (process / thread) | DV 8.1 canvas | HDR10 canvas | HDR10 element | DV 8.1 no draw |
| --- | --- | --- | --- | --- |
| `IOSurfaceImageBacking::WaitForCommandsToBeScheduled::Dawn` (GPU main), wall | 170.5 (7.1 ms x 24/s) | 126.2 (5.2 ms x 24/s) | absent | absent |
| same event, thread CPU | 0.4 | 0.4 | | |
| `CommandBufferService:PutChanged` (GPU main), CPU | 34.2 (about 100/s) | 31.1 | absent | absent |
| `Queue::Submit` (GPU main), CPU | 10.0 (96/s) | 8.4 (97/s) | absent | absent |
| `v8.callFunction` (renderer worker), CPU | 48.1 | 46.1 | 15.9 | 38.0 |
| `CARendererLayerTree::CommitScheduledCALayers` (GPU main), wall | | | 11.8 | |

### Reading

- The cost is the WebGPU canvas presentation, not Dolby Vision. A plain HDR10 file on the canvas costs the same, and
  turning drawing off brings Dolby Vision down to the element route's level.
- It does not scale with pixels (capping the canvas changed nothing). It is a per-frame cost in Chrome's command
  transport: the engine submits once per frame, Chrome issues about four submits per frame internally.
- The 5 to 7 ms wait per frame on the GPU main thread is blocked time, not CPU.
- The element route hands the decoded frame to a CoreAnimation layer and skips all of this.

## Limitations

- One machine and Chrome version; 1 to 2 runs per variant.
- `dvbase=1` drops Dolby Vision processing (dynamic metadata, display management, trims): the picture is the HDR10
  base layer tone mapped by macOS. Its RSS was higher in these runs (1.26 to 1.61 GB against about 1.15 GB); with two
  runs that difference is not established.

## Fixes

None yet. Whether desktop Chrome should default to the base layer for Profile 8.1 (`dvbase`) is an open decision.
