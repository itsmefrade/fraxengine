// SPDX-License-Identifier: LGPL-3.0-only
// Hybrid quality mode test: exact (WASM, web/exact) and hardware (WebCodecs) layers in any combination, composed by FelCompositor.
// window.runHybrid({mode, n, frames, quality, canvasMaxWidth})
//   mode  'exact-bl'  BL exact (WASM pool), EL hardware        (the hybrid quality mode)
//         'exact-el'  BL hardware, EL exact                    (reverse hybrid)
//         'exact-both' both layers WASM                        (fully exact reference)
//         'hw'        both hardware                            (default fast mode)
//   quality: read back the 12 checkpoint frames and compare with the gold composition (slows the run; keep off for fps numbers)
import { FelCompositor } from './fel-compositor.mjs';
import { FelTables } from './fel-tables.mjs';
import { normalizeRpuDisplayMeta } from './fel-meta.mjs';
import { planeStats, manifest, checkpointRpu, fetchBytes, getDevice, pad4, FX } from './fel-test.mjs';
import { ExactStream } from '../exact/stream.js';

const log = s => { const el = document.getElementById('log'); if (el) el.textContent += s + '\n'; console.log(s); };
const W = 3840, H = 2160, EW = 1920, EH = 1080;
const FPS = 24000 / 1001;

/** Hardware decoded layer, pull based, bounded output queue. */
class HwStream {
  constructor({ data, aus, layer, count, maxQueue = 6 }) {
    const keyOf = a => (layer === 'el' ? (a.elKey ?? a.key) : a.key);
    this.q = []; this.errors = []; this.done = false; this.wake = () => {}; this.count = count;
    const cfg = layer === 'bl' ? { codec: 'hvc1.2.4.L153.90', codedWidth: W, codedHeight: H } : { codec: 'hvc1.2.4.L123.90', codedWidth: EW, codedHeight: EH };
    this.dec = new VideoDecoder({ output: f => { this.q.push(f); this.kick(); }, error: e => { this.errors.push(layer + ': ' + e); this.kick(); } });
    this.dec.configure({ ...cfg, hardwareAcceleration: 'prefer-hardware' });
    this.dec.ondequeue = () => this.kick();
    const dur = 1001000 / 24000 * 1000;
    this.producer = (async () => {
      for (let i = 0; i < count; i++) {
        while (this.dec.decodeQueueSize > 6 || this.q.length > maxQueue) { if (this.errors.length || this.closed) return; await this.sleep(); }
        const a = aus[i];
        this.dec.decode(new EncodedVideoChunk({ type: keyOf(a) ? 'key' : 'delta', timestamp: i * dur, data: data.subarray(a[layer][0], a[layer][0] + a[layer][1]) }));
      }
      await this.dec.flush(); this.done = true; this.kick();
    })().catch(e => { this.errors.push(String(e)); this.kick(); });
  }
  kick() { this.wake(); }
  sleep() { return new Promise(r => { this.wake = r; setTimeout(r, 20); }); }
  async next() {
    for (;;) {
      if (this.errors.length) throw new Error(this.errors.join('; '));
      if (this.q.length) { const f = this.q.shift(); this.kick(); return f; }
      if (this.done) return null;
      await this.sleep();
    }
  }
  stop() { this.closed = true; for (const f of this.q) f.close(); this.q = []; try { this.dec.close(); } catch {} }
}

/**
 * Clip description. `fel-p7` is the original 1454 frame fixture (12 gold checkpoints with their own RPUs, web/fixtures).
 * Other names are web/fel/media/<name>-* written by tools/prep-media.mjs and tools/gen-gold.py (FFmpeg gold planes, RPU of every picture).
 */
