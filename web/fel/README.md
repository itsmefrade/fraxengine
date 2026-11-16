# web/fel

Realtime Dolby Vision (Profile 7 FEL/MEL, 8.x, 5) and HDR10 compositor for Chrome: WebCodecs hardware frames in, an HDR
`rgba16float` canvas out, all on the GPU. Plain ES modules, no build step.

| File | Purpose |
| --- | --- |
| `fel-compositor.mjs` | `FelCompositor`, `probeCanvasColorSpaces`. |
| `fel-shaders.mjs` | WGSL: RECOVER, COMPOSE, DISPLAY. |
| `fel-meta.mjs` | RPU display metadata (`normalizeRpuDisplayMeta`), PQ/HLG/primaries maths, tone mapping parameters, f64 model of the DISPLAY stage. |
| `fel-dm.mjs` | ST 2094-10 style display management: parameter building (`buildDmParams`, trim interpolation `selectTrims`) and the float64 reference (`dmMapNits`, `deltaEItp`). Theory and sources: `DM-THEORY.md`. |
| `dm/` | DM checks: `dm-node-ref.mjs` (Node, unit checks and expected outputs), `dm-verify.html` / `dm-verify.mjs` (WGSL against the Node reference, real frames, chart, PNG side by side), `save-snaps.py`. |
| `sps-range.mjs` | `hvccFullRange`: `video_full_range_flag` of an hvcC SPS (what Chrome applies in the YCbCr conversion, see "HLG and range"). |
| `hlg-verify.html`, `hlg-verify.mjs`, `tools/hlg-prep.mjs` | HLG / Profile 8.4 numeric check against an f64 BT.2100 model plus SDR snapshots (see "HLG and range"). |
| `decode-bench.html`, `probe.html`, `tools/dump-packets.mjs` | Raw WebCodecs decode throughput of a packet dump, and a worker queue probe of a `Session` (used for the 4K60 drop investigation). |
| `fel-tables.mjs` | `FelTables`: raw C ABI call for `aether_fel_reset` / `aether_fel_gpu_tables`. Superseded by `web/engine/dovi.mjs` `createDoviSession()` (tables and RPU display metadata); only the older test pages still import it. |
| `fel-reference.test.html`, `fel-test.mjs` | `window.runFelTest({steps, frames, canvasMaxWidth, canvasColorSpace, peakNits})`. Steps `probe display exact hw fps`. `?auto=exact,hw,fps&name=x` posts the JSON to `/results`. |
| `hybrid-test.mjs`, `hybrid.test.html` | Hybrid quality mode test: `?auto=1&mode=exact-bl\|exact-el\|exact-both\|hw&n=4&quality=1&frames=1454&name=x[&canvasMaxWidth=3840]` posts to `/results`. |
| `hdr10plus-test.mjs`, `hdr10plus.test.html` | HDR10+ on `media/hdr10plus.mkv`: parse, GPU against the Rust curve, ramp, hardware decode (see "HDR10+"). |
| `p5-test.mjs`, `p5.test.html`, `mmr-test.mjs`, `mmr.test.html` | Profile 5 and MMR checks on real media (see "Real media: P5, MMR"). |
| `tools/prep-media.mjs`, `tools/gen-gold.py`, `tools/rpu-survey.mjs` | Dev tools: demux a DV file with `web/engine`, write Annex B layers + RPUs + FFmpeg gold planes into `media/` (git-ignored, about 1.3 GB). |
| `canvas-size.test.html` | HDR canvas size / format / thread probe (see "Canvas findings"). |
| `fixtures` | git-ignored local test media (exact planes, RPUs, gold manifest); see docs/testing.md. |

## API

```js
import { FelCompositor } from './fel-compositor.mjs';
import { createDoviSession } from '../engine/dovi.mjs';

const dovi = await createDoviSession();                  // one per playback session; dovi.reset() on load and every seek
const { display, tables } = dovi.parseRpu(rpuNal);       // per picture, in decode order: RPU display metadata + GPU tables
const comp = await FelCompositor.create(device, canvas.getContext('webgpu'), {
  peakNits: 1000, sdrWhiteNits: 203,                      // display (user setting)
  canvasColorSpace: 'display-p3',                         // or 'srgb' | 'rec2100-pq' | 'rec2100-linear' | 'auto'
  canvasMaxWidth: 1920,                                   // canvas is resized to ceil(w / ceil(w / canvasMaxWidth))
});
// P7: blFrame / elFrame are VideoFrames from SPS-relabelled streams (web/engine relabelForChrome / relabelPacketSps)
comp.present(blFrame, elFrame, tables, display, { profile: 'p7' });
comp.present(blFrame, null, tables, display, { profile: 'p8' });        // P8.x (mapping only); null, null + profile 'hdr10' for plain HDR10
comp.present({ planes, width, height }, null, tables, display, { profile: 'p5' });   // IPTPQc2: the RPU's ycc_to_rgb / offsets / rgb_to_lms are used
blFrame.close(); elFrame?.close();                        // the commands are already submitted
await comp.whenIdle();                                    // optional backpressure
```

