// SPDX-License-Identifier: LGPL-3.0-only
// Acceptance run: window.testResult is set to a JSON-serialisable object when finished.
import { Session } from './session.js';

const q = new URLSearchParams(location.search);
const SECS = Number(q.get('secs')) || (q.has('quick') ? 10 : 60);
const BIG = 'fixtures/fel-bl-hdr10-aac.mp4';
const SMALL = ['fixtures/fmp4-h264-aac.mp4', 'fixtures/fmp4-hevc-main10.mp4', 'fixtures/hevc-main10.mp4', 'fixtures/h264-aac.mp4'];
const host = document.getElementById('host');
if (q.has('full')) { Object.assign(host.style, { position: 'fixed', inset: '0', width: '100vw', height: '100vh', zIndex: '1' }); document.getElementById('out').style.display = 'none'; }
const out = document.getElementById('out');
const sleep = ms => new Promise(r => setTimeout(r, ms));
const log = s => { out.textContent += s + '\n'; };
window.testProgress = [];
const prog = s => { window.testProgress.push((performance.now() / 1000).toFixed(1) + ' ' + s); };

window.testResult = null;

async function waitState(s, state, ms) {
  const t0 = performance.now();
  while (performance.now() - t0 < ms) {
    if (s.state === state) return true;
    if (s.state === 'error') throw new Error('session error: ' + s.error);
    await sleep(25);
  }
  return false;
}

function mk(url) {
  const events = [], logs = [];
  const s = new Session({ url, host, wantPlay: true, onEvent: e => { if (e.type === 'log' && logs.length < 80) logs.push(e.text); if (e.type === 'log' && /WARNING|ERROR/.test(e.text)) events.push(e.text); if (e.type === 'error') events.push('ERROR ' + e.message); } });
  s.events = events; s.logs = logs; window.lastSession = s;
  return s;
}

function brief(s) {
  const v = s.stats.video || {}, a = s.stats.audio || {};
  return { quality: s.quality, qualityAuto: s.qualityAuto, bitrateMbps: s.bitrateMbps, dv: v.dv, engine: s.engine, clockMode: s.clockMode, clockSwitches: s.clockSwitches, unsupported: s.unsupported, audioInfo: s.audioInfo, audioDecoded: a.decodedFrames, audioPreroll: a.droppedPreroll, shown: v.shown, dropped: v.dropped, droppedPct: v.droppedPct, avP95AllMs: v.avP95AllMs, avP99AllMs: v.avP99AllMs, avMaxMs: v.avMaxHistMs, avSamples: v.avCountAll, fps: v.fps, openFrames: v.openFrames, decoder: v.decoder, hdr: v.hdr, frameFormat: v.frameFormat, gpu: v.gpu, underruns: a.underruns, audioRoute: s.audioRoute, audioCtx: s.audio ? { state: s.audio.ctx.state, maxCh: s.audio.maxChannelCount, outCh: s.audio.outChannels } : null };
}

async function stopAndCheck(s, r, label) {
  const res = await s.stop();
  const acc = res.accounting;
  const ok = (acc ? acc.open === 0 && acc.framesCreated === acc.framesClosed && !acc.timeout : !s.vTrack) && Object.keys(s.workers).length === 0 && s.canvas === null;
  r.stops.push({ label, ok, acc });
  return ok;
}

const pageErrors = [];
window.addEventListener('error', e => pageErrors.push('error: ' + e.message));
window.addEventListener('unhandledrejection', e => pageErrors.push('rejection: ' + (e.reason && e.reason.message || e.reason)));

function report(r, test = 'player-acceptance') {
  if (!q.has('report')) return Promise.resolve();
  return fetch('/results', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ test, userAgent: navigator.userAgent, href: location.href, result: r }) }).catch(() => {});
}