export async function prepareClip(clip, comp, tablesApi, log) {
  if (clip === 'fel-p7') {
    const points = await manifest();
    const rpus = new Map();
    for (const cp of points) { const r = await checkpointRpu(cp.frameIndex); tablesApi.reset(); rpus.set(cp.frameIndex, { words: tablesApi.tables(r.nal), meta: r.meta }); }
    let current = rpus.get(0);
    return {
      exactFix: { base: new URL('../fixtures/', import.meta.url), prefix: 'fel-p7' }, retagBase: '../fixtures/fel-p7-retag',
      checkpoints: points.map(p => p.frameIndex),
      rpuFor(k) { if (rpus.has(k)) current = rpus.get(k); return current; },
      async gold(k) {
        const [bl, el] = await Promise.all([fetchBytes(`${FX}full-fel-manifest-frame-${pad4(k)}-bl.yuv`), fetchBytes(`${FX}full-fel-manifest-frame-${pad4(k)}-el.yuv`)]);
        return { bl, el, words: rpus.get(k).words };
      },
      info: { tableBuildMsPerRpu: null },
    };
  }
  if (clip === 'p5') {                       // single layer Profile 5: no tables, reshaping LUT from the RPU JSON of the nearest checkpoint
    const [cps, metas] = await Promise.all([fetch('./media/p5-checkpoints.json').then(r => r.json()), fetch('./media/p5-meta.json').then(r => r.json())]);
    const rj = metas[String(cps.frames[0])], meta = normalizeRpuDisplayMeta(rj.vdr_dm_data);
    return {
      exactFix: { base: new URL('./media/', import.meta.url), prefix: 'p5' }, retagBase: './media/p5-retag', checkpoints: cps.frames,
      rpuFor: () => ({ words: null, meta, profile: 'p5', rpuJson: rj }),
      async gold(k) { const i = cps.frames.indexOf(k); return { bl: (await fetchRange('./media/p5-gold-bl.yuv', i * W * H * 3, W * H * 3)), el: null, words: null }; },
      info: { single: true, tableBuildMsPerRpu: null },
    };
  }
  const dir = `./media/${clip}`;
  const [index, rpuBin, cps] = await Promise.all([fetch(`${dir}-index.json`).then(r => r.json()), fetchBytes(`${dir}-rpu.bin`), fetch(`${dir}-checkpoints.json`).then(r => r.json())]);
  const display = index.display;
  const t0 = performance.now();
  // table packets in decode order (mapping history), kept for the display order lookup
  let melTemplate = null;
  tablesApi.reset();
  const words = [];
  let failed = 0, firstErr = null;
  for (let i = 0; i < index.aus.length; i++) {
    const a = index.aus[i];
    try { words.push(tablesApi.tables(rpuBin.subarray(a.rpu[0], a.rpu[0] + a.rpu[1]))); }
    catch (e) { failed++; firstErr ??= String(e.message ?? e); words.push(null); tablesApi.reset(); }
  }
  const buildMs = (performance.now() - t0) / index.aus.length;
  if (failed) {
    // Fallback for RPUs the table build rejects (rust/aether-dovi/src/fel/mapping.rs accepts P7 MEL only when its NLQ is all zero):
    // take the identity tables of the fel-p7 clip (MEL/identity curves) with the residual switched off.
    log(`table build failed for ${failed}/${index.aus.length} RPUs (${firstErr}); using identity MEL template`);
    const cp0 = await checkpointRpu(0); tablesApi.reset();
    melTemplate = tablesApi.tables(cp0.nal); melTemplate[5] = 1;
    for (let i = 0; i < words.length; i++) words[i] ??= melTemplate;
  }
  const rpuFor = k => ({ words: words[display[k]], meta: null });
  return {
    exactFix: { base: new URL('./media/', import.meta.url), prefix: clip }, retagBase: `./media/${clip}-retag`,
    checkpoints: cps.frames, rpuFor,
    async gold(k) {
      const i = cps.frames.indexOf(k);
      const [bl, el] = await Promise.all([fetchRange(`${dir}-gold-bl.yuv`, i * W * H * 3, W * H * 3), fetchRange(`${dir}-gold-el.yuv`, i * EW * EH * 3, EW * EH * 3)]);
      return { bl, el, words: rpuFor(k).words };
    },
    info: { tableBuildMsPerRpu: +buildMs.toFixed(3), tableFailures: failed, firstTableError: firstErr, melTemplate: !!melTemplate,
      mmrFrames: words.filter(w => w && w[6]).length, felFrames: words.filter(w => w && !w[5]).length, rpus: words.length },
  };
}
const wholeFiles = new Map();                        // the dev server ignores Range, so the gold files are fetched once
export async function fetchRange(url, start, len) {
  if (!wholeFiles.has(url)) wholeFiles.set(url, await fetchBytes(url));
  return wholeFiles.get(url).subarray(start, start + len);
}