`present(blFrame, elFrame, gpuTables, rpuDisplayMeta, displayInfo)` encodes one command buffer (recover BL, recover EL, compose, display) and
submits it; CPU cost about 0.2 to 0.3 ms. It returns `{profile, mode, canvas, tone}`. `displayInfo` overrides the constructor defaults per frame:
`dm` (`'bt2390'` default or `'st2094-10'`, see "Display management"), `dmOptions`, `profile` (`p7` default when tables are given, else `hdr10`), `peakNits`, `sdrWhiteNits`, `sourcePeakNits` (used without RPU L1, default 1000),
`toneMapping` (`'l1'` per frame RPU Level 1 max, `'static'` Level 6 mastering max, `'off'`), `chromaSiting`, `trims`.

`rpuDisplayMeta` is the `display` object of `web/engine/dovi.mjs` `parseRpu` (matrices, L1/L2/L5/L6/L8, compressed RPUs already filled from the last full one), the `vdr_dm_data` object of libdovi's JSON, or the normalised form from `normalizeRpuDisplayMeta`. For P5 and P7 the RPU's own `ycc_to_rgb`, offsets and `rgb_to_lms` drive the signal to light conversion (P5: inverse `rgb_to_lms` after the PQ EOTF of L'M'S'); without it the BT.2020 limited range default is used, which is wrong for P5 (18 dB, see below). A Level 5 active area makes the DISPLAY stage write black outside it (`displayInfo.activeArea: false` turns that off).

**P5 colour check** (`web/player/p5-verify.html`, `web/player/tools/p5-prep.mjs`): exact BL planes from FFmpeg and the engine's RPU/tables through `present`, rec2100-pq target, tone mapping off, against an independent float64 implementation of the IPTPQc2 equations at 5 frames of "Test Jellyfin 4K DV P5.mp4": 74.7 to 74.9 dB PSNR in PQ, max error 0.51 of a 10 bit PQ code (the rgba16float target's own rounding), 18 dB when the metadata is dropped.

### Display management: `dm: 'st2094-10'` (Dolby style, experimental)

`displayInfo.dm = 'st2094-10'` replaces the BT.2390 stage by a reconstruction of Dolby's display management from public documents (SMPTE ST 2094-10 Annex B, Dolby patents, BT.2100 ICtCp, IPT-PQ): the tone curve acts on the I channel of ICtCp (IPT-PQ for Profile 5), anchored on RPU L1 min / avg / max, trims (L2, and L8 index to nits) are interpolated in PQ of the display peak (neutral at the mastering peak), Ct and Cp follow the intensity change. `dmOptions` (defaults in `fel-dm.mjs` `DM_DEFAULTS`):
`curve` `'spline'` (default, PQ domain Hermite, US 10,600,166) or `'sigmoid'` (ST 2094-10 B.2 literal), `space` `'auto'` (IPT for P5, ICtCp otherwise) `'ictcp'` `'ipt'`, `trims`, `peakGuard`, `targetMinNits`, `midFraction` (L1 average missing). `toneMapping: 'static'` uses the mastering display (L6) as the source range, `'off'` and HLG keep their previous behaviour; the HDR10+ curve takes precedence. `present` returns the parameters in `tone.dm` (`bypass: true` when the source fits the display). The default stays BT.2390. What is public and what is a design choice here: `DM-THEORY.md`. Checks: `node dm/dm-node-ref.mjs` and `dm/dm-verify.html` (numbers in "DM verification" below).

### DM verification (`dm/`)

Run: `node dm/dm-node-ref.mjs` (unit checks and `dm-samples.json`), then `dm/dm-verify.html?auto=1&parts=gpu,frames,chart&snaps=1&name=x` in `--headless=new` Chrome 157 (own `--user-data-dir`), then `python3 -I dm/save-snaps.py`. Result record: `web/results/dm-verify-st2094-10.json`, PNGs `web/results/dm-<clip>-f<frame>-<target>.png` and `dm-chart-<target>.png` (left BT.2390, right ST 2094-10 style; `sdr100` and `sdr203` use white = that peak, the chart `hdr` ones white = the target peak, `-static` = mastering peak as source range).

