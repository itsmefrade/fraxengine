# Measurements

Each file in [records/](records/) is one measurement, written as described in [METHODOLOGY.md](METHODOLOGY.md).
`Unknown` in a record means the value was not recorded, not that it was zero.

## Dolby Vision

| Record | Type | Status | In one line |
| --- | --- | --- | --- |
| [Profile 8.1, canvas route cost](records/dolby-vision-profile-8-1-canvas-cost.md) | Performance | Current | the extra CPU is Chrome's WebGPU presentation, not Dolby Vision processing |
| [Profile 7 FEL, CPU](records/dolby-vision-profile-7-fel-cpu.md) | Performance | Current | hardware decode of both layers: renderer 7 to 10 % of a core, against 24 to 39 % bit-exact |
| [Profile 7 FEL, quality modes](records/dolby-vision-fel-quality-modes.md) | Quality | Current | hardware about 60 dB, exact base layer up to about 76 dB, both layers exact is bit-identical |
| [COMPOSE exactness](records/dolby-vision-fel-compose-exactness.md) | Quality | Current | the compose shader is bit exact; the remaining error comes from the hardware import |
| [Profile 5](records/dolby-vision-profile-5.md) | Performance | Current | hardware against WebAssembly base layer decode, 4K60 |
| [FEL curve approximation](records/dolby-vision-fel-curves-rejected.md) | Performance | Historical (approach not taken) | about 1 point of CPU for a large quality loss |
| [Approaches not taken](records/dolby-vision-closed-paths.md) | Behaviour | Historical (approach not taken) | exact codes from hardware frames, 10-bit `copyTo`, threaded WebAssembly |

## Playback performance

| Record | Type | Status | In one line |
| --- | --- | --- | --- |
| [Player engines on one Mac](records/comparison-player-engines-m4-pro.md) | Performance | Current | FraxEngine against Chrome `<video>`, hls.js, AVPlayer, AetherEngine, KSPlayer, libmpv, VLCKit |
| [Gap to a plain `<video>`](records/playback-gap-to-plain-video.md) | Performance | see record | 19 local samples and a shaped remote link |
| [Element route, process cost](records/playback-element-route-process-cost.md) | Performance | Current | why a hidden helper element kept the tab loading, and the fix |
| [Seeks](records/playback-element-route-seeks.md) | Performance | Current | seeks on the element route against the plain `<video>` |
| [Route policy and Blu-ray H.264](records/playback-route-policy-bluray.md) | Compatibility | Current | M2TS with redefined parameter sets on the hardware decoder |
| [Chrome in a hidden page](records/behaviour-chrome-hidden-page.md) | Behaviour | Current | Chrome pauses or stops presenting media when the page is hidden |

## Network and memory

| Record | Type | Status | In one line |
| --- | --- | --- | --- |
| [Remote start](records/network-remote-start.md) | Performance | Current | one long request, first frame against the plain `<video>` |
| [Remote 4K memory](records/memory-remote-4k-ring.md) | Performance | Current | the 64 MiB read-ahead ring |

## HDR output and compatibility

| Record | Type | Status | In one line |
| --- | --- | --- | --- |
| [Canvas overlay and EDR](records/hdr-canvas-overlay.md) | Behaviour | Current | which canvas setups reach the screen as HDR |
| [WindowServer cost](records/hdr-windowserver-cost.md) | Performance | Current | the cost of 10-bit video in Chrome against AVPlayer, system processes included |
| [`configure()` colour spaces](records/hdr-webgpu-canvas-configure.md) | Compatibility | see record | which canvas colour spaces Chrome accepts |
| [Native output in Chrome](records/compatibility-native-output-gates.md) | Compatibility | Current | Chrome on macOS passes no Atmos, TrueHD, DTS or Dolby Vision through, so the engine decodes them |
