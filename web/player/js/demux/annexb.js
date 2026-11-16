// SPDX-License-Identifier: LGPL-3.0-only
// Annex B <-> length-prefixed helpers for transport-stream H.264 and HEVC. Chrome's hardware (VideoToolbox) decoder rejects some
// Annex B streams that carry no `description` ("EncodingError: Decoding error" after the first packets), while the same
// stream with an avcC description and 4-byte length prefixes decodes. The Rust demuxer reports TS video without a
// description, so the player builds the avcC from the in-band SPS/PPS and rewrites the packets.

/** NAL units of an Annex B access unit as subarrays (start codes removed, trailing zero bytes trimmed). */
export function splitAnnexB(b) {
  const nals = [];
  let i = 0, start = -1;
  const n = b.length;
  while (i + 2 < n) {
    if (b[i + 2] > 1) { i += 3; continue; }
    if (b[i] === 0 && b[i + 1] === 0 && b[i + 2] === 1) {
      if (start >= 0) { let end = i; while (end > start && b[end - 1] === 0) end--; nals.push(b.subarray(start, end)); }
      start = i + 3; i += 3;
    } else i++;
  }
  if (start >= 0) { let end = n; while (end > start && b[end - 1] === 0) end--; nals.push(b.subarray(start, end)); }
  return nals;
}

// Blu-ray H.264 redefines one picture parameter set id from picture to picture (measured on three M2TS samples: id 0 carries
// two to four different contents, differing in num_ref_idx_l0_default_active_minus1 or the chroma QP offset). Chrome hands
// VideoToolbox slices only and moves the parameter sets through the format description, and VideoToolbox keeps the first
// content it saw for an id: the plain <video> element fails on an ffmpeg stream copy of such a file with -12909 as soon as
// the picture has content, and WebCodecs reports "EncodingError: Decoding error". Giving every distinct content its own id
// makes the stream an ordinary multi-PPS stream. The id is an Exp-Golomb code near the start of the PPS and of every slice
// header; a new id whose code is exactly 8 or 16 bits longer moves everything behind it by whole bytes, so neither the CABAC
// alignment nor the slice data has to be touched (id 0 is 1 bit, ids 15 to 30 are 9 bits, id 255 is 17 bits).
const HEAD = 14;
function ueAt(r, bit) {   // [value, code length] of the Exp-Golomb code at `bit` of the byte array r, or null
  let z = 0, p = bit;
  const get = () => (p < r.length * 8 ? (r[p >> 3] >> (7 - (p & 7))) & 1 : -1);
  for (let g; (g = get()) === 0; p++) z++;
  if (get() < 0 || z > 15) return null;
  let v = 0;
  for (let i = 0; i <= z; i++, p++) { const g = get(); if (g < 0) return null; v = v * 2 + g; }
  return [v - 1, 2 * z + 1];
}
function ueCode(v) { const n = v + 1, len = 2 * (31 - Math.clz32(n)) + 1; return [n, len]; }