- **Node f64 reference** (59 checks, 0 failures): ST 2094-10 B.2 equation (5) reproduces the three anchors to 1e-14 and the worked example (29 cd/m2); spline anchors map to (TMin, TMid, TMax) exactly, monotonic, C1 at SMid, slopes below the 3x bound; trim selection (exact target, PQ-linear mix at 600 nit, fade to neutral at the mastering peak, none at or above it, L8 index to nits); neutral ramps 0.0005 to 4000 nit stay neutral (0 DeltaE ITP in ICtCp, 0.0025 in IPT with the default HPE matrix), monotonic, never above the display peak, in 9 scenarios (4000 to 1000, 600, 203, 100, sigmoid, IPT, L8 only, no RPU).
- **WGSL against the Node reference**: 8 cases x 3600 pixels (log neutral ramp, saturated colours, random colours; spline and sigmoid, ICtCp and IPT, interpolated trims, bypass) through `present` into an `rgba32float` target. Max DeltaE ITP 0.054 (mean 0.002 to 0.007), neutral ramp max PQ error 3.2e-5 and output chroma at most 0.030 DeltaE ITP, no NaN. Channel PQ error is at most 0.0085 and only for a 0.36 nit channel beside a 1000 nit one (gamut edge colour, f32 matrix cancellation, 0.05 DeltaE ITP).
- **Real frames** (exact planes + RPU tables / display metadata from `web/engine`: rangers = DV FEL Power Rangers Credits Test.mkv P7 FEL, awaken = awaken-girl.4K.HDR.DV.mkv P7 MEL, p5 = Test Jellyfin 4K DV P5.mp4; checkpoint frames 1 and 4 of each, 1920 x 1080 output, targets 100, 203, 600, 1000 nit and 100 / 1000 nit with `static`, 36 configurations): 0 monotonic violations (neutral pixels, 100 PQ bins), 0 pixels above the display peak, no NaN, the mean I of pixels around the L1 average equals the curve prediction within 4e-4 PQ (awaken at 100 and 203 nit: TMid = SMid, the average is kept; rangers `static` 4000 to 100 nit: SMid 0.370 maps to TMid 0.254, the mid-tone cap of the spline), hue angle change at most 0.94 degrees (BT.2390: 0.37), near neutral pixels (input chroma below 0.5 DeltaE ITP) stay below 0.66 DeltaE ITP (the chroma scale of the trims).
- **Chart** (4000 nit master, grey and six saturated rows, 0.01 to 4000 nit): 0 monotonic violations at all four targets, hue change at most 2.5 / 3.7 / 5.8 / 6.5 degrees at 100 / 203 / 600 / 1000 nit (BT.2390: 4.2 / 3.6 / 2.5 / 2.0), the saturated primaries at the top of the ramp are where the I based mapping plus the peak guard moves hue.
- Limits seen on the test files: rangers carries L1 max 70 (0.017 PQ, 0.1 nit) in every checked RPU although the picture reaches 260 to 300 nit, so with per frame L1 the BT.2390 path passes 298 nit to a 100 nit display and DM clamps at the peak; `toneMapping: 'static'` (L6 mastering peak) rolls off properly. awaken (1000 nit master, picture 300 to 390 nit) and p5 are identity at 600 and 1000 nit targets in both modes (no L2 trim at 1000 nit for awaken; p5 has trims at 100, 600, 1000 nit). The existing `display` step, hdr10plus and p5 pages still pass unchanged (max PQ error 4.9e-4, as before).

### Quality modes (hybrid input)

`present` also accepts exact decoded planes instead of a `VideoFrame` for either layer: `{ planes: { Y, U, V }, width, height }` (Uint16Array or ArrayBuffer, yuv420p10le codes). Planes skip RECOVER and are copied with `queue.writeBuffer` straight into the code buffer COMPOSE reads (same layout, so no texture and no extra GPU copy; one CPU copy inside the browser, 4 to 6 ms per 4K frame). `web/exact/stream.js` `ExactStream({n, layers, budgetMB})` is the pull based GOP parallel WASM source: `await s.start(); await s.next('bl')` returns the next display order frame in that shape (workers pause once `budgetMB` of decoded frames wait).

```js
comp.present({ planes: bl, width: 3840, height: 2160 }, elVideoFrame, words, meta, { profile: 'p7' });   // hybrid: exact BL, hardware EL
```

