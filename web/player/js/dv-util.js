// SPDX-License-Identifier: LGPL-3.0-only
// Small bitstream helpers for the compositor path (HEVC, length prefixed access units).

export function lengthSizeOfHvcc(desc) { return desc && desc.length > 21 ? (desc[21] & 3) + 1 : 4; }

// IRAP (IDR, CRA, BLA: NAL types 16..23) present in a length prefixed access unit. WebCodecs wants 'key' only on these.
export function isIrap(data, ls = 4) {
  let i = 0;
  while (i + ls < data.length) {
    let len = 0; for (let k = 0; k < ls; k++) len = len * 256 + data[i + k];
    const t = (data[i + ls] >> 1) & 63;
    if (t >= 16 && t <= 23) return true;
    if (t < 16) return false;      // first VCL NAL is not IRAP: the picture is not
    i += ls + len;
  }
  return false;
}

// Length prefixed to Annex B (4 byte start codes). Returns a new array.
export function toAnnexB(data, ls = 4) {
  let n = 0, i = 0;
  while (i + ls <= data.length) { let len = 0; for (let k = 0; k < ls; k++) len = len * 256 + data[i + k]; n += 4 + len; i += ls + len; }
  const out = new Uint8Array(n);
  let o = 0; i = 0;
  while (i + ls <= data.length) {
    let len = 0; for (let k = 0; k < ls; k++) len = len * 256 + data[i + k];
    out[o + 3] = 1; o += 4;
    out.set(data.subarray(i + ls, i + ls + len), o); o += len; i += ls + len;
  }
  return out.subarray(0, o);
}

// Parameter set NALs stored in an hvcC record, as one Annex B blob (empty when the record carries none, e.g. Matroska).
export function hvccAnnexB(desc) {
  if (!desc || desc.length < 23) return new Uint8Array(0);
  const parts = []; let p = 23;
  const arrays = desc[22];
  for (let a = 0; a < arrays && p + 3 <= desc.length; a++) {
    const num = (desc[p + 1] << 8) | desc[p + 2]; p += 3;
    for (let k = 0; k < num && p + 2 <= desc.length; k++) {
      const len = (desc[p] << 8) | desc[p + 1]; p += 2;
      parts.push(Uint8Array.of(0, 0, 0, 1), desc.subarray(p, p + len)); p += len;
    }
  }
  const out = new Uint8Array(parts.reduce((s, x) => s + x.length, 0));
  let o = 0; for (const x of parts) { out.set(x, o); o += x.length; }
  return out;
}

export function concat(list) {
  const out = new Uint8Array(list.reduce((s, x) => s + x.length, 0));
  let o = 0; for (const x of list) { out.set(x, o); o += x.length; }
  return out;
}

// NAL type of the first VCL NAL of a length prefixed access unit (-1 when none).
export function firstVclType(data, ls = 4) {
  let i = 0;
  while (i + ls < data.length) {
    let len = 0; for (let k = 0; k < ls; k++) len = len * 256 + data[i + k];
    const t = (data[i + ls] >> 1) & 63;
    if (t < 32) return t;
    i += ls + len;
  }
  return -1;
}

// Cuts a stream of access units into decodable units for the exact pool. A unit starts at an IRAP picture. A unit that starts
// at a CRA has leading pictures (RASL) that need the previous unit as decode prefix: the previous bytes are decoded again and
// their outputs skipped (web/exact/pool.js loadFixtures does the same from a fixture index). Without a previous unit (stream
// start, after a seek) the RASL pictures are not decodable and the decoder drops them, so their times are not expected.
export class GopBuilder {
  constructor(psAnnexB, submit) { this.ps = psAnnexB; this.submit = submit; this.cur = null; this.prev = null; }
  push(ptsUs, annexB, type) {
    const irap = type >= 16 && type <= 23;
    if (irap) { this.flush(); this.cur = { pts: [], rasl: [], chunks: [this.ps], cra: type === 21 || (type >= 16 && type <= 18) }; }
    if (!this.cur) return false;
    this.cur.pts.push(ptsUs);
    if (type === 8 || type === 9) this.cur.rasl.push(ptsUs);
    this.cur.chunks.push(annexB);
    return true;
  }
  flush() {
    const c = this.cur; if (!c) return;
    this.cur = null;
    const bytes = concat(c.chunks);
    const prefixed = c.cra && this.prev;
    if (prefixed) {
      // the prefix starts at its own CRA when it had one: its leading pictures are dropped by the decoder
      const skip = this.prev.pts.length - (this.prev.cra ? this.prev.rasl.length : 0);
      this.submit({ bytes: concat([this.prev.bytes, bytes]), ptsList: c.pts, skip });
    } else {
      const drop = new Set(c.rasl);
      this.submit({ bytes, ptsList: c.cra ? c.pts.filter(p => !drop.has(p)) : c.pts, skip: 0 });
    }
    this.prev = { bytes, pts: c.pts, rasl: c.rasl, cra: c.cra };
  }
  reset() { this.cur = null; this.prev = null; }
}
