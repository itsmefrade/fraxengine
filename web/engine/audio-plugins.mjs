// SPDX-License-Identifier: LGPL-3.0-only
// Engine-decoded audio (TrueHD, DTS family, AC-3, E-AC-3, LPCM) over the aether-audio decoder ABI.
// E-AC-3 JOC (Dolby Atmos) objects are rendered to 7.1.4 by default (`opts.joc === false` or `setJoc(false)`: bed).
//
//   const plugin = createAudioPlugin(track);        // null when the browser decodes the codec
//   plugin.configure(track);
//   const pcm = plugin.decode(packet);               // {frames, sampleRate, channels, layout, planar, lossless, joc}
//   // joc: false, or {objects, rendered} for an E-AC-3 stream that signals Atmos
//
// Each plugin owns its own WASM instance (the ABI keeps one decoder per instance). Decoding is
// synchronous and CPU bound, run it in a Worker. `loadModule()` (or `openDemuxer`/`initEngine`)
// must have resolved before `configure`.

import { AetherWasm, moduleSync, loadModule } from './aether-wasm.mjs';

const BROWSER_DECODED = /^(mp4a\.|opus$|flac$|mp3$|vorbis$)/;

/** Engine codec name for a track codec: accepts the MP4 `codecString` spellings (ac-3, ec-3, mlpa, dtsc..., pcm-s16le). */
export function engineCodec(codec = '') {
  if (codec === 'ac-3') return 'ac3';
  if (codec === 'ec-3') return 'eac3';
  if (codec === 'mlpa') return 'truehd';
  if (/^dts[cehlx]?$/.test(codec)) return 'dts';
  if (/^pcm(-|$)/.test(codec)) return 'pcm';
  return codec;
}

const PCM_FORMAT = { intLe: 0, intBe: 1, floatLe: 2, floatBe: 3, uint8: 4, hdmv: 5 };

/**
 * LPCM parameters of a track for `aether_audio_open_pcm`: `{format, bits, channels, sampleRate, layoutMask}`.
 * Sources, in order: `track.pcm = {format?: 'int'|'float'|'hdmv', bigEndian?, signed?, bits?, layoutMask?}`;
 * the MP4 `pcmC` box in `track.description` (ipcm/fpcm); the sample entry (`containerCodec`) or
 * `pcm-s16le` style codec string; Matroska `A_PCM/*` with `bitDepth`. Throws when nothing identifies the format.
 */
export function pcmParams(track) {
  const channels = track.channels ?? 0, sampleRate = track.sampleRate ?? 0;
  const mk = (format, bits, layoutMask = track.pcm?.layoutMask ?? 0) => ({ format, bits, channels, sampleRate, layoutMask });
  const o = track.pcm;
  if (o) {
    if (o.format === 'hdmv') return mk(PCM_FORMAT.hdmv, 0);
    const bits = o.bits ?? track.bitDepth ?? 16;
    if (o.format === 'float') return mk(o.bigEndian ? PCM_FORMAT.floatBe : PCM_FORMAT.floatLe, bits);
    if (bits === 8 && o.signed === false) return mk(PCM_FORMAT.uint8, 8);
    return mk(o.bigEndian ? PCM_FORMAT.intBe : PCM_FORMAT.intLe, bits);
  }
  const cc = track.containerCodec ?? '';
  const code = String(track.codec ?? '');
  if (/^hdmv/i.test(cc) || code === 'hdmv-lpcm') return mk(PCM_FORMAT.hdmv, 0);
  let m = /^pcm-([suf])(\d+)(le|be)$/.exec(code);
  if (m) {
    const bits = Number(m[2]), be = m[3] === 'be';
    if (m[1] === 'f') return mk(be ? PCM_FORMAT.floatBe : PCM_FORMAT.floatLe, bits);
    if (m[1] === 'u') return mk(PCM_FORMAT.uint8, 8);
    return mk(be ? PCM_FORMAT.intBe : PCM_FORMAT.intLe, bits);
  }
  const bd = track.bitDepth;
  switch (cc) {
    case 'A_PCM/INT/LIT': return bd === 8 ? mk(PCM_FORMAT.uint8, 8) : mk(PCM_FORMAT.intLe, bd ?? 16);
    case 'A_PCM/INT/BIG': return mk(PCM_FORMAT.intBe, bd ?? 16);
    case 'A_PCM/FLOAT/IEEE': return mk(PCM_FORMAT.floatLe, bd ?? 32);
    case 'sowt': return mk(PCM_FORMAT.intLe, bd ?? 16);
    case 'twos': return mk(PCM_FORMAT.intBe, bd ?? 16);
    case 'in24': return mk(PCM_FORMAT.intBe, 24);
    case 'in32': return mk(PCM_FORMAT.intBe, 32);
    case 'fl32': return mk(PCM_FORMAT.floatBe, 32);
    case 'fl64': return mk(PCM_FORMAT.floatBe, 64);
    case 'raw ': return mk(PCM_FORMAT.uint8, 8);
    default: break;
  }
  if ((cc === 'ipcm' || cc === 'fpcm') && track.description?.length >= 6) {
    // pcmC: version/flags (4 bytes), format_flags (bit 0: little endian), PCM_sample_size
    const d = track.description, le = (d[4] & 1) !== 0, bits = d[5];
    if (cc === 'fpcm') return mk(le ? PCM_FORMAT.floatLe : PCM_FORMAT.floatBe, bits);
    return mk(le ? PCM_FORMAT.intLe : PCM_FORMAT.intBe, bits);
  }
  if (cc === 'ipcm' && bd) return mk(PCM_FORMAT.intBe, bd);
  throw new Error(`cannot tell the PCM sample format of ${cc || code || 'track'}; pass track.pcm`);
}

