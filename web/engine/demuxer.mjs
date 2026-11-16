// SPDX-License-Identifier: LGPL-3.0-only
// Demuxer over the Rust/WASM engine: Matroska, MP4, MPEG-TS / M2TS and raw audio files (AC-3, E-AC-3, FLAC, DTS,
// TrueHD / MLP, WAV, ADTS) packets for WebCodecs or the engine audio decoders, plus the Dolby Vision helpers. The file is indexed lazily: `openDemuxer` reads
// headers only and `nextPacket` indexes further as it goes.
//
//   const d = await openDemuxer(fileOrBlobOrUrlObject);
//   d.tracks(); d.seek(timeUs); d.nextPacket();
//
// `openDemuxer` is async (module compile, indexing) but `nextPacket` and `seek` are synchronous and
// block on source reads, so call them from a Worker. See README.md.

import { AetherWasm, loadModule, moduleSync, SDR_CARRIER, P5_CARRIER } from './aether-wasm.mjs';
import { createSource, sourceTrace } from './sources.mjs';

const RECORD_BYTES = 48;
const DEMUX_ERRORS = { '-2': 'indexing or seek failed', '-3': 'host read failed' };
const DTS_MIN_AHEAD = 16;     // fewest look-ahead packets when the frames are huge (measured: 4 are not enough for the 4K DV file, 16 are)
const DTS_AHEAD_BYTES = 1 << 20; // ...stop looking ahead beyond this much video data: over HTTP every byte walked is a fetched block
const DTS_LOOKAHEAD = 32; // Matroska B-frame reorder window, in packets of one video track
const DTS_PROBE = 64; // packets inspected to size the constant dts offset
// The look-ahead above is a guess for streams that do not say how far they reorder. HEVC says it in the SPS: with a depth of D
// the smallest timestamp still to come is among the next D + 1 packets, and D frame durations is the largest a picture can be
// ahead of its place in display order. Reading 17 packets (4.8 MB of a 39 Mbit/s 4K file) before the first packet cost about
// 330 ms of an 850 ms start on an 85 Mbit/s link, and again after every seek. Beyond this offset the measured one is used.
const REORDER_SHIFT_MAX_US = 500_000;
const REORDER_TICK_US = 1000;   // Matroska timestamps are rounded to the timestamp scale (1 ms as a rule), the frame duration is not: 4 frames of 41.667 ms read 167 ms, 2 read 83

// ---------- codec strings ----------

const hex2 = n => n.toString(16).padStart(2, '0');

function reverseBits32(v) {
  let r = 0;
  for (let i = 0; i < 32; i++) { r = (r << 1) | (v & 1); v >>>= 1; }
  return r >>> 0;
}

/** WebCodecs string from an hvcC record (ISO 14496-15 annex E.3). */
export function hevcCodecString(hvcc, prefix = 'hvc1') {
  if (!hvcc || hvcc.length < 13) return prefix;
  const space = hvcc[1] >> 6, tier = (hvcc[1] >> 5) & 1, idc = hvcc[1] & 31;
  const compat = ((hvcc[2] << 24) | (hvcc[3] << 16) | (hvcc[4] << 8) | hvcc[5]) >>> 0;
  const constraints = Array.from(hvcc.subarray(6, 12));
  while (constraints.length > 1 && constraints[constraints.length - 1] === 0) constraints.pop();
  const parts = [`${prefix}.${space ? 'ABC'[space - 1] : ''}${idc}`, reverseBits32(compat).toString(16).toUpperCase(),
    `${tier ? 'H' : 'L'}${hvcc[12]}`, ...constraints.map(b => hex2(b).toUpperCase())];
  return parts.join('.');
}

export function avcCodecString(avcc) {
  if (!avcc || avcc.length < 4) return 'avc1';
  return `avc1.${hex2(avcc[1])}${hex2(avcc[2])}${hex2(avcc[3])}`;
}

export function av1CodecString(av1c) {
  if (!av1c || av1c.length < 4) return 'av01.0.08M.08';
  const profile = av1c[1] >> 5, level = av1c[1] & 31;
  const tier = (av1c[2] >> 7) & 1, high = (av1c[2] >> 6) & 1, twelve = (av1c[2] >> 5) & 1;
  const depth = high ? (twelve ? 12 : 10) : 8;
  return `av01.${profile}.${String(level).padStart(2, '0')}${tier ? 'H' : 'M'}.${String(depth).padStart(2, '0')}`;
}

