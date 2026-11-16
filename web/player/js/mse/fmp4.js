// SPDX-License-Identifier: LGPL-3.0-only
// Minimal fragmented-MP4 (CMAF style) muxer for Media Source Extensions.
// One video or one audio (AAC, Opus, FLAC, AC-3, E-AC-3) track per instance, big-endian throughout.
import { mseMime } from '../platform.js';
import { eac3Blocks } from './dolby-audio.js';

const enc = new TextEncoder();
const u8 = (...a) => Uint8Array.from(a);
const u16 = (v) => u8(v >> 8, v);
const u32 = (v) => u8(v >>> 24, v >>> 16, v >>> 8, v);
const i32 = (v) => u32(v | 0);
const cc = (s) => enc.encode(s);

function cat(parts) {
  let n = 0;
  for (const p of parts) n += p.length;
  const out = new Uint8Array(n);
  let o = 0;
  for (const p of parts) { out.set(p, o); o += p.length; }
  return out;
}
const box = (type, ...kids) => {
  const body = cat(kids);
  return cat([u32(body.length + 8), cc(type), body]);
};
const fullBox = (type, ver, flags, ...kids) =>
  box(type, u8(ver, flags >> 16, flags >> 8, flags), ...kids);
const zeros = (n) => new Uint8Array(n);
const MATRIX = cat([0x10000, 0, 0, 0, 0x10000, 0, 0, 0, 0x40000000].map(u32));

const AUDIO = { mp4a: 'mp4a', opus: 'Opus', flac: 'fLaC', 'ec-3': 'ec-3', 'ac-3': 'ac-3' };
const desc = (tag, ...kids) => {
  const b = cat(kids);
  return cat([u8(tag, 0x80, 0x80, 0x80, b.length), b]);
};

// esds for AAC: ES_Descriptor > DecoderConfig (0x40, audio) > AudioSpecificConfig, SLConfig predefined 2.
const buildEsds = (asc) => fullBox('esds', 0, 0, desc(0x03, u16(0), u8(0),
  desc(0x04, u8(0x40, 0x15), zeros(3), u32(0), u32(0), desc(0x05, asc)), desc(0x06, u8(2))));

// OpusHead (little-endian, magic) to dOps (big-endian, no magic, version 0).
function buildDOps(h) {
  const dv = new DataView(h.buffer, h.byteOffset, h.byteLength);
  const fam = h[18];
  return box('dOps', u8(0, h[9]), u16(dv.getUint16(10, true)), u32(dv.getUint32(12, true)),
    u16(dv.getUint16(16, true)), u8(fam), fam ? h.subarray(19, 21 + h[9]) : zeros(0));
}

const ENTRY = { hvc1: 'hvcC', hev1: 'hvcC', dvh1: 'hvcC', dvhe: 'hvcC', avc1: 'avcC', avc3: 'avcC', av01: 'av1C', vp09: 'vpcC', vp08: 'vpcC' };

// VP Codec ISO binding, version 1. Chroma 1 = 4:2:0 colocated with luma.
// The codec string may carry chroma/colour fields (vp09.PP.LL.DD.CC.cp.tc.mc.FF);
// an explicit `color` object wins over the string.
export function buildVpcC(codec, color) {
  const f = codec.split('.');
  const num = (i, d) => (f[i] !== undefined && f[i] !== '' ? parseInt(f[i], 10) : d);
  const isVp8 = f[0] === 'vp8' || f[0] === 'vp08';
  const profile = isVp8 ? 0 : num(1, 0);
  const level = isVp8 ? 0 : num(2, 10);
  const depth = isVp8 ? 8 : num(3, 8);
  const chroma = num(4, 1);
  const cp = color?.primaries ?? num(5, 1);
  const tc = color?.transfer ?? num(6, 1);
  const mc = color?.matrix ?? num(7, 1);
  const full = color ? (color.fullRange ? 1 : 0) : num(8, 0);
  return fullBox('vpcC', 1, 0,
    u8(profile, level, (depth << 4) | (chroma << 1) | full, cp, tc, mc), u16(0));
}

// Dolby Vision configuration record. Profiles above 7 go in dvvC, the others in dvcC (Dolby's ISO mapping, section 2.2).
function buildDvConfig(dv) {
  const compat = dv.blCompat ?? (dv.profile === 8 ? 1 : 0);
  const body = zeros(24);
  body.set([1, 0, (dv.profile << 1) | (dv.level >> 5), ((dv.level & 31) << 3) | ((dv.rpuPresent ? 1 : 0) << 2) | ((dv.elPresent ? 1 : 0) << 1) | (dv.blPresent ? 1 : 0), compat << 4]);
  return box(dv.profile > 7 ? 'dvvC' : 'dvcC', body);
}

