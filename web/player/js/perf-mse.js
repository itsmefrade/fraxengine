// SPDX-License-Identifier: LGPL-3.0-only
// Bisect page between the floor (perf-floor.html) and the engine (perf.html): Media Source Extensions with nothing around them.
// A fragmented MP4 that already holds both tracks is read in byte ranges and appended unchanged into one SourceBuffer, so there is
// no demuxer, no muxer, no audio worker and no session. What this page costs above the floor is what MSE itself costs in Chrome;
// what the engine costs above this page is the engine's (docs/perf/CORE-V2.md, G).
//   ?src=zzlocal/ovh/x.mp4  mime=video/mp4; codecs="..."  label=name  secs=30
//   ms=worker (default, MediaSource in a worker and its handle on the element, as the engine does) | main
//   chunk=<bytes per append, default 2097152>  ahead=<seconds buffered ahead, default 10>
// This file is the page's module and, with ?feed in its own URL, the worker.
const inWorker = typeof document === 'undefined';

// Reads the file in ranges and appends while less than `ahead` seconds are buffered past the position; removes what lies more
// than 10 s behind. `position` returns the element's current time (posted by the page when the MediaSource lives in a worker).
async function feed(ms, o, position) {
  if (ms.readyState !== 'open') await new Promise(r => ms.addEventListener('sourceopen', r, { once: true }));
  const sb = ms.addSourceBuffer(o.mime);
  const done = () => new Promise((res, rej) => { sb.addEventListener('updateend', res, { once: true }); sb.addEventListener('error', () => rej(new Error('SourceBuffer error')), { once: true }); });
  let off = 0, appends = 0, lastTrim = 0;
  for (;;) {
    const t = position(), end = sb.buffered.length ? sb.buffered.end(sb.buffered.length - 1) : 0;
    if (end - t > o.ahead) {
      if (t - lastTrim > 5 && sb.buffered.length && sb.buffered.start(0) < t - 10) { lastTrim = t; sb.remove(0, t - 10); await done(); }
      await new Promise(r => setTimeout(r, 250));
      continue;
    }
    const r = await fetch(o.src, { headers: { Range: `bytes=${off}-${off + o.chunk - 1}` } });
    if (r.status === 416) break;
    const buf = await r.arrayBuffer();
    if (!buf.byteLength) break;
    sb.appendBuffer(buf); await done();
    off += buf.byteLength; appends++;
    if (buf.byteLength < o.chunk) break;
  }
  try { ms.endOfStream(); } catch {}
  return appends;
}

if (inWorker) {
  let pos = 0;
  self.onmessage = e => {
    const m = e.data;
    if (m.type === 'position') { pos = m.t; return; }
    const ms = new MediaSource();
    self.postMessage({ type: 'handle', handle: ms.handle }, [ms.handle]);
    feed(ms, m, () => pos).catch(err => self.postMessage({ type: 'error', message: String(err) }));
  };
} else {
  const q = new URLSearchParams(location.search);
  const secs = Number(q.get('secs') || 30);
  const o = { src: new URL(q.get('src'), location.href).href, mime: q.get('mime'), chunk: Number(q.get('chunk')) || 2 << 20, ahead: Number(q.get('ahead')) || 10 };
  const v = document.createElement('video');
  v.muted = false; v.playsInline = true;
  document.getElementById('host').append(v);
  const t0 = performance.now();
  const res = { label: q.get('label') || 'mse', src: q.get('src'), bare: true, wall: { open: Date.now() }, errors: [], route: { video: `bare MSE (${q.get('ms') === 'main' ? 'main thread' : 'worker'})`, quality: 'none', audio: 'browser' } };
  let loadEventMs = null, posted = false;
  addEventListener('load', () => { loadEventMs = Math.round(performance.now()); });
  const post = () => {
    if (posted) return; posted = true;
    res.loadEventMs = loadEventMs;
    document.getElementById('out').textContent = JSON.stringify(res, null, 1);
    fetch('/results', { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ test: 'perf', result: res }) });
  };
  const fail = e => { res.errors.push(String(e)); post(); };
  v.addEventListener('error', () => fail(v.error && v.error.message));
  if (!MediaSource.isTypeSupported(o.mime)) fail(`MSE does not accept ${o.mime}`);
  else if (q.get('ms') === 'main') {
    const ms = new MediaSource();
    v.src = URL.createObjectURL(ms);
    feed(ms, o, () => v.currentTime).catch(fail);
  } else {
    const w = new Worker(new URL('perf-mse.js?feed', import.meta.url), { type: 'module' });
    w.onmessage = e => { if (e.data.type === 'handle') { v.srcObject = e.data.handle; v.play().catch(fail); } else fail(e.data.message); };
    w.postMessage({ type: 'open', ...o });
    setInterval(() => w.postMessage({ type: 'position', t: v.currentTime }), 250);   // the engine's own cadence
  }
  v.requestVideoFrameCallback(() => { res.firstFrameMs = Math.round(performance.now() - t0); });
  v.addEventListener('playing', () => {
    if (res.wall.steadyStart) return;
    res.playingMs = Math.round(performance.now() - t0);
    res.wall.steadyStart = Date.now(); res.loadedAtSteady = document.readyState === 'complete';
    const a = v.getVideoPlaybackQuality(), c0 = v.currentTime, w0 = performance.now();
    setTimeout(() => {
      const b = v.getVideoPlaybackQuality();
      res.wall.steadyEnd = res.wall.end = Date.now();
      res.steady = { wallS: (performance.now() - w0) / 1000, clockS: v.currentTime - c0, shown: b.totalVideoFrames - a.totalVideoFrames - (b.droppedVideoFrames - a.droppedVideoFrames),
        dropped: b.droppedVideoFrames - a.droppedVideoFrames, av: {}, audio: {}, mainThread: {} };
      post();
    }, secs * 1000);
  });
  if (q.get('ms') === 'main') v.play().catch(fail);
}