export function vp9CodecString(priv, bitDepth) {
  let profile = 0, level = 10, depth = bitDepth || 8;
  // Matroska VP9 CodecPrivate: (id, len, value) triples
  for (let i = 0; priv && i + 2 < priv.length; i += 3 + 0) {
    const id = priv[i], len = priv[i + 1];
    if (len !== 1) break;
    if (id === 1) profile = priv[i + 2];
    else if (id === 2) level = priv[i + 2];
    else if (id === 3) depth = priv[i + 2];
  }
  return `vp09.${String(profile).padStart(2, '0')}.${String(level).padStart(2, '0')}.${String(depth).padStart(2, '0')}`;
}

function aacObjectType(asc) {
  if (!asc || asc.length < 2) return 2;
  let aot = asc[0] >> 3;
  if (aot === 31) aot = 32 + (((asc[0] & 7) << 3) | (asc[1] >> 5));
  return aot;
}

/** Map a container codec id / sample entry / TS stream name to the engine's codec string. */
function mapCodec(t, priv) {
  const c = t.codec;
  const up = c.toUpperCase();
  if (t.kind === 'video') {
    if (up.includes('HEVC') || /^(hvc1|hev1|dvh1|dvhe)$/.test(c)) return hevcCodecString(priv, /^hev1|dvhe$/.test(c) ? 'hev1' : 'hvc1');
    if (up.includes('AVC') || /^(avc1|avc3|h264)$/.test(c)) return avcCodecString(priv);
    if (up.includes('AV1') || c === 'av01') return av1CodecString(priv);
    if (up.includes('VP9') || c === 'vp09') return vp9CodecString(priv, t.bitDepth);
    if (up.includes('VP8') || c === 'vp08') return 'vp8';
    if (up.includes('MPEG2') || c === 'mpeg2video') return 'mpeg2';
    if (up.includes('VC1') || c === 'vc1') return 'vc1';
    return c;
  }
  if (t.kind === 'audio') {
    if (up.startsWith('A_AAC') || c === 'mp4a' || c === 'aac') return `mp4a.40.${aacObjectType(priv)}`;
    if (up === 'A_OPUS' || c === 'Opus') return 'opus';
    if (up === 'A_FLAC' || c === 'fLaC' || c === 'flac') return 'flac';
    if (up === 'A_TRUEHD' || up === 'A_MLP' || c === 'mlpa' || c === 'truehd' || c === 'mlp') return 'truehd';
    if (up.startsWith('A_DTS') || /^dts[cehl]?$/.test(c)) return 'dts';
    if (up === 'A_EAC3' || c === 'ec-3' || c === 'eac3') return 'eac3';
    if (up === 'A_AC3' || c === 'ac-3' || c === 'ac3') return 'ac3';
    if (up.startsWith('A_PCM') || /^(sowt|twos|lpcm|ipcm|pcm_bluray)$/.test(c)) return 'pcm';
    if (up.startsWith('A_MPEG/L3')) return 'mp3';
    if (up === 'A_VORBIS') return 'vorbis';
    return c;
  }
  return subtitleCodec(c);
}

/** Subtitle renderer codec names (web/subs): pgs, ass, srt, webvtt; the rest keep a descriptive name. */
export function subtitleCodec(c) {
  const up = String(c).toUpperCase();
  if (up === 'S_HDMV/PGS' || up === 'HDMV_PGS_SUBTITLE') return 'pgs';
  if (up === 'S_TEXT/ASS' || up === 'S_TEXT/SSA') return 'ass';
  if (up === 'S_TEXT/UTF8' || up === 'TX3G') return 'srt';
  if (up === 'S_TEXT/WEBVTT' || up.startsWith('D_WEBVTT') || up === 'WVTT') return 'webvtt';
  if (up === 'S_VOBSUB') return 'vobsub';
  if (up === 'HDMV_TEXTST') return 'textst';
  if (up === 'DVB_SUBTITLE') return 'dvb';
  return c;
}

function fromHex(s) {
  const out = new Uint8Array(s.length / 2);
  for (let i = 0; i < out.length; i++) out[i] = parseInt(s.substr(i * 2, 2), 16);
  return out;
}

/**
 * WebCodecs / MSE Dolby Vision string `dvh1.PP.LL` (out-of-band parameter sets) or `dvhe.PP.LL` (in-band) from a
 * parsed dvcC / dvvC record. `base` is the base-layer family: 'hevc' (default), 'avc' (`dva1` / `dvav`) or 'av1' (`dav1`).
 */
export function dolbyVisionCodecString(dv, { inband = false, base = 'hevc' } = {}) {
  if (!dv) return null;
  const four = base === 'avc' ? (inband ? 'dvav' : 'dva1') : base === 'av1' ? 'dav1' : (inband ? 'dvhe' : 'dvh1');
  return `${four}.${String(dv.profile).padStart(2, '0')}.${String(dv.level).padStart(2, '0')}`;
}