const agg = vals => { const f = vals.filter(v => v !== null && v !== undefined); const fin = f.map(v => (v === Infinity ? 999 : v)); return { min: Math.min(...fin), max: Math.max(...fin), mean: +(fin.reduce((a, b) => a + b, 0) / fin.length).toFixed(2) }; };

export async function runHybrid(opts = {}) {
  const mode = opts.mode ?? 'exact-bl', n = opts.n ?? 4;
  let exactLayers = { 'exact-bl': ['bl'], 'exact-el': ['el'], 'exact-both': ['bl', 'el'], hw: [] }[mode];
  if (!exactLayers) throw new Error('bad mode ' + mode);
  const quality = !!opts.quality;
  const { adapter, device } = await getDevice();
  const out = { mode, n, quality, userAgent: navigator.userAgent, hardwareConcurrency: navigator.hardwareConcurrency };
  const ai = adapter.info ?? {};
  out.adapter = { vendor: ai.vendor, architecture: ai.architecture, description: ai.description, isFallbackAdapter: adapter.isFallbackAdapter ?? ai.isFallbackAdapter ?? null };
  out.videoDecoderHardware = {};
  for (const [l, c] of [['bl', { codec: 'hvc1.2.4.L153.90', codedWidth: W, codedHeight: H }], ['el', { codec: 'hvc1.2.4.L123.90', codedWidth: EW, codedHeight: EH }]]) {
    const hwS = await VideoDecoder.isConfigSupported({ ...c, hardwareAcceleration: 'prefer-hardware' }).catch(e => ({ supported: false, error: String(e) }));
    const swS = await VideoDecoder.isConfigSupported({ ...c, hardwareAcceleration: 'prefer-software' }).catch(e => ({ supported: false, error: String(e) }));
    out.videoDecoderHardware[l] = { preferHardwareSupported: hwS.supported, preferSoftwareSupported: swS.supported };
  }
  const canvas = document.getElementById('out');
  const comp = await FelCompositor.create(device, canvas.getContext('webgpu'), { canvasColorSpace: opts.canvasColorSpace ?? 'display-p3', canvasMaxWidth: opts.canvasMaxWidth ?? 1920, peakNits: 1000 });
  const tables = await FelTables.create(new URL('../wasm/aether_wasm.wasm', import.meta.url));
  const clip = opts.clip ?? 'fel-p7';
  out.clip = clip;
  const cl = await prepareClip(clip, comp, tables, log);
  out.clipInfo = cl.info;
  const cpSet = new Set(cl.checkpoints);
  // MEL (residual disabled for the whole clip): the EL is never decoded
  const melOnly = !!cl.info.melTemplate || !!cl.info.single;
  const usedLayers = melOnly ? ['bl'] : ['bl', 'el'];
  exactLayers = exactLayers.filter(l => usedLayers.includes(l));
  const hwLayers = usedLayers.filter(l => !exactLayers.includes(l));
  out.melOnly = melOnly;
  // gold: exact planes through COMPOSE (matches the manifest hashes, see the `exact` step of fel-test)
  const gold = new Map();
  if (quality) {
    for (const k of cl.checkpoints) {
      const { bl, el, words } = await cl.gold(k);
      if (cl.info.single) { gold.set(k, { bl: new Uint16Array(bl.buffer, bl.byteOffset, bl.byteLength / 2) }); continue; }
      comp.composeExact(bl, el, words);
      const { raw16 } = await comp.readComposed();
      const vdr = new Uint16Array(raw16.length);
      for (let i = 0; i < raw16.length; i++) vdr[i] = Math.min((raw16[i] + 8) >> 4, 4095);
      gold.set(k, { raw16, vdr, bl: new Uint16Array(bl.buffer, bl.byteOffset, bl.byteLength / 2), el: new Uint16Array(el.buffer, el.byteOffset, el.byteLength / 2) });
    }
  }
  const retagBase = cl.retagBase;
  const index = await fetch(retagBase + '-index.json').then(r => r.json());
  const hwData = {};
  for (const l of hwLayers) hwData[l] = await fetchBytes(`${retagBase}-${l}.hevc`);
  const total = index.aus.length, count = Math.min(opts.frames ?? total, total);
  // exact side: GOP aligned frame count so the pool does not wait for GOPs we never consume
  const exact = exactLayers.length ? new ExactStream({ n, layers: exactLayers, budgetMB: opts.budgetMB ?? 1536, fixtures: cl.exactFix }) : null;
  const ready = performance.now();
  const hw = {};
  const tStart = performance.now();
  if (exact) await exact.start();
  const tExactReady = performance.now();
  for (const l of hwLayers) hw[l] = new HwStream({ data: hwData[l], aus: index.aus, layer: l, count });
  const rows = [], stamps = [], inflight = [], presentMs = [];
  let firstPresent = null, readbackMs = 0, error = null, k = 0, planeBlOk = 0, planeBlBad = 0;
  try {
    for (; k < count; k++) {
      const get = async l => (!usedLayers.includes(l) ? {} : exactLayers.includes(l) ? exact.next(l) : hw[l].next());
      const [b, e] = await Promise.all([get('bl'), get('el')]);
      if (!b || !e) { error = `stream ended at ${k}`; b?.close?.(); e?.close?.(); break; }
      const current = cl.rpuFor(k);
      const t = performance.now();
      comp.present(b, e, current.words, current.meta, { profile: current.profile ?? 'p7', rpuJson: current.rpuJson });
      presentMs.push(performance.now() - t);
      if (firstPresent === null) firstPresent = performance.now();
      if (cpSet.has(k) && quality) {
        const r0 = performance.now();
        await comp.whenIdle();
        if (cl.info.single) {
          await comp.whenIdle();
          const blCodes = await comp.readBaseCodes();
          const bs = planeStats(blCodes, gold.get(k).bl, 1023);
          rows.push({ frame: k, blCodes10: bs, vdr12: { all: { psnrDb: 999, maxAbs: 0 }, Y: {}, U: {}, V: {} }, raw16: { all: {} }, elCodes10: { all: {} } });
          log(`${mode} p5 frame ${k}: BL codes PSNR ${bs.all.psnrDb} max ${bs.all.maxAbs}`);
          readbackMs += performance.now() - r0;
          if (b.close) b.close();
          inflight.push(device.queue.onSubmittedWorkDone()); if (inflight.length > 3) await inflight.shift();
          stamps.push(performance.now()); continue;
        }
        const [{ raw16, vdr }, blCodes, elCodes] = await Promise.all([comp.readComposed(), comp.readBaseCodes(), melOnly ? Promise.resolve(null) : comp.readEnhancementCodes()]);
        const g = gold.get(k);
        const blS = planeStats(blCodes, g.bl, 1023), elPlanes = [['Y', 0, EW * EH], ['U', EW * EH, EW * EH / 4], ['V', EW * EH * 5 / 4, EW * EH / 4]];
        const row = { frame: k, vdr12: planeStats(vdr, g.vdr, 4095), raw16: planeStats(raw16, g.raw16, 65535), blCodes10: blS, elCodes10: melOnly ? { all: { psnrDb: 999, maxAbs: 0 }, Y: {}, U: {}, V: {} } : planeStats(elCodes, g.el, 1023, elPlanes) };
        rows.push(row);
        log(`${mode} frame ${k}: vdr12 PSNR ${row.vdr12.all.psnrDb} max ${row.vdr12.all.maxAbs} | BL codes max ${blS.all.maxAbs} EL codes max ${row.elCodes10.all.maxAbs}`);
        readbackMs += performance.now() - r0;
      }
      if (b.close) b.close();
      if (e.close) e.close();
      inflight.push(device.queue.onSubmittedWorkDone());
      if (inflight.length > 3) await inflight.shift();
      stamps.push(performance.now());
    }
  } catch (err) { error = String(err.stack ?? err); }
  await comp.whenIdle();
  const tEnd = performance.now();
  const peak = exact?.stats ?? {};
  const gaps = stamps.slice(1).map((s, i) => s - stamps[i]).sort((a, b) => a - b);
  const pct = p => gaps.length ? +gaps[Math.min(gaps.length - 1, Math.floor(p * gaps.length))].toFixed(2) : null;
  presentMs.sort((a, b) => a - b);
  // minimum prebuffer for glitch free 23.976 playout of this run: startup delay such that frame i is ready by start + i/FPS
  let need = 0; for (let i = 0; i < stamps.length; i++) need = Math.max(need, stamps[i] - tStart - i * 1000 / FPS);
  const wall = (tEnd - (firstPresent ?? tStart)) - readbackMs;           // steady state interval, first present to last, readback removed
  const pairs = stamps.length;
  out.result = {
    pairs, requested: count, error,
    startupLatencyMs: Math.round((firstPresent ?? tEnd) - tStart),       // stream start (workers spawn, WASM compile, first GOP) to first composed frame
    workerInitMs: peak.workerInitMs ?? null,
    minPrebufferForRealtimeMs: Math.round(need),
    sustainedFps: pairs > 1 ? +((pairs - 1) / (wall / 1000)).toFixed(2) : null,
    overallFps: +(pairs / ((tEnd - tStart - readbackMs) / 1000)).toFixed(2),
    realtimeX: pairs > 1 ? +(((pairs - 1) / (wall / 1000)) / FPS).toFixed(2) : null,
    totalMs: Math.round(tEnd - tStart), readbackMs: Math.round(readbackMs),
    pairIntervalMs: { p50: pct(0.5), p95: pct(0.95), p99: pct(0.99), max: pct(1) },
    over41_7ms: gaps.filter(x => x > 41.7).length,
    presentCpuMs: presentMs.length ? { p50: +presentMs[presentMs.length >> 1].toFixed(2), max: +presentMs[presentMs.length - 1].toFixed(2) } : null,
    uploadCpuMsPerFrame: comp.stats.uploadedFrames ? +(comp.stats.uploadCpuMs / comp.stats.uploadedFrames).toFixed(2) : null,
    uploadMBPerFrame: comp.stats.uploadedFrames ? +(comp.stats.uploadBytes / comp.stats.uploadedFrames / 1048576).toFixed(2) : null,
    exact: exact ? { peakBufferedMB: peak.peakBufferedMB, peakBufferedFrames: peak.peakBufferedFrames, wasmMB: peak.wasmMB, firstFrameMs: peak.firstFrameMs } : null,
    jsHeapMB: performance.memory ? Math.round(performance.memory.usedJSHeapSize / 1048576) : null,
  };
  if (rows.length) {
    out.quality = {
      frames: rows.length,
      vdr12: { psnrDb: agg(rows.map(r => r.vdr12.all.psnrDb)), maxAbs: agg(rows.map(r => r.vdr12.all.maxAbs)), exactAll: rows.every(r => r.vdr12.all.maxAbs === 0) },
      raw16: { psnrDb: agg(rows.map(r => r.raw16.all.psnrDb)), maxAbs: agg(rows.map(r => r.raw16.all.maxAbs)) },
      blCodes10: { psnrDb: agg(rows.map(r => r.blCodes10.all.psnrDb)), maxAbs: agg(rows.map(r => r.blCodes10.all.maxAbs)) },
      elCodes10: { psnrDb: agg(rows.map(r => r.elCodes10.all.psnrDb)), maxAbs: agg(rows.map(r => r.elCodes10.all.maxAbs)) },
      vdr12Planes: { Y: agg(rows.map(r => r.vdr12.Y.psnrDb)), U: agg(rows.map(r => r.vdr12.U.psnrDb)), V: agg(rows.map(r => r.vdr12.V.psnrDb)) },
    };
    out.rows = rows;
  }
  log(`${mode} n=${n}: ` + JSON.stringify(out.result));
  if (out.quality) log('quality ' + JSON.stringify(out.quality));
  exact?.stop(); for (const l of hwLayers) hw[l].stop();
  comp.dispose();
  window.hybridResult = out;
  return out;
}
