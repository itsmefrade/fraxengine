// SPDX-License-Identifier: LGPL-3.0-only
// Subtitle overlay renderer: draws styled text cues and PGS bitmaps onto a transparent Canvas 2D
// (HTMLCanvasElement or OffscreenCanvas, main thread or Worker) stacked above the video.
//
//   const r = createSubtitleRenderer(canvas, { whiteLevel: 0.8 });
//   await r.setTrack(track);            // Track from the Demuxer contract, kind 'subtitle'
//   r.pushPacket(packet);               // every subtitle Packet the demuxer yields
//   r.render(nowUs, { x, y, width, height });   // per frame or on timeupdate; rect in canvas pixels
//   r.clear();                          // on seek: clears the canvas and PGS state
//
// Text formats (ASS/SSA, SRT, WebVTT) arrive from the Rust decoder already converted to one styled
// cue model (see rust/aether-subs), so there is a single text drawing path. PGS display sets arrive as
// positioned RGBA bitmaps and are scaled from the stream's video size to the video rect.
//
// SDR white level. The overlay is an SDR (sRGB) canvas composited over a video that may be HDR
// (PQ/HLG, Dolby Vision). Browsers map SDR content to the HDR reference white (about 203 nit), so
// subtitle white 255 is as bright as diffuse white of the picture, which glares in a dark scene.
// Every RGB channel (text, outline and PGS bitmaps) is therefore multiplied by `whiteLevel`,
// default 0.8: sRGB 204 is about 0.6 in linear light, roughly 120 nit under a 203 nit reference
// white, in line with the 100 to 150 nit subtitle white that HDR players and Netflix guidance use.
// Black, alpha and hue are unchanged. Use 1 for SDR video (`setWhiteLevel(1)`).

import { openSubtitleDecoder, subtitleKind, KIND } from './subs-wasm.mjs';

export const DEFAULT_WHITE_LEVEL = 0.8;
const MAX_STORED = 600;

function insertSorted(list, item, key) {
  let lo = 0, hi = list.length;
  while (lo < hi) {
    const mid = (lo + hi) >> 1;
    if (key(list[mid]) <= key(item)) lo = mid + 1; else hi = mid;
  }
  list.splice(lo, 0, item);
}

function lastAtOrBefore(list, t, key) {
  let lo = 0, hi = list.length;
  while (lo < hi) {
    const mid = (lo + hi) >> 1;
    if (key(list[mid]) <= t) lo = mid + 1; else hi = mid;
  }
  return lo - 1;
}

function rgba(c, level, alpha = 1) {
  const f = v => Math.round(Math.min(255, v * level));
  return `rgba(${f(c[0])},${f(c[1])},${f(c[2])},${((c[3] / 255) * alpha).toFixed(3)})`;
}