/** True when an hvcC record carries an SPS array (out of band parameter sets, `hvc1`); Matroska files often carry none. */
// sps_max_num_reorder_pics of the highest sub-layer, from the first SPS of an hvcC: how many pictures can precede a picture in
// decode order and follow it in output order. null when there is no SPS or it does not parse.
export function hvccReorderDepth(hvcc) {
  if (!hvcc || hvcc.length < 23) return null;
  let at = 23, sps = null;
  for (let a = 0; a < hvcc[22] && !sps && at + 3 <= hvcc.length; a++) {
    const type = hvcc[at] & 63, count = (hvcc[at + 1] << 8) | hvcc[at + 2];
    at += 3;
    for (let k = 0; k < count && at + 2 <= hvcc.length; k++) {
      const len = (hvcc[at] << 8) | hvcc[at + 1];
      if (type === 33 && !sps) sps = hvcc.subarray(at + 2, at + 2 + len);
      at += 2 + len;
    }
  }
  if (!sps || sps.length < 16) return null;
  const r = [];
  for (let i = 2, z = 0; i < sps.length && r.length < 600; i++) {      // payload without the emulation prevention bytes
    if (z >= 2 && sps[i] === 3) { z = 0; continue; }
    r.push(sps[i]); z = sps[i] === 0 ? z + 1 : 0;
  }
  let p = 0;
  const u = n => { let v = 0; for (let k = 0; k < n; k++, p++) v = v * 2 + ((r[p >> 3] >> (7 - (p & 7))) & 1); return v; };
  const ue = () => { let z = 0; while (p < r.length * 8 && !u(1)) z++; return z > 31 ? -1 : 2 ** z - 1 + u(z); };
  u(4); const maxSub = u(3); u(1);
  p += 88 + 8;                                            // general profile, tier, level
  const present = [];
  for (let i = 0; i < maxSub; i++) present.push([u(1), u(1)]);
  if (maxSub > 0) p += 2 * (8 - maxSub);
  for (const [pp, lp] of present) p += (pp ? 88 : 0) + (lp ? 8 : 0);
  ue(); const cf = ue(); if (cf === 3) u(1);
  ue(); ue();
  if (u(1)) { ue(); ue(); ue(); ue(); }
  ue(); ue(); ue();
  const ordering = u(1);
  let dpb = 0, d = -1;
  for (let i = ordering ? 0 : maxSub; i <= maxSub; i++) { dpb = ue() + 1; d = ue(); ue(); }
  return p <= r.length * 8 && dpb > 0 && dpb <= 16 && d >= 0 && d < dpb ? d : null;
}

function hvccHasSps(hvcc) {
  if (!hvcc || hvcc.length < 23) return false;
  let p = 23;
  for (let a = 0; a < hvcc[22] && p + 3 <= hvcc.length; a++) {
    const type = hvcc[p] & 63, n = (hvcc[p + 1] << 8) | hvcc[p + 2];
    if (type === 33 && n > 0) return true;
    p += 3;
    for (let i = 0; i < n && p + 2 <= hvcc.length; i++) p += 2 + ((hvcc[p] << 8) | hvcc[p + 1]);
  }
  return false;
}

/** dvcC / dvvC: profile 7 bits, level 6 bits, rpu/el/bl present flags. */
export function parseDolbyVisionRecord(r) {
  if (!r || r.length < 4) return null;
  return {
    profile: r[2] >> 1,
    level: ((r[2] & 1) << 5) | (r[3] >> 3),
    rpuPresent: !!((r[3] >> 2) & 1),
    elPresent: !!((r[3] >> 1) & 1),
    blPresent: !!(r[3] & 1),
    blCompat: r.length > 4 ? r[4] >> 4 : undefined,
  };
}

/**
 * Raw audio files report the engine's codec names (`ac3 eac3 dts truehd flac pcm`) like Matroska tracks do, not the
 * MP4 spellings (`ac-3`, `mlpa`, `dtsc`) the engine puts in `codecString`; ADTS AAC keeps its `mp4a.40.<aot>` string.
 */
function rawCodec(t, container) {
  if (container !== 'raw' || t.kind !== 'audio') return null;
  return t.codec === 'aac' ? t.codecString : mapCodec(t, new Uint8Array(0));
}

