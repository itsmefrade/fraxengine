// SPDX-License-Identifier: LGPL-3.0-only
// Video worker: packets -> WebCodecs VideoDecoder -> bounded frame queue -> WebGPU (OffscreenCanvas).
// Frames are presented against the master clock: the audio clock (shared ring READ counter) or, with no audio,
// performance.now(). Late frames are dropped and counted. Every VideoFrame is close()d exactly once.
import { wasmBytes, jsHeapBytes } from './wasm-mem.js';
import { CTL, audioClockUs, percentile } from './protocol.js';
import { DvPath, EXACT_AHEAD } from './dv-path.js';
import { h264SpsFacts } from './demux/annexb.js';
import { initEngine, relabelForChrome, P5_CARRIER } from '../../engine/demuxer.mjs';
import { isVc1Track } from '../../engine/vc1.mjs';
import { Vc1Path } from './vc1-path.js';
import { stage, stageNow, stages } from './stages.js';

const QMAX = 10;            // frames held (queue + inside the decoder) before decode submission pauses
const READY_FRAMES = 4;     // queued frames before this epoch reports "ready"
const SHADER = `
struct VOut { @builtin(position) pos: vec4f, @location(0) uv: vec2f };
@vertex fn vs(@builtin(vertex_index) i: u32) -> VOut {
  var p = array(vec2f(-1.0, -1.0), vec2f(3.0, -1.0), vec2f(-1.0, 3.0));
  var o: VOut;
  o.pos = vec4f(p[i], 0.0, 1.0);
  o.uv = vec2f((p[i].x + 1.0) * 0.5, (1.0 - p[i].y) * 0.5);
  return o;
}
@group(0) @binding(0) var smp: sampler;
@group(0) @binding(1) var tex: texture_external;
@fragment fn fs(i: VOut) -> @location(0) vec4f { return textureSampleBaseClampToEdge(tex, smp, i.uv); }`;

let epoch = 0, closed = false, decGen = 0;
let dvSleep = true, nodraw = false, presentVideo = false, useVideo = null, presentLeadUs = 0, forceSdr = false, maxW = null, hdrOpts = {};
let track = null, port = null, canvas = null, displayHdr = false, probeRec2100 = false;
let device = null, gctx = null, pipeline = null, sampler = null, pipelineFormat = null;
let canvasConfigured = false, extCS = 'srgb';
let decoder = null, decCfg = null, decoderInfo = {};
let inq = [], early = [], q = [], lastPkt = null;
// Packets submitted since the last random access point. A hardware decoder that fails mid-GOP (VideoToolbox refuses POCAWE at
// packets Chrome's software decoder takes without error, see docs) is replaced by the software decoder and fed this GOP again, so
// the picture continues without waiting for the next IDR. Capped; beyond the cap decoding restarts at the next IRAP.
const GOP_CAP_BYTES = 128 << 20;
let gop = [], gopBytes = 0, gopOverflow = false, maxOutPts = -Infinity, swFallback = null;
let epochPackets = 0, needKey = true, recoveries = 0, skippedNoKey = 0, packetsTotal = 0, dedupeBelow = -Infinity;
let eof = false, flushing = false, flushed = false, endedSent = false, readySent = false;
let targetUs = 0, submitted = 0, outputs = 0;
let clk = { hasAudio: false, ctl: null, baseUs: 0, sampleRate: 48000, wallBase: 0, wallT0: 0, drainT0: 0, drainClock: 0 };
let playing = false;
let dv = null, dvQuality = 'native', dvDisplay = null;   // compositor path (dv-path.js) or the VC-1 software path (vc1-path.js), same interface; null for the native path
// counters
let created = 0, closedN = 0, shown = 0, dropped = 0, preroll = 0, decodeErrors = 0, vsyncMs = 0;
let lastDecodeError = null;   // text of the last decoder error, in the stats: a recovered error left no trace but a count
const hist = new Uint32Array(2001); let histN = 0;   // |A/V offset| in 1 ms bins, cumulative for the session
let offsets = [];       // signed ms (clock - pts) at presentation, last 600
let presentTimes = [];  // performance.now of recent presentations
let hdrInfo = { source: 'unknown', canvas: 'not configured', probe: {} };
let lastFrameInfo = '';
// Per-epoch presentation record (acceptance M5): which frames reached the screen after a seek. `below` counts frames shown with a
// timestamp before the seek target (a frame from before the seek leaking through), `nonMono` frames shown out of order.
let epochT0 = performance.now(), closedExtra = {}, recoverPending = false, latestSeek = 0, seekChain = Promise.resolve();
let er = null, corruptFrac = 0, corrupted = 0;
function newEr() { return { epoch, targetUs, n: 0, first: null, last: null, below: 0, nonMono: 0, dtMin: Infinity, maxAbsMs: 0, droppedAtStart: dropped }; }
function markPresented(f, offsetMs) {
  if (!er) er = newEr();
  const ts = f.timestamp;
  if (er.first === null) er.first = ts;
  if (ts < er.targetUs - 1000) er.below++;
  if (er.last !== null) { if (ts <= er.last) er.nonMono++; else er.dtMin = Math.min(er.dtMin, ts - er.last); }
  er.last = ts; er.n++;
  if (offsetMs != null) er.maxAbsMs = Math.max(er.maxAbsMs, Math.abs(offsetMs));
}
let gpuInfo = '';