| Mode | BL | EL | Use |
| --- | --- | --- | --- |
| `hw` (fast) | hardware | hardware | default, about 60 dB |
| `exact-bl` (hybrid high quality) | WASM pool | hardware | recommended high quality |
| `exact-el` (reverse hybrid) | hardware | WASM pool | not worth it, quality equals `hw` |
| `exact-both` (bit exact) | WASM pool | WASM pool | reference, bit exact |

Quality, speed and memory of each mode: [dolby-vision-fel-quality-modes.md](../../measurements/records/dolby-vision-fel-quality-modes.md).

### Real media (high bitrate FEL)

Measured on three real FEL and MEL titles in all four modes: [dolby-vision-fel-quality-modes.md](../../measurements/records/dolby-vision-fel-quality-modes.md).

### HLG and range

`displayInfo.transfer: 'hlg'` displays the base layer as BT.2100 HLG: inverse OETF per channel (scene light E, 0..1), then the reference OOTF
`F_D = alpha * Y_S^(gamma - 1) * E` (Y_S with the BT.2020 weights, black level 0, `alpha` = peak in nits, `gamma = 1.2 + 0.42 log10(peak / 1000)`),
then the same DISPLAY path as PQ content (BT.2390 EETF, display primaries, canvas encoding; no Level 1 / trims, HLG has no mastering metadata).
The OOTF peak is the display peak when it is 400 to 2000 nit (1000 nit: gamma 1.2, signal 0.75 = 203 nit diffuse white; 600 nit: gamma 1.107, 138 nit).
A display below 400 nit (SDR, `peakNits` 203) gets the 1000 nit reference rendition tone mapped by BT.2390, so diffuse white stays at the SDR white level.
`present` returns `transfer`; the RPU tables are not applied with `transfer: 'hlg'` (the RPU is read for its Level 5 active area only).

**Profile 8.4 (HLG base layer + RPU)**: the player passes `transfer: 'hlg'` when the stream's original VUI transfer is 18 (`dv-path.js`, `colorOriginal` from the
worker). The RPU of the iPhone 4K60 MOV and of "Test Jellyfin 1080p DV P8.4" says `vdr_rpu_profile 1`, `bl_video_full_range_flag 0`, `vdr_bit_depth 16`,
`disable_residual`, `signal_eotf` PQ, and a mapping that is **not** the identity: luma 8 piece order 2 polynomial, Cb and Cr MMR order 3. That mapping turns the
HLG base layer into a PQ coded VDR signal, it is an alternative to HLG decoding, never an addition (HLG decode of the mapped signal would apply the transfer
twice). Both were measured (below); the HLG path is the default because it is the verified one, adapts gamma to the display, and is single layer (no COMPOSE
pass, 4K60 realtime). Cost: Level 2 / 8 trims of a 8.4 RPU are not used (neither file has any). `{ profile: 'p8' }` without `transfer` still runs the RPU mapping.

**Range**: Chrome applies the SPS `video_full_range_flag` when it converts a hardware frame to RGB, and `VideoFrame.colorSpace.fullRange` reports limited
either way. The iPhone MOV carries flag 1 on limited range codes (black 64, HLG reference white 721, RPU `bl_video_full_range_flag 0`): RECOVER used to
return codes compressed by 876/1023 (Y fit slope 0.858, mean error 24 codes), a visibly wrong picture. `sps-range.mjs` reads the flag from the hvcC SPS and
`displayInfo.sourceFullRange: true` makes RECOVER invert the full range matrix (`Y = 1023 l`, `C = 512 + 1023 (B - l)/1.8814`). Y fit slope is then 1.002.

Check (`hlg-verify.html?auto=1&name=iphone|p84&frames=5,100,200,280&snap=1`, `node tools/hlg-prep.mjs <file> <name>` first; Chrome 157 headless, Apple M4 Pro): exact BL
planes from FFmpeg, float64 BT.2100 model with no shared code, rec2100-pq target, tone mapping off, PSNR in PQ (peak 1.0) and in linear light (peak 1000 nit).

| File (frames) | compositor from the hardware frame vs f64 (PQ / linear) | compositor from its own recovered codes vs f64 (PQ) | 600 nit display (PQ) | RPU mapping path vs the HLG model (PQ / linear) |
| --- | --- | --- | --- | --- |
| iPhone 4K60 DV 8.4 (5, 100, 200, 280) | 56.6 to 57.6 dB / 59.5 to 64.2 dB, max 39 to 119 codes | 75.7 to 77.0 dB, max 0.5 code | 56.7 to 57.9 dB | 44.3 to 46.1 dB / 46.0 to 50.8 dB |
| Jellyfin 1080p DV 8.4 (5, 100, 200, 280) | 46.6 to 49.1 dB / 50.9 to 52.0 dB | 74.9 to 75.5 dB, max 0.5 code | 45.4 dB or more | 39.7 to 40.3 dB / 35.2 to 37.0 dB |