function buildTrack(t, container) {
  const priv = t.codecPrivate ? fromHex(t.codecPrivate) : new Uint8Array(0);
  const track = {
    id: t.id,
    kind: t.kind,
    codec: t.kind === 'subtitle' ? subtitleCodec(t.codec) : rawCodec(t, container) ?? (t.codecString || mapCodec(t, priv)),
    timescale: t.timescale,
    containerCodec: t.codec,
  };
  if ((t.codec === 'A_MS/ACM' || t.codec === 'wav') && priv.length >= 16) {
    // WAVEFORMATEX: PCM (tag 1 or WAVE_FORMAT_EXTENSIBLE with a PCM subformat) or IEEE float (tag 3).
    // A raw .wav / RF64 / Wave64 file (`codec 'wav'`) carries its `fmt ` chunk the same way; the channel
    // mask of an extensible header becomes `pcm.layoutMask`, 8 bit samples are unsigned.
    const dv = new DataView(priv.buffer, priv.byteOffset, priv.byteLength);
    let tag = dv.getUint16(0, true);
    const extensible = tag === 0xfffe && priv.length >= 26;
    if (extensible) tag = dv.getUint16(24, true);
    if (tag === 1 || tag === 3) {
      track.codec = 'pcm';
      track.pcm = { format: tag === 3 ? 'float' : 'int', bits: dv.getUint16(14, true) };
      if (t.codec === 'wav') {
        if (tag === 1 && track.pcm.bits === 8) track.pcm.signed = false;
        const mask = extensible && priv.length >= 24 ? dv.getUint32(20, true) : 0;
        if (mask) track.pcm.layoutMask = mask;
      }
    }
  }
  if (t.sourcePid != null) track.sourcePid = t.sourcePid;
  if (t.enhancementLayer) track.enhancementLayer = true;
  if (t.codecDelayNs != null) track.codecDelayUs = Math.round(t.codecDelayNs / 1000);
  if (container === 'mpegts') {
    // MPEG-TS has no avcC/hvcC: video packets are Annex B (start codes, in-band parameter sets).
    if (t.kind === 'video' && (t.codec === 'h264' || t.codec === 'hevc')) track.annexB = true;
    if (t.codec === 'pcm_bluray') { track.codec = 'pcm'; track.pcm = { format: 'hdmv' }; }
  }
  if (t.fallbackCodecString) track.fallbackCodecString = t.fallbackCodecString;
  if (t.jocComplexity != null) track.jocComplexity = t.jocComplexity;
  if (t.dolbyVisionEl) track.dolbyVisionEl = fromHex(t.dolbyVisionEl);
  if (t.sampleDescriptions > 1) track.sampleDescriptions = t.sampleDescriptions;
  if (priv.length) track.description = priv;
  if (t.width) track.width = t.width;
  if (t.height) track.height = t.height;
  if (t.sampleRate) track.sampleRate = t.sampleRate;
  if (t.channels) track.channels = t.channels;
  if (t.bitDepth) track.bitDepth = t.bitDepth;
  // {primaries, transfer, matrix, fullRange, mastering, contentLight}, each null when the container is silent. Not `track.color`: session.js
  // reads that name for the HDR anchor and the subtitle white level, and neither was ever measured with a value in it.
  if (t.color) track.containerColor = t.color;
  if (t.language) track.language = t.language;
  if (t.name) track.name = t.name;
  if (t.defaultDurationNs) track.defaultDurationUs = Math.round(t.defaultDurationNs / 1000);
  if (container === 'matroska' && t.kind === 'video' && /^(hvc1|hev1)/.test(track.codec) && track.defaultDurationUs) {
    const d = hvccReorderDepth(priv);
    if (d !== null && d * track.defaultDurationUs <= REORDER_SHIFT_MAX_US) track.reorderDepth = d;
  }
  if (t.dolbyVision) {
    const dv = parseDolbyVisionRecord(fromHex(t.dolbyVision.record));
    if (dv) {
      track.dolbyVision = dv;
      // `dolbyVisionCodecString` is always a valid dvh1/dvhe/dva1/dvav/dav1 string; `fallbackCodecString` is the base layer
      // string built from the hvcC / avcC / av1C (a Matroska track keeps it in `codec`, which stays the base layer string).
      const fam = /^(hvc1|hev1)/.test(track.codec) ? 'hevc' : /^avc/.test(track.codec) ? 'avc' : /^av01/.test(track.codec) ? 'av1' : null;
      if (/^dv(h1|he|a1|av)|^dav1/.test(track.codec)) track.dolbyVisionCodecString = track.codec;
      else if (fam) {
        track.dolbyVisionCodecString = dolbyVisionCodecString(dv, { base: fam, inband: fam === 'hevc' ? !hvccHasSps(priv) : /^avc3/.test(track.codec) });
        if (!track.fallbackCodecString) track.fallbackCodecString = track.codec;
      }
    }
  }
  return track;
}