// Acceptance of one real file: play SECS seconds (sampling every 250 ms, with a stall detector), then ?seeks=N seeks
// and ?cycles=N stop -> load cycles. A freeze of the page itself shows up as a missing final report; ?progress=1 posts
// a partial report every 10 s so the last known state survives.
async function runFile() {
  document.getElementById('run').disabled = true;
  const src = q.get('src');
  const nSeeks = Number(q.get('seeks')) || 0, nCycles = Number(q.get('cycles')) || 0;
  const r = { mode: 'file', src, label: q.get('label') || '', userAgent: navigator.userAgent, headless: /Headless/i.test(navigator.userAgent) ? 'headless' : 'visible', visibility: document.visibilityState, viewport: { w: innerWidth, h: innerHeight, dpr: devicePixelRatio, screenW: screen.width, hdrMq: matchMedia('(dynamic-range: high)').matches }, secs: SECS, params: Object.fromEntries(q), errors: [], timeline: [], stalls: [], seeks: [], cycles: [], stops: [], pass: false };
  r.visLog = [[0, document.visibilityState]];
  document.addEventListener('visibilitychange', () => r.visLog.push([+(performance.now() / 1000).toFixed(1), document.visibilityState]));
  if (Number(q.get('delay'))) await sleep(Number(q.get('delay')) * 1000);
  r.visibility = document.visibilityState;
  let beat = performance.now(), maxBeat = 0;
  const hb = setInterval(() => { const n = performance.now(); maxBeat = Math.max(maxBeat, n - beat); beat = n; }, 100);
  let lastProg = 0;
  try {
    const s = mk(src);
    const t0o = performance.now();
    await s.open();
    r.openMs = Math.round(performance.now() - t0o);
    r.tracks = s.tracks.map(t => `#${t.id} ${t.kind} ${t.codec}${t.width ? ` ${t.width}x${t.height}` : ''}${t.channels ? ` ${t.channels}ch ${t.sampleRate}Hz` : ''}${t.dolbyVision ? ` DV p${t.dolbyVision.profile}${t.dolbyVision.elPresent ? '+EL' : ''}` : ''}`);
    s.play();
    const reached = await waitState(s, 'playing', 30000);
    r.reachedPlaying = reached; r.startMs = Math.round(performance.now() - t0o);
    const t0 = performance.now();
    // ?suspend=N: suspend the AudioContext for N seconds from the start of playback, then resume it. Goes through the same
    // statechange path as an autoplay-blocked context that a user gesture later resumes.
    const susp = Number(q.get('suspend')) || 0;
    if (susp && s.audio) {
      setTimeout(async () => {
        try { await s.audio.ctx.suspend(); r.suspendedAtMs = Math.round(performance.now() - t0); setTimeout(() => { s.audio?.ctx.resume().then(() => { r.resumedAtMs = Math.round(performance.now() - t0); }); }, susp * 1000); } catch (e) { r.errors.push('suspend: ' + e); }
      }, 3000);
    }
    let lastShown = -1, lastShownT = t0, lastTl = 0;
    const mediaStart = s.currentTimeUs() / 1e6;
    if (q.get('host') === 'full') { host.style.cssText = 'position:fixed;inset:0;width:auto;height:auto;z-index:5'; }
    r.loops = 0;
    while (performance.now() - t0 < SECS * 1000 && (['playing', 'buffering', 'seeking'].includes(s.state) || (q.has('loop') && s.state === 'ended'))) {
      if (s.state === 'ended') { r.loops++; s.play(); await waitState(s, 'playing', 20000); continue; }
      await sleep(250);
      const now = performance.now(), v = s.stats.video;
      if (v) {
        if (v.shown !== lastShown) { lastShown = v.shown; lastShownT = now; }
        else if (now - lastShownT > 4000 && !r.stalls.length) r.stalls.push({ atS: +((now - t0) / 1000).toFixed(1), shown: v.shown, mediaS: +(s.currentTimeUs() / 1e6).toFixed(2) });
      }
      if (now - lastTl >= 2000) {
        lastTl = now;
        r.timeline.push({ t: +((now - t0) / 1000).toFixed(1), media: +(s.currentTimeUs() / 1e6).toFixed(2), shown: v?.shown, dropped: v?.dropped, fps: v?.fps, underruns: s.stats.audio?.underruns, clock: s.clockMode, mainGapMs: Math.round(maxBeat) });
      }
      if (q.has('progress') && now - lastProg > 10000) { lastProg = now; report({ ...r, partial: true, state: s.state, nowS: +((now - t0) / 1000).toFixed(1) }, 'player-progress'); }
    }
    r.playedS = +((performance.now() - t0) / 1000).toFixed(1);
    r.mediaAdvancedS = +(s.currentTimeUs() / 1e6 - mediaStart).toFixed(2);
    r.endState = s.state;
    r.main = brief(s);
    r.warnings = [...s.warnings];
    // seeks
    const dur = s.durationUs / 1e6;
    for (let i = 0; i < nSeeks && dur > 12; i++) {
      const target = Math.max(1, Math.min(dur - 8, dur * [0.5, 0.2, 0.8, 0.35][i % 4]));
      const t1 = performance.now();
      s.seek(target * 1e6); s.play();
      const back = await waitState(s, 'playing', 20000);
      const ms = Math.round(performance.now() - t1);
      await sleep(3000);
      const m = s.currentTimeUs() / 1e6;
      const x = { target: +target.toFixed(1), backToPlaying: back, seekMs: ms, mediaAfter3s: +m.toFixed(2), ok: back && Math.abs(m - (target + 3)) < 1.5, state: s.state, underruns: s.stats.audio?.underruns };
      r.seeks.push(x);
      if (!x.ok) r.errors.push('seek failed ' + JSON.stringify(x));
    }
    r.final = brief(s);
    r.events = s.events;
    await stopAndCheck(s, r, 'main');
    for (let i = 0; i < nCycles; i++) {
      const s2 = mk(src);
      await s2.open(); s2.play();
      const ok1 = await waitState(s2, 'playing', 30000);
      await sleep(3000);
      const b = brief(s2);
      const ok2 = await stopAndCheck(s2, r, 'cycle ' + i);
      r.cycles.push({ reachedPlaying: ok1, shown: b.shown, underruns: b.underruns, stopOk: ok2, events: s2.events });
      if (!ok1 || (b.shown === undefined && !s2.unsupported?.video && s2.vTrack)) r.errors.push('cycle ' + i + ' did not play');   // a sound-only file shows no frames
    }
    const m = r.main;
    r.accept = {
      playing: !!r.reachedPlaying, noStall: r.stalls.length === 0, noErrors: r.errors.length === 0 && !(r.events || []).some(e => /ERROR/.test(e)),
      seeksOk: r.seeks.every(x => x.ok), cyclesOk: r.cycles.every(x => x.reachedPlaying && x.stopOk), noLeak: r.stops.every(x => x.ok),
    };
    r.pass = Object.values(r.accept).every(Boolean);
  } catch (e) { r.errors.push(String(e && e.stack || e)); r.log = window.lastSession?.logs; r.tracks ??= window.lastSession?.tracks?.map(t => `#${t.id} ${t.kind} ${t.codec} ${t.width || ''}x${t.height || ''} ${t.channels || ''}ch`); r.sessionError = window.lastSession?.error; }
  clearInterval(hb);
  r.maxMainThreadGapMs = Math.round(maxBeat);
  r.pageErrors = pageErrors;
  window.testResult = r;
  log(JSON.stringify(r, null, 2));
  await report(r, 'player-file');
}

