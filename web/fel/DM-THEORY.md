# Dolby Vision display management: what is public, what is implemented

Scope: the tone and gamut mapping that a Dolby Vision (DV) player applies when the display is not the mastering display
(Dolby "Content Mapping", CM v2.9 and CM v4.0), as far as it can be reconstructed from public documents and open source. The implementation
is `fel-dm.mjs` (parameters and the float64 reference) and `dm_map` in `fel-shaders.mjs` (WGSL), selected with `displayInfo.dm = 'st2094-10'`
(see README "Display management"). This document is the theory behind it. Nothing here is Dolby's implementation; the shipping algorithm is not
public. Where a statement is a design choice of this project and not a published fact it says so.

Legend used below: **[std]** read in a public standard, **[patent]** read in a published Dolby patent, **[src]** read in open source code,
**[mem]** from memory of a public document that was not re-read for this document (treat as a pointer, not evidence), **[design]** choice made here.

## 1. Sources and licences

| Source | What was read | Licence / status |
| --- | --- | --- |
| SMPTE ST 2094-10:2021 (rev. of 2016), "Dynamic Metadata for Color Volume Transform, Application #1", pub.smpte.org | whole document (15 pages): metadata items, constraints, informative Annex B | Copyrighted SMPTE standard, publicly downloadable; SMPTE notes essential patents are available under RAND terms. Formulas are quoted as facts, no text copied. |
| ITU-R BT.2100-2 (ICtCp, PQ), ITU-R BT.2124 (Delta E ITP) | **[mem]** matrices and the 720 scale | ITU, public. |
| Dolby ICtCp whitepaper ("What is ICtCp?"), Dolby Vision whitepapers | **[mem]** only | Not re-read. |
| US 10,580,367 "Display mapping for high dynamic range images" | sigmoid, three anchors, mid-tone anchor rules | Dolby patent; implementing may need a licence from Dolby (not legal advice). |
| US 10,600,166 "Tone curve mapping for high dynamic range images" | Table 1 of the spline method | same |
| US 10,540,920 / 9,961,237 / 9,613,407 "Display management for high dynamic range video" (Atkins et al.) | IPT-PQ, sigmoid on I, saturation and intensity adjustment, detail | same |
| US 12,573,352 "Multi-step display mapping and metadata reconstruction", US 11,336,895 | slope / offset / power (SOP) trim equation, normalisation, trim extrapolation | same |
| libplacebo `src/tone_mapping.c`, `src/shaders/colorspace.c`, `src/utils/dolbyvision.c` | `st2094_10`, `st2094_pick_knee`, DV reshaping, L1 use | LGPL-2.1-or-later. |
| mpv `--tone-mapping=st2094-10` (via libplacebo) | manual / commit message: "ST2094-10 Annex B.2", "generally worse than st2094-40" | GPL-2.0-or-later (LGPL-2.1-or-later when built with `-Dgpl=false`) **[mem]**. |
| FFmpeg `libavutil/dovi_meta.h` | RPU extension block structs | LGPL-2.1-or-later. |
| dovi_tool / libdovi (`level2.rs`, `level8.rs`, `level10.rs`) | field layouts, defaults, L8 target index to nits | MIT **[mem]**, the repository page could not be re-fetched. |
| Kodi | **not read** for this document (Kodi hands DV to the platform decoder on Android and has no public DM of its own that was examined) | GPL-2.0-or-later **[mem]**. |

LGPL attribution: no libplacebo code is ported. `fel-dm.mjs` implements ST 2094-10 equation (5) directly from the standard; libplacebo's `st2094_10`
solves the identical 3 x 3 system (same matrix, same `alpha`), which is how the implementation was cross-checked. `st2094_pick_knee` (libplacebo's
own adaptation point logic, not part of the standard) is **not** used. If code is ever taken from libplacebo (LGPL-2.1+, compatible with this
project) the file must carry its copyright notice and the LGPL text must stay available.

## 2. SMPTE ST 2094-10 (Application #1) **[std]**

### 2.1 Metadata