// ---------- demuxer ----------

class WasmDemuxer {
  constructor(wasm, source, info) {
    this.wasm = wasm;
    this.source = source;
    this.container = info.container;
    this.mkv = info.container === 'matroska';
    // A seek delivers the sound that plays at its keyframe wherever the container stored it: Matroska writes it with the picture, and for
    // MP4 and MOV the engine starts at the audio stored in front of the keyframe (aether_demux_seek, when the build has it).
    this.seekKeepsAudio = this.mkv || (info.container === 'mp4' && !!this.wasm.x.aether_demux_seek_keeps_audio);
    this.revision = wasm.x.aether_demux_revision();
    this.queue = [];          // Matroska: packets read ahead for the dts reorder window
    this.reorder = new Map(); // Matroska video track -> {pts: sorted ticks of queued packets, shift, depth}
    this.reorderGuess = new Set(); // tracks whose declared reorder depth did not hold
    this.eof = false;
    this.closed = false;
    this.applyInfo(info);
    this.startTimeUs = this.initialStart();
  }

  applyInfo(info) {
    this.info = info;
    const list = info.tracks.filter(t => t.kind !== 'other');
    this._tracks = list.map(t => buildTrack(t, info.container));
    this.scale = new Map(info.tracks.map(t => [t.id, 1e6 / t.timescale]));
    this.keep = new Set(this._tracks.map(t => t.id));
    this.durationUs = info.durationNs == null ? null : Math.round(info.durationNs / 1000);
    this.omittedTracks = info.omittedTracks;
    this.omittedTrackReasons = info.omittedTrackReasons ?? [];
    this.fragmented = !!info.fragmented;
    this.timelineBreaks = (info.timelineBreaks ?? []).map(b => ({ offset: b.offset, shiftUs: Math.round(b.shiftTicks * 1e6 / 90000) }));
  }

  /** Re-reads the track list and breaks when indexing discovered more (MPEG-TS). */
  refresh() {
    const rev = this.wasm.x.aether_demux_revision();
    if (rev === this.revision) return;
    this.revision = rev;
    this.applyInfo(this.wasm.demuxInfo());
  }

  /** Earliest presentation time among the packets indexed at open (0 when none are, as in Matroska). */
  initialStart() {
    const n = this.wasm.demuxPacketCount();
    if (!n) return 0;
    const { count, view } = this.wasm.demuxPackets(0, Math.min(n, 4096));
    let best = Infinity;
    for (let i = 0; i < count; i++) {
      const o = i * RECORD_BYTES;
      if (!(view.getUint32(o + 44, true) & 2)) continue;
      const id = Number(view.getBigUint64(o, true));
      const s = this.scale.get(id);
      if (s && this.keep.has(id)) best = Math.min(best, Math.round(Number(view.getBigInt64(o + 8, true)) * s));
    }
    return Number.isFinite(best) ? best : 0;
  }

  /** Packets indexed so far (grows while reading; the whole file once `complete`). */
  get count() { return this.wasm.demuxPacketCount(); }
  get complete() { return this.wasm.x.aether_demux_complete() === 1; }
  /** Matroska: places where data was skipped so far (a bad block, the rest of a corrupt or cut cluster); those packets are missing. */
  get damaged() { return this.wasm.x.aether_demux_damaged?.() ?? 0; }

  tracks() { this.refresh(); return this._tracks.map(t => ({ ...t })); }

  /** Position the cursor on the verified keyframe at or before `timeUs` of the first video track
   *  (first track when there is no video). `timeUs` is on the packets' own timeline (`startTimeUs`
   *  is not 0 for most MPEG-TS files). Audio packets from the same storage position follow. */
  seek(timeUs) {
    this.refresh();
    const main = this._tracks.filter(x => !x.enhancementLayer);
    const t = main.find(x => x.kind === 'video') || main[0];
    if (!t) return;
    const ticks = Math.round(timeUs / this.scale.get(t.id));
    this.wasm.demuxSeek(t.id, ticks);
    this.queue = [];
    this.reorder.clear();
    this.eof = false;
  }