// Snapshots of video + subtitle overlay at given media times (?times=10.2,36,51&sub=0). Each is posted as its own result with the PNG
// as a data URL (web/results/*.json); a session per time starts a little before it so the display set that is on screen is decoded.
async function runSnap() {
  document.getElementById('run').disabled = true;
  const src = q.get('src'), times = (q.get('times') || '').split(',').map(Number).filter(x => x >= 0);
  const lead = Number(q.get('lead')) || 2.5;
  const out = { mode: 'snap', src, label: q.get('label') || '', snaps: [], errors: [], quality: q.get('q') };
  try {
    for (const t of times) {
      const s = new Session({ url: src, host, wantPlay: true, startUs: Math.max(0, (t - lead)) * 1e6, subtitle: q.has('sub') ? Number(q.get('sub')) : null, quality: q.get('q') });
      window.lastSession = s;
      await s.open();
      s.play();
      const t0 = performance.now();
      while (s.currentTimeUs() < t * 1e6 && performance.now() - t0 < 20000) { if (s.state === 'error') throw new Error(s.error); await sleep(20); }
      if (s.currentTimeUs() < t * 1e6) out.stuck = { t, state: s.state, error: s.error, warnings: s.warnings, video: s.stats.video && { shown: s.stats.video.shown, dropped: s.stats.video.dropped, queue: s.stats.video.queue, inq: s.stats.video.inq, clockUs: s.stats.video.clockUs, dv: s.stats.video.dv }, demux: s.stats.demux };
      await sleep(150);
      const mediaS = s.currentTimeUs() / 1e6;
      const png = await s.snapshotPng();
      const sub = s.snapshot().subtitle;
      out.snaps.push({ t, mediaS, frameTsUs: s.lastSnapTs, ok: !!png, bytes: png ? png.length : 0, sub });
      if (png) await fetch('/results', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ test: 'player-snapshot', label: (q.get('label') || 'snap') + '-' + t, t, mediaS, frameTsUs: s.lastSnapTs, headless: /Headless/i.test(navigator.userAgent) ? 'headless' : 'visible', png }) }).catch(() => {});
      await s.stop();
    }
  } catch (e) { out.errors.push(String(e && e.stack || e)); }
  out.pass = out.errors.length === 0 && out.snaps.every(x => x.ok);
  window.testResult = out;
  log(JSON.stringify(out, null, 2));
  report(out, 'player-snap');
}

