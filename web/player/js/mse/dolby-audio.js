// SPDX-License-Identifier: LGPL-3.0-only
// AC-3 and E-AC-3 sample entry configuration (dac3, dec3; ETSI TS 102 366 annex F), built from the first access unit of the track.
// Matroska carries no such record, and the sample entry is required before the first fragment.
class Bits {
  constructor(d, pos = 0) { this.d = d; this.p = pos * 8; }
  u(n) { let v = 0; for (let i = 0; i < n; i++, this.p++) v = (v * 2) + ((this.d[this.p >> 3] >> (7 - (this.p & 7))) & 1); return v; }
  skip(n) { this.p += n; }
}
class Writer {
  constructor() { this.bits = []; }
  u(n, v) { for (let i = n - 1; i >= 0; i--) this.bits.push(Math.floor(v / 2 ** i) & 1); }
  bytes() { const o = new Uint8Array(this.bits.length >> 3); for (let i = 0; i < o.length; i++) for (let b = 0; b < 8; b++) o[i] = (o[i] << 1) | this.bits[i * 8 + b]; return o; }
}
const AC3_KBPS = [32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384, 448, 512, 576, 640];
const ACMOD_CH = [2, 1, 2, 3, 3, 4, 4, 5];

export function buildDac3(pkt) {
  if (pkt[0] !== 0x0b || pkt[1] !== 0x77) return null;
  const b = new Bits(pkt, 4);
  const fscod = b.u(2), frmsizecod = b.u(6), bsid = b.u(5), bsmod = b.u(3), acmod = b.u(3);
  if (fscod === 3 || bsid > 10) return null;
  if ((acmod & 1) && acmod !== 1) b.skip(2);
  if (acmod & 4) b.skip(2);
  if (acmod === 2) b.skip(2);
  const lfeon = b.u(1);
  const w = new Writer();
  w.u(2, fscod); w.u(5, bsid); w.u(3, bsmod); w.u(3, acmod); w.u(1, lfeon); w.u(5, frmsizecod >> 1); w.u(5, 0);
  return { box: 'dac3', config: w.bytes(), channels: ACMOD_CH[acmod] + lfeon, sampleRate: [48000, 44100, 32000][fscod] };
}

// A legacy AC-3 frame (bsid up to 8, some encoders up to 10) as the independent substream of a 7.1 E-AC-3 stream.
function ac3CoreFrame(d, o) {
  const b = new Bits(d, o + 4);
  const fscod = b.u(2), code = b.u(6), bsid = b.u(5);
  if (fscod === 3 || code >> 1 >= AC3_KBPS.length) return null;
  const kbps = AC3_KBPS[code >> 1];
  const words = fscod === 0 ? 2 * kbps : fscod === 2 ? 3 * kbps : Math.floor((kbps * 96000) / 44100) + (code & 1);
  b.skip(3);
  const acmod = b.u(3);
  if ((acmod & 1) && acmod !== 1) b.skip(2);
  if (acmod & 4) b.skip(2);
  if (acmod === 2) b.skip(2);
  return { strmtyp: 0, substreamid: 0, bytes: words * 2, fscod, acmod, lfeon: b.u(1), bsid, numblks: 6, chanmap: 0 };
}

// The JOC complexity an independent substream announces in its additional bitstream information: first byte's low bit
// flag_ec3_extension_type_a, second byte the complexity. Matroska and MPEG-TS carry no dec3, so without this Apple's decoder is told plain
// E-AC-3 and only ever offers the 5.1 bed (measured: format list ec-3 6 ch only, against ec+3 up to 16 ch with the complexity in dec3).
function jocFromBsi(b, strmtyp, acmod, lfeon, numblks, fscod) {
  const programs = acmod === 0 ? 2 : 1;
  if (b.u(1)) {   // mixmdate
    if (acmod > 2) { b.skip(2); if (acmod & 1) b.skip(6); if (acmod & 4) b.skip(6); }
    if (lfeon && b.u(1)) b.skip(5);
    if (strmtyp === 0) {
      for (let i = 0; i < programs; i++) if (b.u(1)) b.skip(6);
      if (b.u(1)) b.skip(6);
      const mixdef = b.u(2);
      if (mixdef === 1) b.skip(5); else if (mixdef === 2) b.skip(12); else if (mixdef === 3) b.skip((b.u(5) + 2) * 8);
      if (acmod < 2) for (let i = 0; i < programs; i++) if (b.u(1)) b.skip(14);
      if (b.u(1)) for (let blk = 0; blk < numblks; blk++) if (numblks === 1 || b.u(1)) b.skip(5);
    }
  }
  if (b.u(1)) {   // infomdate
    b.skip(5);
    if (acmod === 2) b.skip(4);
    if (acmod >= 6) b.skip(2);
    for (let i = 0; i < programs; i++) if (b.u(1)) b.skip(8);
    if (fscod !== 3) b.skip(1);
  }
  if (strmtyp === 0 && numblks !== 6) b.skip(1);
  if (strmtyp === 2 && (numblks === 6 || b.u(1))) b.skip(6);
  if (!b.u(1) || b.u(6) < 1) return 0;
  b.skip(7);
  return b.u(1) ? b.u(8) : 0;
}