  /**
   * Next packet of a kept track straight from the engine, dts as stored. With `deferPayload` (and an
   * engine that has `aether_demux_next_header`) a video packet comes without its payload, which
   * `loadPayload` fetches when the packet is handed out: the dts look-ahead needs only timestamps, so a
   * remote source does not download the payload of 64 frames before the first packet.
   */
  readRaw(deferPayload = false) {
    for (;;) {
      const defer = deferPayload && !!this.wasm.x.aether_demux_next_header;
      const n = defer ? this.wasm.x.aether_demux_next_header() : this.wasm.x.aether_demux_next();
      if (n < 0) this.wasm.fail('demux_next', n, DEMUX_ERRORS);
      if (n === 0) {
        // A raw audio file only estimated its duration at open; the end of the file makes it exact.
        if (this.container === 'raw') this.applyInfo(this.wasm.demuxInfo());
        return null;
      }
      const r = this.wasm.result();
      const view = new DataView(r.buffer, r.byteOffset, r.byteLength);
      this.refresh();
      const trackId = Number(view.getBigUint64(0, true));
      if (!this.keep.has(trackId)) continue;
      const s = this.scale.get(trackId);
      const f = view.getUint32(44, true);
      const pts = Number(view.getBigInt64(8, true));
      if (defer) {
        const packet = {
          trackId,
          ptsUs: Math.round(pts * s),
          dtsUs: Math.round(Number(view.getBigInt64(16, true)) * s),
          durationUs: Math.round(Number(view.getBigInt64(24, true)) * s),
          key: (f & 1) !== 0,
          data: null,
          pending: { offset: view.getBigUint64(32, true), size: view.getUint32(40, true) },
        };
        if ((f & 2) === 0) packet.presentation = false;
        // Only the video look-ahead window benefits from deferring; everything else is read now.
        return this.isReordered(trackId) ? packet : this.loadPayload(packet);
      }
      const packet = {
        trackId,
        ptsUs: Math.round(pts * s),
        dtsUs: Math.round(Number(view.getBigInt64(16, true)) * s),
        durationUs: Math.round(Number(view.getBigInt64(24, true)) * s),
        key: (f & 1) !== 0,
        data: r.slice(RECORD_BYTES),
      };
      if ((f & 2) === 0) packet.presentation = false;
      return packet;
    }
  }

  isReordered(trackId) {
    return this.mkv && this._tracks.find(t => t.id === trackId)?.kind === 'video';
  }

  /** Fetches the payload of a packet read with `deferPayload`. */
  loadPayload(p) {
    if (!p.pending) return p;
    const m = this.wasm.x.aether_demux_payload(BigInt(p.trackId), p.pending.offset, p.pending.size);
    if (m < 0) this.wasm.fail('demux_payload', m, DEMUX_ERRORS);
    p.data = this.wasm.result();
    delete p.pending;
    return this.settleKey(p);
  }

  /**
   * Matroska video: a block flagged as key is only reported as `key` when the payload really starts a decodable
   * picture (H.264 IDR, HEVC IRAP). Muxers flag any I picture; ffprobe (and the keyframe seek) do not. The first picture
   * of a file can therefore be `key: false`: a decoder still starts there.
   */
  settleKey(p) {
    if (p.key && this.mkv && this.wasm.x.aether_demux_verify_keyframe && this.isReordered(p.trackId)) {
      const v = this.wasm.withBytes(p.data, (ptr, len) => this.wasm.x.aether_demux_verify_keyframe(BigInt(p.trackId), ptr, len));
      if (v === 0) p.key = false;
    }
    return p;
  }

  enqueue() {
    const p = this.readRaw(true);
    if (!p) { this.eof = true; return false; }
    this.queue.push(p);
    if (this.isReordered(p.trackId)) {
      let st = this.reorder.get(p.trackId);
      if (!st) {
        const depth = this._tracks.find(t => t.id === p.trackId)?.reorderDepth ?? null;
        const known = depth !== null && !this.reorderGuess.has(p.trackId);
        this.reorder.set(p.trackId, st = { pts: [], shift: known ? depth * this._tracks.find(t => t.id === p.trackId).defaultDurationUs + REORDER_TICK_US : null, depth: known ? depth : null });
      }
      let lo = 0, hi = st.pts.length;
      while (lo < hi) { const m = (lo + hi) >> 1; if (st.pts[m] <= p.ptsUs) lo = m + 1; else hi = m; }
      st.pts.splice(lo, 0, p.ptsUs);
    }
    return true;
  }