export class Fmp4Muxer {
  constructor({ codec, description = null, width, height, color = null, mastering = null, contentLight = null, timescale = null,
    channels = 2, sampleRate = 48000, trackId = 1, dolbyVision = null }) {
    const prefix = codec.split('.')[0];
    this.trackId = trackId;
    this.audio = prefix in AUDIO;
    if (this.audio) {
      if (!description) throw new Error(`fmp4: ${codec} needs a codec description`);
      this.fourcc = AUDIO[prefix];
      this.codec = codec;
      this.description = description;
      this.channels = prefix === 'opus' ? description[9] : channels;
      this.sampleRate = sampleRate;
      this.timescale = timescale ?? sampleRate;
      this.seq = 0;
      this.offsetUs = null;
      this.ec3Held = null;   // a short E-AC-3 set still waiting for its six blocks (see _ec3Units)
      return;
    }
    this.fourcc = prefix === 'vp8' ? 'vp08' : prefix;
    if (!ENTRY[this.fourcc]) throw new Error(`fmp4: unsupported codec ${codec}`);
    if (!description && this.fourcc !== 'vp09' && this.fourcc !== 'vp08') {
      throw new Error(`fmp4: ${codec} needs a codec description`);
    }
    this.codec = codec;
    this.description = description;
    this.width = width;
    this.height = height;
    this.color = color;
    this.mastering = mastering;       // G, B, R, white point (x, y; 0.00002 units), max and min luminance (0.0001 cd/m2)
    this.contentLight = contentLight; // [MaxCLL, MaxFALL]
    this.dolbyVision = dolbyVision;
    this.timescale = timescale ?? 90000;
    this.seq = 0;
    // Constant shift added to every timestamp (decided at the first fragment so
    // the smallest dts is >= 0). Element time = source time + offsetUs, i.e.
    // video.currentTime * 1e6 = ptsUs + offsetUs. Set timestampOffset or
    // subtract it from seek targets accordingly.
    this.offsetUs = null;
  }

  get mimeType() { return mseMime(this.audio ? 'audio' : 'video', this.codec); }

  _audioEntry() {
    const cfg = this.fourcc === 'mp4a' ? buildEsds(this.description)
      : this.fourcc === 'Opus' ? buildDOps(this.description)
        : this.fourcc === 'ec-3' ? box('dec3', this.description) : this.fourcc === 'ac-3' ? box('dac3', this.description)
          : fullBox('dfLa', 0, 0, u8(0x80, 0, 0, this.description.length), this.description);
    // Rates above 65535 do not fit the 16.16 field; like ffmpeg (and the FLAC
    // mapping spec) halve until they fit, the real rate comes from dfLa.
    let rate = this.sampleRate;
    while (rate > 0xffff) rate >>= 1;
    const bits = this.fourcc === 'fLaC' ? (((this.description[12] & 1) << 4) | (this.description[13] >> 4)) + 1 : 16;
    return box(this.fourcc, zeros(6), u16(1), zeros(8), u16(this.channels), u16(bits), u16(0), u16(0),
      u16(rate), u16(0), cfg);
  }

  _sampleEntry() {
    if (this.audio) return this._audioEntry();
    const cfg = ENTRY[this.fourcc] === 'vpcC'
      ? buildVpcC(this.codec, this.color)
      : box(ENTRY[this.fourcc], this.description);
    const kids = [cfg];
    if (this.dolbyVision) kids.push(buildDvConfig(this.dolbyVision));
    if (this.color) {
      const c = this.color;
      kids.push(box('colr', cc('nclx'), u16(c.primaries), u16(c.transfer), u16(c.matrix),
        u8(c.fullRange ? 0x80 : 0)));
    }
    if (this.mastering) kids.push(box('mdcv', ...this.mastering.slice(0, 8).map(u16), u32(this.mastering[8]), u32(this.mastering[9])));
    if (this.contentLight) kids.push(box('clli', u16(this.contentLight[0]), u16(this.contentLight[1])));
    kids.push(box('pasp', u32(1), u32(1)));
    return box(this.fourcc,
      zeros(6), u16(1), zeros(16), u16(this.width), u16(this.height),
      u32(0x480000), u32(0x480000), u32(0), u16(1), zeros(32), u16(0x18), u16(0xffff),
      ...kids);
  }