// Runs as a module worker, or on the page's thread on WebKit with a host object as its scope (main-video-host.js).
const hostId = new URL(import.meta.url).searchParams.get('mainhost');
const scope = hostId ? globalThis.__fraxVideoHosts.get(hostId) : self;
const post = (m, t) => scope.postMessage(m, t || []);
const log = s => post({ type: 'log', text: s });
const mark = name => post({ type: 'mark', name, at: performance.timeOrigin + performance.now() });
mark('boot');
let markedOutput = false, markedDraw = false, markedPkt = false;

function histP(p) { if (!histN) return 0; let acc = 0; const want = p * histN; for (let i = 0; i < hist.length; i++) { acc += hist[i]; if (acc >= want) return i; } return 2000; }

// Random access point in a length-prefixed access unit. WebCodecs rejects a chunk marked 'key' that is not an IDR (H.264) or
// IRAP (HEVC), and Matroska keyframe flags also mark recovery-point I pictures, so the first chunk after configure/seek is
// searched for a real one and everything before it is dropped.
function isRandomAccess(data) {
  const d = track.description;
  const hevc = /^(hvc1|hev1|dvh1|dvhe)/.test(track.codec);
  if (!hevc && !/^avc/.test(track.codec)) return true;
  const ls = d && d.length > 21 && hevc ? (d[21] & 3) + 1 : d && d.length > 4 && !hevc ? (d[4] & 3) + 1 : 4;
  let i = 0;
  while (i + ls < data.length) {
    let len = 0; for (let k = 0; k < ls; k++) len = len * 256 + data[i + k];
    const h = data[i + ls];
    if (hevc ? (((h >> 1) & 63) >= 16 && ((h >> 1) & 63) <= 23) : (h & 31) === 5) return true;
    i += ls + len;
  }
  return false;
}

// Pictures a DV hardware decoder lost to a decoder error (dv-path.js #recover) count as dropped.
function dvDropped() { return dv?.counts?.recDropped || 0; }
function release(frame) { if (frame.dv) frame.release(); else { frame.close(); closedN++; } }

// ---------------------------------------------------------------- clock
// The audio ring READ counter advances in bursts (one device buffer, 3 to 20 ms), so the raw audio clock jitters by that much. At 60 fps
// (16.7 ms per frame) the presenter then saw two frames due in one tick about every fifth frame and dropped one (iPhone 4K60 MOV with
// audio: 22 % dropped, same file without audio clock 0). The raw reading never runs ahead of the true clock, so wall time plus the upper
// envelope of (raw - wall) is a steady estimate; a frozen counter (stall, pause) or a jump (seek, latency rebase) restarts the envelope.
let envOff = null, envW = 0, rawLast = 0, rawLastW = 0;
function smoothAudioClockUs() {
  const raw = audioClockUs(clk.ctl, clk.baseUs, clk.sampleRate), w = performance.now() * 1000, d = raw - w;
  if (raw !== rawLast) { rawLast = raw; rawLastW = w; }
  if (envOff === null || d > envOff || w - rawLastW > 40000 || envOff - d > 100000) envOff = d;
  else envOff -= 0.002 * (w - envW);
  envW = w;
  return w + envOff;
}
function nowUs() {
  if (clk.hasAudio) {
    if (Atomics.load(clk.ctl, CTL.DRAINED) === 1) {
      // audio ran out before the video did: keep the clock moving by wall time so the tail frames show
      if (!clk.drainT0) { clk.drainT0 = performance.now(); clk.drainClock = audioClockUs(clk.ctl, clk.baseUs, clk.sampleRate); }
      return clk.drainClock + (performance.now() - clk.drainT0) * 1000;
    }
    return smoothAudioClockUs();
  }
  return playing ? clk.wallBase + (performance.now() - clk.wallT0) * 1000 : clk.wallBase;
}

// ---------------------------------------------------------------- GPU
// WebKit's stack does not begin with the message, so the text of an error there was a list of frames only.
const errText = e => !e?.stack ? String(e) : e.stack.includes(e.message) ? e.stack : `${e.message}\n${e.stack}`;
let preGpu = null;   // a device requested while the file's header is still being read (the limits are the compositor's, the larger set)
async function requestGpu(limits) {
  if (!navigator.gpu) throw new Error('WebGPU unavailable in worker');
  const adapter = await navigator.gpu.requestAdapter({ powerPreference: 'high-performance' });
  if (!adapter) throw new Error('no WebGPU adapter');
  const i = adapter.info || {};
  const dev = await adapter.requestDevice(limits ? { requiredLimits: Object.fromEntries(['maxStorageBufferBindingSize', 'maxBufferSize'].map(k => [k, Math.min(adapter.limits[k], 1 << 30)])) } : undefined);
  return { dev, info: [i.vendor, i.architecture, i.description].filter(Boolean).join(' ') };
}
async function initGpu(limits) {
  const g = (await preGpu) || await requestGpu(limits);
  preGpu = null;
  device = g.dev; gpuInfo = g.info;
  device.lost.then(info => { if (!closed) post({ type: 'error', where: 'gpu', message: `device lost: ${info.reason} ${info.message}` }); });
  gctx = canvas.getContext('webgpu');
  sampler = device.createSampler({ magFilter: 'linear', minFilter: 'linear' });
}

