// SPDX-License-Identifier: LGPL-3.0-only
// VC-1 Advanced Profile video decoder (rust/aether-vc1 inside web/wasm/aether_wasm.wasm). Software decode, 8 bit 4:2:0,
// bit identical to FFmpeg. Plain ES module for Workers, Node and the main thread; one `Vc1Decoder` owns one wasm instance.
//
//   const dec = await createVc1Decoder(track);          // track: demuxer video track (Matroska V_MS/VFW/FOURCC "WVC1")
//   for (const f of dec.decode(packet.data, packet.ptsUs)) { f.y; f.u; f.v; f.width; f.height; f.ptsUs; f.interlaced; ... }
//   dec.reset();                                         // after a seek (the next packet must be a key packet or entry point)
//   for (const f of dec.flush()) ...                     // end of stream: the delayed reference picture
//
// Frames come in display order. `y`, `u`, `v` are tightly packed copies (stride == width), the three views share one ArrayBuffer
// (`f.buffer`) so it can be transferred to another thread. With `{copy: false}` the views point into wasm memory instead and are
// only valid until the next decode, flush or reset.

import { AetherWasm, AetherError } from './aether-wasm.mjs';

const FLAG = { interlaced: 1, tff: 2, rff: 4, key: 8, fieldCoded: 16 };
const ERRORS = { '-1': 'invalid VC-1 data', '-2': 'unsupported VC-1 profile (only Advanced Profile is implemented)', '-3': 'no sequence header' };

/** True for a Matroska / AVI VC-1 track (FourCC WVC1 or WMV3, codec "vc1" or a MP4 "vc-1" entry). */
export function isVc1Track(track) {
  if (!track || track.kind !== 'video') return false;
  const c = String(track.codec || '').toLowerCase();
  if (c === 'vc1' || c === 'vc-1' || c === 'wvc1') return true;
  if (/^v_ms\/vfw\/fourcc$/i.test(track.containerCodec || track.codec || '')) return vc1FourCC(track) === 'WVC1';
  return false;
}

/** FourCC inside the BITMAPINFOHEADER that Matroska keeps as CodecPrivate of V_MS/VFW/FOURCC, '' when absent. */
export function vc1FourCC(track) {
  const d = track && track.description;
  if (!d || d.length < 20) return '';
  return String.fromCharCode(d[16], d[17], d[18], d[19]);
}

/**
 * The sequence header + entry point units with start codes (what the decoder wants as extradata). Matroska keeps them behind a
 * 40 byte BITMAPINFOHEADER (its biSize field cannot be trusted); a description that already starts with a start code is returned as it is.
 */
export function vc1Extradata(track) {
  const d = track && track.description;
  if (!d || !d.length) return new Uint8Array(0);
  if (d.length >= 4 && d[0] === 0 && d[1] === 0 && d[2] === 1) return d;
  // biSize of the lab files is the size of the whole record (74), not of the header, so the header length is fixed at 40
  let i = Math.min(40, d.length);
  while (i + 3 < d.length && !(d[i] === 0 && d[i + 1] === 0 && d[i + 2] === 1)) i++;   // the lab files have a 4 byte start code
  return d.subarray(i);
}

/** Display duration in us of a frame of `frameTimeUs`, with FFmpeg's repeat_pict (in field durations): frame * (2 + repeat_pict) / 2. */
export function vc1DisplayDurationUs(frameTimeUs, repeatPict) {
  return Math.round(frameTimeUs * (2 + repeatPict) / 2);
}

/** The pull-down count FFmpeg derives from the picture header: RFF repeats one field, RPTFRM n repeats the frame n times. */
export function vc1RepeatPict(rff, rptfrm) { return rff ? 1 : rptfrm * 2; }

export class Vc1Decoder {
  /** @param {AetherWasm} wasm */
  constructor(wasm, { copy = true, frameDurUs = 0 } = {}) {
    this.wasm = wasm;
    this.x = wasm.x;
    this.copy = copy;
    this.h = this.x.aether_vc1_create();
    if (!this.h) throw new Error('aether_vc1_create failed');
    // Matroska blocks of VC-1 with B pictures carry decode order timestamps. FFmpeg (ffprobe best_effort_timestamp) gives a frame the
    // timestamp of the access unit whose decode made it appear: a B picture appears at once (its own stamp), a reference picture
    // one access unit later (stamp of the next reference). That puts the first frame of the lab files at 83 ms, not 0, and keeps
    // video and audio on the same clock; `decoderPtsUs` keeps what the decoder itself attached (the stamp of the coded unit).
    // Frames drained by flush() have no unit of their own: previous pts plus the previous display duration.
    this.clockPts = null;
    this.lastPts = null;
    this.lastDurUs = 0;
    this.frameDurUs = frameDurUs;   // from the container (default duration), else the sequence header frame rate
    this.decoded = 0;   // access units given to decode()
    this.frames = 0;    // frames returned
  }