  /**
   * Matroska stores no DTS (the engine reports dts == pts), which is not monotonic with B-frames. For a
   * video track the adapter reads ahead and sets dts[k] = k-th smallest pts minus a constant offset
   * (the largest overshoot over the first 64 packets), so dts is monotonic and dts <= pts, as in the
   * negative start of MP4. It needs to look at most 32 packets ahead on the same track.
   */
  nextMatroska() {
    if (!this.queue.length && !this.enqueue()) return null;
    const head = this.queue[0];
    const video = this.isReordered(head.trackId);
    if (video) {
      const st = this.reorder.get(head.trackId);
      const want = st.shift === null ? DTS_PROBE : DTS_LOOKAHEAD;
      const own = () => this.queue.filter(q => q.trackId === head.trackId);
      const enough = () => {
        const q = own();
        if (st.depth !== null) return q.length >= st.depth + 1;
        if (q.length >= want + 1) return true;
        return q.length >= DTS_MIN_AHEAD + 1 && q.reduce((n, x) => n + (x.pending ? x.pending.size : x.data.length), 0) >= DTS_AHEAD_BYTES;
      };
      while (!this.eof && !enough()) this.enqueue();
      if (st.shift === null) {
        const own = this.queue.filter(q => q.trackId === head.trackId).slice(0, DTS_PROBE).map(q => q.ptsUs);
        const sorted = own.slice().sort((x, y) => x - y);
        st.shift = Math.max(0, ...sorted.map((v, k) => v - own[k]));
      }
    }
    this.queue.shift();
    this.loadPayload(head);
    if (video) {
      const st = this.reorder.get(head.trackId);
      const dts = st.pts.shift() - st.shift;
      // A stream that reorders further than its SPS says (or whose frames are longer than the track's frame duration) shows here:
      // the track goes back to the measured window from the next seek on, and this packet's dts is clamped to its pts.
      if (dts > head.ptsUs && st.depth !== null) { this.reorderGuess.add(head.trackId); st.depth = null; }
      head.dtsUs = Math.min(dts, head.ptsUs);
    } else {
      head.dtsUs = head.ptsUs;
    }
    return head;
  }

  nextPacket() {
    if (this.mkv) return this.nextMatroska();
    return this.readRaw();
  }

  close({ keepSource = false } = {}) {
    if (this.closed) return;
    this.closed = true;
    this.wasm.demuxClose();
    if (!keepSource) this.source.close();
  }
}

/**
 * @param {Blob|File|{url:string, headers?:object, blockSize?:number}|{path:string}|Uint8Array} source
 * @param {{wasm?: URL|string|ArrayBuffer, reuse?: object}} [opts] alternate module location; `reuse`: the source of a demuxer closed
 *   with `close({keepSource: true})`, whose cached blocks then serve the reopen (over HTTP the first megabytes are not fetched again)
 */
export async function openDemuxer(source, opts = {}) {
  const [src] = await Promise.all([opts.reuse ?? createSource(source), loadModule(opts.wasm).then(m => { sourceTrace.mark?.('wasm-ready'); return m; })]);
  const wasm = new AetherWasm(moduleSync(), { read: (offset, dest) => src.read(offset, dest) });
  try {
    wasm.demuxOpen(src.length);
    return new WasmDemuxer(wasm, src, wasm.demuxInfo());
  } catch (e) {
    src.close();
    throw e;
  }
}

/**
 * Annex B (start code) access unit to length prefixed NALs, for decoders or the Dolby Vision helpers
 * that expect the MP4 layout. MPEG-TS video packets are Annex B and the tracks carry no `description`.
 */
export function annexBToLengthPrefixed(data, lengthSize = 4) {
  const starts = [];
  for (let i = 0; i + 3 <= data.length; i++) {
    if (data[i] === 0 && data[i + 1] === 0 && data[i + 2] === 1) { starts.push(i + 3); i += 2; }
  }
  const nals = starts.map((st, k) => {
    let end = k + 1 < starts.length ? starts[k + 1] - 3 : data.length;
    while (end > st && data[end - 1] === 0) end--; // zero_byte of a four byte start code, trailing zeros
    return data.subarray(st, end);
  });
  return frame(nals, lengthSize);
}

// ---------- Dolby Vision helpers ----------

let shared = null;
/** Shared module instance for the stateless dovi calls. Needs `loadModule` to have resolved. */
function dovi() {
  if (!shared) shared = new AetherWasm(moduleSync());
  return shared;
}

/** Await once before using the helpers below without a demuxer (openDemuxer already does it). */
export async function initEngine(wasm) {
  await loadModule(wasm);
  dovi();
}

function lengthSizeOf(arg) {
  if (typeof arg === 'number') return arg;
  const d = arg?.description ?? arg;
  if (d instanceof Uint8Array && d.length > 21) return (d[21] & 3) + 1;
  return 4;
}

