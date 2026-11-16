// SPDX-License-Identifier: LGPL-3.0-only
// AudioDecoderPlugin over the Rust/WASM engine (web/engine/audio-plugins.mjs): TrueHD, DTS family, AC-3, E-AC-3, LPCM.
// The engine returns frames without timestamps, so output is stamped from the first packet after a (re)start and then
// runs contiguously at the decoded sample rate. DecodedAudio carries `layout` (canonical labels), `lossless`, `joc` (false, or {objects, rendered} for E-AC-3 Atmos).
import { createAudioPlugin as createEnginePlugin, engineCodec } from '../../../engine/audio-plugins.mjs';
import { loadModule } from '../../../engine/aether-wasm.mjs';

export const ENGINE_CODECS = /^(truehd|dts|dtsx|ac3|eac3|pcm|ac-3|ec-3|mlpa|dts[cehlx]|pcm-)/;

export class EngineAudioPlugin {
  constructor(opts = {}) { this.opts = opts; this.inner = null; this.track = null; this.nextPtsUs = null; this.firstPtsUs = null; this.pending = []; this.info = {}; this.badRun = 0; this.badTotal = 0; }

  async configure(track) {
    await loadModule();
    this.track = track;
    this.inner = createEnginePlugin(track, this.opts);
    if (!this.inner) throw new Error(`engine has no decoder for "${track.codec}"`);
    this.inner.configure(track);
    this.engineCodec = engineCodec(track.codec);
    return { sampleRate: track.sampleRate, channels: track.channels };
  }

  decode(packet) {
    if (this.nextPtsUs === null) this.nextPtsUs = packet.ptsUs;
    let b;
    try { b = this.inner.decode(packet); }
    catch (e) {
      // After a cold seek the first packets can start mid-structure (TrueHD "Parse" error before the next major sync). A packet that
      // does not parse is skipped, the output clock restarts at the next one, and only a long unbroken run of failures is fatal.
      if (++this.badRun > 400) throw e;
      this.badTotal++; this.nextPtsUs = null;
      try { this.inner.flush(); } catch {}   // the engine's TrueHD decoder is 'poisoned' after a parse error until it is reset
      return [];
    }
    this.badRun = 0;
    if (!b.frames) return [];
    const pts = this.nextPtsUs;
    this.nextPtsUs += (b.frames * 1e6) / b.sampleRate;
    this.info = { lossless: b.lossless, joc: b.joc };
    return [{ ptsUs: pts, frames: b.frames, channels: b.channels, sampleRate: b.sampleRate, planar: b.planar, layout: b.layout, lossless: b.lossless, joc: b.joc }];
  }

  flush() { return []; }

  // E-AC-3 JOC: true renders the Atmos objects to 7.1.4, false returns the 5.1/7.1 bed (from the next packet on).
  setJoc(enabled) { this.opts = { ...this.opts, joc: !!enabled }; this.inner?.setJoc(enabled); }

  reset() { this.nextPtsUs = null; this.badRun = 0; this.inner?.flush(); }

  close() { this.inner?.close(); this.inner = null; }
}