  init() {
    const ftyp = box('ftyp', cc('iso6'), u32(0), cc('iso6'), cc('cmfc'), cc('mp41'), ...(this.dolbyVision ? [cc('dby1')] : []));
    const mvhd = fullBox('mvhd', 0, 0,
      u32(0), u32(0), u32(this.timescale), u32(0), u32(0x10000), u16(0x100), zeros(10),
      MATRIX, zeros(24), u32(this.trackId + 1));
    const tkhd = fullBox('tkhd', 0, 3,
      u32(0), u32(0), u32(this.trackId), u32(0), u32(0), zeros(8), u16(0), u16(0), u16(this.audio ? 0x100 : 0), u16(0),
      MATRIX, u32(this.audio ? 0 : this.width * 65536), u32(this.audio ? 0 : this.height * 65536));
    const mdhd = fullBox('mdhd', 0, 0,
      u32(0), u32(0), u32(this.timescale), u32(0), u16(0x55c4), u16(0));
    const hdlr = this.audio
      ? fullBox('hdlr', 0, 0, u32(0), cc('soun'), zeros(12), cc('SoundHandler\0'))
      : fullBox('hdlr', 0, 0, u32(0), cc('vide'), zeros(12), cc('VideoHandler\0'));
    const vmhd = this.audio ? fullBox('smhd', 0, 0, u32(0)) : fullBox('vmhd', 0, 1, zeros(8));
    const dinf = box('dinf', fullBox('dref', 0, 0, u32(1), fullBox('url ', 0, 1)));
    const stbl = box('stbl',
      fullBox('stsd', 0, 0, u32(1), this._sampleEntry()),
      fullBox('stts', 0, 0, u32(0)), fullBox('stsc', 0, 0, u32(0)),
      fullBox('stsz', 0, 0, u32(0), u32(0)), fullBox('stco', 0, 0, u32(0)));
    const trak = box('trak', tkhd,
      box('mdia', mdhd, hdlr, box('minf', vmhd, dinf, stbl)));
    const trex = fullBox('trex', 0, 0, u32(this.trackId), u32(1), u32(0), u32(0), u32(0));
    return cat([ftyp, box('moov', mvhd, trak, box('mvex', trex))]);
  }

  _ts(us) { return Math.round((us * this.timescale) / 1e6); }

  // Audio: every sample is a sync sample and dts equals pts.
  _dts(s) { return this.audio ? s.ptsUs : s.dtsUs; }

  // Apple's decoder assumes a sample holds one 1536 sample access unit. Fed 256 sample syncframes (a raw .eac3 file, ffmpeg-muxed Matroska) it
  // lost 8 ms per second and scrambled the channels against each other (correlation with ffmpeg's decode 0.2 to 0.9; 0.96 or better once merged),
  // so short syncframes are merged into sets of six blocks. Packets of six blocks or more pass through untouched.
  _ec3Units(samples) {
    const out = [];
    for (const s of samples) {
      const blocks = eac3Blocks(s.data);
      if (!this.ec3Held && (blocks >= 6 || blocks === 0)) { out.push(s); continue; }
      let h = this.ec3Held;
      if (h && Math.abs(s.ptsUs - h.endUs) > 20_000) h = null;   // a seek: the blocks before it are not the start of this set
      h ??= { ptsUs: s.ptsUs, endUs: s.ptsUs, blocks: 0, parts: [] };
      h.parts.push(s.data); h.blocks += blocks; h.endUs = s.ptsUs + s.durationUs;
      if (h.blocks >= 6) { out.push({ data: cat(h.parts), ptsUs: h.ptsUs, dtsUs: h.ptsUs, durationUs: h.endUs - h.ptsUs, key: true }); h = null; }
      this.ec3Held = h;
    }
    return out;
  }

  fragment(samples) {
    if (this.fourcc === 'ec-3') samples = this._ec3Units(samples);
    if (!samples.length) return new Uint8Array(0);
    if (this.offsetUs === null) {
      let min = Infinity;
      for (const s of samples) min = Math.min(min, this._dts(s));
      this.offsetUs = min < 0 ? -min : 0;
    }
    const off = this.offsetUs;
    // Each decode time is rounded from its own dts, so durations (differences
    // of rounded values) sum exactly to the next fragment's base time.
    const dts = samples.map((s) => this._ts(this._dts(s) + off));
    const n = samples.length;
    const rows = [];
    let mdatSize = 0;
    for (let i = 0; i < n; i++) {
      const s = samples[i];
      const end = i + 1 < n ? dts[i + 1] : this._ts(this._dts(s) + off + s.durationUs);
      const cts = this._ts(s.ptsUs + off) - dts[i];
      rows.push(cat([u32(Math.max(0, end - dts[i])), u32(s.data.length),
        u32(s.key || this.audio ? 0x02000000 : 0x01010000), i32(cts)]));
      mdatSize += s.data.length;
    }
    const base = Math.max(0, dts[0]);
    const seq = ++this.seq;
    const build = (dataOffset) => box('moof',
      fullBox('mfhd', 0, 0, u32(seq)),
      box('traf',
        fullBox('tfhd', 0, 0x020000, u32(this.trackId)),
        fullBox('tfdt', 1, 0, u32(Math.floor(base / 2 ** 32)), u32(base >>> 0)),
        fullBox('trun', 1, 0xf01, u32(n), i32(dataOffset), ...rows)));
    const moof = build(build(0).length + 8);
    return cat([moof, u32(mdatSize + 8), cc('mdat'), ...samples.map((s) => s.data)]);
  }
}