// Track menu: subtitle on/off, forced-only, audio track switch while playing (no stop). ?src=...&secs=
async function runTracks() {
  document.getElementById('run').disabled = true;
  const src = q.get('src');
  const r = { mode: 'tracks', src, label: q.get('label') || '', steps: [], errors: [], pass: false };
  try {
    const s = mk(src);
    await s.open(); s.play();
    await waitState(s, 'playing', 20000);
    await sleep(2500);
    const step = async (name, fn) => {
      const t0 = s.currentTimeUs() / 1e6, sh0 = s.stats.video?.shown || 0, st0 = s.state;
      const w0 = performance.now(); await fn();
      await sleep(Number(q.get('settle')) || 3000);
      const t1 = s.currentTimeUs() / 1e6;
      r.steps.push({ name, stateBefore: st0, stateAfter: s.state, mediaBefore: +t0.toFixed(2), mediaAfter: +t1.toFixed(2), wallS: +((performance.now() - w0) / 1000).toFixed(2), advanced: +(t1 - t0).toFixed(2), shownDelta: (s.stats.video?.shown || 0) - sh0, audioIndex: s.snapshot().audioIndex, sub: s.snapshot().subtitle, underruns: s.stats.audio?.underruns });
    };
    const nSub = s.subtitleTracks().length, nAud = s.audioTracks.length;
    r.counts = { subtitles: nSub, audio: nAud };
    if (nSub) { await step('subtitle on', () => s.setSubtitle(0)); await step('forced only on', () => s.setSubtitleForcedOnly(true)); await step('forced only off', () => s.setSubtitleForcedOnly(false)); await step('subtitle off', () => s.setSubtitle(null)); }
    if (nAud > 1) { await step('audio ->1', () => s.switchAudio(1)); await step('audio ->0', () => s.switchAudio(0)); }
    r.stopped = (await s.stop()).accounting;
    r.pass = r.steps.every(x => x.stateAfter === 'playing' && x.advanced > 1.5);
    r.events = s.events;
  } catch (e) { r.errors.push(String(e && e.stack || e)); }
  window.testResult = r; log(JSON.stringify(r, null, 2)); report(r, 'player-tracks');
}

