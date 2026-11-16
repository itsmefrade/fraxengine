// SPDX-License-Identifier: LGPL-3.0-only
// INTERIM demuxer: minimal ISO-BMFF (MP4 and fragmented MP4) for H.264/HEVC video and AAC/Opus/FLAC audio.
// Kept only as the ?demux=js fallback; the Rust/WASM demuxer (web/engine/demuxer.mjs) is the default. It implements
// the Demuxer contract (see demux/index.js) and nothing else is imported from it.
// The whole sample index is built up front (moov, or every moof for fragmented files), which is fine for
// test files and local media.

const td = new TextDecoder('latin1');
const fourcc = (u8, o) => td.decode(u8.subarray(o, o + 4));
const u32 = (u8, o) => ((u8[o] << 24) | (u8[o + 1] << 16) | (u8[o + 2] << 8) | u8[o + 3]) >>> 0;
const i32 = (u8, o) => (u32(u8, o) | 0);
const u16 = (u8, o) => (u8[o] << 8) | u8[o + 1];
const u64 = (u8, o) => u32(u8, o) * 4294967296 + u32(u8, o + 4);

function* children(u8, start, end) {
  let p = start;
  while (p + 8 <= end) {
    let size = u32(u8, p);
    const type = fourcc(u8, p + 4);
    let hdr = 8;
    if (size === 1) { size = u64(u8, p + 8); hdr = 16; } else if (size === 0) size = end - p;
    if (size < hdr || p + size > end) return;
    yield { type, start: p + hdr, end: p + size, box: p };
    p += size;
  }
}

const AAC_RATES = [96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350];

function parseEsds(u8, s, e) {
  // fullbox header, then ES_Descriptor (0x03) > DecoderConfigDescriptor (0x04) > DecoderSpecificInfo (0x05)
  let p = s + 4;
  const readLen = () => { let l = 0; for (let i = 0; i < 4; i++) { const b = u8[p++]; l = (l << 7) | (b & 0x7f); if (!(b & 0x80)) break; } return l; };
  let objectType = 0, asc = null;
  while (p < e) {
    const tag = u8[p++]; const len = readLen();
    if (tag === 0x03) { p += 2; const f = u8[p++]; if (f & 0x80) p += 2; if (f & 0x40) p += 1 + u8[p]; if (f & 0x20) p += 2; }
    else if (tag === 0x04) { objectType = u8[p]; p += 13; }
    else if (tag === 0x05) { asc = u8.slice(p, p + len); break; }
    else p += len;
  }
  return { objectType, asc };
}

function parseAsc(asc) {
  let bit = 0;
  const read = n => { let v = 0; for (let i = 0; i < n; i++, bit++) v = (v << 1) | ((asc[bit >> 3] >> (7 - (bit & 7))) & 1); return v; };
  let aot = read(5); if (aot === 31) aot = 32 + read(6);
  const idx = read(4); const sampleRate = idx === 15 ? read(24) : AAC_RATES[idx];
  const channels = read(4);
  return { aot, sampleRate, channels };
}

function reverse32(v) { let r = 0; for (let i = 0; i < 32; i++) { r = (r << 1) | (v & 1); v >>>= 1; } return r >>> 0; }

function hevcCodecString(h) {
  const space = ['', 'A', 'B', 'C'][h[1] >> 6];
  const tier = (h[1] & 0x20) ? 'H' : 'L';
  const compat = reverse32(u32(h, 2)).toString(16);
  const cons = Array.from(h.subarray(6, 12));
  while (cons.length && cons[cons.length - 1] === 0) cons.pop();
  return `hvc1.${space}${h[1] & 0x1f}.${compat}.${tier}${h[12]}${cons.map(b => '.' + b.toString(16).toUpperCase()).join('')}`;
}