export class PpsRenumber {
  constructor() { this.ids = new Map(); this.current = new Map(); this.used = new Set(); this.failed = false; this.rewritten = 0; this.gen = 0; this.seen = new Map(); this.recycled = 0; }
  /** A NAL unit (no start code) -> the same NAL, or a copy that carries the renumbered id. */
  nal(x) {
    const type = x[0] & 31;
    if (this.failed || (type !== 8 && type !== 1 && type !== 5)) return x;
    if (type === 8) {
      const id0 = ueAt(unescapeHead(x, x.length), 0);
      if (!id0) return x;
      const key = id0[0] + ':' + hexOf(x);
      let id = this.ids.get(key);
      if (id === undefined) {
        id = this.#assign(id0[0], id0[1]);
        if (id < 0) { this.failed = true; return x; }
        this.ids.set(key, id);
      }
      this.current.set(id0[0], id); this.seen.set(id, this.gen);
      return id === id0[0] ? x : this.#rewrite(x, 0, id0[1], id, x.length);
    }
    let h = Math.min(x.length, HEAD);
    while (h < x.length && (x[h - 1] === 0 || x[h - 1] === 3)) h++;
    const r = unescapeHead(x, h);
    const a = ueAt(r, 0), b = a && ueAt(r, a[1]), c = b && ueAt(r, a[1] + b[1]);
    if (!c) return x;
    if (type === 5 && a[0] === 0) this.gen++;   // first slice of an IDR picture: nothing before it is referenced again
    const id = this.current.get(c[0]);
    if (id !== undefined) this.seen.set(id, this.gen);
    if (id === undefined || id === c[0]) return x;
    return this.#rewrite(x, a[1] + b[1], c[1], id, h);
  }
  // First content of an id keeps it. Later contents take the lowest free id whose code is 8 (then 16) bits longer.
  #assign(orig, len) {
    if (!this.used.has(orig)) { this.used.add(orig); return orig; }
    for (const extra of [8, 16]) {
      const z = (len + extra - 1) / 2, lo = 2 ** z - 1, hi = Math.min(2 ** (z + 1) - 2, 255);
      for (let id = lo; id <= hi; id++) if (!this.used.has(id)) { this.used.add(id); return id; }
    }
    // Every id of both lengths is taken (18 contents under one original id). A content that no slice has named since the last
    // IDR picture and that no original id currently maps to cannot be needed again without its PPS being sent again, so its id
    // is given to the new content. The longest unused one goes first.
    const live = new Set(this.current.values());
    let best = -1;
    for (const extra of [8, 16]) {
      const z = (len + extra - 1) / 2, lo = 2 ** z - 1, hi = Math.min(2 ** (z + 1) - 2, 255);
      for (let id = lo; id <= hi; id++) if (!live.has(id) && (this.seen.get(id) ?? -1) < this.gen && (best < 0 || (this.seen.get(id) ?? -1) < (this.seen.get(best) ?? -1))) best = id;
    }
    if (best < 0) return -1;
    for (const [k, v] of this.ids) if (v === best) this.ids.delete(k);
    this.recycled++;
    return best;
  }
  #rewrite(x, bit, oldLen, id, h) {
    const r = unescapeHead(x, h), [code, len] = ueCode(id), grow = (len - oldLen) >> 3;
    const out = new Uint8Array(r.length + grow);
    const put = (pos, v) => { if (v) out[pos >> 3] |= 0x80 >> (pos & 7); };
    const get = pos => (r[pos >> 3] >> (7 - (pos & 7))) & 1;
    for (let i = 0; i < bit; i++) put(i, get(i));
    for (let i = 0; i < len; i++) put(bit + i, (code >> (len - 1 - i)) & 1);
    for (let i = bit + oldLen, o = bit + len; i < r.length * 8; i++, o++) put(o, get(i));
    const head = escapeRbsp(out), res = new Uint8Array(1 + head.length + x.length - h);
    res[0] = x[0]; res.set(head, 1); res.set(x.subarray(h), 1 + head.length);
    this.rewritten++;
    return res;
  }
}
const hexOf = b => { let s = ''; for (let i = 0; i < b.length; i++) s += (b[i] < 16 ? '0' : '') + b[i].toString(16); return s; };
// RBSP of NAL bytes [1, h): emulation prevention bytes removed.
function unescapeHead(x, h) {
  const out = new Uint8Array(h - 1);
  let o = 0;
  for (let i = 1; i < h; i++) { if (i >= 3 && x[i] === 3 && x[i - 1] === 0 && x[i - 2] === 0) continue; out[o++] = x[i]; }
  return out.subarray(0, o);
}
function escapeRbsp(r) {
  const out = [];
  let zeros = 0;
  for (const v of r) { if (zeros >= 2 && v <= 3) { out.push(3); zeros = 0; } out.push(v); zeros = v === 0 ? zeros + 1 : 0; }
  return out;
}

/** avcC record from the first SPS and every distinct PPS in `nals` (pass them through PpsRenumber first), or null. */
export function buildAvcCAll(nals) {
  const sps = nals.find(x => (x[0] & 31) === 7), seen = new Set(), pps = [];
  for (const x of nals) if ((x[0] & 31) === 8 && !seen.has(hexOf(x)) && pps.length < 31) { seen.add(hexOf(x)); pps.push(x); }
  if (!sps || !pps.length || sps.length < 4) return null;
  const out = new Uint8Array(9 + sps.length + pps.reduce((n, x) => n + 2 + x.length, 0));
  out.set([1, sps[1], sps[2], sps[3], 0xff, 0xe1, sps.length >> 8, sps.length & 255], 0);
  out.set(sps, 8);
  let o = 8 + sps.length;
  out[o++] = pps.length;
  for (const x of pps) { out[o++] = x.length >> 8; out[o++] = x.length & 255; out.set(x, o); o += x.length; }
  return out;
}

