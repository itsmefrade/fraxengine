// SPDX-License-Identifier: LGPL-3.0-only
// Tier measurement page (docs/perf/TIERS-BASELINE.md). One file per page load:
//   ?src=fixtures/x.mkv  label=name  secs=60 (steady playback)  seeks=1|0  audio=<index>  q=<quality>
// Every other query flag is the session's own (?mse=0, ?present=canvas, ...).
// Records: route, time to first frame, steady playback (frames shown and dropped, clock against wall time, A/V offset, underruns,
// per-worker JS heap and WASM memory, main-thread gaps), then near and far seeks. The result goes to POST /results as test 'perf';
// tools/perfrun.py samples process CPU time and RSS from outside and joins on the wall-clock marks in `wall`.
import { Session } from './session.js';

const q = new URLSearchParams(location.search);
const host = document.getElementById('host'), out = document.getElementById('out');
const sleep = ms => new Promise(r => setTimeout(r, ms));
const num = (k, d) => (q.has(k) && q.get(k) !== '' ? Number(q.get(k)) : d);
const MB = x => x == null ? null : +(x / 1048576).toFixed(1);
const notes = [];
const log = s => { notes.push(`${(performance.now() / 1000).toFixed(1)} ${s}`); out.textContent = notes.slice(-30).join('\n'); };
// Worker creation order, to name the threads tools/perfrun.py samples from outside (workers started inside a worker are not seen here).
const workerOrder = [];
{ const W = self.Worker; self.Worker = function (u, o) { workerOrder.push(`${o?.name || String(u).split('/').pop().split('?')[0]}@${Math.round(performance.now())}`); return new W(u, o); }; self.Worker.prototype = W.prototype; }
const pageErrors = [];
// The document's load event: an element that never gets data (or anything else that holds the event back) keeps the tab's loading
// spinner turning, and the browser then repaints its own window for the whole playback (docs/perf/CORE-V2.md, G).
let loadEventMs = document.readyState === 'complete' ? 0 : null;
addEventListener('load', () => { loadEventMs = Math.round(performance.now()); });
addEventListener('error', e => pageErrors.push('error: ' + e.message));
addEventListener('unhandledrejection', e => pageErrors.push('rejection: ' + (e.reason && e.reason.message || e.reason)));

// main thread: longest gap of a 20 ms heartbeat, and long tasks
let beat = performance.now(), maxGap = 0, gapsOver50 = 0;
setInterval(() => { const n = performance.now(), g = n - beat; if (g > maxGap) maxGap = g; if (g > 70) gapsOver50++; beat = n; }, 20);
let longTasks = 0, longTaskMs = 0;
try { new PerformanceObserver(l => { for (const e of l.getEntries()) { longTasks++; longTaskMs += e.duration; } }).observe({ entryTypes: ['longtask'] }); } catch {}

const wait = async (cond, ms) => { const t0 = performance.now(); while (performance.now() - t0 < ms) { if (cond()) return true; await sleep(5); } return false; };

// Stage counters of every worker (js/stages.js), flattened; the demuxer's blocked waits for the network are listed apart so they can
// be taken out of demux.read.
function stageSnap(s) {
  const o = {};
  for (const st of [s.stats.demux?.stages, s.stats.audio?.stages, s.stats.video?.stages, s.mse?.stats?.stages]) for (const k in st || {}) o[k] = st[k].slice();
  o['demux.net-wait'] = [s.stats.demux?.net?.waitMs || 0, 0, 0];
  return o;
}
function stageDiff(a, b, seconds) {
  const o = {};
  for (const k in b) { const x = a[k] || [0, 0, 0]; o[k] = { msPerS: +((b[k][0] - x[0]) / seconds).toFixed(2), perS: +((b[k][1] - x[1]) / seconds).toFixed(1), MBPerS: +((b[k][2] - x[2]) / seconds / 1048576).toFixed(2) }; }
  return o;
}

