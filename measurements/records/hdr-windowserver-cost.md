# 10-bit video and WindowServer cost in Chrome

| Field | Value |
| --- | --- |
| Date | 2026-11-08 |
| Group | HDR output |
| Type | `Performance` |
| Section | WindowServer and 10-bit video cost, Chrome vs AVPlayer |
| Environment | Apple M4 Pro, built-in XDR display (screen 1512x982 pt), lid open, simulator closed, machine not in use by the user. macOS version Unknown. Chrome version Unknown (stock build). AVPlayer through the bench apps AVBench and AetherBench, versions Unknown. Engine commit Unknown. |
| Status | `Current` |

## Purpose

Where does the "10 times" gap between native playback and Chrome come from? Earlier benchmarks counted only the player
processes. They left out the macOS WindowServer and the VideoToolbox decoder service (VTDecoderXPCService).

## Implementation

| Field | Value |
| --- | --- |
| Test asset | One source clip, 3840x1714, 24 fps, silent, in five versions: 4K HDR10 (stream copy), 4K SDR 10 bit, 4K SDR 8 bit, 1080p HDR10, 1080p SDR 8 bit (VideoToolbox HEVC). Container, bitrate and codec of the other versions Unknown. |
| Baseline | Chrome `<video>` (all Chrome processes) against the native players, AVBench and AetherBench (the AVPlayer column). |
| Methodology | Visible 1920x1080 pt window, fitted to the screen, the same for every player. 15 s settle, then a 30 s measure window. Cumulative CPU time from `ps -o time`, in % of one core. Total = player + VideoToolbox + WindowServer. Section 1 values are the average of two repeats. The 4K HDR10 variant runs have two repeats each. The repeat count for the section 3 page-side runs is Unknown. |
| Observations | The earlier AVPlayer figure of 1.3 was wrong: the VideoToolbox service had been read as 0. 8-bit Chrome is close to AVPlayer, and its WindowServer share is lower than AVPlayer's. Screen captures in the flag tests show HDR kept in all variants (brightest points 2.4 to 3.5). The frames differ between captures, so these values cannot be compared. |
| Margin of error | Unknown. Two repeats per value, and the spread between them is not recorded. |

## Results

### Chrome vs AVPlayer (% of one core)

| Clip | Chrome total (WindowServer) | AVPlayer total (WindowServer) |
| --- | --- | --- |
| 1080p SDR 8 bit | 27 (16) | 21 (17) |
| 1080p HDR10 | 46 (33) | 20 (16) |
| 4K SDR 8 bit | 30 (17) | 28 (23) |
| 4K SDR 10 bit | 44 (29) | 29 (24) |
| 4K HDR10 | 45 (30) | 20 to 32 (15 to 27) |

- 8-bit: Chrome is close to AVPlayer.
- 10-bit, even SDR: WindowServer uses 29 to 33. The cost does not depend on resolution (1080p HDR10 costs as much as 4K).
- The real gap for 4K HDR10 is about 1.5 to 2 times, not 10 times.

### 4K HDR10, flag tests (% of one core, two repeats)

| Variant | Chrome | WindowServer | Total |
| --- | --- | --- | --- |
| Default | 11.2 | 29.5 | 44.6 |
| `--disable-features=UseCALayerContentsHeadroom` | 11.2 | 29.7 | 45.1 |
| `--disable-features=avfoundation-overlays` (AVSampleBufferDisplayLayer off) | 9.6 | 14.7 | 28.4 |
| 4K SDR 10 bit, AVSampleBufferDisplayLayer off | 9.3 | 16.2 | 29.4 |
| Fullscreen (`requestFullscreen`) | 10.5 | 25.2 | 40.0 |
| Fullscreen + `--enable-features=FullscreenLowPowerBackdropMac` | 10.7 | 25.7 | 40.9 |

The total includes VideoToolbox, so it is not the sum of the two columns.

With the AVSampleBufferDisplayLayer path off, WindowServer falls by about half and the total is at AVPlayer level.
The headroom flag has no effect with the value fixed at 16.

### 4K HDR10, page-side options (no flags, % of one core)

| Variant | Chrome | WindowServer | Total |
| --- | --- | --- | --- |
| `<video>` | 11.2 | 29.5 | 44.6 |
| `<video>` cropped 2 px (`object-view-box`) | 11.2 | 30.0 | 45.2 |
| `<video>` with `object-fit: cover` | 11.2 | 29.9 | 44.9 |
| MSE `<video>` in AetherWeb | 17.1 | 23.5 | 44.3 |
| WebGPU canvas in AetherWeb (compositor) | 29.6 | 11.0 | 45.1 |
| Bare WebGPU canvas (WebCodecs, import, single draw) | 21.0 | 12.9 | 39.3 |
| Decode only, no draw (baseline) | 5.8 | 6.3 | 17.0 |

- Cropping does not take the video off AVSampleBufferDisplayLayer. Chrome should do the crop on a separate layer.
- The canvas brings WindowServer down to AVPlayer level, but Chrome's own share rises. Chromium's `external_texture_helper.cc`
  copies P010 (zero copy only for NV12), which costs about 6.5 points, and presentation adds more.
- The bare canvas is the best page path: 44.6 down to 39.3. The engine's own compositor passes (RECOVER and COMPOSE)
  add about 6 points on top of it.
- Fullscreen gains 4 to 5 points.

### Chromium source reading

- Chromium `ui/accelerated_widget_mac/ca_renderer_layer_tree.mm`: 8-bit video is downgraded to a plain CALayer by
  `DowngradeAVLayersToCALayers()`. Above 8 bit (`video_type_can_downgrade_ = false`), the video stays on
  AVSampleBufferDisplayLayer. Each frame is queued as a new CMSampleBuffer with `DisplayImmediately` and timestamp 0.
- The fullscreen low-power backdrop path (`FullscreenLowPowerBackdropMac`) is off by default.

### Summary

- Stock Chrome can reach about 39 (bare canvas) to about 40 (fullscreen `<video>`) for 4K HDR10. AVPlayer measured 20 to 32.
- Two parts of the remaining gap are inside Chrome: the P010 import copy, and the 10-bit `<video>` path through
  AVSampleBufferDisplayLayer.
- The source says the bare canvas path for HDR10 is about 5 points better. It is not yet checked for audio and video sync,
  seek or HDR accuracy. An earlier combined-shader compositor did not match this path exactly, and for HDR10 that match is not needed.

## Limitations

- One machine, one display (built-in XDR), lid open. External displays were not tested.
- The clips are silent. Audio, audio and video sync, seek and HDR accuracy were not measured.
- Two repeats per value. The spread is not recorded.
- The Chrome and macOS versions are not recorded. The numbers apply to the Chrome build in use at the time.
- The Chromium code is cited by file path. The commit is not stated, and the source does not check the installed binary's build flags.
- The bare canvas result is not checked for sync, seek or HDR accuracy.

## Fixes

None in this repository. The source proposes an upstream Chromium change: give 10-bit video an IOSurface CALayer, as 8-bit
video already has, or queue timestamped or timebase-driven sample buffers. It also proposes adding P010 to the zero-copy
list. The source says the data is enough for an upstream bug report. No report or code change is recorded.