- `MinimumPqencodedMaxrgb`, `AveragePqencodedMaxrgb`, `MaximumPqencodedMaxrgb` (L1): computed on the *reduced pixel set* (each 2 x 2 block of the processing
  window averaged per component), as PQ (ST 2084) encoded `maxRGB = max(R, G, B)`; the minimum should be the lowest PQ-encoded `minRGB` (the 2021 edition added this),
  the average is the mean of the PQ-encoded maxRGB values. Range 0..1, steps of 1e-5. Constraint (6.1.9): `0 <= min+dmin < avg+davg < max+dmax <= 1`.
  Offsets (`...Offset`, range [-1, 1], default 0) shift the anchors manually. In a DV RPU these are 12 bit codes (code / 4095): L1 `min_pq, avg_pq, max_pq`; the offsets are CM v4.0 L3.
- Manual adjustments (6.2, eq. 1): `y = min(max(0, x g + o), 1)^P`, gain `g` in [0.5, 1.5] (default 1), offset `o` in [-0.5, 0.5] (default 0), gamma `P` in [0.5, 1.5] (default 1).
  In a DV RPU the same idea is `trim_slope`, `trim_offset`, `trim_power`, 12 bit, 2048 neutral, range [-0.5, 0.5) around neutral (patent: code = (value + 0.5) 4096, so the
  effective multiplier is `1 + (code - 2048)/4096`) **[patent]**.
- `ChromaCompensationWeight` c, `SaturationGain` S, both [-0.5, 0.5], default 0 (6.3). Their effect "depends on the luminance and saturation / chroma inputs of the tone mapping transform".
  `ToneDetailFactor` T in [0, 1] (6.4) blends a detail management result (`r = p (1 - T) + q T`).
- Targeted system display (ST 2094-1): primaries, white point, `TargetSystemDisplayMaximumLuminance`, `...MinimumLuminance`. A metadata set exists per target; the RPU carries several (L2 `target_max_pq`).
- Several sets for the same pixel: the one with the highest `WindowNumber` wins (7.2). **No rule for choosing or interpolating between targets exists in the standard.**

### 2.2 Informative Annex B: the curve

B.2 uses a sigmoid on luminance, `L_m = (c1 + c2 L^n) / (1 + c3 L^n)`, `n = 1`, in cd/m2, fixed by three points:

```
L_m(x1) = y1, L_m(x2) = y2, L_m(x3) = y3
x1, x2, x3 = EOTF_PQ(min + dmin), EOTF_PQ(avg + davg), EOTF_PQ(max + dmax)     (L1 in nits)
y1 = TargetedSystemDisplayMinimumLuminance, y3 = TargetedSystemDisplayMaximumLuminance
y2 = sqrt(x2 sqrt(y3 y1))                                  eq. (4), "adaptation point"
alpha = x3 y3 (x1 - x2) + x2 y2 (x3 - x1) + x1 y1 (x2 - x3)                     eq. (6)
(c1)   1 / x2 x3 (y2 - y3)   x1 x3 (y3 - y1)   x1 x2 (y1 - y2) \ (y1)
(c2) = -- | x3 y3 - x2 y2   x1 y1 - x3 y3   x2 y2 - x1 y1 |  (y2)            eq. (5)
(c3)   alpha \ x3 - x2        x1 - x3         x2 - x1        /  (y3)
```

c3 >= 0 guarantees a monotonic curve. Worked example in the standard: y1 0.5, y3 100, x2 120 gives y2 = 29 cd/m2; for averages above 3162 cd/m2 the adaptation point exceeds y3,
so the standard suggests bounding y2 at a fraction (for example 0.8) of y3. B.3 applies the curve to each RGB component `D_i` of linear light mastering-display RGB
(`E_i = (c1 + c2 D_i)/(1 + c3 D_i)`), then the manual terms normalised by y3 (eq. 8: `F_i = clamp(E_i/y3 g + o, 0, 1)^P y3`), then the saturation step (eq. 9):
`G_i = F_i ((1 + c) F_i / Y)^S`, with `Y` the luminance of the adjusted pixel (0.22897, 0.69174, 0.07929 for DCI-P3 primaries). Annex A says Application #1 consists of tone mapping,
gamut adjustment and detail management blocks and "can be performed in luminance-chrominance, RGB and human-visual-system colour spaces".
Annexes A and B are informative: the standard does not prescribe the colour space, the target interpolation, or how L1 (maxRGB) relates to the luminance the curve acts on.

## 3. Dolby patents **[patent]**