// hev1 files may carry an hvcC with zero parameter-set arrays (sets are in-band). WebCodecs needs them in the
// description, so lift VPS/SPS/PPS out of the first sync sample.
function completeHvcc(h, sample) {
  if (h[22] !== 0) return h;
  const lenSize = (h[21] & 3) + 1;
  const found = new Map();
  for (let p = 0; p + lenSize <= sample.length;) {
    let n = 0; for (let i = 0; i < lenSize; i++) n = n * 256 + sample[p + i];
    p += lenSize;
    const type = (sample[p] >> 1) & 0x3f;
    if ((type === 32 || type === 33 || type === 34) && !found.has(type)) found.set(type, sample.slice(p, p + n));
    p += n;
  }
  if (!found.size) return h;
  const parts = [h.slice(0, 22), Uint8Array.of(found.size)];
  for (const [type, nal] of [...found].sort((a, b) => a[0] - b[0])) parts.push(Uint8Array.of(0x80 | type, 0, 1, nal.length >> 8, nal.length & 255), nal);
  const out = new Uint8Array(parts.reduce((a, b) => a + b.length, 0));
  let o = 0; for (const p of parts) { out.set(p, o); o += p.length; }
  return out;
}

function opusHead(d) {
  // dOps (big endian) -> OpusHead identification header (little endian), the WebCodecs description.
  const ch = d[1], pre = u16(d, 2), rate = u32(d, 4), gain = u16(d, 8), map = d[10];
  const extra = map !== 0 ? d.subarray(11, 11 + 2 + ch) : new Uint8Array(0);
  const out = new Uint8Array(19 + extra.length);
  out.set([0x4f, 0x70, 0x75, 0x73, 0x48, 0x65, 0x61, 0x64, 1, ch, pre & 255, pre >> 8, rate & 255, (rate >> 8) & 255, (rate >> 16) & 255, rate >>> 24, gain & 255, gain >> 8, map]);
  out.set(extra, 19);
  return out;
}

export class Mp4Demuxer {
  constructor(source) {
    this.src = source;
    this._tracks = [];       // internal per-track state
    this.trex = new Map();
    this.durationUs = 0;
    this._parse();
    for (const t of this._tracks) this._completeHevc(t);
    for (const t of this._tracks) { const n = t.s.ptsUs.length; if (n) this.durationUs = Math.max(this.durationUs, t.s.ptsUs[n - 1] + t.s.durUs[n - 1]); }
    this.seek(0);
  }

  tracks() { return this._tracks.map(t => t.track); }

  seek(timeUs) {
    for (const t of this._tracks) {
      const n = t.s.ptsUs.length;
      let idx = 0;
      if (t.track.kind === 'video') {
        // last sync sample whose pts <= target (Matroska cues lie; MP4 stss does not, but verify via the flag array)
        for (let i = 0; i < n; i++) { if (t.s.key[i] && t.s.ptsUs[i] <= timeUs) idx = i; if (t.s.dtsUs[i] > timeUs + 5e6) break; }
      } else {
        for (let i = 0; i < n; i++) { if (t.s.ptsUs[i] <= timeUs) idx = i; else break; }
      }
      t.next = idx;
    }
  }

  nextPacket() {
    let best = null;
    for (const t of this._tracks) {
      if (t.next >= t.s.ptsUs.length) continue;
      if (!best || t.s.dtsUs[t.next] < best.s.dtsUs[best.next]) best = t;
    }
    if (!best) return null;
    const i = best.next++;
    const s = best.s;
    return {
      trackId: best.track.id, ptsUs: s.ptsUs[i], dtsUs: s.dtsUs[i], durationUs: s.durUs[i], key: !!s.key[i],
      data: this.src.read(s.off[i], s.size[i]).slice(),
    };
  }

  close() { this.src.close?.(); }

  // ---- parsing ----
  _parse() {
    const src = this.src;
    let pos = 0, sawMoov = false;
    while (pos + 8 <= src.size) {
      const h = src.read(pos, 16);
      let size = u32(h, 0);
      const type = fourcc(h, 4);
      if (size === 1) size = u64(h, 8); else if (size === 0) size = src.size - pos;
      if (size < 8) break;
      if (type === 'moov') { this._parseMoov(src.read(pos, size).slice(), 0); for (const t of this._tracks) this._finishTrack(t); sawMoov = true; }
      else if (type === 'moof') {
        if (!sawMoov) throw new Error('moof before moov');
        this._parseMoof(src.read(pos, size).slice(), pos);
      }
      pos += size;
    }
    if (!sawMoov) throw new Error('mp4: no moov box');
  }