function configureCanvas(frame) {
  const cs = frame.colorSpace;
  const hdr = !forceSdr && (cs.transfer === 'pq' || cs.transfer === 'hlg' || (track.color && (track.color.transfer === 16 || track.color.transfer === 18)));
  hdrInfo.source = `${cs.primaries || '?'}/${cs.transfer || '?'}/${cs.matrix || '?'} ${cs.fullRange ? 'full' : 'limited'}${hdr ? ' (HDR)' : ' (SDR)'}`;
  // The canvas is the source size unless ?maxw=N says otherwise (0 or absent: no cap). See
  // measurements/records/hdr-webgpu-canvas-configure.md for the 4K rgba16float freeze investigation; the cap is reported in the stats overlay.
  const cap = maxW || 0;
  const sc = cap && frame.displayWidth > cap ? cap / frame.displayWidth : 1;
  canvas.width = Math.round(frame.displayWidth * sc); canvas.height = Math.round(frame.displayHeight * sc);
  hdrInfo.canvasSize = `${canvas.width}x${canvas.height}` + (sc < 1 ? ` (downscaled from ${frame.displayWidth}x${frame.displayHeight})` : '');
  let cfg;
  if (hdr) {
    // Probe whether this Chrome accepts the rec2100 canvas colour spaces; the shipped path stays on display-p3 + extended
    // tone mapping because importExternalTexture can only deliver srgb or display-p3 values.
    if (probeRec2100) for (const c of ['rec2100-pq', 'rec2100-hlg']) {
      try { gctx.configure({ device, format: 'rgba16float', alphaMode: 'opaque', colorSpace: c, toneMapping: { mode: 'extended' } }); hdrInfo.probe[c] = 'accepted'; }
      catch (e) { hdrInfo.probe[c] = 'rejected'; }
    }
    // ?fmt, ?cs, ?tm exist for bisecting presentation problems; the shipped HDR path is rgba16float / display-p3 / extended.
    const fmt = hdrOpts.canvasFormat || 'rgba16float', cs2 = hdrOpts.canvasCS || 'display-p3', tm = hdrOpts.toneMapping || 'extended';
    cfg = { device, format: fmt, alphaMode: 'opaque', colorSpace: cs2, toneMapping: { mode: tm } };
    try {
      gctx.configure(cfg); extCS = cs2;
      hdrInfo.canvas = `${fmt} ${cs2} toneMapping=${tm}`;
      pipelineFormat = fmt;
    } catch (e) {
      log('HDR canvas configure failed, falling back to SDR: ' + e);
      cfg = null; hdrInfo.canvas = 'HDR configure failed: ' + e;
    }
  }
  if (!cfg) {
    pipelineFormat = navigator.gpu.getPreferredCanvasFormat();
    gctx.configure({ device, format: pipelineFormat, alphaMode: 'opaque', colorSpace: 'srgb' });
    extCS = 'srgb';
    if (!hdr) hdrInfo.canvas = `${pipelineFormat} srgb`;
  }
  try { const gc = gctx.getConfiguration?.(); if (gc) hdrInfo.effective = JSON.stringify({ format: gc.format, colorSpace: gc.colorSpace, toneMapping: gc.toneMapping }); } catch {}
  hdrInfo.displayHdr = displayHdr;
  const mod = device.createShaderModule({ code: SHADER });
  pipeline = device.createRenderPipeline({
    layout: 'auto',
    vertex: { module: mod, entryPoint: 'vs' },
    fragment: { module: mod, entryPoint: 'fs', targets: [{ format: pipelineFormat }] },
    primitive: { topology: 'triangle-list' },
  });
  canvasConfigured = true;
  log(`canvas ${canvas.width}x${canvas.height}: ${hdrInfo.canvas}; source ${hdrInfo.source}; probe ${JSON.stringify(hdrInfo.probe)}`);
}