- **US 10,580,367 (sigmoid).** `y = (C1 + C2 x^n)/(1 + C3 x^n)`, `n = 1`, C1..C3 from three anchors by the same closed form as ST 2094-10 eq. (5). Anchors: black `x1 = min(M1, S1)` (M mastering black, S source min), white
  `x3 = max(M3, S3)`, mid `x2 = S2` (average); targets `y1 = max(T1, x1)`, `y3 = min(T3, x3)` so the output range never exceeds the input range. It criticises `y2 = sqrt(y1 y3)`-type rules (they move the average brightness)
  and proposes `y2 = x2` over the middle of the range: with `b1 = y1 (y3/y1)^t1`, `b2 = y1 (y3/y1)^t2` (t1 = 0.2, t2 = 0.8) the mid anchor is `y2 = x2` for `b1 <= x2 <= b2` and
  `y1 (x2/x1)^e1` / `b2 (x2/b2)^e2` outside (log-linear), e1 = ln(b1/y1)/ln(b1/x1), e2 = ln(y3/b2)/ln(x3/b2). A sigmoid-through-three-points variant clamps the mid anchors to 15 %..85 % of the log range.
- **US 10,600,166 (spline, PQ domain).** Two cubic Hermite segments through (SMin, TMin), (SMid, TMid), (SMax, TMax), linear extensions with the end slopes, everything PQ coded. Table 1:
  `TDR = TmaxPQ - TminPQ; midLoc = (SMid - TminPQ)/TDR; headroom = (SMax - SMid)/TDR; tailroom = (SMid - SMin)/TDR; preservationHead = preservationTail = 0.5; cutoff a = 0.5`;
  `offsetHead = min(max(0, midLoc - a) TDR, max(0, headroom pH + midLoc - 1) TDR)`, `offsetTail = min(max(0, a - midLoc) TDR, max(0, tailroom pT - midLoc) TDR)`;
  `TMid = SMid - offsetHead + offsetTail; TMax = min(TMid + SMax - SMid, TmaxPQ); TMin = max(TMid - SMid + SMin, TminPQ)`;
  `slopeMin = min(3 (TMid-TMin)/(SMid-SMin), ((TMid-TMin)/(SMid-SMin))^2)`, `slopeMax = min(3 (TMax-TMid)/(SMax-SMid), 1, ((TMax-TMid)/(SMax-SMid))^4)`,
  `slopeMid = min(maxMinSlope, maxMaxSlope, contrastFactor (1 - SMid + TMid))` (contrastFactor 1, suggested 0.5..2); the factor 3 is the monotonicity bound of Hermite end slopes.
  It is the only published description whose parameters (`TMidContrast`) line up with CM v4.0 fields (L8 `target_mid_contrast`, `clip_trim`).
- **US 10,540,920 / 9,961,237 / 9,613,407 (IPT-PQ DM).** Tone mapping on the I channel of IPT-PQ (PQ in place of the IPT power law). Sigmoid `Im = ((C1 + C2 Io^n)/(1 + C3 Io^n))^Rolloff`, rolloff 1/3, with
  `Crush, Mid, Clip` (scene min, average, max), `Slope = 1/S2Tratio`, `Key = (Mid - Crush)/(Clip - Crush)`, `Shift = Mid (1 - S2Tratio) 2 Key`, anchors `x_i = (Crush, Mid, Clip)^(Slope/Rolloff)`,
  `y = (Min, Mid - Shift, Max)^(1/Rolloff)`, `Min = max(Crush - Shift, Tmin)`, `Max = min(Clip - Shift, Tmax)`. Chroma follows intensity: `S = (Is CEDR)/(Io CMapped)`, `CEDR = k1 Io + k2`, `CMapped = k1 Is + k2`,
  k1 = 0.5, k2 = 1.0, `Ps = Po S`, `Ts = To S`. A Helmholtz-Kohlrausch style intensity term `I' = I + 0.202 CH` (CH = sqrt(P^2 + T^2)) is claimed. The later family (9,613,407) derives a saturation factor from the
  curve slope `SM = (1 - dIm/dIo) W_g` (W_g = 0.25) and an intensity compensation `CM = (sum|S| W_c)^2 W_TM` (W_c = 0.1): `V_OL = SM S + I_mf - CM`. Detail preservation: `D = Io - Im`, `B = F(D, H)` (separable Gaussian, sigma 2), `I_mf = Io - B - E (D - B)`.
