// SPDX-License-Identifier: LGPL-3.0-only
// Audio worker: packets -> AudioDecoderPlugin -> channel mapping/downmix -> resampler -> shared ring (read by the AudioWorklet).
// When the AudioContext is not running (autoplay policy, interruption) the ring is consumed here on the wall clock
// (CTL.VIRTUAL), so the shared READ counter stays the one master clock for the video worker and the switch to the real
// worklet is continuous (session.js re-bases READ by the latency delta).
import { wasmBytes, jsHeapBytes } from './wasm-mem.js';
import { CTL, ringViews, yieldNow } from './protocol.js';
import { stage as stageT, stageNow, stages } from './stages.js';
import { createAudioPlugin } from './audio/plugin.js';
import { buildMixMatrix, defaultLabels, applyMix, OUT_LAYOUTS, layoutName } from './audio/mix.js';
import { SincResampler } from './audio/resampler.js';
import { createOutputSync } from '../../engine/output.mjs';
import { loadModule } from '../../engine/aether-wasm.mjs';

const PREBUFFER_S = 0.4;       // frames in the ring before this epoch reports "ready"
const TARGET_FILL_S = 2.5;     // keep the ring about this full; decoding pauses above it
// Sound that goes into the element's SourceBuffer is not a ring: after an underflow Chrome asks for more than 2.5 s of audio before
// it resumes, so a 2.5 s horizon left the element waiting on a pipeline that was already full. The SourceBuffer's own target
// (mse-worker AHEAD_US) holds the credit back; this is only the decode-ahead bound.
const MSE_FILL_S = 12;

let epoch = 0, closed = false;
let track = null, port = null, outChannels = 2, sampleRate = 48000, outLabels = OUT_LAYOUTS[2];
let plugin = null;
let ring = null, rv = null;
let inq = [], early = [], eof = false, flushed = false, targetUs = 0, ackHeld = 0;
let outq = [], outFrames = 0;      // converted chunks waiting for ring space {planes, n, off}
let baseUs = null, readySent = false, running = false;
let inflightFrames = 0;     // submitted to the decoder, output not yet seen (estimated from packet durations)
let decodedFrames = 0, droppedPreroll = 0, packetsIn = 0;
let route = '', routeInfo = null;
let jocOpt = true;           // E-AC-3 JOC: render the Atmos objects to 7.1.4 (false: the 5.1/7.1 bed)
// Downmix headroom: a fold-down can peak above full scale (7.1 -> stereo +9.9 dB aligned). Nothing is changed silently: normalize
// (null = auto: on for a downmix, off for identity) scales the whole output by 1 / peakGain, limiter is the engine's own and off by default.
let normOpt = null, limiterOpt = false;
let mixKey = '', mix = null, resampler = null, stage = null, outMode = 'speakers', wasmOk = false;
let virtLast = 0, virtCarry = 0, virtActive = false;
// MSE output (session.js plays audio and video through one MediaSource): blocks go to the MSE worker over a port instead of into
// the shared ring. A local control array stands in for the ring's: WRITE counts frames sent, READ follows the element's position,
// so the fill, ready and EOF logic below is the same on both paths.
let mseOut = null;

const post = (m, t) => self.postMessage(m, t || []);

// Output stage: the engine's OutputPlan (web/engine/output.mjs: bit-exact identity, speaker fold-down, KEMAR binaural)
// with the player's own label matrix (audio/mix.js) as the fallback if the WASM output cannot be created.
function buildStage(labels) {
  if (stage?.out) stage.out.close();
  let out = null, why = '';
  if (wasmOk) {
    try { out = createOutputSync({ sourceLayout: labels, deviceChannels: outChannels, mode: outMode, sampleRate, limiter: limiterOpt }); }
    catch (e) { why = String(e && e.message || e); }
  } else why = 'engine WASM unavailable';
  if (out) {
    // Output planes follow out.outLayout; the ring wants the Web Audio speaker order of OUT_LAYOUTS.
    const remap = buildMixMatrix(out.outLayout, outLabels);
    return { out, remap: remap.identity ? null : remap, kind: out.kind, description: out.description, peakGain: out.peakGain, engine: 'engine OutputPlan' };
  }
  const m = buildMixMatrix(labels, outLabels);
  return { out: null, remap: m.identity ? null : m, kind: m.identity ? 'identity' : 'mix', description: `${m.name} (fallback matrix: ${why})`, peakGain: m.peakGain, engine: 'player matrix fallback', mix: m };
}