let snapReq = false;
function takeSnap() {
  if (!snapReq) return;
  snapReq = false;
  if (hostId) { const ts = lastDrawnTs; createImageBitmap(canvas).then(b => post({ type: 'snap', bitmap: b, ts }), e => { log('snapshot failed: ' + e); post({ type: 'snap', bitmap: null }); }); return; }
  try { const b = canvas.transferToImageBitmap(); post({ type: 'snap', bitmap: b, ts: lastDrawnTs }, [b]); } catch (e) { log('snapshot failed: ' + e); post({ type: 'snap', bitmap: null }); }
}
let lastDrawnTs = null;
function draw(frame) {
  if (nodraw) return;
  lastDrawnTs = frame.timestamp;
  if (frame.dv && !dv.comp) { frame.present(); takeSnap(); if (!canvasConfigured) { canvasConfigured = true; Object.assign(hdrInfo, dv.hdrReport(), { displayHdr }); } return; }
  if (frame.dv) { frame.present(); takeSnap(); if (!canvasConfigured) { canvasConfigured = true; const c = dv.comp; hdrInfo.canvas = `${c.options.format} ${c.canvasConfig?.colorSpace} toneMapping=${c.canvasConfig?.toneMapping} (FelCompositor)`; hdrInfo.canvasSize = `${canvas.width}x${canvas.height}`; hdrInfo.source = `compositor ${dv.dvInfo.kind}`; try { const gc = gctx.getConfiguration?.(); if (gc) hdrInfo.effective = JSON.stringify({ format: gc.format, colorSpace: gc.colorSpace, toneMapping: gc.toneMapping }); } catch {} hdrInfo.displayHdr = displayHdr; } return; }
  if (presentVideo && !canvasConfigured && useVideo === null) {
    // SDR frames go to a <video> on the page through a MediaStreamTrackGenerator: Chrome composites that like any video, and the
    // WebGPU canvas present cost about 5 points of CPU at 1080p24 (23.9 % -> 19.1 %, browser and GPU processes). That path drops
    // PQ/HLG and BT.2020 signalling (an HDR10 clip showed as SDR), so HDR keeps the canvas.
    const cs = frame.colorSpace, tc = track.color || {};
    const wide = cs.transfer === 'pq' || cs.transfer === 'hlg' || cs.primaries === 'bt2020' || tc.transfer === 16 || tc.transfer === 18 || tc.primaries === 9;
    useVideo = !wide;
    if (useVideo) {
      hdrInfo.source = `${cs.primaries || '?'}/${cs.transfer || '?'}/${cs.matrix || '?'} ${cs.fullRange ? 'full' : 'limited'} (SDR)`;
      hdrInfo.canvas = 'video element (MediaStreamTrackGenerator)';
      post({ type: 'present', mode: 'video' });
    }
  }
  if (useVideo) {
    if (snapReq) { snapReq = false; const c = frame.clone(); post({ type: 'snap', bitmap: c, ts: frame.timestamp }, [c]); }
    post({ type: 'vframe', frame }, [frame]);
    return;
  }
  if (!canvasConfigured) configureCanvas(frame);
  const bg = device.createBindGroup({
    layout: pipeline.getBindGroupLayout(0),
    entries: [
      { binding: 0, resource: sampler },
      { binding: 1, resource: device.importExternalTexture({ source: frame, colorSpace: extCS }) },
    ],
  });
  const enc = device.createCommandEncoder();
  const pass = enc.beginRenderPass({ colorAttachments: [{ view: gctx.getCurrentTexture().createView(), loadOp: 'clear', clearValue: { r: 0, g: 0, b: 0, a: 1 }, storeOp: 'store' }] });
  pass.setPipeline(pipeline); pass.setBindGroup(0, bg); pass.draw(3); pass.end();
  device.queue.submit([enc.finish()]);
  takeSnap();
}

// ---------------------------------------------------------------- decoder
async function makeDecoder() {
  decGen++;
  const gen = decGen;
  if (decoder && decoder.state !== 'closed') decoder.close();
  submitted = 0; outputs = 0;
  decoder = new VideoDecoder({
    output: frame => onFrame(frame, gen),
    error: e => {
      if (gen !== decGen || closed) return;
      decodeErrors++;
      const detail = `${e} (submitted ${submitted}, outputs ${outputs}, codec ${decCfg && decCfg.codec}, hw ${decCfg && decCfg.hardwareAcceleration}, last packet key=${lastPkt && lastPkt.key} ${lastPkt && lastPkt.data.length} bytes)`;
      // A decoder error (a bad access unit, a hardware decoder refusing one stream) should not end playback: rebuild the
      // decoder, let software take over after the first failure, and resume at the next random access point.
      lastDecodeError = detail;
      if (++recoveries > 6) { post({ type: 'error', where: 'video-decoder', message: detail }); return; }
      log(`WARNING: video decoder error, recovering (${recoveries}/6): ${detail}`);
      const wasHw = decCfg && decCfg.hardwareAcceleration !== 'prefer-software' && decoderInfo.hardwareAcceleration !== 'prefer-software';
      recoverPending = true;
      const go = swOk => {
      if (gen !== decGen || closed) return;   // a seek or close replaced this decoder while the support check ran
      if (wasHw && swOk) {
        // 'no-preference' still lands on the same hardware decoder, so ask for software explicitly (and say so in the overlay).
        decCfg = { ...decCfg, hardwareAcceleration: 'prefer-software' };
        decoderInfo = { ...decoderInfo, hardwareAcceleration: 'prefer-software', fallbackFrom: 'prefer-hardware', fallbackReason: String(e), fallbackAtPacket: packetsTotal };
        swFallback = decoderInfo;
        if (!gopOverflow && gop.length) {
          // replay the current GOP (its first packet is a random access point); frames already output are skipped by pts
          inq = gop.map(g => ({ ...g, replay: true })).concat(inq);
          gop = []; gopBytes = 0; dedupeBelow = maxOutPts;
          needKey = true;
          log(`falling back to the software decoder, replaying ${inq.filter(x => x.replay).length} packets of the current GOP`);
        } else needKey = true;
      } else needKey = true;
      makeDecoder().then(() => pump()).catch(err => post({ type: 'error', where: 'video-decoder', message: String(err) }));
      };
      // Chrome on macOS has no software HEVC decoder: when 'prefer-software' is not available the hardware decoder is rebuilt
      // and resumes at the next random access point instead of failing every recovery.
      if (wasHw) VideoDecoder.isConfigSupported({ ...decCfg, hardwareAcceleration: 'prefer-software' }).then(r => go(!!r.supported), () => go(false)); else go(false);
    },
  });
  if (!decCfg) {
    const base = { codec: track.codec, codedWidth: track.width, codedHeight: track.height, optimizeForLatency: hdrOpts.ofl !== '0' };
    if (track.description) base.description = track.description;
    // Dolby Vision string first (a Chrome that takes it knows the profile), then the base layer string of the dvcC / hvcC record, then
    // the container codec; every candidate is also tried with the level in the main tier ('.H' -> '.L'), some builds list only that.
    const codecs = [];
    for (const c of [track.dolbyVisionCodecString, track.fallbackCodecString, track.codec]) {
      if (!c || codecs.includes(c)) continue;
      codecs.push(c);
      if (/^hvc1\..*\.H/.test(c)) codecs.push(c.replace('.H', '.L'));
    }
    outer: for (const codec of codecs) for (const hw of (hdrOpts.hw ? [hdrOpts.hw] : ['prefer-hardware', 'prefer-software'])) {
      const cfg = { ...base, codec, hardwareAcceleration: hw };
      const sup = await VideoDecoder.isConfigSupported(cfg);
      if (sup.supported) { decCfg = cfg; decoderInfo = { codec, hardwareAcceleration: hw, accepted: true }; break outer; }
    }
    if (!decCfg) throw new Error(`VideoDecoder does not support ${track.codec}`);
    if (decoderInfo.hardwareAcceleration !== 'prefer-hardware') {
      // Say why: the usual cause is interlaced H.264 (frame_mbs_only_flag 0), which Chrome's VideoToolbox decoder refuses
      const f = /^avc/.test(track.codec) ? h264SpsFacts(track.description) : null;
      const reason = f && !f.frameMbsOnly ? `interlaced H.264 (frame_mbs_only_flag 0${f.mbaff ? ', MBAFF' : ''}): the hardware decoder does not take it` : 'the hardware decoder does not accept this stream configuration';
      decoderInfo = { ...decoderInfo, fallbackFrom: 'prefer-hardware', fallbackReason: reason };
      log('WARNING: hardware decode refused (' + reason + '), using the ' + decoderInfo.hardwareAcceleration + ' decoder');
    }
  }
  // Submission resumes when the decoder's input queue drains, not only when it outputs a frame: a decoder that holds frames back until
  // it has more input (reorder delay) never outputs again while this worker waits for it, and a cold seek then hung in 'seeking'.
  decoder.ondequeue = () => { if (gen === decGen) pump(); };
  decoder.configure(decCfg);
}

