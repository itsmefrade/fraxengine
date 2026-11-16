# web/subs

Subtitle overlay for the browser player. Decoding lives in Rust (`rust/aether-subs`, exported by
`rust/aether-wasm/src/subs.rs`), drawing in `renderer.mjs` (Canvas 2D).

| File | Purpose |
| --- | --- |
| `renderer.mjs` | `createSubtitleRenderer(canvas, opts)`, `layoutCue` |
| `subs-wasm.mjs` | `openSubtitleDecoder(track, wasmUrl)`, `subtitleKind`, `parseDisplays` over the `aether_subs_*` ABI |

```js
import { createSubtitleRenderer } from './subs/renderer.mjs';
const r = createSubtitleRenderer(overlayCanvas /* HTMLCanvasElement | OffscreenCanvas */, { whiteLevel: 0.8 });
await r.setTrack(track);                       // demuxer Track: kind 'subtitle', codec 'pgs'|'ass'|'srt'|'webvtt', description = codec private
for (const p of subtitlePackets) r.pushPacket(p);   // demuxer Packets (ptsUs, durationUs, data); other trackIds are ignored
r.render(nowUs, { x, y, width, height });      // video rect in canvas pixels; redraws only on change, returns true if something shows
r.clear();                                     // seek / track switch: wipes the canvas and PGS state
```

Formats: ASS/SSA (script header from `description`, packets `ReadOrder,Layer,Style,Name,MarginL,MarginR,MarginV,Effect,Text`),
SRT and WebVTT (Matroska blocks), and PGS (headerless segments per block). SRT and WebVTT markup is rewritten as ASS override
text in Rust, so all text goes through one cue model (spans with resolved font, size, colours, outline, shadow; alignment,
margins, `\pos`, `\move` start point, `\fad`) and one drawing path. Supported ASS tags: `\b \i \u \s \fn \fs \c \1c \3c \4c \alpha
\1a \3a \4a \bord \shad \an \a \pos \move \fad \r \p` and `\N \n \h`; the rest is ignored. WebVTT: `line`, `align`, `size` cue
settings, `<b> <i> <u> <c.color> <v> <lang> <ruby>` (not `position`, regions, `::cue` CSS).

Seeking: text cues are kept (the decoder drops repeated packets, so re-delivered packets after a seek are harmless). PGS state
depends on stream continuity: `clear()` drops the display sets and resets the decoder, and a PGS display that started before the
seek point is not shown until the next display set (the demuxer restarts at a video keyframe).

## SDR white level over HDR video

The overlay is an SDR canvas on top of video that may be HDR (PQ, HLG, Dolby Vision). Browsers composite SDR content at the HDR
reference white (about 203 nit), so subtitle white 255 is as bright as the picture's diffuse white and glares in dark scenes.
All RGB channels (text, outlines, PGS bitmaps) are multiplied by `whiteLevel`, default **0.8** (sRGB 204, about 0.6 linear, roughly
120 nit under a 203 nit reference white); alpha and black are untouched. Pass `whiteLevel: 1` or call `setWhiteLevel(1)` for SDR video.

## Tests

```sh
node --test web/subs/test/*.test.mjs     # needs web/wasm/aether_wasm.wasm built with subtitle exports (sh rust/build.sh) or AETHER_WASM=<path>; ffmpeg for the Matroska fixtures
```
