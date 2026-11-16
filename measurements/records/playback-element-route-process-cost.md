# Element route: GPU and browser process cost above the floor

| Field | Value |
| --- | --- |
| Date | Unknown |
| Group | Playback performance |
| Type | `Performance` |
| Section | Element route, GPU and browser process |
| Environment | macOS (version Unknown), own Chrome with a temporary profile, Chrome version Unknown, machine and CPU Unknown. Visible window for G.2, headless for G.3. Lid open, no Simulator. Engine commit 51a4ecb for the fix. |
| Status | `Current` |

## Purpose

On the MSE element route every sample, whatever the codec, cost about 5 points more in the GPU process and about 5 more in the browser process than a plain `<video>` (Chrome total 22 to 30 % against 10 to 15 %). This record finds the cause, the fix, and what is left against the floor.

## Implementation

| Field | Value |
| --- | --- |
| Test asset | `zzlocal/hevc4k-flac.mkv` (4K HEVC Main 10 with FLAC) for G.1 to G.3. `fixtures/zz-h264-1080p.mp4` (1080p H.264 High with AAC) for G.2. A fragmented MP4 copy of the 4K file, and the Matroska file, for the bare MSE floor in G.3. |
| Baseline | The floor: a plain `<video>` on the same file. G.2 also runs "floor + starved element" (one hidden `<video>` on an empty MediaStream next to the playing one, no engine code), as the stand-in for the old behaviour. |
| Methodology | Local files, no link shaping, 30 s steady playback, no seeks. Each run inside the shared measurement lock. Per-thread CPU from `web/player/tools/perfrun.py`, which now covers the browser process. Chrome trace (`--trace-startup`, categories toplevel, viz, cc, ui, views) for G.1 only; the trace run is not a CPU measurement. G.2: three interleaved passes per row, visible window. G.3: three interleaved passes, headless. |
| Observations | The machine was shared and ran at two speeds: whole runs differ by about a factor of two in every process at once, so only differences that hold in every pass count. The network service IO thread cost 0.2 % while the tab was loading, against 1.3 to 1.8 % otherwise (not attributed). WindowServer is outside Chrome's total: 34 to 38 % with the spinner, 25 to 30 % after, 26 to 32 % on the floor, in a visible window. |
| Margin of error | Unknown. No error margin is stated. Passes are listed one by one. |

## Results

### G.1 Attribution

Per thread, the extra sat on `CrBrowserMain` (2.9 to 3.9 % against 0.4 to 0.5) and `CrGpuMain` (2.8 to 3.7 against 1.5 to 2.0). Events per second over 14 s of steady playback, 4K HEVC, headless:

| Per second | engine before | engine after | floor |
| --- | --- | --- | --- |
| browser process, `TabIcon::PaintLoadingAnimation` | 30.7 | 0 | 0 (8 paints in all, at the start) |
| browser process, UI compositor frames (`SingleThreadProxy::DoComposite`) | 31.9 | 2.3 | 2.2 |
| browser main thread tasks | 635 | not counted | 120 |
| GPU process, `Display::DrawAndSwap` | 47.4 | 25.4 | 25.3 |

The page's `load` event never fired before the fix (`loadEventMs` null, `document.readyState` not `complete`). After it, the event fired 114 to 175 ms after the start (floor 244 to 317 ms, visible window).

Cause: `Session.open()` made a hidden `<video>` on a `MediaStreamTrackGenerator` for the WebCodecs-to-element route. A media element holds the document's load event back until it has data. On the MSE and compositor routes that element never got a frame, so a session opened before the page's load event left the tab in its loading state. Chrome repainted the spinner about 30 times a second.

| Candidate | Test | Result |
| --- | --- | --- |
| Loading spinner, because a media element that never gets data holds the load event | Floor page with and without `?starve=1`; engine with the element made at open (before) and on demand (after) | The whole finding. Browser main thread 1.9 to 4.9 % with the element against 0.3 to 0.6 without; GPU main thread 2.1 to 4.9 against 1.0 to 2.7. The starved floor costs what the engine cost. |
| The same element added after the load event | Floor page with `?starve=late`, headless, 3 passes | Nothing: browser 1.8 / 1.8 / 1.8 against 1.6 / 1.5 / 2.0. The cost exists only for sessions opened while the page loads. |
| MSE itself in Chrome | `perf-mse.html`: the same bytes as a fragmented MP4 in one SourceBuffer, against the floor on that file | Nothing in these two processes: GPU 3.1 / 3.7 / 3.0 (worker) and 2.9 / 3.0 / 3.1 (main) against 2.9 / 3.0 / 3.0. |
| MediaSource in a worker instead of on the main thread | `perf-mse.html` against `perf-mse.html?ms=main` | Chrome total 8.6 / 9.6 / 8.5 against 7.7 / 7.7 / 8.6. Under the 2 point bar, not a finding. |
| Lost overlay or direct composition path | Not tested by itself | No sign of one. Inferred, not read from the layer tree. |
| Per frame work of the page | Not tested one by one | Renderer work only (`CrRendererMain` 0.7 against 0.2 to 0.3 at 4K). |
| Cross-origin isolation | Read, not measured | The same COOP and COEP headers for the floor page and the engine page. |