/** Annex B access unit -> 4-byte length prefixed NALs (AUD dropped). `renumber`: a PpsRenumber applied to every NAL. */
export function annexBToLengthPrefixed(b, renumber) {
  let nals = splitAnnexB(b).filter(x => (x[0] & 31) !== 9);
  if (renumber) nals = nals.map(x => renumber.nal(x));
  let total = 0;
  for (const x of nals) total += 4 + x.length;
  const out = new Uint8Array(total);
  let o = 0;
  for (const x of nals) {
    out[o] = x.length >>> 24; out[o + 1] = (x.length >>> 16) & 255; out[o + 2] = (x.length >>> 8) & 255; out[o + 3] = x.length & 255;
    out.set(x, o + 4); o += 4 + x.length;
  }
  return out;
}

export function avcCodecFromAvcC(avcc) {
  const h = v => v.toString(16).padStart(2, '0');
  return `avc1.${h(avcc[1])}${h(avcc[2])}${h(avcc[3])}`;
}

// ------------------------------------------------------------------ HEVC
const hevcType = nal => (nal[0] >> 1) & 63;

function unescapeRbsp(nal) {
  const out = new Uint8Array(nal.length);
  let o = 0;
  for (let i = 0; i < nal.length; i++) {
    if (i >= 2 && nal[i] === 3 && nal[i - 1] === 0 && nal[i - 2] === 0) continue;
    out[o++] = nal[i];
  }
  return out.subarray(0, o);
}

class Bits {
  constructor(b) { this.b = b; this.p = 0; }
  u(n) { let v = 0; for (let i = 0; i < n; i++) { const byte = this.b[this.p >> 3] ?? 0; v = v * 2 + ((byte >> (7 - (this.p & 7))) & 1); this.p++; } return v; }
  ue() { let z = 0; while (this.u(1) === 0 && z < 32) z++; return z ? 2 ** z - 1 + this.u(z) : 0; }
  skip(n) { this.p += n; }
}

// profile_tier_level, general part: [space, tier, idc, compat(4), constraints(6), level]; also skips the sub-layer parts.
function parseSps(nal) {
  const r = unescapeRbsp(nal.subarray(2));
  const bits = new Bits(r);
  bits.u(4);                              // vps id
  const maxSub = bits.u(3);
  bits.u(1);                              // temporal id nesting
  const space = bits.u(2), tier = bits.u(1), idc = bits.u(5);
  const compat = bits.u(32);
  const constraints = []; for (let i = 0; i < 6; i++) constraints.push(bits.u(8));
  const level = bits.u(8);
  const subProfile = [], subLevel = [];
  for (let i = 0; i < maxSub; i++) { subProfile.push(bits.u(1)); subLevel.push(bits.u(1)); }
  if (maxSub > 0) for (let i = maxSub; i < 8; i++) bits.u(2);
  for (let i = 0; i < maxSub; i++) { if (subProfile[i]) bits.skip(88); if (subLevel[i]) bits.skip(8); }
  bits.ue();                              // sps id
  const chroma = bits.ue();
  if (chroma === 3) bits.u(1);
  const w = bits.ue(), h = bits.ue();
  let crop = [0, 0, 0, 0];
  if (bits.u(1)) crop = [bits.ue(), bits.ue(), bits.ue(), bits.ue()];
  const bdY = bits.ue() + 8, bdC = bits.ue() + 8;
  return { space, tier, idc, compat, constraints, level, chroma, w, h, crop, bdY, bdC };
}

