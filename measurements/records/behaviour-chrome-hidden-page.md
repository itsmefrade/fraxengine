# What Chrome does to playback in a hidden page

| Field | Value |
| --- | --- |
| Date | 2026-11-16 |
| Group | Playback behaviour |
| Type | Behaviour |
| Section | Hidden or occluded page |
| Environment | MacBook Pro 14", Apple M4 Pro, macOS 27.2, Chrome 157.0.8103.51, FraxEngine `abbc45f` |
| Status | Current |

## Purpose

Two files stopped presenting frames in a benchmark run (`vp9.webm`, `hevc-subs.mkv`). Find out whether the engine
stalls or the browser stops playback, and whether a plain `<video>` behaves the same.

## Implementation

| Field | Value |
| --- | --- |
| Test asset | `hevc-subs.mkv` (HEVC, two subtitle tracks, no audio), `vp9.webm` (VP9 with audio) |
| Baseline | Chrome's plain `<video src>` under the same conditions |
| Methodology | The page is opened in a normal tab; after 8 s a new foreground tab is opened through the DevTools protocol, which makes the player tab hidden. In one variant the cover tab is closed again at 16 s. The page logs `visibilitychange` and the media events, and every 2 s the element's `paused`, `currentTime` and the presented-frame count. 1 run per variant and engine. Covering the window with another window was also tried: Chrome marked the page hidden only occasionally (one run out of several), so that method was not used for the result. |
| Observations | With `--disable-backgrounding-occluded-windows --disable-renderer-backgrounding --disable-background-timer-throttling` the page never became hidden and both files played cleanly (2 of 2 runs each). |
| Margin of error | Not applicable (event order, not a quantity). 1 run per variant. |

## Results

| File | What Chrome does when the page is hidden | Engine state | When visible again | Plain `<video>` |
| --- | --- | --- | --- | --- |
| `hevc-subs.mkv` (no audio) | pauses the element in the same millisecond as `visibilitychange` (`pause` event, `paused: true`) | session state stays `playing` | `play` and `playing` fire, playback resumes at once (593 of 720 frames in the window) | same (591 of 720) |
| `vp9.webm` (with audio) | keeps playing (time and sound advance), stops presenting video after about 10 s | unchanged | Unknown (not measured for this file) | presents no frames while hidden (0 of 720) |

## Limitations

- One Chrome version on macOS. The delays (immediate pause, about 10 s for the video track) are Chrome's and may change.
- The engine's session state does not follow Chrome's pause of a video-only element; nobody sees it while the page is
  hidden, so it was left as is.

## Fixes

None in the engine. The benchmark method was changed: Chrome runs keep the window visible or use the three flags above.
