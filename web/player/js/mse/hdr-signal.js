// SPDX-License-Identifier: LGPL-3.0-only
// What a video sample entry should say about HDR: colr nclx, mdcv and clli values for Fmp4Muxer, taken field by field from the
// container (track.containerColor), then the SPS and SEI in the hvcC, then the SPS and SEI of the first keyframe (hev1 tracks, and most
// Matroska files, keep them in band). Why: AVFoundation decides a track is HDR from the format description, and an hvcC without
// parameter sets gives it nothing to read.

class Bits {
  constructor(b) { this.b = b; this.pos = 0; }
  bit() {
    if (this.pos >= this.b.length * 8) throw new RangeError('SPS truncated');
    const v = (this.b[this.pos >> 3] >> (7 - (this.pos & 7))) & 1;
    this.pos++;
    return v;
  }
  u(n) { let v = 0; for (let i = 0; i < n; i++) v = v * 2 + this.bit(); return v; }
  skip(n) { if (this.pos + n > this.b.length * 8) throw new RangeError('SPS truncated'); this.pos += n; }
  ue() {
    let zeros = 0;
    while (!this.bit()) if (++zeros > 31) throw new RangeError('bad Exp-Golomb code');
    return 2 ** zeros - 1 + this.u(zeros);
  }
}

function rbsp(nal) {
  const out = new Uint8Array(nal.length);
  let n = 0;
  for (let i = 0, zeros = 0; i < nal.length; i++) {
    if (zeros >= 2 && nal[i] === 3) { zeros = 0; continue; }
    out[n++] = nal[i];
    zeros = nal[i] === 0 ? zeros + 1 : 0;
  }
  return out.subarray(0, n);
}

function skipProfileTierLevel(r, maxSubLayersMinus1) {
  r.skip(96);
  const profile = [], level = [];
  for (let i = 0; i < maxSubLayersMinus1; i++) { profile.push(r.bit()); level.push(r.bit()); }
  if (maxSubLayersMinus1 > 0) r.skip(2 * (8 - maxSubLayersMinus1));
  for (let i = 0; i < maxSubLayersMinus1; i++) r.skip((profile[i] ? 88 : 0) + (level[i] ? 8 : 0));
}

function skipScalingLists(r) {
  for (let size = 0; size < 4; size++) {
    for (let m = 0; m < 6; m += size === 3 ? 3 : 1) {
      if (!r.bit()) { r.ue(); continue; }
      if (size > 1) r.ue();
      const coefs = Math.min(64, 1 << (4 + (size << 1)));
      for (let i = 0; i < coefs; i++) r.ue();
    }
  }
}

// Inter predicted sets need the real delta POCs: how many flags the next set has depends on them.
function readRps(r, idx, sets) {
  if (!idx || !r.bit()) {
    const neg = r.ue(), pos = r.ue();
    if (neg > 16 || pos > 16) throw new RangeError('short term set too long');
    const s0 = [], s1 = [];
    for (let i = 0, poc = 0; i < neg; i++) { poc -= r.ue() + 1; r.skip(1); s0.push(poc); }
    for (let i = 0, poc = 0; i < pos; i++) { poc += r.ue() + 1; r.skip(1); s1.push(poc); }
    return { s0, s1 };
  }
  const ref = sets[idx - 1];
  const sign = r.bit(), abs = r.ue() + 1;
  const delta = sign ? -abs : abs;
  const nNeg = ref.s0.length, nPos = ref.s1.length, n = nNeg + nPos;
  const use = [];
  for (let j = 0; j <= n; j++) use.push(r.bit() ? true : r.bit() === 1);
  const s0 = [], s1 = [];
  for (let j = nPos - 1; j >= 0; j--) { const d = ref.s1[j] + delta; if (d < 0 && use[nNeg + j]) s0.push(d); }
  if (delta < 0 && use[n]) s0.push(delta);
  for (let j = 0; j < nNeg; j++) { const d = ref.s0[j] + delta; if (d < 0 && use[j]) s0.push(d); }
  for (let j = nNeg - 1; j >= 0; j--) { const d = ref.s0[j] + delta; if (d > 0 && use[j]) s1.push(d); }
  if (delta > 0 && use[n]) s1.push(delta);
  for (let j = 0; j < nPos; j++) { const d = ref.s1[j] + delta; if (d > 0 && use[nNeg + j]) s1.push(d); }
  return { s0, s1 };
}

const specified = (v, zeroValid) => (v === 2 || (v === 0 && !zeroValid) ? null : v);

/**
 * Colour description of one SPS NAL (type 33, 2 byte header): `{primaries, transfer, matrix, fullRange}`, each null when the VUI
 * does not say (unspecified counts as not saying). Null when the SPS has no video signal type or cannot be parsed.
 */