- **US 12,573,352 (SOP).** Trim: `y(x) = ((Slope y(x)) + Offset)^Power` applied to the tone curve output in PQ, optionally after normalising the curve to [TminPQ, TmaxPQ] (`y' = (y - TminPQ)/TDR`) to reduce clipping.
  Parameters in [-0.5, 0.5]: `Slope = Gain (1 - Lift) - 1`, `Offset = Gain Lift`, `Power = 1/Gamma - 1`. For targets above the mastering peak with L8 trims it extrapolates linearly between two reference
  trims (eq. 12: `Yout = y1 (1 - alpha) + y2 alpha`, alpha = (Xin - x1)/(x2 - x1), PQ coded luminance); SDR-range trims are "interpolated linearly to the target luminance" depending only on the target maximum.

## 4. DV RPU display metadata **[src]**

From FFmpeg `dovi_meta.h` and dovi_tool: L1 (min, avg, max PQ, 12 bit), L2 (`target_max_pq`, `trim_slope`, `trim_offset`, `trim_power`, `trim_chroma_weight`, `trim_saturation_gain`, `ms_weight`; all 12 bit, 2048 neutral,
`target_max_pq` 2081 = 100 nit, 2851 = 600 nit, 3079 = 1000 nit, 3696 = 4000 nit), L3 (L1 offsets), L4 (`anchor_pq`, `anchor_power`, unknown use), L5 (active area), L6 (static HDR10: mastering max / min, MaxCLL, MaxFALL),
L8 (CM v4.0: `target_display_index` plus the L2 fields plus `target_mid_contrast`, `clip_trim`, six `saturation_vector_field`, six `hue_vector_field`; "for CM v4.0, L8 only is present and used to compute L2"),
L9 (source primaries), L10 (target display: index, max, min PQ, primaries), L11 (content type, white point, reference mode flag). L8 index to the target peak (dovi_tool `trim_target_nits`):
16, 18, 21 = 48 nit; 42 = 108 nit; 24, 25 = 300 nit; 27, 28 = 600 nit; 48, 49 = 1000 nit; 37, 38 = 2000 nit; every other index = 100 nit.
libplacebo reads only L1 `max_pq` and `avg_pq` (`max_pq_y`, `avg_pq_y`, with a TODO that L1 is maxRGB based, not maxY); it ignores L2, L8, chroma weight and saturation gain. mpv inherits that.

## 5. Colour volume: ICtCp and IPT-PQ

- **ICtCp (BT.2100-2)** [mem, matrices verified by construction in `dm-node-ref.mjs`: rows of RGB to LMS sum to 1, neutrals have Ct = Cp = 0]. Linear BT.2020 RGB to LMS: `L = (1688 R + 2146 G + 262 B)/4096`, `M = (683 R + 2951 G + 462 B)/4096`, `S = (99 R + 309 G + 3688 B)/4096`; PQ encode each; `I = (L' + M')/2`, `Ct = (6610 L' - 13613 M' + 7003 S')/4096`, `Cp = (17933 L' - 17390 M' - 543 S')/4096`.
  ITU-R BT.2124 `DeltaE_ITP = 720 sqrt(dI^2 + dT^2 + dP^2)` with `T = Ct/2`. A just noticeable difference is about 1.
- **IPT-PQ** (Ebner and Fairchild IPT with PQ instead of the power law; DV Profile 5 stores IPTPQc2): `I = 0.4 L' + 0.4 M' + 0.2 S'`, `P = 4.455 L' - 4.851 M' + 0.396 S'`, `T = 0.8056 L' + 0.3572 M' - 1.1628 S'`, LMS from BT.2020 RGB through
  the RPU's `rgb_to_lms` (Hunt-Pointer-Estevez based; `inverse(HPE_to_2020)` is used when absent). In P7 / P8 RPUs the same `rgb_to_lms` is present and is the identity chain to BT.2020 R'G'B', which suggests Dolby's DM uses the IPT-PQ LMS space for all profiles; this
  implementation uses ICtCp for non-P5 content as the task requested and offers `space: 'ipt'` for all.
- Why I: tone mapping acts on intensity only and Ct, Cp are scaled with it, so neutrals stay neutral exactly and hue is preserved approximately, unlike per-channel RGB mapping (Annex B.3), which desaturates highlights.

## 6. Trim selection and interpolation