function runStage(planes, n) {
  const res = outLabels.map(() => new Float32Array(n));
  const MAX = 4096;
  for (let o = 0; o < n; o += MAX) {
    const m = Math.min(MAX, n - o);
    const slice = planes.map(p => p.subarray(o, o + m));
    const rendered = stage.out ? stage.out.process(slice) : slice;
    const planesOut = stage.remap ? applyMix(stage.remap.gains, stage.out ? rendered : slice, m, outLabels.map(() => new Float32Array(m))) : rendered;
    for (let c = 0; c < outLabels.length && c < planesOut.length; c++) res[c].set(planesOut[c].subarray(0, m), o);
  }
  const g = stage.normGain;
  if (g !== 1) for (const p of res) for (let i = 0; i < n; i++) p[i] *= g;
  return res;
}

function prepare(ch) {
  const labels = ch.layout && ch.layout.length === ch.channels ? ch.layout : defaultLabels(ch.channels);
  const key = labels.join(',') + '>' + outLabels.join(',') + '>' + outMode + '>' + limiterOpt;
  if (key !== mixKey) { mixKey = key; mix = { srcLabels: labels }; stage = buildStage(labels); }
  { const pk = stage.peakGain || 1, eff = normOpt === null ? stage.kind !== 'identity' : normOpt;
    stage.normGain = eff && pk > 1 ? 1 / pk : 1; stage.normEffective = eff; }
  if (!resampler || resampler.inRate !== ch.sampleRate || resampler.channels !== ch.channels) {
    resampler = new SincResampler(ch.sampleRate, sampleRate, ch.channels);
  }
}

const limTot = { frames: 0, maxReductionDb: 0 };
function limiterTotals() {
  try { const s = stage.out.limiterStats(); if (s) { limTot.frames += s.frames ?? s.reducedFrames ?? 0; limTot.maxReductionDb = Math.max(limTot.maxReductionDb, Math.abs(s.deepestDb ?? 0)); } } catch {}
  return { reducedFrames: limTot.frames, deepestReductionDb: limTot.maxReductionDb };
}

function describe(ch) {
  routeInfo = {
    srcCodec: track.codec, srcChannels: ch.channels, srcLayout: mix.srcLabels.join(' '), srcRate: ch.sampleRate,
    lossless: !!ch.lossless, joc: ch.joc || false, jocSetting: jocOpt, decoder: plugin.pluginName === 'rust-engine' ? 'engine Rust/WASM' : 'WebCodecs',
    outChannels, outLayout: outLabels.join(' '), outRate: sampleRate, mode: outMode, kind: stage.kind, renderer: stage.engine,
    downmix: stage.description, peakGain: +(stage.peakGain || 1).toFixed(3), peakGainDb: +(20 * Math.log10(stage.peakGain || 1)).toFixed(1),
    normalize: { setting: normOpt === null ? 'auto' : normOpt ? 'on' : 'off', active: stage.normGain !== 1, gainDb: +(20 * Math.log10(stage.normGain)).toFixed(2) }, limiter: { enabled: limiterOpt, ...(stage.out && limiterOpt ? limiterTotals() : {}) },
    resample: ch.sampleRate === sampleRate ? 'none' : `windowed-sinc polyphase ${ch.sampleRate}->${sampleRate} (${resampler.L}/${resampler.M}, ${resampler.taps} taps)`,
  };
  route = `${track.codec} ${ch.channels}ch ${layoutName(mix.srcLabels)} ${ch.sampleRate} Hz -> ${outChannels}ch [${outMode}, ${stage.kind}] ${stage.description}; ${routeInfo.resample === 'none' ? 'no resampling' : routeInfo.resample}`;
  post({ type: 'route', route, info: routeInfo });
}

