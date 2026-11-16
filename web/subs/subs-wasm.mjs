// SPDX-License-Identifier: LGPL-3.0-only
// Subtitle decoder over the raw C ABI (rust/aether-wasm/src/subs.rs). One AetherWasm instance per
// open track (the module keeps one subtitle track in thread-local state).
import { AetherWasm, AetherError, loadModule } from '../engine/aether-wasm.mjs';

export const KIND = { pgs: 0, ass: 1, srt: 2, webvtt: 3 };

/** Maps `Track.codec` (and the container codec id as a fallback) to a decoder kind, or null. */
export function subtitleKind(track) {
  const direct = KIND[track?.codec];
  if (direct !== undefined) return direct;
  const cc = String(track?.containerCodec ?? track?.codec ?? '').toUpperCase();
  if (cc.startsWith('D_WEBVTT') || cc === 'S_TEXT/WEBVTT' || cc === 'WEBVTT') return KIND.webvtt;
  if (cc === 'S_TEXT/ASS' || cc === 'S_TEXT/SSA' || cc === 'SSA') return KIND.ass;
  if (cc === 'S_TEXT/UTF8' || cc === 'SUBRIP') return KIND.srt;
  if (cc === 'S_HDMV/PGS' || cc === 'HDMV_PGS_SUBTITLE') return KIND.pgs;
  return null;
}

function checkExports(aw) {
  if (typeof aw.x.aether_subs_open !== 'function') {
    throw new Error('aether wasm has no subtitle exports: rebuild web/wasm with `sh rust/build.sh`');
  }
}

function fail(aw, op, code) {
  throw new AetherError('subtitle error', code, op, aw.lastError());
}

/** Parses the binary display list (see subs.rs). Bitmap pixels are copied out of wasm memory. */
export function parseDisplays(bytes) {
  const v = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  const n = v.getUint32(0, true);
  let o = 4;
  const out = [];
  for (let i = 0; i < n; i++) {
    const startUs = Number(v.getBigInt64(o, true));
    const end = Number(v.getBigInt64(o + 8, true));
    const d = {
      startUs,
      endUs: end < 0 ? null : end,
      videoWidth: v.getUint32(o + 16, true),
      videoHeight: v.getUint32(o + 20, true),
      clear: (v.getUint32(o + 24, true) & 1) === 1,
      bitmaps: [],
    };
    const count = v.getUint32(o + 28, true);
    o += 32;
    for (let k = 0; k < count; k++) {
      const w = v.getUint32(o + 8, true);
      const h = v.getUint32(o + 12, true);
      const rgba = bytes.slice(o + 20, o + 20 + w * h * 4);
      d.bitmaps.push({ x: v.getInt32(o, true), y: v.getInt32(o + 4, true), width: w, height: h, forced: v.getUint32(o + 16, true) !== 0, rgba });
      o += 20 + w * h * 4;
    }
    out.push(d);
  }
  return out;
}

/**
 * Opens a decoder for a demuxer subtitle track.
 * `push(ptsUs, durationUs, data)` returns `{cues}` (text kinds) or `{displays}` (PGS).
 */
export async function openSubtitleDecoder(track, wasmUrl) {
  const kind = subtitleKind(track);
  if (kind === null) throw new Error(`unsupported subtitle codec: ${track?.codec}`);
  const aw = await AetherWasm.create({}, wasmUrl);
  checkExports(aw);
  const desc = track.description instanceof Uint8Array ? track.description : new Uint8Array(0);
  const s = aw.withBytes(desc, (p, l) => aw.x.aether_subs_open(kind, p, l));
  if (s < 0) fail(aw, 'subs_open', s);
  const info = JSON.parse(aw.resultText());
  return {
    kind,
    info,
    push(ptsUs, durationUs, data) {
      const n = aw.withBytes(data, (p, l) => aw.x.aether_subs_push(BigInt(Math.round(ptsUs)), BigInt(Math.round(durationUs || 0)), p, l));
      if (n < 0) fail(aw, 'subs_push', n);
      if (kind === KIND.pgs) return { displays: parseDisplays(aw.result()) };
      return { cues: JSON.parse(aw.resultText()) };
    },
    /** Whole sidecar file (`.sup`, `.ass`, `.srt`, `.vtt`); text kinds return `{info, cues}`. */
    loadFile(bytes) {
      const n = aw.withBytes(bytes, (p, l) => aw.x.aether_subs_load_file(kind, p, l));
      if (n < 0) fail(aw, 'subs_load_file', n);
      if (kind === KIND.pgs) return { displays: parseDisplays(aw.result()) };
      const j = JSON.parse(aw.resultText());
      return { info: j.track, cues: j.cues };
    },
    /** After a seek: drops PGS epoch state. */
    reset() { aw.x.aether_subs_reset(); },
    close() { aw.x.aether_subs_close(); },
  };
}

export { loadModule };