// One E-AC-3 syncframe header. bytes: the frame length.
function eac3Frame(d, o) {
  if (o + 6 > d.length || d[o] !== 0x0b || d[o + 1] !== 0x77) return null;
  if ((d[o + 5] >> 3) <= 10) return ac3CoreFrame(d, o);
  const b = new Bits(d, o + 2);
  const strmtyp = b.u(2), substreamid = b.u(3), frmsiz = b.u(11), fscod = b.u(2);
  let numblks = 6;
  if (fscod === 3) b.skip(2); else numblks = [1, 2, 3, 6][b.u(2)];
  const acmod = b.u(3), lfeon = b.u(1), bsid = b.u(5);
  if (bsid <= 10 || bsid > 16) return null;
  b.skip(5);   // dialnorm
  if (b.u(1)) b.skip(8);
  if (acmod === 0) { b.skip(5); if (b.u(1)) b.skip(8); }
  let chanmap = 0;
  if (strmtyp === 1 && b.u(1)) chanmap = b.u(16);
  const joc = strmtyp === 1 ? 0 : jocFromBsi(b, strmtyp, acmod, lfeon, numblks, fscod);
  return { strmtyp, substreamid, bytes: (frmsiz + 1) * 2, fscod, acmod, lfeon, bsid, numblks, chanmap, joc };
}

// Audio blocks the first independent substream carries in this packet (six make one access unit), 0 if the packet does not parse.
export function eac3Blocks(pkt) {
  let blocks = 0, id = -1;
  for (let o = 0; o < pkt.length;) {
    const f = eac3Frame(pkt, o);
    if (!f) break;
    if (f.strmtyp !== 1 && (id < 0 || f.substreamid === id)) { id = f.substreamid; blocks += f.numblks; }
    o += f.bytes;
  }
  return blocks;
}

// Dependent substream channel map (bits from the left) to the 9 bit chan_loc of dec3 (TS 102 366 tables E.1.4 and F.6.2).
function chanLoc(chanmap) {
  const map = [[5, 0], [6, 1], [7, 2], [8, 3], [9, 4], [10, 5], [11, 6], [12, 7], [13, 8]];   // [bit from the left in chanmap, chan_loc bit]
  let loc = 0;
  for (const [from, to] of map) if ((chanmap >> (15 - from)) & 1) loc |= 1 << to;
  return loc;
}
const LOC_CH = [2, 2, 1, 1, 2, 2, 2, 1, 1];

export function buildDec3(pkt, jocComplexity = 0) {
  const frames = [];
  for (let o = 0; o < pkt.length;) {
    const f = eac3Frame(pkt, o);
    if (!f || (f.strmtyp !== 1 && frames.some(x => x.strmtyp !== 1 && x.substreamid === f.substreamid))) break;   // the next access unit
    frames.push(f); o += f.bytes;
  }
  const ind = frames.filter(f => f.strmtyp !== 1);
  if (!ind.length) return null;
  const f0 = ind[0];
  // kbit/s of this access unit, for the informative data_rate field
  const bytes = frames.reduce((n, f) => n + f.bytes, 0);
  const rate = [48000, 44100, 32000][f0.fscod] ?? 48000;
  const w = new Writer();
  w.u(13, Math.min(8191, Math.round((bytes * 8 * rate) / (f0.numblks * 256) / 1000)));
  w.u(3, ind.length - 1);
  let channels = 0;
  for (const f of ind) {
    const deps = frames.filter(d => d.strmtyp === 1 && d.substreamid === f.substreamid && frames.indexOf(d) > frames.indexOf(f));
    let loc = 0;
    for (const d of deps) loc |= chanLoc(d.chanmap);
    w.u(2, f.fscod); w.u(5, f.bsid); w.u(1, 0); w.u(1, 0); w.u(3, 0); w.u(3, f.acmod); w.u(1, f.lfeon); w.u(3, 0);
    w.u(4, deps.length);
    if (deps.length) w.u(9, loc); else w.u(1, 0);
    if (!channels) { channels = ACMOD_CH[f.acmod] + f.lfeon; LOC_CH.forEach((n, i) => { if ((loc >> i) & 1) channels += n; }); }
  }
  const joc = jocComplexity || f0.joc || 0;
  if (joc > 0) { w.u(7, 0); w.u(1, 1); w.u(8, joc); }
  while (w.bits.length % 8) w.bits.push(0);
  return { box: 'dec3', config: w.bytes(), channels, sampleRate: rate };
}