function onFrame(frame, gen) {
  created++;
  if (!markedOutput) { markedOutput = true; mark('first-output'); }
  if (gen !== decGen || closed) { release(frame); return; }
  outputs++;
  if (recoverPending && outputs >= 30) { recoverPending = false; recoveries = 0; }
  if (frame.timestamp <= dedupeBelow) { release(frame); pump(); return; }
  if (frame.timestamp > maxOutPts) maxOutPts = frame.timestamp;
  lastFrameInfo = `${frame.format} ${frame.codedWidth}x${frame.codedHeight}`;
  if (frame.timestamp < targetUs) { preroll++; release(frame); pump(); return; }
  q.push(frame);
  checkReady();
  pump();
}

function pump() {
  if (closed) return;
  if (dv) {
    dv.pump();
    if (eof && !dv.inq.length && !flushing && !flushed) {
      flushing = true; const g = decGen;
      dv.finish().then(() => { if (g !== decGen || closed) return; flushed = true; flushing = false; checkReady(); checkEnded(); }).catch(e => { if (g === decGen && !closed) post({ type: 'error', where: 'dv-finish', message: String(e) }); });
    }
    return;
  }
  if (!decoder || decoder.state !== 'configured') return;
  // Gate on frames held, not on frames still inside the decoder: a software decoder with frame threads (the fallback after a hardware
  // error) keeps more than QMAX frames in flight before it outputs the first one, and a gate on (submitted - outputs) alone then waits for
  // output that only more input can produce. With the output queue empty submission always continues.
  while (inq.length && decoder.decodeQueueSize < 4 && q.length < QMAX && (q.length + (submitted - outputs) < QMAX || q.length === 0 || (submitted - outputs) > 8 && q.length < 2)) {
    const p = inq.shift();
    if (needKey) {
      // Start at a verified key packet (the demuxer's flag is container flag AND IDR / IRAP check; after a seek the first packet is the one
      // `seek` landed on) or at any random access point found in the payload. The very first packet of a file is NOT accepted when it is
      // neither: POCAWE begins with a non-IDR recovery point I picture, and Chrome's VideoDecoder rejects it as a key chunk
      // ("marked as type key but wasn't a key frame"), so that file starts at its first IDR.
      if (!(p.key || isRandomAccess(p.data))) { skippedNoKey++; epochPackets++; if (!p.replay) port.postMessage({ type: 'ack', epoch, n: 1 }); continue; }
      needKey = false; p.key = true;
    }
    if (!p.replay) epochPackets++;
    lastPkt = p;
    packetsTotal++;
    if (!p.replay) {
      if (p.key && isRandomAccess(p.data)) { gop = []; gopBytes = 0; gopOverflow = false; }
      if (!gopOverflow) { gop.push({ ptsUs: p.ptsUs, durationUs: p.durationUs, key: p.key, data: p.data }); gopBytes += p.data.length; if (gopBytes > GOP_CAP_BYTES) { gop = []; gopOverflow = true; } }
    } else if (p.key) { gop = [{ ptsUs: p.ptsUs, durationUs: p.durationUs, key: p.key, data: p.data }]; gopBytes = p.data.length; gopOverflow = false; }
    else gop.push({ ptsUs: p.ptsUs, durationUs: p.durationUs, key: p.key, data: p.data });
    try {
      decoder.decode(new EncodedVideoChunk({ type: p.key ? 'key' : 'delta', timestamp: p.ptsUs, duration: p.durationUs, data: p.data }));
    } catch (e) { post({ type: 'error', where: 'video-decode', message: String(e) }); return; }
    submitted++;
    if (!p.replay) port.postMessage({ type: 'ack', epoch, n: 1 });
  }
  if (eof && !inq.length && !flushing && !flushed) {
    flushing = true;
    const gen = decGen;
    decoder.flush().then(() => {
      if (gen !== decGen || closed) return;
      flushed = true; flushing = false;
      checkReady(); checkEnded();
    }).catch(e => { if (gen === decGen && !closed) post({ type: 'error', where: 'video-flush', message: String(e) }); });
  }
}