The 75 dB column is the shader's own accuracy (f32, rgba16float rounding); the gap to the first column is the hardware decode (BL codes max 15 to 49 off FFmpeg, the
README's hardware figure) and Chrome's RGB clamping near black. The RPU mapping agrees with plain HLG to about 4 PQ codes on the iPhone clip and 7 on the Jellyfin test
pattern (a synthetic chart, the mapped version is brighter): the two renderings are close but not equal, which is why the choice is documented. After display management
snapshots (SDR display, peak 203 nit, display-p3 target, HLG path and RPU path side by side): `web/results/hlg-<name>-hlg-f<n>.png`, `hlg-<name>-rpu-f<n>.png`.

### HDR10+ (ST 2094-40)

`present(frame, null, null, null, { profile: 'hdr10', hdr10plus: params })`, `params` = `extractHdr10Plus(packet, track)` of `web/engine/hdr10plus.mjs` (after `await initHdr10Plus()`). A `null`/`undefined` keeps the previous picture's metadata (a packet may carry none), `false` clears it (seek, new stream). Applied for plain HDR10 only (no RPU, no FEL tables). The compositor builds the 144 byte block with `hdr10plusDisplayBlock(params, peakNits, h10pMap)` (see below) and uploads it to `var<uniform> h10p` (DISPLAY binding 3). In the shader, `HDR10PLUS_WGSL` is appended to DISPLAY; with a curve (`h10p[0]` and `h10p[1]`) the linear BT.2020 light is PQ encoded, mapped with `hdr10plus_map` and decoded again, replacing the BT.2390 stage; with statistics but no curve the scene maximum (`h10p[2]`) becomes the BT.2390 source peak. Match the parameters to the picture by timestamp (hardware decoders output in presentation order). Tested with `hdr10plus.test.html` on HDR10Plus_PB_EAC3JOC.mkv (4K, 15.6 Mbps): 240 of 240 packets carry v1, 1 window, 9 anchors, target 400 nits, scene max 652 to 1171 nits, all with a curve (the no-curve branch has no media here); GPU against `hdr10plusApply` (Rust reference) on 18 synthetic patches built from real parameters: max PQ error 4.9e-4, no NaN; 15 luma ramps monotone; 240 hardware decoded pictures, 20 read back at 1920 x 1080: no NaN or inf, output never above the 1000 nit peak (max PQ 0.7432 against 0.7518).

The player applies the ST 2094-40 curve itself (see `hdr10plusDisplayBlock` in `web/engine/hdr10plus.mjs`). The default (`h10pMap` unset or `'panel'`) gives the curve the panel peak, headroom x anchor (6.11 x 150 = 917 nits assumed, the page cannot read it), with libplacebo's interpolation of knee and anchors towards linear; it is identity for a scene under that peak, and the spec is silent above the target (B.4.1). `?h10pmap=real` builds the block for the display the metadata targets, so fade is 0 and every pixel gets `curve(maxRGB / scene_max) * target` nits; its output stays under target / 150 (2.67 EDR at the common 400 nit target), and only a target above 917 nits passes the roll-off `hdr10plus_fit_ceiling`. In both, nits go to the canvas at 150 nits per 1.0 EDR, the anchor macOS uses for PQ on this screen (measured 147 to 157 from 10 to 390 nits, Chrome `<video>`, six frames, linearised capture). That is not the BT.2408 203 nit white, which sits 1.35x below every other HDR video on the same screen. `curve` is the plain path above at `peakNits`, used by `hdr10plus-test.mjs`, and `macos` / `macos-flat` the measured macOS table `hdr10plus_system_edr` with and without `hdr10plus_scene_gain` (measurement only). Dolby Vision (`h10p` off) never takes this branch.

### Lean path (plain HDR10, HDR10+, HLG)

`FelCompositor` option `lean` (the player sets it, `?lean=0` turns it off): a hardware decoded single layer picture with `profile: 'hdr10'` (PQ or HLG, with or without HDR10+) skips RECOVER and the base layer code buffer. `DISPLAY_LEAN_WGSL` is DISPLAY with `decode` reading the relabelled external texture directly: one sample per pixel, the inverse BT.2020 matrix of RECOVER, luma rounded to its code, then the same uniforms, tone mapping (BT.2390, HDR10+ curve) and output as DISPLAY. One render pass per picture, the external texture is the only per picture binding; the uniforms are written only when an input changes. Dolby Vision (P5, P7, P8.x), planes / GPU input and `options.diag` keep the full path; with `lean` off the output is bit identical to before (float16 hash, zz-ws-4k-hdr10).