Public facts: trims are authored per target (L2 `target_max_pq`, L8 index); the DM "interpolates the trims for targets between" (US 12,573,352: SDR-range trims interpolated linearly to the target luminance, depending only on the target max; extrapolation for targets above the mastering display from two reference trims).
Not public: the exact domain and weights. **[design]** used here (`selectTrims` in `fel-dm.mjs`): collect L2 targets and L8 targets (index to nits above), the L2 entry wins when both exist within 20 PQ codes; sort by 12 bit PQ of the target peak; linear interpolation of every trim field in that PQ value between the two neighbours; below the lowest
target the lowest set; a neutral set (all 2048) at the mastering peak (`source_max_pq`) so trims fade out towards the mastering display; at or above the mastering peak no trim and no mapping. Where trims are applied: after the tone curve, on the PQ I value, as `y = clamp(slope y + offset, 0, 1)^power` with
`slope = 1 + (s - 2048)/4096`, `offset = (o - 2048)/4096`, `power = 1 + (p - 2048)/4096` (not normalised to the target range).

## 7. What this implementation does

Per pixel in `dm_map` (display stage, BT.2020 linear nits in and out; the existing decode before and gamut / encode after are unchanged):

1. RGB to LMS, PQ encode, matrix to I, Ct, Cp (ICtCp, or I, P, T for `space: 'ipt'` / Profile 5).
2. Curve on I (PQ in, PQ out). Anchors SMin, SMid, SMax from RPU L1 (+ L3 offsets); targets TminPQ = 0 (or `targetMinNits`) and TmaxPQ = PQ(display peak). Without L1: `source_max_pq`, then `sourcePeakNits`; the average defaults to `SMin + 0.4 (SMax - SMin)` **[design]**.
   - `curve: 'spline'` (default): US 10,600,166 Table 1 as above, output clamped to [TminPQ, TmaxPQ]. Identity when `SMax <= TmaxPQ`.
   - `curve: 'sigmoid'`: ST 2094-10 B.2 on the I luminance in nits, `y2 = min(sqrt(x2 sqrt(y1 y3)), 0.8 y3)`, `y3 = min(display peak, x3)`, c3 clamped >= 0. The literal formula darkens mid-tones even when no compression is needed, so the curve is the identity when `x3 <= display peak` **[design]**.
3. Trims (section 6).
4. Chroma: `(Ct, Cp)` are multiplied by `k = (Is/Io) (k1 Io + k2)/(k1 Is + k2)` (Io, Is the I before and after the curve and trims; k1 0.5, k2 1; US 10,540,920) times `(1 + saturation gain)`; chroma weight lowers the intensity by `chroma weight x |(Ct, Cp)|`
   (the sign and the scale of both terms are **[design]**, calibrated only to be small: RPUs seen here carry values of at most +0.1).
5. Inverse transform to BT.2020 RGB; if `max(R, G, B)` exceeds the display peak the pixel is scaled down (peak guard, **[design]**: I-based mapping of a saturated highlight can leave a component above the peak).
6. Display primaries, SDR white scaling and encoding as before. When the transform would be the identity (source inside the target range and neutral trims) the stage is skipped (`bypass`), which also avoids a matrix round trip.

BT.2390 stays the default and is unchanged (`displayInfo.dm = 'bt2390'`).

## 8. What remains unknown

- Dolby's actual curve for CM v2.9 and v4.0 (sigmoid in nits, PQ sigmoid, Hermite spline, or a mix per version) and the production constants (cutoff, preservation, rolloff, mid-tone rules).
- On which channel the curve acts and how the maxRGB-based L1 anchors map to it. Here L1 is applied to the I of ICtCp; for saturated pixels maxRGB exceeds I, so source peaks of colourful highlights are under-represented (hence the peak guard).
- The trim domain (raw PQ or normalised to the target range), the interpolation between targets and the extrapolation above the highest target, the use of `ms_weight`, L4 `anchor_pq` / `anchor_power`, L8 `target_mid_contrast`, `clip_trim`, `saturation_vector_field`, `hue_vector_field` (6-axis hue / saturation adjustments), L9 / L10 / L11.
- The exact effect and scale of `chroma_weight` and `saturation_gain` ("depends on luminance and chroma"), detail management (`ToneDetailFactor`, `ms_weight`), the gamut mapping to the target primaries (this project clamps), temporal behaviour (shot / scene smoothing of per-frame L1, `scene_refresh_flag`), black level and ambient adaptation, and any difference between P5, P7, P8 pipelines in a real display.
- Therefore the output cannot be called bit exact or "Dolby certified"; it is a published-algorithms reconstruction, best judged by the checks in `dm/` and by eye.

## 9. Verification

Numbers (Node f64 reference, WGSL against it, three real files, chart) and the PNG side by side images are in README.md "DM verification"; the code is in `dm/`.