function checkReady() {
  if (readySent) return;
  // The exact (bit-exact WASM) path reports ready only with a cushion: it decodes at about the display rate, and starting on 4 frames
  // dropped 8 to 10 pictures in the first 1.5 s. A pool that cannot fill the cushion in 4 s starts on READY_FRAMES.
  const need = dv && dv.exactBl ? (performance.now() - epochT0 > 4000 ? READY_FRAMES : dv.readyNeed()) : READY_FRAMES;
  const aheadOk = !(dv && dv.exactBl) || need === READY_FRAMES || !dv.ahead || dv.ahead() >= EXACT_AHEAD || dv.aheadFull?.() || dv.eof;
  if ((q.length >= need && aheadOk) || flushed) {
    readySent = true;
    // Poster: show the first frame of this epoch right away so a paused seek is visible.
    post({ type: 'ready', epoch, firstPtsUs: q.length ? q[0].timestamp : targetUs });
    drawPoster();
  }
}

function checkEnded() {
  if (flushed && !q.length && !endedSent && readySent) { endedSent = true; post({ type: 'ended', epoch }); }
}

function drawPoster() {
  if (!q.length) return;
  const f = q.shift();
  if (!markedDraw) { markedDraw = true; mark('first-draw'); }
  try { draw(f); shown++; markPresented(f, null); } catch (e) { post({ type: 'error', where: 'gpu-draw', message: String(e) }); }
  release(f);
}

// ---------------------------------------------------------------- presentation
function tick() { const t = stageNow(); tickBody(); schedule(); stage('video.tick(incl)', t); }
// The <video> path shows a frame about two refreshes after it is written (33 ms write-to-display at 1080p24 on a 120 Hz panel with no
// lead; a 20 ms lead left the picture 4 ms early), so it writes frames that much ahead. Feeding the measured latency back instead ran
// away (the element paces by timestamp, latency grew with the lead). ?vlead=<ms> overrides.
function leadUs() { return useVideo ? (presentLeadUs || Math.round(Math.min(34, Math.max(8, 2 * (vsyncMs || 16.7))) * 1000)) : 0; }
function tickBody() {
  if (closed) return;
  if (dv) { dv.pump(); checkReady(); }
  const t = performance.now();
  if (readySent && playing && q.length) {
    const clock = nowUs() + leadUs();
    // A frame is due up to half a refresh early: the display quantises to vsync anyway, and a hard "due at pts" edge made every tick that
    // came a millisecond early push the frame one full tick late, where it then collided with its successor and one of the two was
    // dropped (P8.4 1080p60 without audio: 0 or 88 drops per run depending on the phase between the wall clock and vsync).
    // q[0] is dropped for q[1] only when it is itself more than one frame (at most one refresh) late.
    const tol = Math.round(Math.min(8500, Math.max(1000, (vsyncMs || 16.7) * 500)));
    while (q.length >= 2 && q[1].timestamp <= clock + tol && clock - q[0].timestamp > Math.min(q[1].timestamp - q[0].timestamp, (vsyncMs || 16.7) * 1000)) { dropped++; release(q.shift()); }
    if (q[0].timestamp <= clock + tol) {
      const f = q.shift();
      try { draw(f); shown++; { const o = (clock - f.timestamp) / 1000; markPresented(f, o); offsets.push(o); hist[Math.min(2000, Math.round(Math.abs(o)))]++; histN++; } if (offsets.length > 600) offsets.shift(); presentTimes.push(t); }
      catch (e) { post({ type: 'error', where: 'gpu-draw', message: String(e) }); }
      release(f);
      pump();
    }
  }
  checkEnded();
  while (presentTimes.length && t - presentTimes[0] > 1000) presentTimes.shift();
}
// Only back-to-back rAF callbacks one refresh apart measure it: schedule() sleeps between frames (a 41 ms gap at 24 fps is not a
// vsync), and a callback that skipped a refresh reads double.
let lastTickT = 0, lastTickPerf = 0;
function schedule() {
  if (closed) return;
  // Sleep until about one refresh before the next frame is due instead of waking every vsync (24 fps on a 120 Hz panel woke 5 times
  // per frame). The DV paths pump their decoders from this tick, so while they sleep a timer keeps pumping every 20 ms.
  if (readySent && playing && q.length) {
    const vs = vsyncMs || 16.7;
    const waitMs = (q[0].timestamp - nowUs() - leadUs()) / 1000 - 2 * vs;
    if (waitMs > 4 && (!dv || dvSleep)) {   // ?dvsleep=0 keeps the DV per-vsync loop (A/B)
      if (dv) setTimeout(() => { if (closed) return; dv.pump(); checkReady(); schedule(); }, Math.min(waitMs, 20));
      else setTimeout(schedule, Math.min(waitMs, 100));
      return;
    }
  }
  if (typeof self.requestAnimationFrame === 'function') self.requestAnimationFrame(ts => { const gap = ts - lastTickT; if (lastTickT && gap > 0 && gap < 34 && (!hostId || gap >= 4) && (!vsyncMs || gap < vsyncMs * 1.5)) vsyncMs = vsyncMs ? vsyncMs * 0.9 + gap * 0.1 : gap; lastTickT = ts; lastTickPerf = performance.now(); tick(); });
  else setTimeout(tick, 4);
}
// rAF stops in a hidden/occluded page; keep presenting from a timer so the clock never runs away from the frames.
const rafGuard = setInterval(() => {
  if (closed || !playing) return;
  if (performance.now() - lastTickPerf > 50) { lastTickPerf = performance.now() - 40; tickBody(); }
}, 25);