function convert(ch, skip) {
  prepare(ch);
  if (!route || routeInfo.srcChannels !== ch.channels || routeInfo.srcRate !== ch.sampleRate || routeInfo.srcLayout !== mix.srcLabels.join(' ') || routeInfo.mode !== outMode || routeInfo.jocSetting !== jocOpt || routeInfo.joc?.objects !== ch.joc?.objects) describe(ch);
  const n = ch.frames - skip;
  const src = skip ? ch.planar.map(p => p.subarray(skip)) : ch.planar;
  const r = resampler.process(src, n);
  if (!r.frames) return { planes: [], n: 0 };
  return { planes: runStage(r.planes, r.frames), n: r.frames };
}

function ingest(chunks) {
  for (const ch of chunks) {
    decodedFrames += ch.frames;
    inflightFrames = Math.max(0, inflightFrames - Math.round((ch.frames * sampleRate) / ch.sampleRate));
    let skip = 0;
    const endUs = ch.ptsUs + (ch.frames * 1e6) / ch.sampleRate;
    if (endUs <= targetUs) { droppedPreroll += ch.frames; continue; }
    if (ch.ptsUs < targetUs) skip = Math.min(ch.frames - 1, Math.round(((targetUs - ch.ptsUs) * ch.sampleRate) / 1e6));
    if (baseUs === null) baseUs = ch.ptsUs + (skip * 1e6) / ch.sampleRate;
    const tConv = stageNow();
    const c = convert(ch, skip);
    stageT('audio.convert', tConv);
    if (c.n > 0) { outq.push({ planes: c.planes, n: c.n, off: 0 }); outFrames += c.n; }
  }
}

function ringFill() { return (Atomics.load(rv.ctl, CTL.WRITE) - Atomics.load(rv.ctl, CTL.READ)) | 0; }

function writeMse() {
  const ctl = rv.ctl;
  while (outq.length) {
    const o = outq.shift();
    const w = Atomics.load(ctl, CTL.WRITE);
    let planes = o.planes.map(p => (o.off === 0 && p.length === o.n ? p : p.slice(o.off, o.off + o.n)));
    // FLAC's 7.1 order is FL FR FC LFE BL BR SL SR; the output layout is the Web Audio order (sides before backs).
    if (planes.length === 8 && outLabels[4] === 'SL' && outLabels[6] === 'BL') planes = [planes[0], planes[1], planes[2], planes[3], planes[6], planes[7], planes[4], planes[5]];
    mseOut.port.postMessage({ type: 'pcm', epoch, ptsUs: baseUs + (w * 1e6) / sampleRate, frames: o.n, sampleRate, planes }, planes.map(p => p.buffer));
    Atomics.store(ctl, CTL.WRITE, (w + o.n) | 0);
    outFrames -= o.n;
  }
}

function markEof() {
  if (Atomics.load(rv.ctl, CTL.EOF)) return;
  Atomics.store(rv.ctl, CTL.EOF, 1);
  if (mseOut) mseOut.port.postMessage({ type: 'pcm-eof', epoch });
}

function writeRing() {
  if (!rv) return;
  if (mseOut) { writeMse(); return; }
  const ctl = rv.ctl, cap = rv.cap;
  while (outq.length) {
    const w = Atomics.load(ctl, CTL.WRITE);
    const free = cap - ((w - Atomics.load(ctl, CTL.READ)) | 0);
    if (free <= 0) return;
    const o = outq[0];
    const n = Math.min(free, o.n);
    let start = w % cap; if (start < 0) start += cap;
    const first = Math.min(n, cap - start);
    for (let c = 0; c < rv.planes.length; c++) {
      const dst = rv.planes[c], src = o.planes[c];
      dst.set(src.subarray(o.off, o.off + first), start);
      if (n > first) dst.set(src.subarray(o.off + first, o.off + n), 0);
    }
    Atomics.store(ctl, CTL.WRITE, (w + n) | 0);
    o.off += n; o.n -= n; outFrames -= n;
    if (o.n === 0) outq.shift();
  }
}