function mem(s) {
  const v = s.stats.video || {}, a = s.stats.audio || {}, d = s.stats.demux || {}, m = s.mse?.stats || {};
  const r = { mainMB: MB(performance.memory?.usedJSHeapSize), vHeapMB: MB(v.heap), vWasmMB: MB(v.wasm), aHeapMB: MB(a.heap), aWasmMB: MB(a.wasm), dHeapMB: MB(d.heap), dWasmMB: MB(d.wasm), mseHeapMB: MB(m.heap) };
  r.sumMB = +Object.values(r).reduce((x, y) => x + (y || 0), 0).toFixed(1);
  return r;
}

// Frames on screen: the element's own counters on the MSE path, the video worker's on the canvas path.
function frames(s) {
  const el = s.mseEl, v = s.stats.video || {};
  if (el) { const pq = el.getVideoPlaybackQuality(); return { shown: pq.totalVideoFrames - pq.droppedVideoFrames, dropped: pq.droppedVideoFrames, corrupted: pq.corruptedVideoFrames }; }
  return { shown: v.shown || 0, dropped: v.dropped || 0 };
}

function route(s) {
  const v = s.stats.video || {};
  const video = !s.vTrack ? 'none' : s.mseEl ? 'MSE element' : v.dv ? `canvas compositor (${s.quality})` : s.videoEl && s.videoEl.style.display !== 'none' ? 'WebCodecs to video element' : 'WebCodecs canvas';
  return { video, quality: s.quality, decoder: v.decoder || null, audio: s.audioRoute || null, audioInfo: s.audioInfo || null, clockMode: s.clockMode || null, mseAudio: !!(s.mseAV || s.mseA), canvas: v.hdr?.canvas || null, canvasSize: v.hdr?.canvasSize || null, dv: v.dv ? { kind: v.dv.kind, exact: v.dv.exact ?? null } : null };
}