Not bit identical to RECOVER + DISPLAY, by chroma only: the import already delivers chroma at full resolution (a nearest and a linear sampler read the same values at pixel centres), and the lean path uses it as it is, where RECOVER takes it at even columns, averages two rows and DISPLAY interpolates it again. A sample shifted to the left chroma siting moved further from the compositor (p99 0.0136 -> 0.0179), so the lean path reads the pixel centre. Paused snapshots at the same frame timestamp, float16 readback of the display-p3 canvas, sRGB EOTF to linear, absolute difference over all channels, lean against `?lean=0` (Chrome 157, macOS):

| source | p50 | p90 | p99 | p99.9 | max |
|---|---|---|---|---|---|
| zz-ws-4k-hdr10 (2 s, 10 s) | 0.0002 | 0.0035 | 0.0136 | 0.0295 | 0.34 |
| zz-hevc-4k-hdr10 (3 s, 20 s) | 0.0004 | 0.0043, 0.0066 | 0.0148, 0.0223 | 0.048 | 0.38 |
| zz-n13-hlgplain (1 s, 5 s) | 0.0004 | 0.0029 | 0.0070, 0.0110 | 0.019 | 0.06 |
| hdr10plus (2 s, 8 s) | 0.0003 | 0.0034 | 0.0106, 0.0125 | 0.027 | 0.17 |

The source values reach 1.0 to 1.27 at p99 (1.34 max). Playback (headless, `--disable-audio-output`): zz-ws-4k-hdr10 and hdr10plus 4K24, zz-n13-hlgplain 4K60, full rate with 0 dropped, three seeks each, A/V offset p95 5 to 8 ms (7 to 8 ms on the full path).

### Real media: P5, MMR

- **MMR is exercised and correct.** Woman and rangers carry MMR on Cb and Cr in every RPU (table word 6 mask = 6, 1440 and 1447 pictures). `mmr.test.html` composes FFmpeg's exact planes with a zero residual EL and compares the WGSL result with a BigInt port of `predict_reference16` (integer MMR orders, `chroma_site_luma`): 144000 samples per file (6000 Cb, 6000 Cr, 6000 luma at each of 8 pictures), 0 mismatches; 60 to 90 percent (woman) and 1 to 10 percent (rangers) of the chroma samples are moved by the MMR by more than one 10-bit code, so it is not the identity. The 8 piece luma polynomial is covered by the same run.
- **P5 hardware path, as first tested: the codes could not be recovered.** The player now decodes P5 in hardware: the base layer is relabelled as an SDR carrier with the BT.2020 matrix (`P5_CARRIER` in `web/engine/aether-wasm.mjs`; full range where the stream states no signal type, and the compositor reads the range from the in-band SPS) so Chrome hands over the raw codes, and the mapping runs as a LUT in one pass (`docs/perf/CORE-V2.md` section E). The finding below is the state before that relabel. "Test Jellyfin 4K DV P5.mp4" (4K 60 fps, 44 Mbps, IPTPQc2, reshaping curves are identity polynomials, `disable_residual`). The DISPLAY P5 chain (reshape LUT, RPU ycc matrix, inverse rgb_to_lms) matches the f64 model to 4.9e-4 PQ on 4000 pixels per picture (exact planes input). With hardware decode, RECOVER cannot restore the codes: BL Y 29.4 to 32.4 dB (max error 96 to 106 codes, bias +4 to +14), U 40 to 47 dB, V 36 to 37 dB, composed PQ differs from the exact result by 0.015 to 0.023 on average (max 0.10), because IPT codes interpreted as YCbCr produce RGB outside [0,1] that Chrome clamps (relabelling matrix 9 does not help). Decoding the BL exactly (`exact-bl` mode, planes input, `profile: 'p5'` with `rpuJson`) is bit exact against FFmpeg at all 8 checkpoints including the open GOP prefix path, and runs at 51 / 57 / 62 fps (N = 4 / 5 / 6), i.e. about 1.0 times realtime for a 60 fps source at 44 Mbps. Tone mapped output was finite.
- **Rust change for P5 tables (made since: `aether_fel_gpu_tables` accepts P5, the player uses it).** Original finding: `aether_fel_gpu_tables` returns -6 (`Error::Unsupported`) for every P5 RPU: `from_rpu` in `rust/aether-dovi/src/fel/mapping.rs` has `compatible_profile = (P7 and not MEL) or (P8 and disable_residual and el_spatial_resampling_filter and vdr_bit_depth 12 and vdr_rpu_profile == 1 and rpu_format&0x700 == 0)`. The P5 RPU is `dovi_profile 5, vdr_rpu_profile 0, el_spatial_resampling_filter_flag false, disable_residual_flag true, bl/el depth 10, vdr depth 12, denom 23, rpu_format 18`. Add a branch `rpu.dovi_profile == 5 && h.disable_residual_flag && h.vdr_rpu_profile == 0 && h.rpu_format & 0x700 == 0` (and the same relaxation in the `use_prev_vdr_rpu_flag` check, which requires `vdr_rpu_profile == 1` and `el_spatial_resampling_filter_flag`). Then P5 runs as a mapping only pass (`mapping-exact`) with polynomial and MMR support instead of the polynomial-only LUT; the compositor already accepts it (`profile: 'p5'` with tables, no `rpuJson`).
- **P7 MEL with explicit zero NLQ was also rejected** (fixed since: `rust/aether-dovi/src/fel/mapping.rs` accepts MEL with all-zero NLQ; awaken-girl: libdovi reports `el_type MEL`, `disable_residual_flag false`, all NLQ values zero): `compatible_profile` requires `el_type != MEL` for profile 7. The fix accepts `dovi_profile == 7` for MEL too (the NLQ tables are all zero, so the residual is 0); `hybrid-test.mjs` still substitutes the identity tables of the 1454 picture clip with the residual disabled.
- **The RPU display metadata used to come from `dovi_tool` JSON** (now `web/engine/dovi.mjs`; text below is the state when the tests were written) (`vdr_dm_data`), which is why P5 and the FEL clips above were composed without tone mapping inputs (`meta` null for the FEL files, JSON for P5). The Rust side now exports it: `parseRpu(nal).display` in `web/engine/dovi.mjs`.