function concat(blocks) {
  if (blocks.length === 1) return blocks[0];
  const channels = blocks[0].channels;
  const frames = blocks.reduce((n, b) => n + b.frames, 0);
  const planar = Array.from({ length: channels }, () => new Float32Array(frames));
  let o = 0;
  for (const b of blocks) {
    for (let c = 0; c < channels; c++) planar[c].set(b.planar[c], o);
    o += b.frames;
  }
  const first = blocks[0];
  return { frames, sampleRate: first.sampleRate, channels, layout: first.layout, planar, lossless: first.lossless, joc: blocks.findLast(b => b.joc)?.joc ?? false };
}

class EngineAudioPlugin {
  /**
   * @param {'truehd'|'dts'|'dtsx'|'ac3'|'eac3'|'pcm'} kind
   * @param {{presentation?: number}} [opts] TrueHD presentation index (default 2: 7.1 bed).
   */
  constructor(kind, opts = {}) {
    this.kind = kind;
    this.opts = opts;
    this.wasm = null;
    this.packets = 0;
  }

  configure(track = {}) {
    this.close();
    this.track = track;
    this.wasm = new AetherWasm(moduleSync());
    this.wasm.audioSetJoc(this.opts.joc !== false);
    if (this.kind === 'truehd') this.wasm.audioOpen(0, this.opts.presentation ?? 2);
    else if (this.kind === 'ac3') this.wasm.audioOpen(3, 0);
    else if (this.kind === 'eac3') this.wasm.audioOpen(4, 0);
    else if (this.kind === 'pcm') {
      const p = pcmParams(track);
      this.wasm.audioOpenPcm(p.format, p.bits, p.channels, p.sampleRate, p.layoutMask);
    } else this.wasm.audioOpen(this.kind === 'dtsx' ? 2 : 1, 0);
    return this;
  }

  /** Decodes one packet (`{data}` or bytes). Returns zero frames while the decoder is priming. */
  decode(packet) {
    if (!this.wasm) throw new Error('audio plugin is not configured');
    this.wasm.audioPush(packet.data ?? packet);
    this.packets++;
    const blocks = [];
    for (let b; (b = this.wasm.audioPull());) {
      blocks.push({ frames: b.frames, sampleRate: b.sampleRate, channels: b.channels, layout: b.labels, planar: b.planar, lossless: b.lossless, joc: b.joc });
    }
    if (!blocks.length) {
      const channels = this.track?.channels ?? 0;
      return { frames: 0, sampleRate: this.track?.sampleRate ?? 48000, channels, layout: [], planar: Array.from({ length: channels }, () => new Float32Array(0)), lossless: true, joc: false };
    }
    return concat(blocks);
  }

  /**
   * E-AC-3 JOC (Dolby Atmos): true renders the objects to 7.1.4 (default), false returns the 5.1/7.1 bed.
   * Applies from the next packet on and changes the channel layout of the decoded blocks.
   */
  setJoc(enabled) {
    this.opts = { ...this.opts, joc: !!enabled };
    this.wasm?.audioSetJoc(enabled);
  }

  /** Drops decoder state and reopens it: use after a seek (the next packet starts a new run). */
  flush() {
    if (this.wasm) this.configure(this.track);
  }

  close() {
    if (this.wasm) { this.wasm.audioClose(); this.wasm = null; }
  }
}

/**
 * Plugin for a track the engine decodes, or null for codecs the browser decodes
 * (mp4a, opus, flac).
 * `opts.dtsx` selects the DTS:X 7.1.4 presentation, `opts.presentation` the TrueHD one.
 */
export function createAudioPlugin(track, opts = {}) {
  const codec = engineCodec(track.codec);
  if (BROWSER_DECODED.test(codec)) return null;
  if (codec === 'truehd') return new EngineAudioPlugin('truehd', opts);
  if (codec === 'dts') return new EngineAudioPlugin(opts.dtsx ? 'dtsx' : 'dts', opts);
  if (codec === 'ac3' || codec === 'eac3' || codec === 'pcm') return new EngineAudioPlugin(codec, opts);
  return null;
}

/** Async convenience: loads the module first. */
export async function createAudioPluginAsync(track, opts) {
  await loadModule();
  return createAudioPlugin(track, opts);
}
