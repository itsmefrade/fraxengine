# HDR WebGPU canvas: overlay promotion and EDR output

| Field | Value |
| --- | --- |
| Date | 2026-11-08 |
| Group | HDR output |
| Type | Behaviour |
| Section | WebGPU canvas, CoreAnimation overlay and EDR |
| Environment | M4 Pro MacBook, built-in XDR display, Chrome 157.0.8103.38 (stock), visible `--app` window, separate temporary profile per run, lid open, caffeinate on. macOS version and engine commit: Unknown. |
| Status | Historical (approach not taken) |

## Purpose

Does Chrome promote the engine's HDR WebGPU canvas to a CoreAnimation overlay, would an overlay lower CPU, and do the HDR values reach the screen? The answer decided that moving the canvas to an overlay is not pursued for CPU.

## Implementation

| Field | Value |
| --- | --- |
| Test asset | Synthetic probe pages: 1920x1080 at 24 or 120 fps, and 3840x1714 at 24 fps, one opaque canvas, with a band pattern at 20, 100, 203, 406, 1000 and 2000 nits (203 nits = 1.0). Real content: `zz-dv-p81.mp4` (Dolby Vision profile 8.1, 3840x1714 canvas, hardware base layer decode). Control: an HDR10 `<video>` (MSE, hardware decode), file Unknown. |
| Baseline | A plain HDR10 `<video>`, used to calibrate the overlay test (overlay in 343 of 343 frames). Canvas configurations are compared with each other and with that control. |
| Methodology | Overlay decision: the `Compositing.Renderer.CALayerResult` histogram delta from `chrome://histograms`, read through a CDP browser target. Bucket 0 is an overlay, bucket 5 a failure. Independent check: `--trace-startup` with viz quads, counting `SkiaRenderer::DoDrawQuad` (0 when promoted, 2 per frame when Skia composites the canvas). Visual check: `--show-mac-overlay-borders` with `screencapture`. HDR check: ScreenCaptureKit float16 capture (`hdrcap.swift`), pixel values read against the band pattern. CPU: cumulative `ps` time per Chrome process type over 30 s after a 10 to 15 s settle, interleaved A B A B A B, plus WindowServer. |
| Observations | No person looked at the screen. HDR was judged from captured pixel values. Other agents were compiling Rust (two rustc at 100 %), and one ran headless Chrome during the runs. |
| Margin of error | About plus or minus 1 point on Chrome process CPU and about plus or minus 5 points on WindowServer, from the background load. CPU results are averages of 3. |

## Results

### Overlay per canvas configuration (1080p probe, one opaque canvas)

The dividing line is `toneMapping.mode`, not the format or the colour space. `extended` (HDR requested) is never promoted. `standard` is always promoted and always SDR range.