// ---------------------------------------------------------------- state
function resetEpoch(newEpoch, target) {
  epoch = newEpoch;
  for (const f of q) release(f);
  q = []; inq = []; needKey = true; epochPackets = 0; gop = []; gopBytes = 0; gopOverflow = false; maxOutPts = -Infinity; dedupeBelow = -Infinity;
  eof = false; flushing = false; flushed = false; endedSent = false; readySent = false;
  targetUs = target; playing = false; er = newEr(); epochT0 = performance.now(); markedOutput = markedPkt = false;
  offsets = []; presentTimes = [];
  clk.drainT0 = 0;
  const keep = early.filter(m => m.epoch === epoch);
  early = [];
  for (const m of keep) accept(m);
}

function accept(m) {
  if (m.type === 'pkt') {
    if (m.epoch < epoch) return;
    if (m.epoch > epoch) { early.push(m); return; }
    if (corruptFrac && m.data.byteLength > 64 && Math.random() < corruptFrac) {
      const u = new Uint8Array(m.data); corrupted++;
      for (let k = 0; k < 4; k++) u[16 + Math.floor(Math.random() * (u.length - 16))] ^= 1 << Math.floor(Math.random() * 8);
    }
    if (!markedPkt) { markedPkt = true; mark('first-packet'); }
    if (dv) { dv.accept(m); return; }
    inq.push({ ptsUs: m.ptsUs, durationUs: m.durationUs, key: m.key, data: new Uint8Array(m.data) });
  } else if (m.type === 'eof') {
    if (m.epoch === epoch) eof = true; else if (m.epoch > epoch) early.push(m);
  }
}

