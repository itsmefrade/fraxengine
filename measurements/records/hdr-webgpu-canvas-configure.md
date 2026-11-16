# WebGPU canvas configure() colour spaces

| Field | Value |
| --- | --- |
| Date | Unknown |
| Group | HDR output |
| Type | Compatibility |
| Section | WebGPU canvas configure() colour spaces |
| Environment | Chrome 156 (existing install, build number Unknown) and Chrome 157.0.8103.38 (clean profile). Machine, OS and engine commit: Unknown. |
| Status | Current |

## Purpose

Which colour spaces `configure()` accepts on an `rgba16float` canvas with `extended` tone mapping, so the engine asks only for values that work across Chrome builds.

## Implementation

| Field | Value |
| --- | --- |
| Test asset | None. The test calls `configure()` only. |
| Baseline | Unknown. The two Chrome builds are compared with each other. |
| Methodology | Call `configure()` with each colour space and record whether it is accepted or throws. Chrome 156 was also tested with `bgra8unorm` and with `standard` tone mapping. Repeat count: Unknown. |
| Observations | The Chrome 157 error is "not a valid enum value of PredefinedColorSpace". No frame is drawn, so what the display shows is not part of this test. |
| Margin of error | Unknown. The result is accept or throw, with no timing. |

## Results

| Colour space | Chrome 156 (`rgba16float`, `extended`) | Chrome 157.0.8103.38 (clean profile) |
| --- | --- | --- |
| `rec2100-pq`, `rec2100-linear`, `rec2100-hlg` | accepted | throws |
| `display-p3-linear`, `srgb-linear` | accepted | accepted |
| `display-p3`, `srgb` | accepted | accepted |

Only `display-p3` and `srgb` are portable across both builds.

## Limitations

- Whether a display honours `rec2100-pq` was not verified visually.
- Flags used for these runs: Unknown. The HDR overlay record used `rec2100-pq` under an experimental flag and saw HDR values on screen (see [hdr-canvas-overlay.md](hdr-canvas-overlay.md)). This record does not say whether the flag was set for the clean Chrome 157 run.
- A 4K HDR canvas freeze reported from the player was not reproduced. Chrome 156 ran 20 frames at 1920x1080 and 3840x2160 (`bgra8unorm` and `rgba16float`, on the main thread and in a worker), and a 1454 frame run at 3840x2160 ran cleanly in a fresh Chrome 157. The cause was not isolated.

## Fixes

- Engine: `canvasColorSpace: 'auto'` falls back to `display-p3` or `srgb`, the portable values. Commit: Unknown.