| Configuration | CALayerResult | Overlay | Screen shows HDR highlights |
| --- | --- | --- | --- |
| rgba16float, display-p3, extended (the engine's config) | bucket 5 on 100 % of frames | no | yes (9.85) |
| rgba16float, srgb, extended | bucket 5 | no | yes |
| rgba16float, srgb-linear, extended (experimental flag) | bucket 5 | no | n/a |
| rgba16float, rec2100-pq, extended (experimental flag) | bucket 5 | no | yes (PQ decodes absolute, 2000 nits = 9.85) |
| rgba8unorm, display-p3 or rec2100-pq, extended | bucket 5 | no | 8 bit sRGB and P3 clamp at 1.0; PQ yes |
| rgba16float, display-p3, standard | bucket 0 | yes | no, clamped to 1.0 |
| rgba8unorm or bgra8unorm, srgb or display-p3, standard | bucket 0 | yes | no (SDR) |
| rgba8unorm or bgra8unorm, rec2100-pq, standard (experimental flag) | bucket 0 | yes | no, tone mapped into 0.58 (203 nits) to 0.83 (2000 nits) |
| Control: HDR10 `<video>` (MSE, hardware decode) | bucket 0 | yes | yes |

`--enable-features=CoreAnimationHDRUnderlays` and its disable form change nothing (still bucket 5). With `--enable-experimental-web-platform-features` the result is the same: no configuration is both overlay and HDR on this Chrome.

### On-screen HDR depends on where the canvas is drawn

| Where it is drawn | On-screen band values (20 / 100 / 203 / 406 / 1000 / 2000 nits) | Overlay |
| --- | --- | --- |
| Main thread canvas element | 0.099 / 0.492 / 1.0 / 2.0 / 4.9 / 9.85 (HDR) | no |
| Worker, `transferControlToOffscreen()` (what the player does) | 0.098 / 0.494 / 1.0 / 1.0 / 1.0 / 1.0 (clamped) | no |
| Worker WebGPU, `transferToImageBitmap()` posted to the main thread, `bitmaprenderer` canvas | 0.099 / 0.492 / 1.0 / 2.0 / 4.9 / 9.85 (HDR) | yes (bucket 0, DoDrawQuad 0) |

- The `dynamic-range-limit: no-limit` CSS and the COPY_SRC usage flag do not change the worker clamp.
- Real file (`zz-dv-p81.mp4`, canvas `rgba16float`, display-p3, extended): grid p99 of the per-pixel max channel is exactly 1.000, with 0 of 13410 samples above 1.0 (p50 0.32).
- The same content as an HDR10 `<video>`: p99 1.97, and 38 % of samples above 1.0.
- The player's HDR10 canvas path peaks at 0.958.

### CPU, interleaved, same workload (averages of 3, % of one core)

Chrome total is browser, GPU process, renderer and utility processes together. WS is WindowServer.

| Comparison | GPU process | Renderer | Chrome total | WS | Chrome total + WS |
| --- | --- | --- | --- | --- | --- |
| 1080p24 probe, rgba16float p3 extended (no overlay) | 4.1 | 2.7 | 9.1 | 22.6 | 31.6 |
| 1080p24 probe, rgba16float p3 standard (overlay) | 4.0 | 2.5 | 9.0 | 27.2 | 36.2 |
| 3840x1714 24 fps probe, extended (no overlay) | 4.4 | 2.7 | 9.5 | 22.6 | 32.0 |
| 3840x1714 24 fps probe, standard (overlay) | 5.4 | 2.6 | 10.4 | 19.8 | 30.2 |
| HDR10 4K `<video>` control (overlay) | 3.3 | 1.2 | 6.8 | 31.7 | 38.5 |
| DV P8.1 player, stock (canvas, no overlay, SDR clamped) | 7.2 | 4.6 | 13.0 | 20.5 | 33.5 |
| DV P8.1 player, worker bitmap path (overlay, real HDR) | 9.0 | 20.9 | 31.4 | 16.7 | 48.1 |
| DV P8.1 base layer as HDR10 MSE `<video>` (overlay, DV mapping skipped) | 3.8 | 3.1 | 7.9 | 31.3 | 39.2 |

Overlay promotion alone saves nothing measurable: with the same workload the GPU process is 4.0 against 4.1 (1080p) and 5.4 against 4.4 (4K). The only GPU drop (7.2 to 3.8) comes from the MSE variant, which also removes the WebGPU DV compositor, so it is a decoder-route saving. The worker bitmap path costs about 18 points more renderer CPU at 4K F16, because the ImageBitmap transfer into a `bitmaprenderer` canvas is not zero copy at that size.

## Limitations

- Stock Chrome 157.0.8103.38 on one machine only.
- The on-screen result (Result 2) is not yet checked against any earlier HDR-on-screen verification. That check may have used the main thread, another Chrome build, or another capture. Open.
- The Dolby Vision result comes from one file. The HDR10 control file is not named.
- Visual output was judged by captured values, not by eye.
- CPU differences between overlay and non-overlay runs are within noise, except the MSE variant, which changes the decoder route as well.

## Fixes

None. The probe changed no repository file. Candidate fix, not adopted: draw the DV frame in the worker and present it through `transferToImageBitmap()` into a main-thread `bitmaprenderer` canvas. That keeps HDR and promotes the canvas to an overlay, at a renderer CPU cost that would have to come down before it is viable at 4K.