  #pass(op, code) {
    if (code < 0) throw new AetherError(ERRORS[String(code)] || 'VC-1 error', code, op);
  }

  setExtradata(bytes) {
    const w = this.wasm;
    this.#pass('vc1_set_extradata', w.withBytes(bytes, (p, n) => this.x.aether_vc1_set_extradata(this.h, p, n)));
  }

  /** Decodes one access unit (a Matroska block) and returns the frames that became ready. */
  decode(data, ptsUs = 0) {
    const w = this.wasm;
    const pts = Math.round(ptsUs), hi = Math.floor(pts / 4294967296), lo = pts - hi * 4294967296;
    this.decoded++;
    this.clockPts = pts;
    const code = w.withBytes(data, (p, n) => this.x.aether_vc1_decode(this.h, p, n, lo, hi));
    // a damaged unit is concealed by the decoder; only a missing sequence header or an unsupported profile is fatal
    if (code === -2 || code === -3) this.#pass('vc1_decode', code);
    this.lastStatus = code;
    return this.#drain();
  }

  /** Frames still held back (the last reference picture) at the end of a stream. */
  flush() {
    this.x.aether_vc1_flush(this.h);
    this.clockPts = null;
    return this.#drain();
  }

  /** Forget all references (seek). The next access unit has to be a random access point. */
  reset() { this.x.aether_vc1_reset(this.h); this.clockPts = null; this.lastPts = null; this.lastDurUs = 0; }

  #drain() {
    const out = [];
    while (this.x.aether_vc1_next_frame(this.h)) out.push(this.#frame());
    return out;
  }

  #frame() {
    const x = this.x, h = this.h;
    const w = x.aether_vc1_frame_info(h, 0), ht = x.aether_vc1_frame_info(h, 1), flags = x.aether_vc1_frame_info(h, 2);
    const cw = (w + 1) >> 1, ch = (ht + 1) >> 1;
    const pts = x.aether_vc1_frame_info(h, 5) * 4294967296 + (x.aether_vc1_frame_info(h, 4) >>> 0);
    const mem = this.wasm.mem;
    const sizes = [[w, ht], [cw, ch], [cw, ch]];
    let buffer = null, views;
    if (this.copy) {
      buffer = new ArrayBuffer(w * ht + 2 * cw * ch);
      views = [];
      let off = 0;
      for (const [pw, ph] of sizes) { views.push(new Uint8Array(buffer, off, pw * ph)); off += pw * ph; }
    }
    const planes = [0, 1, 2].map(p => {
      const [pw, ph] = sizes[p];
      const stride = x.aether_vc1_frame_info(h, 8 + p), ptr = x.aether_vc1_frame_plane(h, p);
      if (!this.copy) return { data: mem.subarray(ptr, ptr + stride * ph), stride };
      const dst = views[p];
      for (let r = 0; r < ph; r++) dst.set(mem.subarray(ptr + r * stride, ptr + r * stride + pw), r * pw);
      return null;
    });
    this.frames++;
    const repeatPict = x.aether_vc1_frame_info(h, 3);
    const durationUs = this.#durationUs(repeatPict);
    const ptsUs = this.clockPts !== null ? this.clockPts : (this.lastPts !== null ? this.lastPts + this.lastDurUs : pts);
    this.lastPts = ptsUs; this.lastDurUs = durationUs;
    return {
      ptsUs, durationUs, decoderPtsUs: pts, width: w, height: ht, buffer,
      y: this.copy ? views[0] : planes[0].data, u: this.copy ? views[1] : planes[1].data, v: this.copy ? views[2] : planes[2].data,
      strides: this.copy ? [w, cw, cw] : planes.map(p => p.stride),
      interlaced: !!(flags & FLAG.interlaced), topFieldFirst: !!(flags & FLAG.tff), repeatFirstField: !!(flags & FLAG.rff),
      key: !!(flags & FLAG.key), fieldCoded: !!(flags & FLAG.fieldCoded),
      repeatPict, repeatFrames: x.aether_vc1_frame_info(h, 7),
      pictType: String.fromCharCode(x.aether_vc1_frame_info(h, 6)),
    };
  }

  /** Display duration: one frame time plus half a frame time per repeat_pict field (RFF adds 1, RPTFRM n adds 2n), as FFmpeg. */
  #durationUs(repeatPict) { return vc1DisplayDurationUs(this.#frameTimeUs(), repeatPict); }

  #frameTimeUs() {
    if (this.frameDurUs > 0) return this.frameDurUs;
    const i = this.info();
    return i && i.fpsNum ? 1e6 * i.fpsDen / i.fpsNum : 0;
  }

  /** Stream parameters once the headers were seen (else null). `fps` is num/den of the sequence header (0 when absent). */
  info() {
    const g = n => this.x.aether_vc1_stream_info(this.h, n);
    const width = g(0);
    if (!width) return null;
    return { width, height: g(1), interlace: !!g(2), fpsNum: g(3), fpsDen: g(4), aspectNum: g(5), aspectDen: g(6), profile: g(7), level: g(8) };
  }

  get errors() { return this.x.aether_vc1_error_count(this.h); }

  close() { if (this.h) { this.x.aether_vc1_destroy(this.h); this.h = 0; } }
}

/** New decoder on its own wasm instance, extradata taken from the track when given. */
export async function createVc1Decoder(track = null, { url, copy = true } = {}) {
  const dec = new Vc1Decoder(await AetherWasm.create({}, url), { copy, frameDurUs: track && track.defaultDurationUs > 0 ? track.defaultDurationUs : 0 });
  const extra = track ? vc1Extradata(track) : null;
  if (extra && extra.length) dec.setExtradata(extra);
  return dec;
}