/** hvcC record from the in-band VPS, SPS and PPS of an Annex B access unit (NALs from splitAnnexB), or null. */
export function buildHvcC(nals) {
  const vps = nals.find(x => hevcType(x) === 32), sps = nals.find(x => hevcType(x) === 33), pps = nals.find(x => hevcType(x) === 34);
  if (!vps || !sps || !pps || sps.length < 16) return null;
  let p;
  try { p = parseSps(sps); } catch { return null; }
  const sets = [[32, vps], [33, sps], [34, pps]];
  const out = [];
  const head = new Uint8Array(23);
  head[0] = 1;
  head[1] = (p.space << 6) | (p.tier << 5) | p.idc;
  head[2] = (p.compat >>> 24) & 255; head[3] = (p.compat >>> 16) & 255; head[4] = (p.compat >>> 8) & 255; head[5] = p.compat & 255;
  head.set(p.constraints, 6);
  head[12] = p.level;
  head[13] = 0xf0; head[14] = 0;           // min_spatial_segmentation_idc 0
  head[15] = 0xfc;                          // parallelismType 0
  head[16] = 0xfc | (p.chroma & 3);
  head[17] = 0xf8 | ((p.bdY - 8) & 7);
  head[18] = 0xf8 | ((p.bdC - 8) & 7);
  head[19] = 0; head[20] = 0;               // avgFrameRate
  head[21] = (0 << 6) | (0 << 3) | (0 << 2) | 3;   // constantFrameRate 0, numTemporalLayers 0, nested 0, lengthSizeMinusOne 3
  head[22] = 3;
  out.push(head);
  for (const [t, n] of sets) out.push(Uint8Array.of(0x80 | t, 0, 1, n.length >> 8, n.length & 255), n);
  const tot = out.reduce((s, x) => s + x.length, 0);
  const rec = new Uint8Array(tot);
  let o = 0; for (const x of out) { rec.set(x, o); o += x.length; }
  return rec;
}

/** Annex B HEVC access unit -> 4-byte length prefixed NALs (access unit delimiters dropped). */
export function annexBToLengthPrefixedHevc(b) {
  const nals = splitAnnexB(b).filter(x => hevcType(x) !== 35);
  let total = 0;
  for (const x of nals) total += 4 + x.length;
  const out = new Uint8Array(total);
  let o = 0;
  for (const x of nals) {
    out[o] = x.length >>> 24; out[o + 1] = (x.length >>> 16) & 255; out[o + 2] = (x.length >>> 8) & 255; out[o + 3] = x.length & 255;
    out.set(x, o + 4); o += 4 + x.length;
  }
  return out;
}

// H.264 SPS facts that decide whether Chrome's hardware decoder takes the stream (interlaced coding is refused). Input: an avcC record.
export function h264SpsFacts(avcc) {
  try {
    if (!avcc || avcc.length < 12 || (avcc[5] & 31) < 1) return null;
    const len = (avcc[6] << 8) | avcc[7];
    const sps = unescapeRbsp(avcc.subarray(8 + 1, 8 + len));    // skip the NAL header byte
    const b = new Bits(sps);
    const profile = b.u(8); b.u(8); const level = b.u(8);
    b.ue();
    let chroma = 1;
    if ([100, 110, 122, 244, 44, 83, 86, 118, 128, 138, 139, 134, 135].includes(profile)) {
      chroma = b.ue(); if (chroma === 3) b.u(1);
      b.ue(); b.ue(); b.u(1);
      if (b.u(1)) { const n = chroma !== 3 ? 8 : 12; for (let i = 0; i < n; i++) if (b.u(1)) { let last = 8, next = 8; const size = i < 6 ? 16 : 64; for (let j = 0; j < size; j++) { if (next !== 0) { const d = b.ue(); const delta = (d & 1) ? (d + 1) >> 1 : -(d >> 1); next = (last + delta + 256) % 256; } last = next === 0 ? last : next; } } }
    }
    b.ue();
    const pocType = b.ue();
    if (pocType === 0) b.ue();
    else if (pocType === 1) { b.u(1); b.ue(); b.ue(); const n = b.ue(); for (let i = 0; i < n; i++) b.ue(); }
    const refs = b.ue(); b.u(1); b.ue(); b.ue();
    const frameMbsOnly = b.u(1);
    const mbaff = frameMbsOnly ? 0 : b.u(1);
    return { profile, level, refs, pocType, frameMbsOnly: !!frameMbsOnly, mbaff: !!mbaff };
  } catch { return null; }
}