### G.2 Fix and result

The fix deletes the element from every route that does not use it. It is made when the video worker first asks to present through it (`#presentEl` in `web/player/js/session.js`). The MSE element route and every compositor route never ask, so they never have one. Nothing else changed.

Visible window, three interleaved passes, percent of one core. 4K HEVC (`zzlocal/hevc4k-flac.mkv`):

| | browser | of which `CrBrowserMain` | GPU process | of which `CrGpuMain` | `VizCompositorThread` | page renderer | Chrome total | WindowServer | shown / dropped |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| floor | 1.4 / 1.8 / 1.7 | 0.3 / 0.3 / 0.3 | 3.5 / 5.1 / 5.1 | 1.1 / 1.6 / 1.6 | 0.7 / 1.0 / 1.0 | 1.3 / 2.0 / 2.0 | 7.5 / 10.5 / 10.5 | 26.2 / 27.2 / 27.1 | 719 / 719 / 718, 0 dropped |
| floor + starved element | 4.7 / 2.6 / 2.5 | 3.7 / 2.0 / 1.9 | 10.3 / 6.0 / 5.8 | 3.9 / 2.1 / 2.1 | 2.2 / 1.3 / 1.2 | 2.5 / 1.4 / 1.4 | 18.3 / 10.1 / 9.9 | 34.9 / 34.5 / 34.0 | 720 / 719 / 718, 0 dropped |
| engine before | 5.5 / 3.7 / 5.0 | 4.3 / 2.8 / 3.9 | 11.5 / 8.2 / 11.0 | 4.3 / 2.9 / 3.9 | 2.4 / 1.9 / 2.5 | 6.1 / 4.8 / 6.3 | 23.5 / 16.9 / 22.4 | 34.6 / 34.3 / 35.7 | 722 / 721 / 720, 0 dropped |
| engine after | 2.5 / 2.7 / 1.9 | 0.6 / 0.6 / 0.3 | 7.3 / 7.5 / 3.6 | 2.1 / 2.1 / 1.0 | 1.7 / 1.8 / 0.8 | 6.8 / 7.0 / 3.2 | 18.2 / 18.9 / 9.1 | 29.9 / 29.2 / 25.1 | 721 / 722 / 722, 0 dropped |

Committed fix (51a4ecb), visible window, three interleaved passes. The machine was busier than in the table above (load 5 to 48):

| | browser | of which `CrBrowserMain` | GPU process | of which `CrGpuMain` | `VizCompositorThread` | page renderer | Chrome total | shown / dropped |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 4K HEVC, floor | 3.2 / 3.5 / 2.8 | 0.7 / 0.8 / 0.6 | 4.0 / 3.9 / 3.8 | 1.2 / 1.2 / 1.2 | 0.8 / 0.8 / 0.8 | 1.6 / 1.5 / 1.5 | 10.3 / 10.3 / 9.5 | 719 / 718 / 719, 0 dropped |
| 4K HEVC, floor + starved element | 4.9 / 3.8 / 2.8 | 3.7 / 2.8 / 2.0 | 5.5 / 5.4 / 5.4 | 1.9 / 1.8 / 1.8 | 1.2 / 1.2 / 1.2 | 1.6 / 1.5 / 1.9 | 12.5 / 11.3 / 10.7 | 719 / 719 / 718, 0 dropped |
| 4K HEVC, engine | 3.6 / 4.2 / 2.0 | 0.9 / 1.6 / 0.5 | 4.1 / 4.5 / 4.3 | 1.1 / 1.2 / 1.2 | 1.0 / 1.1 / 1.1 | 3.8 / 4.0 / 3.7 | 12.6 / 13.8 / 11.2 | 721 / 721 / 721, 0 dropped |
| 1080p H.264, floor | 3.2 / 2.3 / 2.4 | 0.7 / 0.8 / 0.8 | 3.2 / 4.2 / 3.7 | 1.3 / 1.7 / 1.5 | 0.8 / 1.1 / 0.9 | 1.3 / 1.7 / 1.6 | 9.0 / 9.5 / 9.1 | 719 / 719 / 719, 0 dropped |
| 1080p H.264, floor + starved element | 4.6 / 5.0 / 4.2 | 3.4 / 3.8 / 3.0 | 5.3 / 8.5 / 5.8 | 2.2 / 3.6 / 2.4 | 1.3 / 2.2 / 1.6 | 1.5 / 2.0 / 1.5 | 12.0 / 15.4 / 11.4 | 720 / 719 / 720, 0 dropped |
| 1080p H.264, engine | 2.5 / 2.6 / 2.3 | 0.9 / 0.8 / 0.6 | 3.1 / 4.3 / 4.5 | 1.2 / 1.7 / 1.8 | 0.8 / 1.1 / 1.2 | 2.0 / 2.8 / 2.9 | 8.6 / 10.8 / 10.5 | 721 / 721 / 721, 0 dropped |