  _parseMoov(u8) {
    const top = [...children(u8, 0, u8.length)][0];
    let movieTs = 1000;
    for (const b of children(u8, top.start, top.end)) {
      if (b.type === 'mvhd') { const v = u8[b.start]; movieTs = u32(u8, b.start + (v ? 20 : 12)); this.movieTs = movieTs; }
    }
    for (const b of children(u8, top.start, top.end)) {
      if (b.type === 'trak') this._parseTrak(u8, b, movieTs);
      else if (b.type === 'mvex') {
        for (const c of children(u8, b.start, b.end)) if (c.type === 'trex') {
          this.trex.set(u32(u8, c.start + 4), { dur: u32(u8, c.start + 12), size: u32(u8, c.start + 16), flags: u32(u8, c.start + 20) });
        }
      }
    }
  }

  _parseTrak(u8, trak, movieTs) {
    const t = { s: { off: [], size: [], dtsUs: [], ptsUs: [], durUs: [], key: [] }, next: 0, edits: [], frag: false, dtsCursor: 0 };
    const stbl = {};
    for (const b of children(u8, trak.start, trak.end)) {
      if (b.type === 'tkhd') { const v = u8[b.start]; t.id = u32(u8, b.start + (v ? 20 : 12)); }
      else if (b.type === 'edts') {
        for (const e of children(u8, b.start, b.end)) if (e.type === 'elst') {
          const v = u8[e.start], n = u32(u8, e.start + 4);
          let p = e.start + 8;
          for (let i = 0; i < n; i++) {
            if (v) { t.edits.push({ dur: u64(u8, p), mediaTime: i32(u8, p + 12) === -1 && u32(u8, p + 8) === 0xffffffff ? -1 : u64(u8, p + 8) }); p += 20; }
            else { t.edits.push({ dur: u32(u8, p), mediaTime: i32(u8, p + 4) }); p += 12; }
          }
        }
      } else if (b.type === 'mdia') {
        for (const m of children(u8, b.start, b.end)) {
          if (m.type === 'mdhd') { const v = u8[m.start]; t.timescale = u32(u8, m.start + (v ? 20 : 12)); }
          else if (m.type === 'hdlr') t.handler = fourcc(u8, m.start + 8);
          else if (m.type === 'minf') for (const n of children(u8, m.start, m.end)) if (n.type === 'stbl') for (const c of children(u8, n.start, n.end)) stbl[c.type] = c;
        }
      }
    }
    t.movieTs = movieTs;
    if (t.handler !== 'vide' && t.handler !== 'soun') return;
    if (!stbl.stsd) return;
    this._parseStsd(u8, stbl.stsd, t);
    if (!t.track) return;
    t.stbl = { u8, boxes: stbl };
    this._tracks.push(t);
  }

  _parseStsd(u8, stsd, t) {
    const e = [...children(u8, stsd.start + 8, stsd.end)][0];
    if (!e) return;
    const fmt = e.type;
    const track = { id: t.id, kind: t.handler === 'vide' ? 'video' : 'audio', timescale: t.timescale };
    if (t.handler === 'vide') {
      track.width = u16(u8, e.start + 24); track.height = u16(u8, e.start + 26);
      let cstart = e.start + 78;
      for (const c of children(u8, cstart, e.end)) {
        if (c.type === 'avcC' && (fmt === 'avc1' || fmt === 'avc3')) {
          const d = u8.slice(c.start, c.end);
          track.codec = 'avc1.' + [d[1], d[2], d[3]].map(x => x.toString(16).padStart(2, '0')).join('').toUpperCase();
          track.description = d;
        } else if (c.type === 'hvcC' && (fmt === 'hvc1' || fmt === 'hev1')) {
          t.hvcc = u8.slice(c.start, c.end);
          track.codec = hevcCodecString(t.hvcc);
          track.description = t.hvcc;
        } else if (c.type === 'colr' && fourcc(u8, c.start) === 'nclx') {
          track.color = { primaries: u16(u8, c.start + 4), transfer: u16(u8, c.start + 6), matrix: u16(u8, c.start + 8), fullRange: !!(u8[c.start + 10] & 0x80) };
        }
      }
      if (!track.codec) return;   // unsupported by this interim demuxer (av01, vp09, ...)
    } else {
      const ver = u16(u8, e.start + 8);
      track.channels = u16(u8, e.start + 16);
      track.sampleRate = u16(u8, e.start + 24);
      let cstart = e.start + 28 + (ver === 1 ? 16 : ver === 2 ? 36 : 0);
      for (const c of children(u8, cstart, e.end)) {
        if (c.type === 'esds' && fmt === 'mp4a') {
          const { objectType, asc } = parseEsds(u8, c.start, c.end);
          if (objectType === 0x40 && asc) {
            const a = parseAsc(asc);
            track.codec = 'mp4a.40.' + a.aot; track.description = asc;
            track.sampleRate = a.sampleRate || track.sampleRate; track.channels = a.channels || track.channels;
          }
        } else if (c.type === 'dOps' && fmt === 'Opus') {
          track.codec = 'opus'; track.description = opusHead(u8.subarray(c.start, c.end));
          track.sampleRate = 48000;
        } else if (c.type === 'dfLa' && fmt === 'fLaC') {
          const blocks = u8.subarray(c.start + 4, c.end);
          const d = new Uint8Array(4 + blocks.length); d.set([0x66, 0x4c, 0x61, 0x43]); d.set(blocks, 4);
          track.codec = 'flac'; track.description = d;
          // STREAMINFO: 4 byte block header, then 10 bits ... sample rate is 20 bits at byte offset 10
          const si = blocks.subarray(4);
          track.sampleRate = (si[10] << 12) | (si[11] << 4) | (si[12] >> 4);
          track.channels = ((si[12] >> 1) & 7) + 1;
        }
      }
      if (!track.codec) return;
    }
    t.track = track;
  }