export function hevcSpsColour(nal) {
  try {
    const r = new Bits(rbsp(nal.subarray(2)));
    r.skip(4);
    const maxSub = r.u(3);
    r.skip(1);
    skipProfileTierLevel(r, maxSub);
    r.ue();
    if (r.ue() === 3) r.skip(1);
    r.ue(); r.ue();
    if (r.bit()) for (let i = 0; i < 4; i++) r.ue();
    r.ue(); r.ue();
    const log2PocLsb = r.ue() + 4;
    const allLayers = r.bit();
    for (let i = allLayers ? 0 : maxSub; i <= maxSub; i++) { r.ue(); r.ue(); r.ue(); }
    for (let i = 0; i < 6; i++) r.ue();
    if (r.bit() && r.bit()) skipScalingLists(r);
    r.skip(2);
    if (r.bit()) { r.skip(8); r.ue(); r.ue(); r.skip(1); }
    const count = r.ue();
    if (count > 64) return null;
    const sets = [];
    for (let i = 0; i < count; i++) sets.push(readRps(r, i, sets));
    if (r.bit()) {
      const n = r.ue();
      if (n > 32) return null;
      for (let i = 0; i < n; i++) r.skip(log2PocLsb + 1);
    }
    r.skip(2);
    if (!r.bit()) return null;
    if (r.bit() && r.u(8) === 255) r.skip(32);
    if (r.bit()) r.skip(1);
    if (!r.bit()) return null;
    r.skip(3);
    const fullRange = r.bit() === 1;
    if (!r.bit()) return { primaries: null, transfer: null, matrix: null, fullRange };
    return { primaries: specified(r.u(8), false), transfer: specified(r.u(8), false), matrix: specified(r.u(8), true), fullRange };
  } catch { return null; }
}

/** Mastering display (payload 137) and content light (144) of one prefix SEI NAL (type 39, 2 byte header), in box layout. */
export function hevcHdrSei(nal) {
  const out = { mastering: null, contentLight: null };
  const b = rbsp(nal.subarray(2));
  const dv = new DataView(b.buffer, b.byteOffset, b.byteLength);
  let p = 0;
  while (p + 2 <= b.length && !(p === b.length - 1 && b[p] === 0x80)) {
    let type = 0, size = 0;
    while (b[p] === 255) { type += 255; p++; }
    type += b[p++];
    while (b[p] === 255) { size += 255; p++; }
    size += b[p++];
    if (!(p + size <= b.length)) break;
    if (type === 137 && size >= 24) {
      out.mastering = [];
      for (let i = 0; i < 8; i++) out.mastering.push(dv.getUint16(p + 2 * i));
      out.mastering.push(dv.getUint32(p + 16), dv.getUint32(p + 20));
    } else if (type === 144 && size >= 4) {
      out.contentLight = [dv.getUint16(p), dv.getUint16(p + 2)];
    }
    p += size;
  }
  return out;
}

function hvccNals(hvcc) {
  const list = [];
  if (!hvcc || hvcc.length < 23) return list;
  let p = 23;
  for (let a = hvcc[22]; a > 0 && p + 3 <= hvcc.length; a--) {
    const n = (hvcc[p + 1] << 8) | hvcc[p + 2]; p += 3;
    for (let i = 0; i < n && p + 2 <= hvcc.length; i++) {
      const len = (hvcc[p] << 8) | hvcc[p + 1]; p += 2;
      list.push(hvcc.subarray(p, p + len)); p += len;
    }
  }
  return list;
}

// The NALs in front of the first picture: parameter sets and prefix SEI are never behind it.
function accessUnitNals(au, ls) {
  const list = [];
  for (let i = 0; i + ls < au.length;) {
    let len = 0; for (let k = 0; k < ls; k++) len = len * 256 + au[i + k];
    if (((au[i + ls] >> 1) & 63) < 32) break;
    list.push(au.subarray(i + ls, i + ls + len));
    i += ls + len;
  }
  return list;
}

function fromNals(nals) {
  const out = { colour: null, mastering: null, contentLight: null };
  for (const nal of nals) {
    if (nal.length < 3) continue;
    const type = (nal[0] >> 1) & 63;
    if (type === 33 && !out.colour) out.colour = hevcSpsColour(nal);
    else if (type === 39) {
      const s = hevcHdrSei(nal);
      out.mastering ??= s.mastering;
      out.contentLight ??= s.contentLight;
    }
  }
  return out;
}

// A colour volume outside the box's range (chromaticity above 1, a peak not above the black level) describes no display, and an
// all zero one is how some encoders say "not filled in".
function validMastering(m) {
  if (!m || m.length !== 10) return null;
  return m.slice(0, 8).every(v => v > 0 && v <= 50000) && m[8] > m[9] ? m : null;
}

const isHevc = codec => /^(hvc1|hev1|dvh1|dvhe)/.test(codec);

/**
 * Sample entry values for a video track: `{color, mastering, contentLight}`. `au` is the first keyframe (length prefixed), when
 * there is one yet. `color` is null unless primaries, transfer and matrix are all known: a colr box with holes says less than none.
 */
export function hdrSignal(track, au = null) {
  const c = track.containerColor || {};
  const hevc = isHevc(track.codec);
  const box = hevc ? fromNals(hvccNals(track.description)) : {};
  const ls = track.description?.length > 21 ? (track.description[21] & 3) + 1 : 4;
  const key = hevc && au ? fromNals(accessUnitNals(au, ls)) : {};
  const field = name => c[name] ?? box.colour?.[name] ?? key.colour?.[name] ?? null;
  const [primaries, transfer, matrix] = [field('primaries'), field('transfer'), field('matrix')];
  const color = primaries !== null && transfer !== null && matrix !== null
    ? { primaries, transfer, matrix, fullRange: !!field('fullRange') } : null;
  const mastering = validMastering(c.mastering) ?? validMastering(box.mastering) ?? validMastering(key.mastering);
  const contentLight = c.contentLight ?? box.contentLight ?? key.contentLight ?? null;
  return { color, mastering, contentLight };
}

/** True when the first keyframe can still add something: only HEVC is read there, and only what the track is short of. */
export function hdrSignalNeedsKeyframe(track, signal) {
  if (!isHevc(track.codec)) return false;
  return !signal.color || (signal.color.transfer === 16 && (!signal.mastering || !signal.contentLight));
}
