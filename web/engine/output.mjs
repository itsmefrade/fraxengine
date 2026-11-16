// SPDX-License-Identifier: LGPL-3.0-only
// Output renderer stage (milestone M3): decoded planar audio -> the best signal for the device.
//
//   const out = await createOutput({ sourceLayout, deviceChannels, mode: 'speakers' | 'headphones' });
//   const planarOut = out.process(planarIn);   // Float32Array[] -> Float32Array[]
//   out.outLayout, out.description, out.kind   // what is actually done, for the UI
//
// Runs in the audio worker before the PCM ring (block size 1024 at 48 kHz, 7.1.4 -> binaural costs
// about a tenth of a millisecond in WASM, see test/output.test.mjs). The rendering is in
// rust/aether-audio/src/output (speaker matrices, KEMAR HRTF convolution, optional limiter).
//
// Kinds: 'identity' (the planes pass through, bit-exact: whenever the device has at least as many
// channels as the source), 'mix' (speaker fold-down to the largest standard layout the device
// carries: 7.1.4 / 7.1.2 / 5.1.4 -> 7.1 -> 5.1 -> 2.0 -> 1.0) and 'binaural' (headphones).
// Nothing changes the level silently: the limiter is off unless `limiter: true`, and `description`
// names the layouts, folds, LFE handling and the worst-case peak of a downmix.
//
// `sourceLayout` is the `layout` of a decoded block (canonical names, see rust/README.md). Output
// planes follow `outLayout`; for 'identity' that is the source order unchanged, for 'mix' and
// 'binaural' the fixed order of the target (FL FR [FC LFE SL SR [BL BR]] [heights]), mapping them to
// device channel positions is the caller's job. The device sample rate must be passed as
// `sampleRate` (the HRTF is resampled to it).

import { AetherWasm, AetherError, loadModule, moduleSync } from './aether-wasm.mjs';

export const MODES = { speakers: 0, headphones: 1 };
export const DEFAULT_MAX_FRAMES = 4096;

function checkMode(mode) {
  if (typeof mode === 'number' && (mode === 0 || mode === 1)) return mode;
  if (mode in MODES) return MODES[mode];
  throw new TypeError(`output mode must be 'speakers' or 'headphones', got ${String(mode)}`);
}

/**
 * Synchronous variant; the module must be loaded (`await loadModule()` or a previous `createOutput`),
 * or `wasm` (an existing `AetherWasm`) passed.
 * @param {object} o
 * @param {string[]} o.sourceLayout canonical channel labels of the input planes
 * @param {number} o.deviceChannels `AudioContext.destination.maxChannelCount` (or what the sink takes)
 * @param {'speakers'|'headphones'} [o.mode='speakers']
 * @param {number} [o.sampleRate=48000]
 * @param {boolean} [o.limiter=false] linked peak limiter (-0.18 dBFS), off unless asked for
 * @param {boolean} [o.virtualizeStereo=false] headphones: also run plain stereo through the HRTF
 * @param {number} [o.lfeGain] LFE level folded into a stereo/mono mix (default: dropped for speakers, 0.5 for headphones)
 * @param {number} [o.maxFrames=4096] largest frame count of one process() call
 * @param {AetherWasm} [o.wasm]
 */
export function createOutputSync({
  sourceLayout, deviceChannels, mode = 'speakers', sampleRate = 48000, limiter = false,
  virtualizeStereo = false, lfeGain, maxFrames = DEFAULT_MAX_FRAMES, wasm,
}) {
  if (!Array.isArray(sourceLayout) || !sourceLayout.length) throw new TypeError('sourceLayout must be a non-empty array of channel labels');
  const w = wasm ?? new AetherWasm(moduleSync());
  const x = w.x;
  const labels = new TextEncoder().encode(sourceLayout.join(','));
  const flags = (limiter ? 1 : 0) | (virtualizeStereo ? 2 : 0);
  const handle = w.withBytes(labels, (p, l) => x.aether_output_create(
    p, l, deviceChannels, checkMode(mode), sampleRate, flags, lfeGain ?? -1, maxFrames));
  if (handle < 0) throw new AetherError('output create failed', handle, 'output_create', w.lastError());
  if (x.aether_output_info(handle) < 0) throw new AetherError('output info failed', -1, 'output_info', w.lastError());
  const info = JSON.parse(w.resultText());
  const nIn = info.inChannels, nOut = info.outChannels, max = info.maxFrames;
  const inPtr = x.aether_output_input_ptr(handle);
  const outPtr = x.aether_output_output_ptr(handle);
  const passthrough = info.kind === 'identity' && !info.limiter;

  let cache = { buffer: null, n: -1, in32: null, views: null };
  const refresh = n => {
    const buffer = x.memory.buffer;
    if (cache.buffer !== buffer) {
      cache = { buffer, n: -1, in32: new Float32Array(buffer, inPtr, nIn * max), views: null };
    }
    if (cache.n !== n) {
      cache.views = Array.from({ length: nOut }, (_, o) => new Float32Array(buffer, outPtr + o * max * 4, n));
      cache.n = n;
    }
    return cache;
  };

  let closed = false;
  const self = {
    kind: info.kind,
    /** Labels of the output planes. */
    outLayout: info.outLayout,
    inLayout: info.inLayout,
    inChannels: nIn,
    outChannels: nOut,
    /** Human readable statement of what is done to the signal. */
    description: info.description,
    /** Worst-case level of a full-scale aligned input (1 = cannot exceed full scale). */
    peakGain: info.peakGain,
    maxFrames: max,
    sampleRate: info.sampleRate,
    info,

    /**
     * Renders one block. `planarIn` has one Float32Array per source channel, all the same length
     * (at most `maxFrames`). Returns one Float32Array per output channel of the same length.
     * The result is a view into WASM memory (identity without limiter: the input arrays
     * themselves), valid until the next call: copy it, or write it to the PCM ring, before then.
     */
    process(planarIn) {
      if (closed) throw new Error('output is closed');
      if (planarIn.length !== nIn) throw new RangeError(`expected ${nIn} planes, got ${planarIn.length}`);
      const n = planarIn[0].length;
      if (n > max) throw new RangeError(`${n} frames exceed maxFrames ${max}`);
      for (let c = 1; c < nIn; c++) if (planarIn[c].length !== n) throw new RangeError('planes differ in length');
      if (passthrough) return planarIn;
      const st = refresh(n);
      for (let c = 0; c < nIn; c++) st.in32.set(planarIn[c], c * max);
      const s = x.aether_output_process(handle, n);
      if (s < 0) throw new AetherError('output process failed', s, 'output_process');
      return refresh(n).views;
    },

    /** Forget filter tails and limiter state (after a seek). */
    reset() { x.aether_output_reset(handle); },

    /** Frames the limiter reduced and the deepest reduction in dB since the last call. */
    limiterStats() {
      x.aether_output_limiter_stats(handle);
      return JSON.parse(w.resultText());
    },

    close() {
      if (!closed) { closed = true; x.aether_output_close(handle); }
    },
  };
  return self;
}

/** Loads the WASM module if needed, then `createOutputSync`. */
export async function createOutput(options) {
  if (!options.wasm) await loadModule();
  return createOutputSync(options);
}