  // Build the sample arrays from the sample tables (non-fragmented part of the file).
  _finishTrack(t) {
    if (!t.track) return;
    const ts = t.timescale;
    // edit list -> timestamp shift
    let shiftTicks = 0, delayUs = 0, seenMedia = false;
    for (const ed of t.edits) {
      if (ed.mediaTime === -1) { if (!seenMedia) delayUs += (ed.dur * 1e6) / t.movieTs; }
      else if (!seenMedia) { shiftTicks = -ed.mediaTime; seenMedia = true; }
    }
    t.shiftTicks = shiftTicks; t.delayUs = delayUs;
    const us = ticks => Math.round(((ticks + shiftTicks) * 1e6) / ts + delayUs);
    t.us = us;
    const { u8, boxes } = t.stbl;
    const s = t.s;
    if (boxes.stsz) {
      const b = boxes.stsz;
      const fixed = u32(u8, b.start + 4), n = u32(u8, b.start + 8);
      const sizes = new Array(n);
      for (let i = 0; i < n; i++) sizes[i] = fixed || u32(u8, b.start + 12 + 4 * i);
      // chunk offsets
      const co = boxes.co64 || boxes.stco;
      const nChunks = co ? u32(u8, co.start + 4) : 0;
      const chunkOff = new Array(nChunks);
      for (let i = 0; i < nChunks; i++) chunkOff[i] = boxes.co64 ? u64(u8, co.start + 8 + 8 * i) : u32(u8, co.start + 8 + 4 * i);
      // stsc -> sample offsets
      const sc = boxes.stsc; const nRuns = u32(u8, sc.start + 4);
      const runs = [];
      for (let i = 0; i < nRuns; i++) runs.push({ first: u32(u8, sc.start + 8 + 12 * i), per: u32(u8, sc.start + 12 + 12 * i) });
      const off = new Array(n); let si = 0;
      for (let c = 0; c < nChunks && si < n; c++) {
        let per = 0;
        for (let r = 0; r < nRuns; r++) if (runs[r].first <= c + 1) per = runs[r].per; else break;
        let o = chunkOff[c];
        for (let k = 0; k < per && si < n; k++, si++) { off[si] = o; o += sizes[si]; }
      }
      // stts / ctts
      const dts = new Array(n), dur = new Array(n);
      let i = 0, tick = 0;
      const st = boxes.stts, nst = u32(u8, st.start + 4);
      for (let r = 0; r < nst; r++) { const cnt = u32(u8, st.start + 8 + 8 * r), d = u32(u8, st.start + 12 + 8 * r); for (let k = 0; k < cnt && i < n; k++, i++) { dts[i] = tick; dur[i] = d; tick += d; } }
      const cto = new Array(n).fill(0);
      if (boxes.ctts) {
        const cb = boxes.ctts, nc = u32(u8, cb.start + 4); i = 0;
        for (let r = 0; r < nc; r++) { const cnt = u32(u8, cb.start + 8 + 8 * r), d = i32(u8, cb.start + 12 + 8 * r); for (let k = 0; k < cnt && i < n; k++, i++) cto[i] = d; }
      }
      const key = new Uint8Array(n);
      if (boxes.stss) { const ss = boxes.stss, nk = u32(u8, ss.start + 4); for (let k = 0; k < nk; k++) key[u32(u8, ss.start + 8 + 4 * k) - 1] = 1; } else key.fill(1);
      for (let k = 0; k < n; k++) {
        s.off.push(off[k]); s.size.push(sizes[k]); s.dtsUs.push(us(dts[k])); s.ptsUs.push(us(dts[k] + cto[k]));
        s.durUs.push(Math.round((dur[k] * 1e6) / ts)); s.key.push(key[k]);
      }
      t.dtsCursor = tick;
    }
    delete t.stbl;
  }