Verification hooks: `composeExact(blBytes, elBytes, tables)` (compose only, from exact yuv420p10le planes), `readComposed()`, `readBaseCodes()`, `displayOnly()`, `readTarget()`.

## Colour pipeline

1. **Decode.** BL (4K) and EL (1080p) decode with hardware WebCodecs. Chrome ignores `VideoDecoderConfig.colorSpace`, so the SPS VUI is relabelled in the
   bitstream to primaries 1 / transfer 13 (matrix 9 kept). Otherwise Chrome tone maps PQ on import.
2. **RECOVER** (compute, 1 thread per 4 x 2 luma block). `importExternalTexture(colorSpace 'srgb')`, nearest sampler. The inverse BT.2020 NCL limited range
   matrix (full range when `sourceFullRange`, see "HLG and range") gives Y'CbCr codes: `Y = 64 + 876 Y'`, `Cb = 512 + 896 (B - Y')/1.8814`, `Cr = 512 + 896 (R - Y')/1.4746`, rounded to integers 0..1023. Chroma (left
   sited 4:2:0) is the mean of the two rows of each 2 x 2 block at the even column. Output: packed u16 planar Y, U, V (same layout as yuv420p10le).
3. **COMPOSE** (compute, exact). Port of `reference/fel-reference16.wgsl`, fed by the 6832 word table packet of `rust/aether-dovi/src/fel/gpu.rs`:
   - Reshaping per component and pivot piece. Polynomial pieces are a table lookup (`predict_reference16(code)` precomputed in Rust). MMR pieces (orders 1 to 3) run
     in WGSL with emulated i64 (u32 pairs), inputs clamped to the pivot range, luma downsampled `[1 2 1]/4` for chroma components, chroma taken at `x/2, y/2` for luma.
   - NLQ residual: EL code (upsampled: 2x bilinear for luma, vertical 64/192 then 8 tap `22 94 -524 2456 2456 -524 94 22` horizontally for chroma), residual from the
     per component NLQ table. `raw16 = clamp(prediction + residual, 0, 65535)`, `vdr = min((raw16 + 8) >> 4, 4095)` (12 bit, half up).
   - Output buffer: `raw16 | vdr << 16` per sample, planar Y, U, V (chroma at 1920 x 1080).
   - Skipped (no EL decode, no EL recover) when the table header says `disable_residual` (MEL, mapping only).
4. **DISPLAY** (fragment, f32). Per output pixel:
   - Chroma bilinear upsample (left, vertically centred siting), `raw16 / 65536` normalisation.
   - `nonlinear = M (ycc - offset)` with the RPU's `ycc_to_rgb_coef` (Q13) and `ycc_to_rgb_offset` (Q28). P7 and P8: BT.2020 R'G'B' (PQ). HDR10 without RPU: standard BT.2020 limited matrix.
   - PQ EOTF per channel to nits (`transfer: 'hlg'`: inverse OETF and OOTF instead, see "HLG and range"), then `linear` matrix to BT.2020 nits: `HPE_to_2020 * rgb_to_lms` for P7 (it is identity within 2e-4 for the fixture), `inverse(rgb_to_lms)` for P5 (L'M'S' signal), identity for HDR10.
   - Tone mapping on `max(R,G,B)` (hue preserving): BT.2390-3 EETF in the PQ domain. Source range `[L1.min_pq, L1.max_pq]` (CMv4.0 Level 3 offsets applied), fallbacks: RPU `source_max_pq`,
     `displayInfo.sourcePeakNits`. Target = `peakNits`. When the source maximum is at or below the target the curve is the identity (no compression, nothing clipped here).
     Then the Level 2 / Level 8 trim whose target is within 250 PQ codes of the panel peak is applied as `e' = clamp(slope e + offset)^power` (`(v - 2048)/4096` scaling, an approximation of Dolby's
     CM trims; `chroma_weight`, `saturation_gain`, `ms_weight`, L1 `avg_pq` and the L5 active area are parsed but not used).
   - BT.2020 to display primaries (P3 or sRGB, or none), clamped at 0, divided by `sdrWhiteNits` and sRGB-encoded (extended range, sign preserving) for the `display-p3` / `srgb` canvas.
     `rec2100-pq`: PQ of absolute nits. `rec2100-linear`: nits / sdrWhite (unverified semantics).
   - Canvas: `rgba16float`, `toneMapping {mode: 'extended'}`, `alphaMode 'opaque'`. Values above 1.0 are HDR headroom (1000 / 203 = 4.9 for an XDR panel).
   - Canvas wider than `canvasMaxWidth` (default 1920) is rendered with integer supersampling (2 x 2 mean of decoded pixels in linear light before tone mapping).