async function main() {
  // ?play=tv/123 asks a same-origin site backend for the title's source (the address stays out of the result); ?trace=1 keeps the
  // session log (addresses removed) and the element's buffer state per second.
  let src = q.get('src');
  const secs = num('secs', 60), trace = q.has('trace');
  const redact = t => String(t).replace(/(https?:\/\/|\/api\/)[^\s"',)]+/g, '<address>');
  if (q.get('play')) src = (await (await fetch(`/api/play/${q.get('play')}?season=${num('season', 1)}&episode=${num('episode', 1)}&channel=${q.get('channel') || 'jellyfin'}`)).json()).native.source;
  const r = { label: q.get('label') || src, src: q.get('play') ? 'play:' + q.get('play') : src, query: location.search, wall: {}, errors: [], events: [] };
  const events = r.events;
  if (trace) r.trace = [];
  const tlog = t => { if (trace && r.trace.length < 4000) r.trace.push(`${((performance.now() - t0) / 1000).toFixed(2)} ${redact(t).slice(0, 400)}`); };
  const wallOpen = Date.now(), t0 = performance.now();
  r.wall.open = wallOpen;
  const s = new Session({ host, url: src, wantPlay: true, quality: q.get('q'), audioIndex: q.has('audio') ? num('audio', 0) : null, onEvent: e => {
    if (e.type === 'log') tlog(e.text);
    if (e.type === 'log' && /WARNING|ERROR/.test(e.text) && events.length < 40) events.push(redact(e.text).slice(0, 240));
    if (e.type === 'error' && events.length < 40) events.push('ERROR ' + e.message);
  } });
  window.lastSession = s;
  // First picture: the element's first frame callback on the MSE path (found as soon as the element exists), the worker's
  // 'first-draw' mark on the canvas path.
  let firstElFrame = null;
  const elWatch = setInterval(() => {
    const el = s.mseEl;
    if (!el || el.__perfWatched) return;
    el.__perfWatched = true;
    el.requestVideoFrameCallback(() => { firstElFrame = performance.now() - t0; });
  }, 5);
  try {
    await s.open();
    r.openMs = Math.round(performance.now() - t0);
    r.tracks = s.tracks.map(t => `${t.kind} ${t.codec}${t.width ? ` ${t.width}x${t.height}` : ''}${t.channels ? ` ${t.channels}ch` : ''}`);
    r.durationS = +(s.durationUs / 1e6).toFixed(1);
    s.play();
    if (!await wait(() => s.state === 'playing' || s.state === 'error', 60000) || s.state === 'error') throw new Error('never reached playing: ' + s.state + ' ' + (s.error || ''));
    r.playingMs = Math.round(performance.now() - t0);
    await wait(() => firstElFrame != null || s.startupMarks.some(m => /first-draw/.test(m[0])), 5000);
    clearInterval(elWatch);
    const fd = s.startupMarks.find(m => /first-draw/.test(m[0]));
    r.firstFrameMs = firstElFrame != null ? Math.round(firstElFrame) : fd ? fd[1] + Math.round((s.markT0 - (performance.timeOrigin + t0))) : null;
    r.startupMarks = s.startupMarks;
    log(`playing after ${r.playingMs} ms, first frame ${r.firstFrameMs} ms`);

    // ---- steady playback
    const dur = s.durationUs / 1e6;
    const steadyS = Math.max(5, Math.min(secs, dur - 6));
    await sleep(2000);   // startup settles, stats arrive
    r.route = route(s);
    r.wall.steadyStart = Date.now(); r.loadedAtSteady = document.readyState === 'complete';
    maxGap = 0; gapsOver50 = 0; longTasks = 0; longTaskMs = 0;
    const st0 = performance.now(), f0 = frames(s), c0 = s.currentTimeUs(), u0 = s.stats.audio?.underruns || 0;
    const series = [];
    let at10 = null, stage3 = null;
    while (performance.now() - st0 < steadyS * 1000) {
      await sleep(1000);
      const t = (performance.now() - st0) / 1000, f = frames(s), v = s.stats.video || {}, a = s.stats.audio || {};
      const row = { t: +t.toFixed(1), clockS: +((s.currentTimeUs() - c0) / 1e6).toFixed(2), shown: f.shown - f0.shown, dropped: f.dropped - f0.dropped, queue: v.queue ?? null, avMeanMs: v.avMeanMs != null ? +v.avMeanMs.toFixed(1) : null, ringMs: a.ringMs ?? null, under: (a.underruns || 0) - u0, memMB: mem(s).sumMB, state: s.state };
      { const d = s.stats.demux || {}, n = d.net || {}, m = v.mse || {}, la = Object.values(d.lanes || {}).find(l => l.kind === 'audio') || {}; Object.assign(row, { netPosMB: n.pos != null ? +(n.pos / 1048576).toFixed(1) : null, netAheadMB: n.ahead != null ? +(n.ahead / 1048576).toFixed(1) : null, netWaitMs: n.waitMs ?? null, pendMB: d.pendingBytes != null ? +(d.pendingBytes / 1048576).toFixed(1) : null, aPend: la.pending ?? null, aInq: a.inq ?? null, dbg: d.dbg, sent: d.packetsSent, read: d.packetsRead, vAheadMs: m.video?.aheadMs ?? null, aAheadMs: m.audio?.aheadMs ?? null }); }
      series.push(row);
      if (trace && s.mseEl) { const el = s.mseEl, m = v.mse || {}; tlog(`el t=${el.currentTime.toFixed(2)} rs=${el.readyState} paused=${el.paused} buf=${JSON.stringify([...Array(el.buffered.length)].map((_, i) => [+el.buffered.start(i).toFixed(2), +el.buffered.end(i).toFixed(2)]))} mse=${JSON.stringify({ v: m.video, a: m.audio })}`); }
      if (!at10 && t >= 10) at10 = { t: row.t, mem: mem(s) };
      if (!stage3 && t >= 3) stage3 = { t, snap: stageSnap(s) };
      if (s.state === 'ended' || s.state === 'error') break;
    }
    r.wall.steadyEnd = Date.now();
    if (stage3) r.stages = stageDiff(stage3.snap, stageSnap(s), (performance.now() - st0) / 1000 - stage3.t);
    const wallS = (performance.now() - st0) / 1000, f1 = frames(s), v = s.stats.video || {}, a = s.stats.audio || {};
    const fps = s.vTrack?.fps || s.vTrack?.frameRate || null;
    r.steady = {
      wallS: +wallS.toFixed(2), clockS: +((s.currentTimeUs() - c0) / 1e6).toFixed(2), shown: f1.shown - f0.shown, dropped: f1.dropped - f0.dropped,
      trackFps: fps, shownFps: +((f1.shown - f0.shown) / wallS).toFixed(2),
      av: { meanMs: v.avMeanMs ?? null, p95Ms: v.avP95Ms ?? null, p95AllMs: v.avP95AllMs ?? null, p99AllMs: v.avP99AllMs ?? null, maxMs: v.avMaxHistMs ?? null, samples: v.avCountAll ?? v.avSamples ?? null },
      mse: v.mse ? { seeks: v.mse.seeks, rateNudges: v.mse.rateNudges, recoveries: v.mse.recoveries, gapJumps: v.mse.gapJumps, quotaHits: v.mse.video?.quotaHits ?? null, videoAheadMB: v.mse.video?.aheadMB ?? null, videoBuffered: v.mse.video?.buffered ?? null } : null,
      audio: { underruns: (a.underruns || 0) - u0, ringMs: a.ringMs ?? null, decoder: a.decoder ?? null },
      decodeErrors: v.decodeErrors ?? 0, recoveries: v.recoveries ?? 0,
      mainThread: { maxGapMs: Math.round(maxGap), gapsOver70Ms: gapsOver50, longTasks, longTaskMs: Math.round(longTaskMs) },
      memAt10: at10, memEnd: { t: +wallS.toFixed(1), mem: mem(s) },
      state: s.state,
    };
    // What the renderer's script side holds, by owner (Chrome's own accounting; shared buffers count where they were made). What the
    // process holds beyond this is media: SourceBuffers, decoder surfaces, frames.
    if (performance.measureUserAgentSpecificMemory && self.crossOriginIsolated) {
      const m = await Promise.race([performance.measureUserAgentSpecificMemory().catch(() => null), sleep(25000).then(() => null)]);
      if (m) r.steady.uaMemory = { totalMB: MB(m.bytes), parts: m.breakdown.filter(b => b.bytes > 262144).sort((x, y) => y.bytes - x.bytes).slice(0, 12)
        .map(b => ({ MB: MB(b.bytes), types: b.types.join('+'), owner: b.attribution.map(a => `${a.scope.replace('GlobalScope', '')}:${a.url.startsWith('blob:') ? 'blob' : a.url.split('?')[0].split('/').pop()}`).join(',') })) };
    }
    r.steady.memGrowthMB = at10 ? +(r.steady.memEnd.mem.sumMB - at10.mem.sumMB).toFixed(1) : null;
    r.series = series;
    r.snapEnd = { video: s.stats.video, audio: s.stats.audio, demux: s.stats.demux, source: s.sourceInfo, warnings: s.warnings };
    log(`steady ${wallS.toFixed(0)} s: shown ${r.steady.shown}, dropped ${r.steady.dropped}`);

    // ---- seeks: near (within the buffered neighbourhood) and far
    if (q.get('seeks') !== '0') {
      r.wall.seeksStart = Date.now();
      r.seeks = [];
      const now = () => s.currentTimeUs() / 1e6;
      const clampT = t => Math.max(0.5, Math.min(dur - 8, t));
      // ?seekat=<s>: the pass starts when playback reaches that position, so this page and the floor page seek from the same place
      // to the same targets (the steady phase ends at a different position in each).
      if (q.has('seekat')) await wait(() => now() >= num('seekat', 0) || s.state !== 'playing', 90000);
      // The same plan as perf-floor.html. 'start' is a seek to 0 from far away: the head of the file, not a clamped 0.5 s.
      const plan = [['near', 'forward 10 s', () => now() + 10], ['near', 'back 5 s', () => now() - 5], ['near', 'forward 30 s', () => now() + 30], ['far', 'forward', () => dur * 0.8], ['far', 'back', () => dur * 0.15],
        ['far', 'forward', () => dur * 0.55], ['far', 'back', () => dur * 0.05], ['far', 'forward', () => dur * 0.7], ['start', 'back to 0', () => 0]];
      for (const [kind, dir, where] of plan) {
        const target = kind === 'start' ? 0 : clampT(where());
        const el = s.mseEl;
        let frameAt = null, frameUs = null;
        const ts = performance.now(), mark0 = s.startupMarks.length;
        s.seek(target * 1e6); s.play();
        const epoch = s.epoch;
        if (el) {
          // The first picture at the target. The element presents it while `seeking` is still true (the flag falls once it can play on),
          // so the flag is not asked for a picture within 100 ms of the target; a later one (a picture of the old position, when the
          // target lies just before it) counts only after the seek has landed.
          const cb = (n, md) => { if (s.epoch !== epoch) return; const us = md.mediaTime * 1e6 - (s.mse?.offsetUs || 0), d = us - target * 1e6; if (d >= -50000 && (d < 100000 || (d < 3e6 && el.seeking === false))) { frameAt = performance.now() - ts; frameUs = us; } else el.requestVideoFrameCallback(cb); };
          el.requestVideoFrameCallback(cb);
        }
        const back = await wait(() => s.state === 'playing' || s.state === 'ended' || s.state === 'error', 30000);
        const playingMs = Math.round(performance.now() - ts);
        if (el) await wait(() => frameAt != null, 5000);
        const x = { kind, dir, targetS: +target.toFixed(1), playingMs, frameMs: el ? (frameAt == null ? null : Math.round(frameAt)) : playingMs, state: s.state, ok: back && s.state === 'playing' };
        const fa = frames(s);
        await sleep(2000);
        const fb = frames(s), vv = s.stats.video || {};
        x.shown2s = fb.shown - fa.shown; x.dropped2s = fb.dropped - fa.dropped; x.advancedS = +(now() - target).toFixed(2);
        x.firstDeltaMs = vv.ep && vv.ep.epoch === s.epoch && vv.ep.first != null ? +((vv.ep.first - s.seekTargetUs) / 1000).toFixed(1) : null;
        // The picture that counted: how far after the target it is (a frame or less when the seek is exact), and the timeline of this
        // seek from every thread, in ms after the call.
        x.frameAfterTargetMs = frameUs == null ? null : +((frameUs - target * 1e6) / 1000).toFixed(1);
        { const base = performance.timeOrigin + ts - s.markT0; x.marks = s.startupMarks.slice(mark0).filter(m => m[1] - base < (frameAt ?? playingMs) + 200).map(m => [m[0], Math.round(m[1] - base)]); }
        r.seeks.push(x);
        log(`seek ${kind} to ${x.targetS}: playing ${x.playingMs} ms, frame ${x.frameMs} ms`);
        if (s.state === 'error') break;
      }
      r.wall.seeksEnd = Date.now();
      r.memAfterSeeks = mem(s);
      r.openFramesAfterSeeks = s.stats.video?.openFrames ?? null;
    }
  } catch (e) { r.errors.push(String(e && e.stack || e)); }
  try { const res = await s.stop(); r.accounting = res?.accounting ?? null; } catch (e) { r.errors.push('stop: ' + e); }
  r.pageErrors = pageErrors; r.workerOrder = workerOrder; r.wall.end = Date.now(); r.loadEventMs = loadEventMs;
  out.textContent = JSON.stringify({ ...r, series: undefined, snapEnd: undefined, startupMarks: undefined }, null, 1).slice(0, 8000);
  await fetch('/results', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ test: 'perf', userAgent: navigator.userAgent, href: location.href, result: r }) }).catch(() => {});
  window.perfDone = true;
}
main();