scope.onmessage = async e => {
  const m = e.data;
  try {
    if (m.type === 'preboot') {
      // Runs while the header is being read; a failure here is the normal path's to report.
      mark('preboot');
      preGpu = requestGpu(true).catch(() => null);
      await initEngine().catch(() => {});
      mark('preboot-done');
    } else if (m.type === 'init') {
      corruptFrac = Number(m.corrupt) || 0; track = m.track; port = m.port; canvas = m.canvas; epoch = m.epoch; displayHdr = m.displayHdr; probeRec2100 = !!m.probeRec2100; nodraw = !!m.nodraw; dvSleep = m.dvSleep !== false; presentVideo = !!m.presentVideo; forceSdr = !!m.forceSdr; maxW = m.maxW; hdrOpts = { canvasFormat: m.canvasFormat, canvasCS: m.canvasCS, toneMapping: m.toneMapping, hw: m.hw, ofl: m.ofl };
      dvQuality = m.quality || 'native'; dvDisplay = m.display || null;
      const soft = isVc1Track(track);
      const comp = !soft && dvQuality !== 'native' && /^(hvc1|hev1|dvh1|dvhe)/.test(track.codec);
      mark('init');
      await initGpu(comp);
      mark('gpu-device');
      if (soft) {
        // VC-1: no WebCodecs decoder exists; packets go to a Rust/WASM decode worker, planes are converted on the GPU
        dv = new Vc1Path({ track, device, gctx, canvas, deint: m.deint || undefined, log, post, push: it => { q.push(it); }, ack: n => port.postMessage({ type: 'ack', epoch, n }), qlen: () => q.length });
        await dv.init();
        dv.targetUs = 0;
        decoderInfo = { codec: 'vc1', hardwareAcceleration: 'WASM (aether-vc1)', note: 'no WebCodecs VC-1 decoder in any browser' };
      } else if (comp) {
        await initEngine();
        mark('engine');
        const r = track.dolbyVision?.profile === 5 ? relabelForChrome(track, P5_CARRIER) : relabelForChrome(track);
        track = r.track;
        dv = new DvPath({ track, colorOriginal: r.original, device, gctx, quality: dvQuality, display: dvDisplay, log, post, push: it => { q.push(it); }, ack: n => port.postMessage({ type: 'ack', epoch, n }), qlen: () => q.length });
        await dv.init();
        mark('compositor-inited');
        dv.targetUs = 0;
        decoderInfo = { codec: track.codec, hardwareAcceleration: dv.exactBl ? 'WASM (exact)' : 'prefer-hardware', compositor: dvQuality };
      } else await makeDecoder();
      port.onmessage = ev => { const t = stageNow(); accept(ev.data); pump(); stage('video.packet(incl)', t, ev.data.data?.byteLength || 0); };
      post({ type: 'inited', gpu: gpuInfo, decoder: decoderInfo, codec: track.codec, size: `${track.width}x${track.height}` });
      schedule();
    } else if (m.type === 'seek') {
      // Seeks run one at a time: dv.reset() and makeDecoder() await, and a second seek (a scrub) would interleave with the first
      // one's awaits (EL decoder rebuilt under it: 'key frame required after configure()'). A seek that is already superseded is skipped.
      latestSeek = Math.max(latestSeek, m.epoch);
      const run = async () => {
        if (m.epoch < latestSeek || closed) return;
        if (dv) { epoch = m.epoch; decGen++; if (m.initial) dv.targetUs = m.targetUs; else await dv.reset(m.targetUs); }
        if (m.epoch < latestSeek || closed) return;
        resetEpoch(m.epoch, m.targetUs);
        if (!dv && !m.initial) await makeDecoder();
        pump();
      };
      seekChain = seekChain.then(run).catch(err => post({ type: 'error', where: 'video', message: errText(err) }));
      await seekChain;
    } else if (m.type === 'snap') {
      snapReq = true;
      if (!playing && q.length && readySent) { const f = q.shift(); draw(f); release(f); }
    } else if (m.type === 'lead') {
      presentLeadUs = m.us;
    } else if (m.type === 'clock') {
      // m.audio: {ctl: SharedArrayBuffer, sampleRate} or null; m.baseUs
      clk.hasAudio = !!m.audio;
      if (m.audio) { clk.ctl = new Int32Array(m.audio.ctl); clk.sampleRate = m.audio.sampleRate; }
      clk.baseUs = m.baseUs; clk.wallBase = m.baseUs; clk.drainT0 = 0;
    } else if (m.type === 'play') {
      clk.wallT0 = performance.now(); playing = true;
    } else if (m.type === 'pause') {
      if (!clk.hasAudio) clk.wallBase = nowUs();
      playing = false;
    } else if (m.type === 'close') {
      if (hostId) { clearInterval(rafGuard); clearInterval(statsTimer); }
      closed = true; decGen++;
      if (decoder && decoder.state !== 'closed') decoder.close();
      for (const f of q) release(f);
      q = [];
      if (dv) await dv.close();
      for (const f of q) release(f);
      q = []; inq = []; early = [];
      try { gctx?.unconfigure(); } catch {}
      try { device?.destroy(); } catch {}
      port?.close();
      { const dc = dv ? dv.createdFrames || 0 : 0, dk = dv ? dv.closedFrames || 0 : 0; { let lost = 'pending'; try { if (device) lost = await Promise.race([device.lost.then(i => i.reason), new Promise(r => setTimeout(() => r('not-lost'), 300))]); else lost = 'none'; } catch (e) { lost = String(e); } closedExtra = { deviceLost: lost, decoderState: decoder ? decoder.state : 'none' }; }
      post({ type: 'closed', ...closedExtra, framesCreated: created + dc, framesClosed: closedN + dk, open: created + dc - closedN - dk }) }
    }
  } catch (err) {
    post({ type: 'error', where: 'video', message: errText(err) });
  }
};
const statsTimer = setInterval(() => {
  if (closed) return;
  const st0 = stageNow();
  const abs = offsets.map(Math.abs);
  post({
    type: 'stats', epoch,
    fps: presentTimes.length, shown, dropped: dropped + dvDropped(), preroll, droppedPct: shown + dropped + dvDropped() ? ((dropped + dvDropped()) * 100) / (shown + dropped + dvDropped()) : 0,
    avP95Ms: percentile(abs, 0.95), avMeanMs: offsets.length ? offsets.reduce((a, b) => a + b, 0) / offsets.length : 0, avSamples: offsets.length, avP95AllMs: histP(0.95), avP99AllMs: histP(0.99), avMaxHistMs: (() => { for (let i = 2000; i >= 0; i--) if (hist[i]) return i; return 0; })(), avCountAll: histN,
    exactAhead: dv && dv.ahead ? dv.ahead() : null, dbg: { dstate: decoder ? decoder.state : null, needKey, submitted, outputs, eof, flushing, flushed, readySent, epochPackets, skippedNoKey, qn: q.length, inqn: inq.length, targetUs, closed }, ep: er && { ...er, dtMin: er.dtMin === Infinity ? null : er.dtMin }, corrupted, heap: jsHeapBytes(), wasm: wasmBytes(), held: created - closedN - q.length, queue: q.length, inq: inq.length, decodeQueue: decoder ? decoder.decodeQueueSize : 0,
    openFrames: created - closedN + (dv ? (dv.createdFrames || 0) - (dv.closedFrames || 0) : 0), created: created + (dv ? dv.createdFrames || 0 : 0), closedN: closedN + (dv ? dv.closedFrames || 0 : 0), decodeErrors: decodeErrors + (dv?.counts?.decodeErrors || 0), recoveries: recoveries + (dv?.totalRecoveries || 0), lastDecodeError: lastDecodeError || dv?.lastError || null, skippedNoKey,
    clockUs: readySent ? nowUs() : null, vsyncMs: Math.round(vsyncMs * 10) / 10, presentLeadMs: leadUs() / 1000,
    dv: dv ? dv.info() : null, decoder: decoderInfo, hdr: hdrInfo, frameFormat: lastFrameInfo, gpu: gpuInfo, stages: stages(),
  });
  stage('video.stats', st0);
}, 250);