// Wall-clock consumption while the AudioContext is not running. Advances READ by elapsed real time (never past WRITE);
// READ stays the single master clock, so nothing else changes when the real worklet takes over.
function virtualConsume() {
  if (!rv) return;
  const ctl = rv.ctl;
  if (Atomics.load(ctl, CTL.VIRTUAL) !== 1) { virtActive = false; return; }
  const now = performance.now();
  if (!virtActive) { virtActive = true; virtLast = now; virtCarry = 0; return; }
  virtCarry += ((now - virtLast) * sampleRate) / 1000;
  virtLast = now;
  const want = Math.floor(virtCarry);
  const avail = ringFill();
  const take = Math.max(0, Math.min(want, avail));
  if (take > 0) Atomics.add(ctl, CTL.READ, take);
  virtCarry = take < want ? 0 : virtCarry - take;     // data starvation: do not bank the missed time
  if (avail - take <= 0 && Atomics.load(ctl, CTL.EOF) === 1 && Atomics.load(ctl, CTL.DRAINED) !== 1) {
    Atomics.store(ctl, CTL.DRAINED, 1);
    post({ type: 'drained', epoch });
  }
}

async function pump() {
  if (running || closed || !plugin || !rv) return;
  running = true;
  const my = epoch;
  try {
    for (;;) {
      if (my !== epoch || closed) return;
      virtualConsume();
      if (plugin.poll) ingest(plugin.poll());
      writeRing();
      const buffered = ringFill() + outFrames + inflightFrames;
      if (inq.length && buffered < (mseOut ? MSE_FILL_S : TARGET_FILL_S) * sampleRate && outFrames + inflightFrames < 0.5 * sampleRate) {
        const p = inq.shift();
        if (++ackHeld >= 16 || !inq.length) { port.postMessage({ type: 'ack', epoch: my, n: ackHeld }); ackHeld = 0; }
        inflightFrames += Math.round((p.durationUs * sampleRate) / 1e6) || 1024;
        const tDec = stageNow();
        const res = await plugin.decode(p);
        stageT('audio.decode', tDec, p.data ? p.data.byteLength : 0);
        if (my !== epoch || closed) return;
        ingest(res);
        // A synchronous plugin (the engine decoders) returns all its output from decode(): nothing is still in flight. Without this, a
        // decoder that swallows packets until its next sync point (TrueHD after a cold seek) left the estimate above the cap and stalled the seek.
        if (!plugin.poll) inflightFrames = 0;
        continue;
      }
      if (eof && !inq.length && !flushed) {
        flushed = true;
        const res = await plugin.flush();
        if (my !== epoch || closed) return;
        ingest(res);
        continue;
      }
      if (flushed && !outq.length) markEof();
      break;
    }
    checkReady();
  } catch (e) {
    post({ type: 'error', where: 'audio', message: String(e && e.message || e) });
    closed = true;
  } finally {
    running = false;
  }
}

function checkReady() {
  if (readySent || !rv) return;
  const have = ringFill() + outFrames;
  if (have >= PREBUFFER_S * sampleRate || (flushed && baseUs !== null)) {
    readySent = true;
    post({ type: 'ready', epoch, baseUs: baseUs ?? targetUs });
  } else if (flushed && baseUs === null) {
    readySent = true;     // nothing audible after the seek point (seek past the end)
    post({ type: 'ready', epoch, baseUs: targetUs });
    markEof();
  }
}

function resetState(newEpoch, r, target) {
  epoch = newEpoch;
  inq = inq.filter(() => false);
  outq = []; outFrames = 0; inflightFrames = 0; ackHeld = 0;
  eof = false; flushed = false; baseUs = null; readySent = false;
  targetUs = target; decodedFrames = 0; droppedPreroll = 0;
  virtActive = false;
  ring = r; rv = mseOut ? { ctl: new Int32Array(CTL.SIZE), cap: Infinity, planes: [] } : ringViews(r);
  plugin?.reset();
  resampler?.reset();
  stage?.out?.reset();
  const keep = early.filter(m => m.epoch === epoch);
  early = [];
  for (const m of keep) accept(m);
}