function fontString(span, scale) {
  const px = Math.max(1, span.fs * scale);
  const family = String(span.fn || 'sans-serif').replace(/["\\]/g, '');
  return `${span.i ? 'italic ' : ''}${span.b ? '700' : '400'} ${px}px "${family}", sans-serif`;
}

function fadeAlpha(cue, nowUs) {
  if (!cue.fad) return 1;
  const [fin, fout] = cue.fad;
  let a = 1;
  const t = nowUs - cue.s;
  if (fin > 0 && t < fin * 1000) a = Math.min(a, Math.max(0, t / (fin * 1000)));
  const left = cue.e - nowUs;
  if (fout > 0 && left < fout * 1000) a = Math.min(a, Math.max(0, left / (fout * 1000)));
  return a;
}

/**
 * Lays out one cue: word wrap inside the margins (unless WrapStyle 2), line metrics, block
 * placement from the numpad alignment or `\pos`. Returns lines with absolute item positions.
 */
export function layoutCue(ctx, cue, info, rect) {
  const sx = rect.width / info.playResX;
  const sy = rect.height / info.playResY;
  const maxW = Math.max(1, (info.playResX - cue.ml - cue.mr) * sx);
  const wrap = info.wrapStyle !== 2;
  const measure = (span, text) => {
    ctx.font = fontString(span, sy);
    const m = ctx.measureText(text);
    const px = span.fs * sy;
    return { w: m.width, asc: m.fontBoundingBoxAscent ?? px * 0.8, desc: m.fontBoundingBoxDescent ?? px * 0.2 };
  };
  const lines = [];
  let line = null;
  const newLine = () => { line = { items: [], width: 0, asc: 0, desc: 0 }; lines.push(line); };
  const place = (span, text, m) => {
    line.items.push({ span, text, w: m.w, x: 0 });
    line.width += m.w;
    line.asc = Math.max(line.asc, m.asc);
    line.desc = Math.max(line.desc, m.desc);
  };
  newLine();
  for (const span of cue.spans) {
    const segs = span.t.split('\n');
    segs.forEach((seg, si) => {
      if (si > 0) {
        if (line.items.length === 0) {
          const m = measure(span, 'M');
          line.asc = m.asc; line.desc = m.desc;
        }
        newLine();
      }
      for (const tok of seg.split(/( +)/)) {
        if (!tok) continue;
        const m = measure(span, tok);
        const space = tok[0] === ' ';
        if (wrap && !space && line.width + m.w > maxW && line.items.some(it => it.text.trim() !== '')) {
          while (line.items.length && line.items[line.items.length - 1].text.trim() === '') {
            line.width -= line.items.pop().w;
          }
          newLine();
        }
        if (space && line.items.length === 0 && lines.length > 1 && wrap) continue;
        place(span, tok, m);
      }
    });
  }
  const last = cue.spans[cue.spans.length - 1];
  for (const l of lines) {
    if (l.items.length === 0 && l.asc === 0) {
      const m = measure(last, 'M');
      l.asc = m.asc; l.desc = m.desc;
    }
  }
  const height = lines.reduce((h, l) => h + l.asc + l.desc, 0);
  const col = (cue.an - 1) % 3; // 0 left, 1 centre, 2 right
  const row = Math.floor((cue.an - 1) / 3); // 0 bottom, 1 middle, 2 top
  let ax, ay;
  if (cue.pos) {
    ax = cue.pos[0] * sx;
    ay = cue.pos[1] * sy;
  } else {
    const left = cue.ml * sx;
    const right = rect.width - cue.mr * sx;
    ax = col === 0 ? left : col === 1 ? (left + right) / 2 : right;
    ay = row === 0 ? rect.height - cue.mv * sy : row === 2 ? cue.mv * sy : rect.height / 2;
  }
  let y = row === 0 ? ay - height : row === 1 ? ay - height / 2 : ay;
  y += rect.y;
  for (const l of lines) {
    const x0 = (col === 0 ? ax : col === 1 ? ax - l.width / 2 : ax - l.width) + rect.x;
    let x = x0;
    for (const it of l.items) { it.x = x; x += it.w; }
    l.x = x0;
    l.top = y;
    l.base = y + l.asc;
    y += l.asc + l.desc;
  }
  return { lines, sx, sy, height };
}

function drawCue(ctx, cue, info, rect, nowUs, level) {
  const { lines, sy } = layoutCue(ctx, cue, info, rect);
  const alpha = fadeAlpha(cue, nowUs);
  ctx.globalAlpha = alpha;
  ctx.textBaseline = 'alphabetic';
  ctx.lineJoin = 'round';
  ctx.miterLimit = 2;
  const items = lines.flatMap(l => l.items.map(it => ({ ...it, l })));
  // Opaque box (BorderStyle 3): outline colour behind each line.
  if (cue.bs === 3) {
    for (const l of lines) {
      if (!l.items.length) continue;
      const pad = Math.max(...l.items.map(i => i.span.bord)) * sy;
      const oc = l.items[0].span.oc;
      ctx.fillStyle = rgba(oc, level);
      ctx.fillRect(l.x - pad, l.top - pad, l.width + 2 * pad, l.asc + l.desc + 2 * pad);
    }
  }
  // Passes like libass: all shadows, then outlines, then fills, then decorations.
  for (const it of items) {
    const s = it.span;
    if (!(s.shad > 0) || s.bc[3] === 0 || it.text.trim() === '') continue;
    ctx.font = fontString(s, sy);
    const d = s.shad * sy;
    if (s.bord > 0 && cue.bs !== 3) {
      ctx.lineWidth = s.bord * 2 * sy;
      ctx.strokeStyle = rgba(s.bc, level, 1);
      ctx.strokeText(it.text, it.x + d, it.l.base + d);
    }
    ctx.fillStyle = rgba(s.bc, level, 1);
    ctx.fillText(it.text, it.x + d, it.l.base + d);
  }
  if (cue.bs !== 3) {
    for (const it of items) {
      const s = it.span;
      if (!(s.bord > 0) || s.oc[3] === 0 || it.text.trim() === '') continue;
      ctx.font = fontString(s, sy);
      ctx.lineWidth = s.bord * 2 * sy;
      ctx.strokeStyle = rgba(s.oc, level);
      ctx.strokeText(it.text, it.x, it.l.base);
    }
  }
  for (const it of items) {
    const s = it.span;
    if (it.text.trim() === '' && !s.u && !s.st) continue;
    ctx.font = fontString(s, sy);
    ctx.fillStyle = rgba(s.c, level);
    ctx.fillText(it.text, it.x, it.l.base);
    const px = s.fs * sy;
    if (s.u) ctx.fillRect(it.x, it.l.base + px * 0.1, it.w, Math.max(1, px / 14));
    if (s.st) ctx.fillRect(it.x, it.l.base - px * 0.3, it.w, Math.max(1, px / 14));
  }
  ctx.globalAlpha = 1;
}

function defaultSurface(w, h, rgbaPixels) {
  const c = typeof OffscreenCanvas !== 'undefined' ? new OffscreenCanvas(w, h) : Object.assign(document.createElement('canvas'), { width: w, height: h });
  c.getContext('2d').putImageData(new ImageData(rgbaPixels, w, h), 0, 0);
  return c;
}

/**
 * @param {HTMLCanvasElement|OffscreenCanvas} canvas transparent overlay canvas
 * @param {{whiteLevel?: number, forcedOnly?: boolean, wasm?: URL|string|ArrayBuffer|Uint8Array,
 *          createSurface?: (w:number,h:number,rgba:Uint8ClampedArray)=>any, maxStored?: number}} [opts]
 */
export function createSubtitleRenderer(canvas, opts = {}) {
  const ctx = canvas.getContext('2d');
  const createSurface = opts.createSurface ?? defaultSurface;
  const maxStored = opts.maxStored ?? MAX_STORED;
  let whiteLevel = opts.whiteLevel ?? DEFAULT_WHITE_LEVEL;
  let forcedOnly = !!opts.forcedOnly;
  let track = null;
  let kind = null;
  let dec = null;
  let info = null;
  let gen = 0;
  let pending = [];
  let cues = [];
  let maxDur = 0;
  let displays = [];
  let nextId = 1;
  let lastSig = null;
  let lastNow = 0;

  function prune(list, key) {
    if (list.length <= maxStored) return;
    list.sort((a, b) => Math.abs(key(a) - lastNow) - Math.abs(key(b) - lastNow));
    list.length = Math.floor(maxStored * 0.8);
    list.sort((a, b) => key(a) - key(b));
  }

  function ingest(packet) {
    const r = dec.push(packet.ptsUs, packet.durationUs ?? 0, packet.data);
    if (r.cues) {
      // Text cues are never pruned: the decoder drops repeated packets, so a pruned cue could not come back after a seek.
      for (const c of r.cues) { c.id = nextId++; maxDur = Math.max(maxDur, c.e - c.s); insertSorted(cues, c, x => x.s); }
    } else if (r.displays) {
      for (const d of r.displays) { d.id = nextId++; insertSorted(displays, d, x => x.startUs); }
      prune(displays, d => d.startUs);
    }
    lastSig = null;
  }

  function resetStores() {
    cues = [];
    maxDur = 0;
    displays = [];
    pending = [];
    lastSig = null;
  }

  function wipe() {
    ctx.clearRect(0, 0, canvas.width, canvas.height);
  }

  function activeDisplay(nowUs) {
    const i = lastAtOrBefore(displays, nowUs, d => d.startUs);
    if (i < 0) return null;
    const d = displays[i];
    if (d.clear || (d.endUs != null && nowUs >= d.endUs)) return null;
    return d;
  }

  function bitmapSurface(b) {
    if (b.surface && b.surfaceLevel === whiteLevel) return b.surface;
    let px = new Uint8ClampedArray(b.rgba);
    if (whiteLevel < 1) {
      for (let i = 0; i < px.length; i += 4) {
        px[i] = Math.round(px[i] * whiteLevel);
        px[i + 1] = Math.round(px[i + 1] * whiteLevel);
        px[i + 2] = Math.round(px[i + 2] * whiteLevel);
      }
    }
    b.surface = createSurface(b.width, b.height, px);
    b.surfaceLevel = whiteLevel;
    return b.surface;
  }

  return {
    /** Selects the track (a demuxer Track with kind 'subtitle'), or null. Resolves when the decoder is ready. */
    setTrack(t) {
      const g = ++gen;
      if (dec) { dec.close(); dec = null; }
      resetStores();
      wipe();
      track = t ?? null;
      kind = t ? subtitleKind(t) : null;
      info = null;
      if (!t) return Promise.resolve();
      if (kind === null) return Promise.reject(new Error(`unsupported subtitle codec: ${t.codec}`));
      return openSubtitleDecoder(t, opts.wasm).then(d => {
        if (g !== gen) { d.close(); return; }
        dec = d;
        info = d.info;
        const queued = pending;
        pending = [];
        for (const p of queued) ingest(p);
      });
    },

    /** Feeds one demuxer Packet of the selected track (packets of other tracks are ignored). */
    pushPacket(packet) {
      if (!track || (track.id !== undefined && packet.trackId !== undefined && packet.trackId !== track.id)) return;
      if (!dec) { pending.push(packet); return; }
      ingest(packet);
    },

    /**
     * Draws what is visible at `nowUs` into `videoRect` ({x, y, width, height} in canvas pixels,
     * default the whole canvas). Returns true when something is on screen. Redraws only when the
     * visible set, the rect or a fade changed.
     */
    render(nowUs, videoRect) {
      lastNow = nowUs;
      const rect = videoRect ?? { x: 0, y: 0, width: canvas.width, height: canvas.height };
      let drawList = [];
      let sig = '';
      let animated = false;
      if (kind === KIND.pgs) {
        const d = activeDisplay(nowUs);
        if (d) { drawList = [d]; sig = `p${d.id}`; }
      } else if (kind !== null && info) {
        const i = lastAtOrBefore(cues, nowUs, c => c.s);
        // Sorted by start; a cue that started up to `maxDur` ago can still be showing.
        for (let k = i; k >= 0 && cues[k].s >= nowUs - maxDur; k--) {
          if (nowUs < cues[k].e) drawList.push(cues[k]);
        }
        drawList.sort((a, b) => a.layer - b.layer || a.s - b.s);
        sig = drawList.map(c => c.id).join(',');
        animated = drawList.some(c => c.fad && fadeAlpha(c, nowUs) < 1);
      }
      const key = `${canvas.width}x${canvas.height}|${rect.x},${rect.y},${rect.width},${rect.height}|${whiteLevel}|${forcedOnly}|${sig}`;
      if (!animated && key === lastSig) return drawList.length > 0;
      lastSig = animated ? null : key;
      wipe();
      if (!drawList.length || rect.width <= 0 || rect.height <= 0) return false;
      if (kind === KIND.pgs) {
        const d = drawList[0];
        const sx = rect.width / d.videoWidth, sy = rect.height / d.videoHeight;
        let n = 0;
        for (const b of d.bitmaps) {
          if (forcedOnly && !b.forced) continue;
          ctx.drawImage(bitmapSurface(b), rect.x + b.x * sx, rect.y + b.y * sy, b.width * sx, b.height * sy);
          n++;
        }
        return n > 0;
      }
      for (const c of drawList) drawCue(ctx, c, info, rect, nowUs, whiteLevel);
      return true;
    },

    /** Seek or track switch: clears the canvas and PGS state (text cues stay, packets are de-duplicated). */
    clear() {
      wipe();
      lastSig = null;
      pending = [];
      if (kind === KIND.pgs) {
        displays = [];
        dec?.reset();
      }
    },

    setWhiteLevel(v) { whiteLevel = Math.min(1, Math.max(0.1, v)); lastSig = null; },
    get whiteLevel() { return whiteLevel; },
    /** PGS: draw only objects flagged forced (foreign-language parts) when true. */
    setForcedOnly(on) { forcedOnly = !!on; lastSig = null; },
    /** Cues parsed so far (text tracks) and display sets held (PGS), for diagnostics. */
    stats() { return { cues: cues.length, displays: displays.length, ready: !!dec }; },
    dispose() { gen++; dec?.close(); dec = null; track = null; kind = null; resetStores(); wipe(); },
  };
}
