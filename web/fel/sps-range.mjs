// SPDX-License-Identifier: LGPL-3.0-only
// video_full_range_flag of an HEVC SPS (VUI), read without parsing the whole SPS.
//
// Why: Chrome applies the range flag of the bitstream's VUI when it converts a hardware decoded frame to RGB, and VideoFrame.colorSpace.fullRange
// does not report it (it says limited). Some files carry the flag wrong: the iPhone 4K60 Dolby Vision 8.4 + HLG MOV has video_full_range_flag = 1
// but limited range codes (black 64, HLG reference white 721; the RPU says bl_video_full_range_flag = 0). The compositor's RECOVER stage has to invert
// the conversion Chrome really did to get the codes back, so it needs the flag the SPS carries, whatever the picture is.
//
// How: the VUI holds `video_format u(3), video_full_range_flag u(1), colour_description_present_flag u(1) = 1, primaries u(8), transfer u(8), matrix u(8)`
// (the player relabels primaries / transfer, so those two are known), a 29 bit pattern that is searched in the unescaped SPS from the end.

/** Unescapes an RBSP (drops emulation prevention bytes). */
function rbsp(nal) {
  const out = [];
  for (let i = 0, zeros = 0; i < nal.length; i++) {
    if (zeros >= 2 && nal[i] === 3) { zeros = 0; continue; }
    out.push(nal[i]); zeros = nal[i] === 0 ? zeros + 1 : 0;
  }
  return out;
}

/** The SPS NALs (type 33, with their 2 byte header) of an hvcC record. */
export function hvccSps(hvcc) {
  const list = [];
  if (!hvcc || hvcc.length < 23) return list;
  let p = 22;
  for (let a = hvcc[p++]; a > 0 && p + 3 <= hvcc.length; a--) {
    const type = hvcc[p] & 63, n = (hvcc[p + 1] << 8) | hvcc[p + 2]; p += 3;
    for (let i = 0; i < n && p + 2 <= hvcc.length; i++) {
      const len = (hvcc[p] << 8) | hvcc[p + 1]; p += 2;
      if (type === 33) list.push(hvcc.subarray(p, p + len));
      p += len;
    }
  }
  return list;
}

const PRIMARIES = new Set([1, 4, 5, 6, 7, 8, 9, 10, 11, 12, 22]);
const TRANSFERS = new Set([1, 4, 5, 6, 7, 8, 13, 14, 15, 16, 17, 18]);
const MATRICES = new Set([0, 1, 4, 5, 6, 7, 8, 9, 10]);

/**
 * @param {Uint8Array} sps one SPS NAL (type 33, 2 byte header)
 * @param {{primaries?:number, transfer?:number}} [known] colour values to look for (the SDR carrier after relabelling); any plausible set when omitted
 * @returns {boolean|null} video_full_range_flag, null when the VUI has no colour description (or no VUI)
 */
export function spsFullRange(sps, known = {}) {
  const b = rbsp(sps);
  const bit = i => (b[i >> 3] >> (7 - (i & 7))) & 1;
  const field = (at, n) => { let v = 0; for (let i = 0; i < n; i++) v = v * 2 + bit(at + i); return v; };
  const total = b.length * 8;
  for (let o = total - 40; o >= 16; o--) {          // o: the video_format bit; the flags follow (full range at o+3, description present at o+4)
    if (!bit(o + 4)) continue;
    const p = field(o + 5, 8), t = field(o + 13, 8), m = field(o + 21, 8);
    const hit = known.primaries !== undefined && known.transfer !== undefined ? p === known.primaries && t === known.transfer : PRIMARIES.has(p) && TRANSFERS.has(t);
    if (hit && MATRICES.has(m) && field(o, 3) <= 5) return bit(o + 3) === 1;
  }
  return null;
}

/** Range flag of the first SPS of an hvcC record (see `spsFullRange`). */
export function hvccFullRange(hvcc, known) {
  const s = hvccSps(hvcc)[0];
  return s ? spsFullRange(s, known) : null;
}

/**
 * HDR transfer of an hvcC record's first SPS: 16 (PQ) or 18 (HLG) with BT.2020 primaries, null otherwise (SDR, no colour description,
 * or no parameter sets in the record). Same pattern search as `spsFullRange`.
 */
export function hvccHdrTransfer(hvcc) {
  const s = hvccSps(hvcc)[0];
  if (!s) return null;
  for (const transfer of [16, 18]) if (spsFullRange(s, { primaries: 9, transfer }) !== null) return transfer;
  return null;
}

/**
 * Range flag of the first SPS carried in a length prefixed access unit, null when it carries none (or its SPS has no colour description).
 * A stream whose hvcC record holds no parameter sets states its range only here, and the in-band SPS is the one the decoder goes by.
 */
export function packetFullRange(data, lengthSize = 4, known) {
  for (let p = 0; p + lengthSize <= data.length;) {
    let len = 0;
    for (let i = 0; i < lengthSize; i++) len = len * 256 + data[p + i];
    const start = p + lengthSize;
    if (len < 3 || start + len > data.length) return null;
    if (((data[start] >> 1) & 63) === 33) return spsFullRange(data.subarray(start, start + len), known);
    p = start + len;
  }
  return null;
}