  _completeHevc(t) {
    if (!(t.hvcc && t.hvcc[22] === 0)) return;
    const s = t.s, ki = s.key.findIndex(k => k === 1);
    if (ki >= 0) t.track.description = completeHvcc(t.hvcc, this.src.read(s.off[ki], s.size[ki]).slice());
  }

  _parseMoof(u8, moofPos) {
    const top = [...children(u8, 0, u8.length)][0];
    for (const traf of children(u8, top.start, top.end)) {
      if (traf.type !== 'traf') continue;
      let tfhd = null, tfdt = null; const truns = [];
      for (const c of children(u8, traf.start, traf.end)) {
        if (c.type === 'tfhd') tfhd = c; else if (c.type === 'tfdt') tfdt = c; else if (c.type === 'trun') truns.push(c);
      }
      if (!tfhd) continue;
      const id = u32(u8, tfhd.start + 4);
      const t = this._tracks.find(x => x.id === id);
      if (!t) continue;
      const flags = u32(u8, tfhd.start) & 0xffffff;
      let p = tfhd.start + 8;
      let base = moofPos;
      if (flags & 1) { base = u64(u8, p); p += 8; }
      if (flags & 2) p += 4;
      const trex = this.trex.get(id) || { dur: 0, size: 0, flags: 0 };
      let defDur = trex.dur, defSize = trex.size, defFlags = trex.flags;
      if (flags & 8) { defDur = u32(u8, p); p += 4; }
      if (flags & 0x10) { defSize = u32(u8, p); p += 4; }
      if (flags & 0x20) { defFlags = u32(u8, p); p += 4; }
      if (tfdt) { const v = u8[tfdt.start]; t.dtsCursor = v ? u64(u8, tfdt.start + 4) : u32(u8, tfdt.start + 4); }
      let dataPos = base;
      const s = t.s;
      for (const tr of truns) {
        const tf = u32(u8, tr.start) & 0xffffff, ver = u8[tr.start];
        const n = u32(u8, tr.start + 4); let q = tr.start + 8;
        if (tf & 1) { dataPos = base + i32(u8, q); q += 4; }
        let firstFlags = null;
        if (tf & 4) { firstFlags = u32(u8, q); q += 4; }
        for (let k = 0; k < n; k++) {
          let d = defDur, sz = defSize, fl = defFlags, co = 0;
          if (tf & 0x100) { d = u32(u8, q); q += 4; }
          if (tf & 0x200) { sz = u32(u8, q); q += 4; }
          if (tf & 0x400) { fl = u32(u8, q); q += 4; } else if (k === 0 && firstFlags !== null) fl = firstFlags;
          if (tf & 0x800) { co = ver ? i32(u8, q) : u32(u8, q) | 0; q += 4; }
          s.off.push(dataPos); s.size.push(sz); dataPos += sz;
          s.dtsUs.push(t.us(t.dtsCursor)); s.ptsUs.push(t.us(t.dtsCursor + co));
          s.durUs.push(Math.round((d * 1e6) / t.timescale));
          s.key.push((fl & 0x10000) ? 0 : 1);
          t.dtsCursor += d;
        }
      }
    }
  }
}