5. **Single layer** (HDR10, P8.1, P8.4 base): steps 3 is skipped, DISPLAY reads the RECOVER buffer directly (10-bit codes / 1024).

## Measured

COMPOSE is bit exact against an independent integer reference; the hardware path is about 60 dB from it (the error is
Chrome clamping RGB on import near black and in saturated colours). Details: [dolby-vision-fel-compose-exactness.md](../../measurements/records/dolby-vision-fel-compose-exactness.md).

## Canvas findings

Which `configure()` colour spaces Chrome accepts, and the 4K canvas freeze investigation:
[hdr-webgpu-canvas-configure.md](../../measurements/records/hdr-webgpu-canvas-configure.md).

## Gaps

- Hardware path is about 60 dB, not bit exact (see above). `exact-bl` hybrid reaches 75.8 dB, `exact-both` is bit exact (see Quality modes).
- MMR and the 8 piece luma curve are now covered by real streams (see "Real media: P5, MMR"); the 12 checkpoint gold hashes still use identity curves.
- P5: colour decode (IPTPQc2 via RPU matrices, `inverse(rgb_to_lms)`) and reshaping are checked against the f64 model on a real stream with exact planes; the player decodes P5 in hardware with the `P5_CARRIER` relabel and takes its tables from `aether_fel_gpu_tables`, which accepts profile 5.
- P8 and HDR10 use the standard BT.2020 matrix and the same code path, tested with the P7 base layer only (it is an HDR10 stream).
- SDR, the HDR10+ no-curve branch on real media, Level 5 active area, trim chroma/saturation terms (BT.2390 path; the DM path has approximations), ICtCp (DM path only) are not handled.
- Tone mapping defaults to BT.2390 on max(RGB); `dm: 'st2094-10'` is the closest public reconstruction of Dolby's display management (not Dolby's algorithm, see `DM-THEORY.md`). Trims of the BT.2390 path are approximate. Out of gamut colours are clamped, not gamut mapped.
- `fel-tables.mjs` uses the raw C ABI and is kept for the test pages; the player goes through `web/engine/dovi.mjs`, which wraps the FEL calls and the RPU display metadata.
- Frame pairing is by output order; the caller must pair BL and EL by POC and pick the RPU of that picture.
- Geometry: width multiple of 4, even height. EL smaller than BL is assumed to be the FEL 2x case (width equality test).
- Visual HDR output was not verified on screen (no screenshot comparison); canvas values are checked numerically only.