function accept(m) {
  if (m.type === 'pkts') { for (const x of m.list) accept({ type: 'pkt', epoch: m.epoch, trackId: m.trackId, ptsUs: x.ptsUs, dtsUs: x.dtsUs, durationUs: x.durationUs, key: x.key, data: x.data }); return; }
  if (m.type === 'pkt') {
    if (m.epoch < epoch) return;
    if (m.epoch > epoch) { early.push(m); return; }
    packetsIn++;
    inq.push({ trackId: m.trackId, ptsUs: m.ptsUs, dtsUs: m.dtsUs, durationUs: m.durationUs, key: m.key, data: new Uint8Array(m.data) });
  } else if (m.type === 'eof') {
    if (m.epoch === epoch) eof = true; else if (m.epoch > epoch) early.push(m);
  }
}

// The ring holds 4 s and is kept about 2.5 s full, so a 40 ms poll has seconds of margin; packet arrival also pumps. An 8 ms poll
// cost a worker wake-up 125 times a second for nothing.
setInterval(() => { if (!closed) pump(); }, 40);

self.onmessage = async e => {
  const m = e.data;
  try {
    if (m.type === 'init') {
      track = m.track; outChannels = m.outChannels; sampleRate = m.sampleRate; epoch = m.epoch; port = m.port;
      outLabels = OUT_LAYOUTS[outChannels] || defaultLabels(outChannels);
      outMode = m.mode === 'headphones' ? 'headphones' : 'speakers';
      try { await loadModule(); wasmOk = true; } catch { wasmOk = false; }
      jocOpt = m.joc !== false;
      plugin = createAudioPlugin(track, { dtsx: !!m.dtsx, joc: jocOpt });
      const fmt = await plugin.configure(track);
      port.onmessage = ev => { accept(ev.data); pump(); };
      if (m.msePort) {
        mseOut = { port: m.msePort };
        mseOut.port.onmessage = ev => {
          const x = ev.data;
          if (x.type === 'position' && rv && baseUs !== null && x.epoch === epoch) {
            const read = Math.round(((x.us - baseUs) * sampleRate) / 1e6);
            Atomics.store(rv.ctl, CTL.READ, Math.max(0, Math.min(read, Atomics.load(rv.ctl, CTL.WRITE))) | 0);
            pump();
          }
        };
      }
      post({ type: 'inited', plugin: plugin.pluginName, codec: track.codec, srcChannels: fmt.channels, outChannels, sampleRate: fmt.sampleRate, ringRate: sampleRate });
    } else if (m.type === 'mixopts') {
      normOpt = m.normalize === null || m.normalize === undefined ? null : !!m.normalize;
      limiterOpt = !!m.limiter;
      if (stage && stage.out) { /* limiter flag changes the key, normalisation is applied on the next prepare */ }
      mixKey = '';   // rebuilt (and the route re-described) on the next decoded block
    } else if (m.type === 'joc') {
      jocOpt = !!m.enabled;
      plugin?.setJoc?.(jocOpt);   // the layout of the next decoded blocks changes (12 channels or the bed); the output stage is rebuilt then
    } else if (m.type === 'mode') {
      outMode = m.mode === 'headphones' ? 'headphones' : 'speakers';   // rebuilt on the next decoded block
    } else if (m.type === 'seek') {
      resetState(m.epoch, m.ring, m.targetUs);
      pump();
    } else if (m.type === 'close') {
      closed = true;
      plugin?.close(); plugin = null; stage?.out?.close(); stage = null;
      outq = []; inq = []; early = [];
      port?.close();
      post({ type: 'closed' });
    }
  } catch (err) {
    post({ type: m.type === 'init' ? 'init-failed' : 'error', where: 'audio', message: String(err && err.message || err) });
  }
};

setInterval(() => {
  if (closed || !rv) return;
  post({
    type: 'stats', epoch, heap: jsHeapBytes(), wasm: wasmBytes(), limiter: limiterOpt && stage?.out ? limiterTotals() : null,
    ringMs: Math.round((ringFill() * 1000) / sampleRate), outqMs: Math.round((outFrames * 1000) / sampleRate),
    inq: inq.length, underruns: Atomics.load(rv.ctl, CTL.UNDERRUNS), decodedFrames, droppedPreroll, route, info: routeInfo,
    virtual: Atomics.load(rv.ctl, CTL.VIRTUAL) === 1, stages: stages(),
  });
}, 250);