async function run() {
  if (q.get('mode') === 'snap') return runSnap();
  if (q.get('mode') === 'tracks') return runTracks();
  if (q.get('mode') === 'file') return runFile();
  document.getElementById('run').disabled = true;
  const r = { userAgent: navigator.userAgent, secs: SECS, errors: [], files: [], stops: [], seeks: [], cycles: [], big: null, pass: false };
  try {
    // 1) small fixtures play to the end
    for (const f of SMALL) {
      prog('small ' + f); const s = mk(f);
      await s.open(); prog('opened');
      s.play();
      const ended = await waitState(s, 'ended', 8000);
      prog('ended=' + ended + ' state=' + s.state); const b = brief(s);
      r.files.push({ f, ended, ...b, events: s.events });
      await stopAndCheck(s, r, 'small ' + f); prog('stopped');
      if (!ended || !b.shown) r.errors.push(`small fixture ${f}: ended=${ended} shown=${b.shown}`);
    }
    // 2) big file: play SECS seconds, then three seeks, then stop
    {
      const s = mk(BIG);
      await s.open();
      s.play();
      if (!(await waitState(s, 'playing', 8000))) r.errors.push('big: never reached playing (state ' + s.state + ')');
      const t0 = performance.now();
      let maxFps = 0;
      while (performance.now() - t0 < SECS * 1000 && s.state === 'playing') { await sleep(250); maxFps = Math.max(maxFps, s.stats.video?.fps || 0); }
      const main = brief(s);
      main.playedS = (performance.now() - t0) / 1000; main.endState = s.state; main.mediaTimeS = s.currentTimeUs() / 1e6;
      r.big = main;
      for (const target of [10, 30, 5]) {
        const before = s.stats.video?.dropped || 0;
        s.seek(target * 1e6);
        const back = await waitState(s, 'playing', 8000);
        await sleep(3000);
        const t = s.currentTimeUs() / 1e6;
        r.seeks.push({ target, backToPlaying: back, mediaTimeAfter3s: +t.toFixed(2), ok: back && Math.abs(t - (target + 3)) < 1.0, droppedDelta: (s.stats.video?.dropped || 0) - before, state: s.state });
        if (!r.seeks.at(-1).ok) r.errors.push('seek to ' + target + ' failed: ' + JSON.stringify(r.seeks.at(-1)));
      }
      r.bigFinal = brief(s); r.bigEvents = s.events;
      await stopAndCheck(s, r, 'big');
    }
    // 3) three stop -> load cycles (alternating 4K and small)
    for (let i = 0; i < 3; i++) {
      const f = i % 2 === 0 ? BIG : SMALL[0];
      const s = mk(f);
      await s.open();
      s.play();
      const ok1 = await waitState(s, 'playing', 8000);
      await sleep(2000);
      const b = brief(s);
      const ok2 = await stopAndCheck(s, r, 'cycle ' + i);
      r.cycles.push({ f, reachedPlaying: ok1, shown: b.shown, stopOk: ok2, events: s.events });
      if (!ok1 || !b.shown) r.errors.push('cycle ' + i + ' did not play');
    }
    const big = r.big || {};
    r.accept = {
      dropPctUnder0_1: (big.droppedPct ?? 100) < 0.1,
      avP95Under40: (big.avP95AllMs ?? 999) <= 40,
      seeksOk: r.seeks.length === 3 && r.seeks.every(x => x.ok),
      cyclesOk: r.cycles.length === 3 && r.cycles.every(x => x.reachedPlaying && x.stopOk),
      noLeakedFrames: r.stops.length > 0 && r.stops.every(x => x.ok),
      noErrors: r.errors.length === 0,
    };
    r.pass = Object.values(r.accept).every(Boolean);
  } catch (e) {
    r.errors.push(String(e && e.stack || e));
  }
  window.testResult = r;
  log(JSON.stringify(r, null, 2));
  // Lets a run started without the debugger attached report back (web/server.py POST /results).
  report(r);
}

document.getElementById('run').onclick = run;
window.runPlayerTest = run;
if (q.has('auto') || navigator.userActivation?.hasBeenActive) run();