Engine against floor in this batch: GPU process +0.1 to +0.6 at 4K and -0.1 to +0.8 at 1080p; browser process -0.8 to +0.7 and -0.7 to +0.3. All under the 2 point bar. The starved floor is 1.5 to 4.3 above the floor in the GPU process. The engine's Chrome total is +1.7 to +3.5 above the floor at 4K and -0.4 to +1.4 at 1080p, and the page renderer accounts for that gap (+2.2 to +2.5 and +0.7 to +1.3).

Compositor routes had the same never-fed element. Dolby Vision 8.1 4K60 on the canvas, headless, two passes each: the load event now fires (359 and 412 ms), and the browser main thread goes from 2.9 / 3.9 to 1.1 / 1.0 %. The total does not move outside the spread (Chrome total 30.9 / 31.7 before, 30.5 / 31.4 after). On that route the canvas frames, not the spinner, set the cost of both processes.

### G.3 What remains against the floor

Headless, 4K HEVC, three interleaved passes, after the fix. The third pass ran at load 8 to 23.

| | browser | GPU process | page renderer | Chrome total | shown / dropped |
| --- | --- | --- | --- | --- | --- |
| floor, fragmented MP4 | 1.6 / 1.7 / 1.6 | 2.9 / 3.0 / 3.0 | 1.4 / 1.5 / 1.6 | 7.3 / 7.3 / 7.4 | 717 / 717 / 717, 0 dropped |
| floor, Matroska | 1.6 / 1.5 / 2.0 | 3.2 / 2.9 / 3.0 | 1.6 / 1.4 / 1.5 | 8.0 / 7.1 / 7.6 | 719 / 719 / 719, 0 dropped |
| bare MSE in a worker | 1.7 / 2.2 / 2.0 | 3.1 / 3.7 / 3.0 | 1.9 / 2.2 / 1.8 | 8.6 / 9.6 / 8.5 | 719 / 720 / 719, 0 dropped |
| bare MSE on the main thread | 1.6 / 1.6 / 2.1 | 2.9 / 3.0 / 3.1 | 1.6 / 1.8 / 1.8 | 7.7 / 7.7 / 8.6 | 719 / 719 / 719, 0 dropped |
| engine after | 1.7 / 3.0 / 2.3 | 3.1 / 3.0 / 3.3 | 3.6 / 3.3 / 4.0 | 9.1 / 10.9 / 10.2 | 722 / 720 / 721, 0 / 1 / 0 dropped |

- GPU process: at the floor (3.0 to 3.3 against 2.9 to 3.2). Nothing of the finding is left there, and nothing of it is inherent to MSE.
- Browser process: the main thread is at the floor (0.3 to 0.5 in every row). The engine's second and third passes read 3.0 and 2.3 against 1.5 to 2.0, on other browser threads at load 8 to 10. With one pass at 1.7 this is inside the spread and not called a finding.
- Page renderer: 1.8 to 2.5 points above the floor at 4K in this table. That is the engine's own work: media path stages (`demux.read` 8.7 to 10.5 ms per second, `mse.flac-encode` 1.0 to 1.3, `mse.mux-video` 0.9 to 1.2, `audio.decode` 0.5 to 0.7 at the fast machine speed). Bare MSE needs 0.2 to 0.7 points of renderer by itself.
- One pass of the engine dropped 1 frame of 721 (load 7.9 to 13.8 during that pass). The other 16 element route passes of the engine after the fix dropped none.

## Limitations

- The per frame work of the page was not split further. It stays in the renderer.
- Overlay promotion was not read directly.
- The engine cannot open the fragmented FLAC copy made by ffmpeg for the bisect ("audio: Unsupported configuration"). That is a separate defect of FLAC in MP4, found here and not fixed here.
- The machine was shared. Results hold only for differences that appear in every pass.
- The G.1 cross-origin isolation point was read from the server code, not measured.

## Fixes

- 51a4ecb: the element is made only when the video worker first asks to present through it (`#presentEl`, `web/player/js/session.js`). The MSE element route and the compositor routes never make one. The "before" switch used for the measurement existed only in that run.