function frame(nals, size) {
  let total = 0;
  for (const n of nals) total += size + n.length;
  const out = new Uint8Array(total);
  let o = 0;
  for (const n of nals) {
    for (let i = 0; i < size; i++) out[o + i] = (n.length >>> (8 * (size - 1 - i))) & 255;
    out.set(n, o + size);
    o += size + n.length;
  }
  return out;
}

/**
 * Split one Dolby Vision access unit (length prefixed, as stored in Matroska/MP4) into
 * `bl`: base layer NALs, `el`: enhancement layer NALs with the type-63 wrapper removed, both
 * length prefixed with the same size, and `rpu`: the first RPU NAL (type 62, no prefix), or null.
 * `lengthSize` may be a number, a Track or an hvcC; default 4. `el` is empty for single layer streams.
 */
export function splitDolbyVision(packet, lengthSize = 4) {
  const size = lengthSizeOf(lengthSize);
  const data = packet.data ?? packet;
  const nals = dovi().doviSplit(data, size);
  const bl = [], el = [], rpus = [];
  for (const n of nals) {
    const bytes = data.subarray(n.offset, n.offset + n.length);
    if (n.kind === 0) bl.push(bytes);
    else if (n.kind === 1) el.push(bytes.subarray(2));
    else rpus.push(bytes);
  }
  return { bl: frame(bl, size), el: frame(el, size), rpu: rpus[0] ? rpus[0].slice() : null, rpus: rpus.map(r => r.slice()) };
}

/** 'fel', 'mel' or 'unknown' for one Profile 7 access unit, from its first RPU. */
export function dolbyVisionElKind(packet, lengthSize = 4) {
  const { rpu } = splitDolbyVision(packet, lengthSize);
  if (!rpu) return 'unknown';
  try { return dovi().doviInspectRpu(rpu).enhancement; } catch { return 'unknown'; }
}

/** A Profile 7 access unit as a single layer Profile 8.1 one: the enhancement layer goes, each RPU is rewritten. Same length prefix. */
export function convertPacketTo81(packet, lengthSize = 4) {
  const size = lengthSizeOf(lengthSize);
  const data = packet.data ?? packet;
  const out = [];
  for (const n of dovi().doviSplit(data, size)) {
    const bytes = data.subarray(n.offset, n.offset + n.length);
    if (n.kind === 0) out.push(bytes);
    else if (n.kind === 2) { try { out.push(dovi().doviTo81(bytes)); } catch { /* the picture plays on the previous metadata */ } }
  }
  return frame(out, size);
}

/**
 * Chrome ignores VideoDecoderConfig.colorSpace and tone-maps PQ on WebGPU import, so the bitstream
 * tag has to say SDR (primaries 1, transfer 13, matrix kept). Returns a copy of the track whose
 * hvcC SPS carry the carrier colour, and the original colour to apply later.
 * In-band SPS (hev1-style streams, as in many Matroska files) need `relabelPacketSps`.
 */
export function relabelForChrome(track, patch = SDR_CARRIER) {
  const out = { ...track };
  let original = null;
  if (track.description && track.description.length > 23) {
    try {
      const r = dovi().doviRelabelHvcc(track.description, patch);
      out.description = r.nal; // the record, same buffer layout as the NAL case
      original = r.original;
    } catch (e) {
      // An hvcC without parameter sets (the engine's Matroska files) has nothing to patch.
      if (!/relabel/.test(String(e.message))) throw e;
    }
  }
  out.relabelled = true;
  return { track: out, original };
}

/**
 * Relabel every SPS (NAL type 33) in a length prefixed access unit. Returns the new packet data and
 * the original colour of the first SPS found (null when the packet has none). The packet is
 * returned unchanged (same buffer) when it has no SPS.
 */
export function relabelPacketSps(data, lengthSize = 4, patch = SDR_CARRIER) {
  const size = lengthSizeOf(lengthSize);
  const view = data.subarray ? data : new Uint8Array(data);
  const parts = [];
  let original = null, changed = false;
  let pos = 0;
  while (pos + size <= view.length) {
    let len = 0;
    for (let i = 0; i < size; i++) len = len * 256 + view[pos + i];
    const start = pos + size, end = start + len;
    if (end > view.length) throw new Error('relabelPacketSps: truncated NAL');
    let nal = view.subarray(start, end);
    if (len > 2 && ((nal[0] >> 1) & 63) === 33) {
      const r = dovi().doviRelabelSps(nal, patch);
      original ??= r.original;
      nal = r.nal;
      changed = true;
    }
    parts.push(nal);
    pos = end;
  }
  return { data: changed ? frame(parts, size) : data, original };
}

export { SDR_CARRIER, P5_CARRIER };
