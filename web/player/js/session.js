// SPDX-License-Identifier: LGPL-3.0-only
// Session: one playback of one source. Owns the workers, the AudioContext and the canvas, and the epoch counter.
//
// Lifecycle rules (learned the hard way):
//  - A Session is single-use. Loading another source means stop() the old one and make a new Session; nothing is
//    reset in place, so no stale decoder/GPU/audio state can leak between loads.
//  - Seek and stop bump `epoch`. Every worker message that carries media state is tagged and dropped when stale.
//  - stop() asks each worker to close its decoder/GPU device (and report its VideoFrame accounting), then
//    terminates the workers, closes the AudioContext and removes the canvas.
import { isWebKit, isHandheld, MSE, mseInWorker, mseAudioSupported, nativeDolbyVision, dolbyVisionBaseLayer } from './platform.js';
import { MainVideoHost } from './main-video-host.js';
import { armMediaUnlock, unlockedElement, takeAudioContext, returnAudioContext } from './media-unlock.js';
import { CTL, allocRing, audioClockUs } from './protocol.js';
import { netCushionS } from './net-policy.js';
import { snapChannels } from './audio/mix.js';
import { h264SpsFacts } from './demux/annexb.js';
import { hevcCodecString } from '../../engine/demuxer.mjs';
import { isVc1Track } from '../../engine/vc1.mjs';
import { createSubtitleRenderer } from '../../subs/renderer.mjs';
import { hvccHdrTransfer } from '../../fel/sps-range.mjs';
import { holdsTarget, elementRanges, downloadedReachS } from './seek-policy.js';

const wurl = name => new URL(`./${name}`, import.meta.url);

// WebKit, element with an audio track, no audio output rendering (closed lid, macOS 27.2): the element's audio queue start hangs and
// CoreAudio gives up after 15.000 s (AQMEIO "timed out", then -66681). Tearing that element down parks the GPU process's main thread
// until then (no line from it from 00:07:30.075 to 00:07:37.13, the moment the queue timed out). A new element attached inside that
// window gets RemoteAudioVideoRendererProxyManager_AddTrack rejected as invalid, and the GPU process has WebKit kill the page
// (reason RequestedByGPUProcess, shown as a crash with no crash report). Measured in one page, 4K HEVC or HDR10+ with sound: second
// source 6 s or 10 s after the first play died (4 of 4), 15.3 s and later lived (3 of 3), video only lived. So the next element waits.
const WEBKIT_AUDIO_START_LIMIT_MS = 16000;   // the 15 s timeout, counted from play(), which comes slightly before the queue start
// A stop this soon after play() says nothing about the queue: a healthy start takes well under this, and a wrong verdict costs a healthy
// quick skip up to 14 s of waiting.
const WEBKIT_AUDIO_START_JUDGE_MS = 2000;
let webkitElementsBlockedUntil = 0;

const NET_FIRST_S = 1;          // cushion before the first start of remote media. Near the bitrate (55.7 Mbit/s file, 40 to 70 Mbit/s link, 5 far seeks with 20 s of play each): 3 to 4 s took 5.3 to 8.8 s a seek, 1 s took 3.2 to 3.7 s, both no rebuffer; 0.25 s took 2.7 to 3.2 s and rebuffered twice, 18 s in all
const NET_REBUFFER_S = 4;       // where the doubling after a dry ring starts: a link that really is too slow still gets long waits
const NET_SEEK_S = 1;           // after a seek, see NET_FIRST_S
const NET_MAX_S = 32;           // a rebuffer doubles the cushion up to this (the ring's size can end a wait earlier)
const NET_HOLD_MAX_MS = 45000;  // a link that cannot even fill the target plays what it has rather than sitting silent forever
const NET_LOW_AUDIO_MS = 1000;  // audio left (the worker keeps about 2.5 s) that counts as running dry
const NET_FAST_AUDIO_MS = 800;   // audio ring wanted before a start on a small cushion: audio is what the clock runs on, and the ring starts at 0.4 s and refills only from the link's surplus
const NET_LOW_START_MS = 250;   // the same while the audio ring is still filling for the first time (it starts on 0.4 s): the demuxer blocks on the network most of the time on any link at this point
const NET_START_GRACE_MS = 1000;   // after a start: the last audio statistics may predate it (a ring that was still empty reads as dry)
const SOFT_SEEK_LIMIT_MS = 4000;   // a seek inside the element's buffer that has not landed by then starts over (4K with 10 s GOPs lands in 50 to 300 ms)
const NET_STARVED_FRAC = 0.5;   // share of the last stats interval the demuxer spent blocked on the network

// WebCodecs (or nothing) must be able to decode the video track; the engine does not decode video.
async function videoUnsupportedReason(t) {
  if (typeof VideoDecoder === 'undefined') return 'WebCodecs VideoDecoder unavailable';
  const base = { codedWidth: t.width, codedHeight: t.height };
  if (t.description) base.description = t.description;
  // VC-1 has no WebCodecs decoder in any browser: the engine's Rust decoder (web/engine/vc1.mjs) in WASM takes it
  if (isVc1Track(t)) return null;
  const codecs = [];
  for (const c of [t.dolbyVisionCodecString, t.fallbackCodecString, t.codec]) {
    if (!c || codecs.includes(c)) continue;
    codecs.push(c);
    if (/^hvc1\..*\.H/.test(c)) codecs.push(c.replace('.H', '.L'));
  }
  for (const codec of codecs) {
    try { if ((await VideoDecoder.isConfigSupported({ ...base, codec })).supported) return null; } catch { /* not a WebCodecs codec string */ }
  }
  return 'no WebCodecs decoder in this browser';
}

// The demuxer of the title a play is expected on, opened before Play: the header, the Cues in the last megabyte and the resume position are
// all read, and the ring holds WARM_FILL_BYTES from there. Each of those cost a CDN first byte (0.15 to 3 s) inside the opening. Only one is
// kept, and the download stops at the cap, so a hover that leads nowhere has cost a few megabytes.
const WARM_FILL_BYTES = 16 << 20, WARM_OPEN_TTL_MS = 120000;
let warmOpen = null;
function dropWarmOpen() {
  if (!warmOpen) return;
  clearTimeout(warmOpen.timer);
  warmOpen.worker.terminate();
  warmOpen = null;
}
export function warmSourceOpen(url, size, startUs = 0) {
  if (!url || !size || !self.crossOriginIsolated) return;
  if (warmOpen?.url === url && warmOpen.startUs === startUs) return;
  dropWarmOpen();
  const worker = spare.demux || new Worker(wurl('demux-worker.js'), { type: 'module', name: 'demux' });
  spare.demux = null;
  const entry = warmOpen = { url, startUs, worker, msgs: [], timer: setTimeout(dropWarmOpen, WARM_OPEN_TTL_MS) };
  worker.onmessage = e => { if (e.data.type === 'error') { if (warmOpen === entry) dropWarmOpen(); } else if (e.data.type !== 'stats') entry.msgs.push(e.data); };
  worker.onerror = () => { if (warmOpen === entry) dropWarmOpen(); };
  worker.postMessage({ type: 'open', url, headers: {}, sourceOpts: { size, requireRange: true, fillLimit: WARM_FILL_BYTES }, startUs, demux: 'engine' });
}
function takeWarmOpen(url) {
  if (warmOpen?.url !== url) { dropWarmOpen(); return null; }
  const entry = warmOpen; warmOpen = null;
  clearTimeout(entry.timer);
  return entry;
}

// Workers booted ahead of a play the host expects (a hover on a title): each costs 250 ms or more of revalidated modules, the video one its GPU device too.
const spare = { demux: null, video: null };
export function warmWorkers() {
  if (!spare.demux) { spare.demux = new Worker(wurl('demux-worker.js'), { type: 'module', name: 'demux' }); spare.demux.onmessage = () => {}; }
  if (!spare.video) { spare.video = new Worker(wurl('video-worker.js'), { type: 'module', name: 'video' }); spare.video.onmessage = () => {}; spare.video.postMessage({ type: 'preboot' }); }
}

// Element route watchdog: stuck means every track holds this much ahead and the position has not moved for this long.
const STUCK_FULL_S = 4, STUCK_MS = 8000;

export class Session {
  #snapResolve = null;
  constructor({ file = null, url = null, host, onEvent = () => {}, wantPlay = false, quality = null, startUs = 0, audioIndex = null, subtitle = null, headers = null, size = null, requireRange = false }) {
    this.file = file; this.url = url ? new URL(url, document.baseURI).href : null; this.host = host; this.onEvent = onEvent;
    // Extra request headers for every media request of a {url} source (e.g. Authorization), so credentials stay out of the URL.
    this.headers = headers ? { ...headers } : null;
    // size: the file length when the caller knows it (a cross-origin 206 may hide Content-Range); requireRange: a server without
    // Range support is an error instead of a whole-file download.
    this.sourceOpts = { size: size || null, requireRange: !!requireRange };
    this.wantPlay = wantPlay;
    this.startUs = startUs;
    this.qualityRequest = quality;
    this.audioIndex = audioIndex;
    this.quality = 'native';
    this.subIndex = null; this.subForced = false; this.subRenderer = null; this.subCanvas = null;
    this.subInitial = subtitle;
    this.epoch = 0;
    this.state = 'idle';
    this.workers = {};
    this.waiters = [];
    this.stats = { video: null, audio: null, demux: null };
    this.finalAccounting = null;
    this.durationUs = 0;
    this.tracks = [];
    this.audio = null;          // {ctx, node, ring, ctl, sampleRate, outChannels, baseUs}
    this.endedFlags = { video: false, audio: false };
    this.pendingReady = new Set();
    this.readyInfo = {};
    this.latencyTimer = 0;
    this.seekTargetUs = 0;
    this.warnings = [];
    this.lastClockUs = 0;
    this.qs = new URLSearchParams(location.search);
    this.unsupported = {};
    this.clockMode = 'none';
    this.clockSwitches = [];
    this.gestureHandler = null;
    this.engine = '';
    this.outputMode = this.qs.get('outmode') === 'headphones' ? 'headphones' : 'speakers';
    this.startupMarks = [];     // [name, ms since open()]: the opening timeline, workers included (absolute clocks, so they compare across threads)
  }

  // Quality of the Dolby Vision / HDR10+ compositor path: native | fast | high | reference | auto (default).
  // auto (see #chooseQuality): every Dolby Vision profile (P5, P7 FEL and MEL, P8) and HDR10+ run fast on the hardware decoder;
  // other HEVC is native, and HDR HEVC that would reach a canvas through Chrome's import goes to the compositor too. WebKit runs high.
  // Decoder workers of the bit exact pool. Parallel instances compete for memory bandwidth: Rangers FEL (19.6 Mbit/s, 24 fps)
  // took 100 % of a core with 2 workers, 106 % with 3 and 127 % with 5, all in real time (one worker decodes its 4K base layer
  // at about 87 fps). Bit rate drives the decode cost (CABAC), so heavier streams get more workers.
  #exactWorkers() {
    const mbps = this.bitrateMbps, max = Math.max(2, Math.min(5, (navigator.hardwareConcurrency || 4) - 3));
    if (mbps == null) return max;
    return Math.min(max, mbps <= 25 ? 2 : mbps <= 45 ? 3 : max);
  }

  // Entropy threads of the GPU path's base layer (WPP rows; ?entropythreads=N overrides, 1 turns them off). One thread decodes
  // a 20 Mbit/s 4K FEL base layer in 5 to 9 ms per picture; the 91 Mbit/s awaken stream needed 43 ms on one thread (19 fps,
  // 14 dropped in 11 s) and 21 ms on 3 (24 fps, 0 dropped). Below 40 Mbit/s threads only add CPU.
  #entropyThreads() {
    const q = Number(this.qs.get('entropythreads'));
    if (q > 0) return q;
    return this.bitrateMbps != null && this.bitrateMbps > 40 && (navigator.hardwareConcurrency || 4) >= 6 ? 3 : 1;
  }

  #chooseQuality(t, info) {
    const want = this.qualityRequest || this.qs.get('q') || 'auto';
    const hevc = /^(hvc1|hev1|dvh1|dvhe)/.test(t.codec);
    const dv = t.dolbyVision;
    const mbps = info.durationUs && info.source?.size ? (info.source.size * 8) / (info.durationUs / 1e6) / 1e6 : null;
    this.bitrateMbps = mbps;
    let q = want;
    if (want === 'auto') {
      if (!hevc) q = 'native';
      // Profile 5: hardware decode and one render pass on the imported picture (94 % of a core down to about 13 % at 4K60,
      // docs/perf/CORE-V2.md E). ?p5=wasm keeps the exact WASM base layer (comparison, bit exact codes); dv-path.js falls back to it by
      // itself when no hardware HEVC decoder takes the stream.
      else if (dv?.profile === 5) q = this.qs.get('p5') === 'wasm' ? 'high' : 'fast';
      // FEL plays on the two hardware decoders: 8.8 % of a renderer core on rangers-fel and 7.1 % on fel-v6, against 38.5 % and
      // 24 % for the bit exact route (both layers entropy decoded in WASM, docs/perf/CORE-V2.md F). About 60 dB against the bit
      // exact picture; ?q=reference or the quality menu selects that one.
      else if (dv?.profile === 7 && dv.elPresent) q = 'fast';
      // HDR10+ stays on the canvas (scene metadata, ?hdr10plus=video for the element); the picture there is the ST 2094-40 scene curve, see hdr10plusDisplayBlock.
      else if (dv || (t.hdr10plus && this.qs.get('hdr10plus') !== 'video')) q = 'fast';
      else q = 'native';
      this.qualityAuto = q;
    }
    if (isWebKit && this.#webkitNative(t) && !this.mseDisabled && this.qs.get('mse') !== '0') q = 'native';
    if (this.#baseLayerOnly(t) && !this.mseDisabled && this.qs.get('mse') !== '0') q = 'native';
    if (!hevc || !navigator.gpu) q = 'native';
    // HDR HEVC that would reach the canvas through Chrome's own import goes through the compositor instead (?nativehdr=import keeps
    // the import). Chrome 157 converts a PQ / HLG VideoFrame in importExternalTexture to SDR: HDR screen capture of a paused HDR10
    // frame read p10 0.43 / p99 0.87 / max 0.98 where the source is 0.06 / 1.78 / 2.7 (washed out and clamped), the same for HDR10+,
    // DV P8.1 with q=native and HLG. The compositor decodes PQ / HLG from the relabelled code values and matched the source within
    // 0.02 up to p99 on all of them. Cost: about 5 points more (renderer + GPU, 4K24 HDR10, A B x3). MSE (a real <video>) stays native.
    if (q === 'native' && hevc && navigator.gpu && this.qs.get('nativehdr') !== 'import' && !this.#mseEligible(t, 'native')
      && (dv || t.hdr10plus || hvccHdrTransfer(t.description))) {
      q = isWebKit ? 'high' : 'fast';   // WebKit's WebCodecs HEVC is 8 bit, so the compositor needs the exact software decode there
      this.nativeHdrRouted = true;
      this.#log('HDR on a canvas: compositor (fast) instead of Chrome\'s SDR import');
    }
    if (isWebKit && q === 'fast') q = 'high';
    this.compositorMode = q === 'native' ? null : dv ? 'dv' : 'all';
    return q;
  }

  // Downmix headroom options (audio-worker.js). normalize: true | false | null (auto: on for a downmix, off for identity).
  setMixOptions({ normalize = null, limiter = false } = {}) {
    this.mixOptions = { normalize, limiter };
    this.workers.audio?.postMessage({ type: 'mixopts', normalize, limiter });
    this.#mseAudioRefresh();
  }

  // E-AC-3 JOC (Dolby Atmos): true renders the objects to 7.1.4 (default), false plays the 5.1/7.1 bed. Takes effect on the next audio packet.
  setJoc(enabled) {
    this.jocEnabled = !!enabled;
    this.workers.audio?.postMessage({ type: 'joc', enabled: this.jocEnabled });
    this.#mseAudioRefresh();
  }

  // 'speakers' | 'headphones' (binaural). Takes effect on the next decoded audio block.
  setOutputMode(mode) {
    this.outputMode = mode === 'headphones' ? 'headphones' : 'speakers';
    this.workers.audio?.postMessage({ type: 'mode', mode: this.outputMode });
    this.#mseAudioRefresh();
  }

  // ---------------------------------------------------------------- plumbing
  #setState(s) {
    if (this.state === s) return;
    this.state = s; this.onEvent({ type: 'state', state: s });
    if (this.audioRefreshPending && (s === 'playing' || s === 'paused')) queueMicrotask(() => this.#mseAudioRefresh());
  }
  #log(text) { this.onEvent({ type: 'log', text }); }
  #mark(name, at = performance.timeOrigin + performance.now()) { this.markT0 ??= at; if (this.startupMarks.length < 4096) this.startupMarks.push([name, Math.round(at - this.markT0)]); }
  #warn(text) { this.warnings.push(text); this.#log('WARNING: ' + text); }

  #spawn(name, label) {
    const ready = label === 'demux' ? spare.demux : null;
    if (ready) spare.demux = null;
    const w = ready || new Worker(wurl(name), { type: 'module', name: label });
    w.onmessage = e => this.#onMsg(label, e.data);
    w.onerror = e => this.#fail(`${label} worker crashed: ${e.message || 'script error'}`);
    this.workers[label] = w;
    return w;
  }

  #adoptVideoWorker(main) {
    if (main) {
      const h = new MainVideoHost();
      h.onmessage = e => this.#onMsg('video', e.data);
      h.onerror = e => this.#fail(`video module failed: ${e.message}`);
      this.workers.video = h;
      return h;
    }
    const w = this.prevideo; this.prevideo = null;
    if (!w) return this.#spawn('video-worker.js', 'video');
    w.onmessage = e => this.#onMsg('video', e.data);
    w.onerror = e => this.#fail(`video worker crashed: ${e.message || 'script error'}`);
    this.workers.video = w;
    return w;
  }

  #expect(label, type, timeoutMs = 60000) {
    const p = new Promise((resolve, reject) => {
      const w = { label, type, resolve, reject };
      w.timer = setTimeout(() => { this.waiters = this.waiters.filter(x => x !== w); reject(new Error(`${label}: timeout waiting for ${type}`)); }, timeoutMs);
      this.waiters.push(w);
    });
    p.catch(() => {});     // rejections are reported through #fail; avoid unhandled-rejection noise for the losers
    return p;
  }

  #onMsg(label, m) {
    if (this.state === 'stopped') return;
    if (m.type === 'log') { this.#log(`[${label}] ${m.text}`); return; }
    if (m.type === 'mark') { this.#mark(`${label}:${m.name}`, m.at); return; }
    if (m.type === 'present') { if (m.mode === 'video' && this.#presentEl()) { this.videoEl.style.display = ''; this.canvas.style.display = 'none'; if (this.qs.has('vlead')) this.workers.video?.postMessage({ type: 'lead', us: Number(this.qs.get('vlead')) * 1000 }); } return; }
    if (m.type === 'vframe') {
      if (!this.#presentEl()) { m.frame.close(); return; }
      this.vwritten.set(m.frame.timestamp, performance.now());
      if (this.vwritten.size > 64) this.vwritten.delete(this.vwritten.keys().next().value);
      this.vwriter.write(m.frame).catch(() => {});
      return;
    }
    if (m.type === 'mse-handle') { this.#attachMse(m.handle); if (label === 'mse') this.#startMse(); return; }
    if (m.type === 'mse-stats') { if (this.mse) this.mse.stats = m; return; }
    if (m.type === 'subpkt') { if (m.epoch === this.epoch) this.#onSubPacket(m); return; }
    if (m.type === 'snap') { this.lastSnapTs = m.ts ?? null; this.#snapResolve?.(m.bitmap); this.#snapResolve = null; return; }
    if (m.type === 'error') {
      this.#fail(`${m.where || label}: ${m.message}`);
      return;
    }
    if (m.type === 'flushed') { this.#mseEarlySeek(m); return; }
    const staleGated = m.epoch !== undefined && m.epoch !== this.epoch && (m.type === 'ready' || m.type === 'ended');
    if (staleGated) return;
    for (const w of this.waiters.filter(x => x.label === label && x.type === m.type)) {
      clearTimeout(w.timer); this.waiters = this.waiters.filter(x => x !== w); w.resolve(m);
    }
    switch (`${label}:${m.type}`) {
      case 'video:stats': this.stats.video = m; this.onEvent({ type: 'stats', stats: this.snapshot() }); break;
      case 'audio:stats': this.stats.audio = m; break;
      case 'demux:stats': this.stats.demux = m; this.#netPolicy(); break;
      case 'video:ready': this.readyInfo.video = m; if (this.mse && !this.mseA) this.mse.offsetUs = m.offsetUs || 0; this.#markReady('video'); break;
      case 'mse:ready': this.readyInfo.audio = { baseUs: m.firstPtsUs }; if (this.mse) this.mse.offsetUs = m.offsetUs || 0; this.#markReady('mse'); break;
      case 'audio:ready': if (!this.mseAV && !this.mseA) { this.readyInfo.audio = m; this.#markReady('audio'); } break;
      case 'video:ended': this.endedFlags.video = true; this.#checkEnded(); break;
      case 'audio:route': this.audioRoute = m.route; this.audioInfo = m.info; this.#log('audio route: ' + m.route); break;
      case 'audio:drained': this.#onDrained(); break;
    }
  }

  #fail(message) {
    if (this.state === 'error' || this.state === 'stopped' || this.state === 'stopping') return;
    this.error = message;
    for (const w of this.waiters) { clearTimeout(w.timer); w.reject(new Error(message)); }
    this.waiters = [];
    this.#setState('error');
    this.onEvent({ type: 'error', message });
  }

  // Chrome 157 composites a canvas transferred to a worker (transferControlToOffscreen) into an SDR output, clamped at 1.0,
  // even when its WebGPU context is rgba16float + toneMapping 'extended'. viz picks HDR output from the content colour
  // usage in each frame's metadata, and the OffscreenCanvas frame sink (CanvasResourceDispatcher) never sets it, so only
  // HDR content in the main thread's own frame turns HDR output on (Chromium 4078f37e38, M158, derives it from the
  // resources instead). A 2x2 black PQ (cICP) image under the canvas is such content and lifts the clamp: HDR screen
  // capture of a test pattern 2.0 / 4.9 / 9.85 instead of 1.0 / 1.0 / 1.0; DV P8.1 p99 1.7 to 2.2 instead of 1.000.
  #hdrAnchor(vTrack) {
    if (isWebKit) return;   // a Chrome 157 workaround, WebKit's EDR canvas does not depend on the page's other content
    if (this.anchor || this.qs.has('sdr') || !matchMedia('(dynamic-range: high)').matches) return;
    if (this.quality === 'native' && ![16, 18].includes(vTrack?.color?.transfer)) return;
    const im = this.anchor = document.createElement('img');
    im.src = 'data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAIAAAACEAIAAACtREYwAAAABGNJQ1AJEAABTSMj/gAAAAtJREFUeJxjYMAFAAAaAAG8POBBAAAAAElFTkSuQmCC';
    im.alt = ''; im.setAttribute('aria-hidden', 'true');
    // No negative z-index: behind a host with an opaque background the anchor is culled and the canvas clamps again (Jellyfin's black
    // stage, measured on screen: maxRGB exactly 1.000 in 5 of 5 captures with z-index -1, above 1 up to 1.65 in 4 of 5 without).
    // Prepended and positioned, it still paints under the canvas that comes later in the host.
    im.style.cssText = 'position:absolute;left:0;top:0;width:1px;height:1px;pointer-events:none';
    // inside the host's box: an anchor scrolled or placed off screen is culled and no longer counts
    if (getComputedStyle(this.host).position === 'static') this.host.style.position = 'relative';
    this.host.prepend(im);
  }

  // SDR video off the MSE route is presented through a <video> element fed from a MediaStreamTrackGenerator (the video worker decides
  // on the first frame and says 'present'); ?present=canvas keeps the WebGPU canvas for everything.
  // Made when the worker first asks for it, never at open: a media element holds the document's load event back until it has data,
  // and on the MSE and compositor routes this one never gets any. A session opened before the page's load event then left the tab
  // loading for the whole playback, and Chrome repainted its tab spinner 30 times a second: on the element route 2 to 5 % of a core
  // on the browser's main thread against 0.3 to 0.6, twice the GPU main thread, 7 points of WindowServer (docs/perf/CORE-V2.md, G).
  #presentElWanted() { return this.qs.get('present') !== 'canvas' && typeof MediaStreamTrackGenerator === 'function'; }
  #presentEl() {
    if (this.videoEl) return this.videoEl;
    if (!this.#presentElWanted() || this.state === 'stopping' || this.state === 'stopped' || !this.canvas) return null;
    const gen = new MediaStreamTrackGenerator({ kind: 'video' });
    this.vwriter = gen.writable.getWriter();
    const el = this.videoEl = document.createElement('video');
    Object.assign(el, { muted: true, autoplay: true, playsInline: true, className: 'video-canvas' });
    el.style.display = 'none';
    el.srcObject = new MediaStream([gen]);
    this.host.append(el);
    // Write-to-display latency of this path, from requestVideoFrameCallback's expected display time (stats overlay).
    this.vwritten = new Map(); this.vlat = [];
    const onv = (now, md) => {
      if (this.videoEl !== el) return;
      const w = this.vwritten.get(Math.round(md.mediaTime * 1e6)) ?? null;
      if (w !== null) { this.vlat.push(md.expectedDisplayTime - w); if (this.vlat.length > 240) this.vlat.shift(); }
      el.requestVideoFrameCallback(onv);
    };
    el.requestVideoFrameCallback(onv);
    return el;
  }

  // ---------------------------------------------------------------- open
  async open() {
    armMediaUnlock();
    this.#mark('open');
    this.#setState('loading');
    try {
      this.canvas = document.createElement('canvas');
      this.canvas.className = 'video-canvas';
      this.host.append(this.canvas);
      // WebKit: transferred only once the path is known, the compositor presents from this thread (main-video-host.js).
      const offscreen = isWebKit ? null : this.canvas.transferControlToOffscreen();

      // Booted next to the header read instead of after it: its modules, the WASM engine and the GPU device.
      if (this.url && this.qs.get('preboot') !== '0') {
        const ready = spare.video; spare.video = null;
        const w = this.prevideo = ready || new Worker(wurl('video-worker.js'), { type: 'module', name: 'video' });
        w.onmessage = e => { if (e.data.type === 'mark') this.#onMsg('video', e.data); };
        if (!ready) w.postMessage({ type: 'preboot' });
      }
      const warm = this.url && !this.file ? takeWarmOpen(this.url) : (dropWarmOpen(), null);
      const tracksP = this.#expect('demux', 'tracks');
      if (warm) {
        const w = this.workers.demux = warm.worker;
        w.onmessage = e => this.#onMsg('demux', e.data);
        w.onerror = e => this.#fail(`demux worker crashed: ${e.message || 'script error'}`);
        this.#mark('warm-open-adopted');
        for (const m of warm.msgs) this.#onMsg('demux', m);
        w.postMessage({ type: 'release-fill' });
      } else {
        const demux = this.#spawn('demux-worker.js', 'demux');
        demux.postMessage({ type: 'open', file: this.file, url: this.url, headers: this.headers, sourceOpts: this.sourceOpts, startUs: this.startUs, demux: this.qs.get('demux') === 'js' ? 'js' : 'engine' });
      }
      const info = await tracksP;
      this.#mark('tracks');
      this.tracks = info.tracks; this.durationUs = info.durationUs; this.sourceInfo = info.source; this.engine = info.engine;
      this.#log(`source ${info.source.name} (${info.source.mode}, ${info.source.size} bytes); tracks: ` +
        info.tracks.map(t => `#${t.id} ${t.kind} ${t.codec}${t.width ? ` ${t.width}x${t.height}` : ''}${t.channels ? ` ${t.channels}ch ${t.sampleRate}Hz` : ''}`).join(', '));

      let vTrack = info.tracks.find(t => t.kind === 'video' && !t.enhancementLayer) || null;
      // MP4 Dolby Vision entries (dvh1 / dvhe) come with the Dolby Vision profile as codec string ('dvh1.05.09'); WebCodecs wants the
      // HEVC string of the base layer, which the hvcC record in the description has.
      if (vTrack && vTrack.fallbackCodecString && /^(dvh1|dvhe)/.test(vTrack.codec)) {
        // the engine already derived the base layer string from the dvcC / hvcC record
        vTrack.containerCodec ??= vTrack.codec; vTrack.codec = vTrack.fallbackCodecString;
      } else if (vTrack && vTrack.description && /^(dvh1|dvhe|hvc1\.0\d\.\d+$)/.test(vTrack.codec) && vTrack.description.length > 22) {
        try { const c = hevcCodecString(vTrack.description); if (c) { vTrack.containerCodec ??= vTrack.codec; vTrack.codec = c; } } catch {}
      }
      const audioTracks = info.tracks.filter(t => t.kind === 'audio');
      let aTrack = (this.audioIndex != null ? audioTracks[this.audioIndex] : null) || audioTracks[0] || null;
      if (this.qs.has('noaudio')) aTrack = null;   // debug: video only on the wall clock
      this.audioTracks = audioTracks;
      if (vTrack) {
        const why = await videoUnsupportedReason(vTrack);
        if (why) {
          this.unsupported.video = `video codec not supported: ${vTrack.codec}${vTrack.containerCodec && vTrack.containerCodec !== vTrack.codec ? ` (${vTrack.containerCodec})` : ''} (${why})`;
          this.#warn(this.unsupported.video + (aTrack ? '; playing audio only' : ''));
          vTrack = null;
        }
      }
      if (!vTrack && !aTrack) throw new Error(this.unsupported.video || 'no playable tracks');
      this.vTrack = vTrack; this.aTrack = aTrack;
      this.epoch = 1;

      const lanes = [];
      const inits = [];
      if (vTrack) this.quality = this.#chooseQuality(vTrack, info);
      // Audio through the same MediaSource (FLAC in fMP4, the element is the clock) when the video goes there too, or there is none.
      const av = aTrack && (!vTrack || this.#mseEligible(vTrack)) ? this.#mseAudioFormat(aTrack) : null;
      if ((vTrack && this.#mseEligible(vTrack)) || (!vTrack && av)) {
        await this.#waitForElementSlot();
        const ch = new MessageChannel();
        const w = this.#spawn('mse-worker.js', 'video');
        const ok = this.#expect('video', 'inited', 8000), bad = this.#expect('video', 'init-failed', 8000);
        w.postMessage({ type: 'init', track: vTrack || null, audio: av, port: vTrack ? ch.port2 : null, epoch: this.epoch, debug: this.qs.has('msedebug'), cutAtTarget: this.qs.get('msecut') !== '0', fragMs: Number(this.qs.get('msefrag')) || 0, aheadS: Number(this.qs.get('mseahead')) || 0, behindS: Number(this.qs.get('msebehind')) || 0 }, vTrack ? [ch.port2] : []);
        const r = await Promise.race([ok.then(m => ({ m }), e => ({ fail: String(e.message || e) })), bad.then(m => ({ fail: m.message }))]);
        if (r.m) {
          if (vTrack) lanes.push({ trackId: vTrack.id, kind: 'video', port: ch.port1 });
          this.mseAV = !!(av && r.m.audio);
          if (av && !r.m.audio) this.#log(`MSE audio unavailable (${r.m.audioError || 'refused'}); audio stays on Web Audio`);
          this.#log('inited: ' + JSON.stringify({ ...r.m, type: undefined }));
          this.#startMse();
        } else {
          this.#log(`MSE path unavailable (${r.fail}); using the WebCodecs video worker`);
          w.terminate(); delete this.workers.video;
          this.waiters = this.waiters.filter(x => x.label !== 'video');
          clearInterval(this.mse?.timer); this.mse = null; this.mseEl?.remove(); this.mseEl = null;
          if (this.canvas) this.canvas.style.display = '';   // #attachMse hid it when the handle arrived
        }
      }
      // Opt-in (?msea=all): sound through MSE next to the WebCodecs/canvas video too (Dolby Vision, HDR10+, VC-1, interlaced). The
      // element is the clock; the video worker reads it from a shared counter written below, as it reads the Web Audio ring.
      if (vTrack && !this.workers.video && aTrack && this.qs.get('msea') === 'all') {
        const fmt = this.#mseAudioFormat(aTrack);
        if (fmt) {
          await this.#waitForElementSlot();
          const w = this.#spawn('mse-worker.js', 'mse');
          const ok = this.#expect('mse', 'inited', 8000), bad = this.#expect('mse', 'init-failed', 8000);
          w.postMessage({ type: 'init', track: null, audio: fmt, port: null, epoch: this.epoch });
          const r = await Promise.race([ok.then(m => ({ m }), e => ({ fail: String(e.message || e) })), bad.then(m => ({ fail: m.message }))]);
          if (r.m?.audio) {
            this.mseA = true; this.mseAFmt = fmt;
            const sab = new SharedArrayBuffer(CTL.SIZE * 4);
            this.elClock = { sab, ctl: new Int32Array(sab), sampleRate: fmt.sampleRate, baseUs: 0 };
            this.#startElClock();
          } else { this.#log(`MSE audio unavailable (${r.fail || r.m?.audioError}); audio stays on Web Audio`); w.terminate(); delete this.workers.mse; this.waiters = this.waiters.filter(x => x.label !== 'mse'); this.mseEl?.remove(); this.mseEl = null; clearInterval(this.mse?.timer); this.mse = null; }
        }
      }
      if (vTrack && !this.workers.video) {
        const ch = new MessageChannel();
        lanes.push({ trackId: vTrack.id, kind: 'video', port: ch.port1 });
        // On by default (?hdranchor=0 turns it off) until the minimum Chrome has 4078f37e38 (M158).
        if (this.qs.get('hdranchor') !== '0') this.#hdrAnchor(vTrack);
        const mainComp = isWebKit && this.qs.get('maincomp') !== '0' && this.quality !== 'native' && /^(hvc1|hev1|dvh1|dvhe)/.test(vTrack.codec);
        const w = this.#adoptVideoWorker(mainComp);
        const canvasFor = mainComp ? this.canvas : offscreen || this.canvas.transferControlToOffscreen();
        inits.push(this.#expect('video', 'inited'));
        w.postMessage({ type: 'init', track: vTrack, canvas: canvasFor, port: ch.port2, epoch: this.epoch, displayHdr: matchMedia('(dynamic-range: high)').matches, probeRec2100: this.qs.has('hdrprobe'), corrupt: Number(this.qs.get('corrupt')) || 0, nodraw: this.qs.has('nodraw'), dvSleep: this.qs.get('dvsleep') !== '0', presentVideo: this.#presentElWanted(), forceSdr: this.qs.has('sdr'), quality: this.quality, display: { peakNits: Number(this.qs.get('peak')) || (matchMedia('(dynamic-range: high)').matches ? 1000 : 203), sdrWhiteNits: Number(this.qs.get('sdrwhite')) || 203, canvasColorSpace: mainComp ? (this.qs.get('cs') || 'display-p3-linear') : undefined, elementCurve: mainComp && this.qs.get('ecurve') !== '0', canvasMaxWidth: this.qs.has('maxw') ? Number(this.qs.get('maxw')) : 3840, upload: this.qs.get('upload') || undefined, diag: this.qs.get('dvdiag') || undefined, exactN: Number(this.qs.get('exactn')) || this.#exactWorkers(), exactMB: Number(this.qs.get('exactmb')) || undefined, gpuRecon: this.qs.get('gpurecon') !== '0', entropyThreads: this.#entropyThreads(), gpuFailAt: Number(this.qs.get('gpufail')) || 0, lean: this.qs.get('lean') !== '0', h10pMap: this.qs.get('h10pmap') || undefined }, toneMapping: this.qs.get('tm'), canvasFormat: this.qs.get('fmt'), canvasCS: this.qs.get('cs'), hw: this.qs.get('hw'), ofl: this.qs.get('ofl'), deint: this.qs.get('deint'), maxW: this.qs.has('maxw') ? Number(this.qs.get('maxw')) : null }, mainComp ? [] : [canvasFor, ch.port2]);
      }
      this.prevideo?.terminate(); this.prevideo = null;   // MSE path or sound only: not needed
      if (aTrack) {
        try { if (this.mseAV) await this.#setupMseAudio(aTrack, lanes, av); else if (this.mseA) await this.#setupMseAudio(aTrack, lanes, this.mseAFmt); else await this.#setupAudio(aTrack, lanes); }
        catch (e) {
          this.#warn(`audio track unusable (${e.message}); playing video only`);
          // An audio SourceBuffer that never gets data would hold the element at its first frame.
          if (this.mseAV || this.mseA) { this.#mseW()?.postMessage({ type: 'audio-off' }); this.mseAV = false; this.#dropMseA(); }
          this.aTrack = null; if (this.audio?.ctx) this.#closeCtx(this.audio.ctx); this.audio = null;
          clearInterval(this.latencyTimer);
          this.workers.audio?.terminate(); delete this.workers.audio;
          this.waiters = this.waiters.filter(w => w.label !== 'audio');
          const i = lanes.findIndex(l => l.kind === 'audio'); if (i >= 0) lanes.splice(i, 1);
        }
      }
      this.#mark('audio-set-up');
      const initMsgs = await Promise.all(inits);
      this.#mark('workers-inited');
      for (const m of initMsgs) this.#log('inited: ' + JSON.stringify({ ...m, type: undefined }));

      this.#startEpoch(this.startUs, lanes);
      const si = this.subInitial ?? (this.qs.has('sub') ? Number(this.qs.get('sub')) : null);
      if (si != null && this.subtitleTracks()[si]) this.setSubtitle(si).catch(e => this.#warn('subtitle: ' + e.message));
    } catch (e) {
      this.#fail(String(e && e.message || e));
      throw e;
    }
  }

  async #setupAudio(aTrack, lanes) {
    if (this.mse && this.mseEl && !this.mseA) queueMicrotask(() => this.#mseFrameLoop());
    // The context runs at the source rate when the browser accepts it (Chrome resamples to the device itself);
    // otherwise (rate outside 8..96 kHz, or ?ctxrate=N forcing one) the audio worker resamples with a windowed sinc.
    const forced = Number(this.qs.get('ctxrate')) || 0;
    const want = forced || (aTrack.sampleRate >= 8000 && aTrack.sampleRate <= 96000 ? aTrack.sampleRate : 0);
    const unlocked = isWebKit ? takeAudioContext() : null;
    const ctx = unlocked || new AudioContext({ ...(want ? { sampleRate: want } : {}), latencyHint: 'playback' });
    this.unlockedCtx = unlocked;
    this.audio = { ctx };
    const maxCh = ctx.destination.maxChannelCount;
    const outCh = snapChannels(aTrack.channels || 2, this.qs.has('maxch') ? Number(this.qs.get('maxch')) : maxCh);
    ctx.destination.channelCount = Math.min(outCh, maxCh);
    ctx.destination.channelInterpretation = 'discrete';
    this.#log(`AudioContext ${ctx.sampleRate} Hz (state ${ctx.state}), destination.maxChannelCount=${maxCh}, using ${outCh} channels (source ${aTrack.channels}${aTrack.sampleRate !== ctx.sampleRate ? `, ${aTrack.sampleRate} Hz source, worker resamples` : ''}), baseLatency ${(ctx.baseLatency * 1000).toFixed(1)} ms`);
    if (!ctx.fraxWorklet) { await ctx.audioWorklet.addModule(wurl('audio-worklet.js')); ctx.fraxWorklet = true; }
    const node = new AudioWorkletNode(ctx, 'aether-ring', {
      numberOfInputs: 0, numberOfOutputs: 1, outputChannelCount: [Math.min(outCh, maxCh)],
      channelCount: Math.min(outCh, maxCh), channelCountMode: 'explicit', channelInterpretation: 'discrete',
    });
    node.connect(ctx.destination);
    node.port.onmessage = e => { if (e.data.type === 'drained') this.#onDrained(); };
    Object.assign(this.audio, { node, sampleRate: ctx.sampleRate, outChannels: outCh, maxChannelCount: maxCh });
    ctx.addEventListener('statechange', () => this.#onCtxState());
    const ch = new MessageChannel();
    lanes.push({ trackId: aTrack.id, kind: 'audio', port: ch.port1 });
    const w = this.#spawn('audio-worker.js', 'audio');
    const ok = this.#expect('audio', 'inited'), bad = this.#expect('audio', 'init-failed');
    w.postMessage({ type: 'init', track: aTrack, port: ch.port2, epoch: this.epoch, outChannels: outCh, sampleRate: ctx.sampleRate, dtsx: this.qs.has('dtsx'), mode: this.outputMode, joc: this.jocEnabled ?? this.qs.get('joc') !== '0' }, [ch.port2]);
    { const mo = this.mixOptions || { normalize: this.qs.has('norm') ? this.qs.get('norm') !== '0' : null, limiter: this.qs.get('limiter') === '1' }; this.mixOptions = mo; w.postMessage({ type: 'mixopts', ...mo }); }
    const m = await Promise.race([ok, bad]);
    if (m.type === 'init-failed') throw new Error(m.message);
    this.#log('inited: ' + JSON.stringify({ ...m, type: undefined }));
    this.latencyTimer = setInterval(() => this.#updateLatency(), 500);
    // Autoplay policy: the first user gesture on the page resumes a suspended context.
    this.gestureHandler = () => { if (this.audio?.ctx.state === 'suspended') this.audio.ctx.resume().catch(() => {}); };
    for (const ev of ['pointerdown', 'keydown', 'touchstart']) document.addEventListener(ev, this.gestureHandler, { capture: true });
  }

  // WebKit: detached before the worker that owns the MediaSource goes. This alone did not stop the page being killed on the next
  // source (see WEBKIT_AUDIO_START_LIMIT_MS); an element whose audio never started leaves the deadline the next one waits for.
  #detachElement() {
    if (!isWebKit || !this.mseEl) return;
    const p = this.mse?.audioPlay;
    if (p && performance.now() - p.at >= WEBKIT_AUDIO_START_JUDGE_MS && this.mseEl.currentTime <= p.time + 0.05) webkitElementsBlockedUntil = Math.max(webkitElementsBlockedUntil, p.at + WEBKIT_AUDIO_START_LIMIT_MS);
    try { this.mseEl.pause(); this.mseEl.srcObject = null; this.mseEl.load(); } catch {}
  }

  // Before anything makes a new element. Throws when the session is stopped meanwhile, as a stop during the other waits of open() does.
  async #waitForElementSlot() {
    const wait = webkitElementsBlockedUntil - performance.now();
    if (!isWebKit || wait <= 0) return;
    this.#log(`waiting ${Math.ceil(wait / 1000)} s: the previous element's audio never started and WebKit is still tearing it down`);
    while (performance.now() < webkitElementsBlockedUntil) {
      if (this._stopping) throw new Error('stopped');
      await new Promise(r => setTimeout(r, 100));
    }
    if (this._stopping) throw new Error('stopped');
  }

  #closeCtx(c) { if (c === this.unlockedCtx) { returnAudioContext(c); this.unlockedCtx = null; } else c.close().catch(() => {}); }

  #latencyUs() {
    const a = this.audio;
    return Math.round((a.ctx.baseLatency + (a.ctx.outputLatency || 0)) * 1e6);
  }

  // `rebase`: keep the media clock (READ/rate - latency) continuous across a latency change by moving READ by the same amount.
  #updateLatency(rebase = false) {
    const a = this.audio;
    if (!a || !a.ctl) return;
    const lat = this.#latencyUs();
    if (rebase) {
      const old = Atomics.load(a.ctl, CTL.LATENCY_US);
      const delta = Math.round(((lat - old) * a.sampleRate) / 1e6);
      const fill = (Atomics.load(a.ctl, CTL.WRITE) - Atomics.load(a.ctl, CTL.READ)) | 0;
      if (delta > 0 && delta < fill) Atomics.add(a.ctl, CTL.READ, delta);
      else if (delta < 0) Atomics.add(a.ctl, CTL.READ, delta);
    }
    Atomics.store(a.ctl, CTL.LATENCY_US, lat);
  }

  // Clock source while playing. Audio ring READ is always the master; either the worklet consumes it (context running)
  // or the audio worker does on the wall clock (context suspended: autoplay policy, interruption). No audio track: the
  // video worker's own wall clock.
  #engageClock() {
    const a = this.audio;
    if (!a || !a.ctl) { this.clockMode = 'wall'; return; }
    const real = a.ctx.state === 'running';
    const mode = real ? 'audio' : 'wall-fallback';
    if (mode === this.clockMode && a.engaged === a.ring) return;
    if (real) {
      Atomics.store(a.ctl, CTL.VIRTUAL, 0);
      this.#updateLatency(this.clockMode === 'wall-fallback');
      Atomics.store(a.ctl, CTL.PLAY, 1);
    } else {
      Atomics.store(a.ctl, CTL.PLAY, 0);
      Atomics.store(a.ctl, CTL.VIRTUAL, 1);
    }
    a.engaged = a.ring;
    this.clockSwitches.push({ atMs: Math.round(performance.now()), to: mode, mediaUs: Math.round(this.currentTimeUs()) });
    if (this.clockMode !== 'none' && this.clockMode !== mode) this.#log(`clock source: ${this.clockMode} -> ${mode}`);
    else if (!real) this.#warn('AudioContext is not running (autoplay policy): playing on the fallback wall clock; audio starts at the next user gesture');
    this.clockMode = mode;
  }

  #disengageClock() {
    const a = this.audio;
    if (a?.ctl) { Atomics.store(a.ctl, CTL.PLAY, 0); Atomics.store(a.ctl, CTL.VIRTUAL, 0); }
    if (a) a.engaged = null;
    this.clockMode = this.audio ? 'paused' : 'wall';
  }

  #onCtxState() {
    if (this.state === 'playing') this.#engageClock();
  }

  // Begin (or restart after a seek) playback state at targetUs. `lanes` is only passed for the first epoch.
  #startEpoch(targetUs, lanes) {
    this.seekTargetUs = targetUs;
    if (this.mse) { clearTimeout(this.mse.soft?.timer); this.mse.soft = null; }
    this.endedFlags = { video: !this.vTrack && !this.mseAV, audio: !this.aTrack || !!this.mseAV };
    this.readyInfo = {};
    this.pendingReady = new Set(this.mseAV ? ['video'] : this.mseA ? ['video', 'mse'] : [this.vTrack && 'video', this.aTrack && 'audio'].filter(Boolean));
    this.workers.mse?.postMessage({ type: 'seek', epoch: this.epoch, targetUs });
    if (this.elClock) { Atomics.store(this.elClock.ctl, CTL.READ, 0); Atomics.store(this.elClock.ctl, CTL.DRAINED, 0); }
    const a = this.audio;
    let ring = null;
    if (a) {
      ring = allocRing(a.sampleRate, a.outChannels);
      a.ring = ring; a.ctl = new Int32Array(ring.ctl); a.baseUs = targetUs;
      a.engaged = null;
      this.#updateLatency();
      a.node.port.postMessage({ type: 'attach', ring });
    }
    const first = !!lanes;
    if (!first) this.subRenderer?.clear();
    if (!first) this.#mark(`seek ${(targetUs / 1e6).toFixed(1)}`);
    this.#setState(first ? 'buffering' : 'seeking');
    // The demux worker reads ahead in blocking reads; this count, raised before each seek is posted, lets it see that one is waiting.
    if (first && typeof SharedArrayBuffer !== 'undefined' && this.qs.get('seekintr') !== '0') this.seekWord = new Int32Array(new SharedArrayBuffer(4));
    const seq = !first && this.seekWord ? Atomics.add(this.seekWord, 0, 1) + 1 : null;
    if (first) this.workers.demux.postMessage({ type: 'select', epoch: this.epoch, lanes, seekWord: this.seekWord?.buffer, startUs: targetUs, compositor: this.compositorMode, native81: this.mse && this.vTrack?.nativeDv?.convert ? this.vTrack.id : null, baseOnly: this.mse && this.vTrack?.baseOnly ? this.vTrack.id : null }, lanes.map(l => l.port));
    else this.workers.demux.postMessage({ type: 'seek', epoch: this.epoch, seq, timeUs: targetUs, lookbackUs: this.aTrack ? 3e6 : 0, forceLookback: this.qs.get('lookback') === '1', gate: this.qs.get('seekgate') !== '0' });
    this.workers.video?.postMessage({ type: 'seek', epoch: this.epoch, targetUs, initial: first });
    this.workers.audio?.postMessage({ type: 'seek', epoch: this.epoch, targetUs, ring });
  }

  #markReady(which) {
    this.pendingReady.delete(which);
    this.#mark(`ready-${which}`);
    if (this.pendingReady.size) return;
    const a = this.audio;
    let baseUs;
    if (a) { baseUs = this.readyInfo.audio.baseUs; a.baseUs = baseUs; }
    else if (this.mseA) { baseUs = this.readyInfo.audio.baseUs; this.elClock.baseUs = baseUs; }
    else baseUs = this.readyInfo.video.firstPtsUs;
    const clockAudio = a ? { ctl: a.ring.ctl, sampleRate: a.sampleRate } : this.mseA ? { ctl: this.elClock.sab, sampleRate: this.elClock.sampleRate } : null;
    this.workers.video?.postMessage({ type: 'clock', baseUs, audio: clockAudio });
    // The element starts where both tracks have data: a file whose video begins after the target (POCAWE: first picture at 0.709 s)
    // otherwise leaves it in front of the buffered range, waiting.
    if (this.mse) { const v0 = (this.mseA ? this.readyInfo.audio.baseUs : this.readyInfo.video?.firstPtsUs) ?? this.seekTargetUs; this.mse.pausedAtUs = Math.max(this.seekTargetUs, v0); if (this.mse.seekEpoch !== this.epoch || !this.#mseElementAt(this.mse.pausedAtUs)) this.#mseSeekElement(this.mse.pausedAtUs); this.#log(`MSE element at ${(this.mse.pausedAtUs / 1e6).toFixed(3)} s (offset ${(this.mse.offsetUs || 0) / 1e6} s, data from ${(v0 / 1e6).toFixed(3)} s)`); }
    this.#setState('paused');
    if (this.wantPlay) this.#startPlayback();
  }

  // Network cushion. Remote media that starts the moment the first pictures are decoded runs on whatever has arrived, and a link
  // below the bitrate stutters every second. So playback starts with a cushion of seconds of media in the read-ahead ring, and
  // when the ring runs dry the clock stops until a bigger one has been built (doubling up to NET_MAX_S): a few long waits, each
  // honest about what the link can do. MSE paths buffer inside the element instead.
  #startNetHold(targetS, kind) {
    this.netHold = { targetS, kind, t0: performance.now(), epoch: this.epoch };
    this.#mark(`hold-${kind}-${targetS}s`);
    if (kind !== 'preroll') { this.rebufferCount = (this.rebufferCount || 0) + 1; this.#log(`buffering: rebuffer ${this.rebufferCount}, cushion target ${targetS} s`); }
    this.#setState('buffering');
  }

  #endNetHold(h, held) {
    this.netHold = null;
    this.#mark(`hold-end`);
    if (h.kind !== 'preroll') this.rebufferMs = (this.rebufferMs || 0) + held;
    this.#log(`buffering: ${h.kind} done after ${Math.round(held)} ms`);
    this.#setState('paused');
    if (this.wantPlay) this.#startPlayback();
  }

  #cushionS(base) {
    const net = this.stats.demux?.net, s = netCushionS(base, net, this.bitrateMbps);
    this.#mark(`cushion ${s}s link ${Math.round((net?.linkBps || 0) / 1e6)} Mbit/s media ${Math.round((net?.bytesPerMediaS || 0) * 8e-6)} Mbit/s`);
    return s;
  }

  #netPolicy() {
    if (this.mse) return;
    const demux = this.stats.demux, net = demux?.net;
    if (!net || demux.epoch !== this.epoch) return;
    const h = this.netHold;
    if (h) {
      const held = performance.now() - h.t0;
      const a = this.stats.audio;
      const audioOk = !h.audioMs || !this.audio || (a?.epoch === this.epoch && a.ringMs + a.outqMs >= h.audioMs);
      if (net.ended || net.ahead >= (net.aheadCap ?? net.ringBytes) * 0.9 || (audioOk && (net.aheadS != null ? net.aheadS >= h.targetS : this.bitrateMbps ? (net.ahead * 8e-6) / this.bitrateMbps >= h.targetS : held >= 2000)) || held >= NET_HOLD_MAX_MS) this.#endNetHold(h, held);
      return;
    }
    // Starving: the demuxer spends most of its time blocked in reads. The bytes ahead of the read position say little (measured at
    // 40 Mbit/s on a 73 Mbit/s file: 5 MiB "ahead" on every sample while the demuxer was blocked 100 % of the time and audio underran for
    // 6 s), so the audio left is what decides when to stop, early enough that the cushion is built before it runs out.
    if (this.state !== 'playing' || net.ended || !(net.waitFrac > NET_STARVED_FRAC) || performance.now() - this.netPlayAt < NET_START_GRACE_MS) return;
    const a = this.stats.audio, v = this.stats.video;
    if (this.audio && a?.epoch === this.epoch && a.ringMs + a.outqMs >= NET_LOW_AUDIO_MS) this.netFilled = this.epoch;
    if (!(this.audio ? a?.epoch === this.epoch && a.ringMs + a.outqMs < (this.netFilled === this.epoch ? NET_LOW_AUDIO_MS : NET_LOW_START_MS) : v?.queue < 2)) return;
    this.netTargetS = Math.min(NET_MAX_S, (this.netTargetS || NET_REBUFFER_S) * 2);
    this.#disengageClock();
    this.workers.video?.postMessage({ type: 'pause' });
    this.#startNetHold(this.netTargetS, 'rebuffer');
  }

  async #startPlayback() {
    const my = this.epoch;
    if (!this.mse && this.stats.demux?.net && this.prerolled !== my) {
      this.prerolled = my;
      const base = my === 1 ? Number(this.qs.get('netfirst')) || NET_FIRST_S : Number(this.qs.get('netseek')) || NET_SEEK_S, s = this.#cushionS(base);
      this.#startNetHold(s, 'preroll');
      if (s < base) this.netHold.audioMs = NET_FAST_AUDIO_MS;
      this.#netPolicy();
      if (this.netHold) return;
    }
    const a = this.audio;
    if (a && a.ctx.state !== 'running') {
      // Give an allowed context a moment to start; a blocked one never resolves, so do not wait for it.
      try { await Promise.race([a.ctx.resume(), new Promise(r => setTimeout(r, 300))]); } catch {}
    }
    if (my !== this.epoch || (this.state !== 'paused')) return;
    this.workers.video?.postMessage({ type: 'play' });
    this.netPlayAt = performance.now();
    this.#mark('playing');
    this.#setState('playing');
    this.#engageClock();
    if (this.mse) this.#msePlay();
  }

  #onDrained() {
    const a = this.audio;
    if (!a || !a.ctl || Atomics.load(a.ctl, CTL.DRAINED) !== 1) return;   // message from a replaced ring
    this.endedFlags.audio = true; this.#checkEnded();
  }

  #checkEnded() {
    if (this.state !== 'playing') return;
    if (this.endedFlags.video && this.endedFlags.audio) {
      this.mseEl?.pause();
      this.#disengageClock();
      this.workers.video?.postMessage({ type: 'pause' });
      this.wantPlay = false;
      this.#setState('ended');
    }
  }

  // ---------------------------------------------------------------- controls
  play() {
    this.wantPlay = true;
    if (this.audio?.ctx.state === 'suspended') this.audio.ctx.resume().catch(() => {});   // inside the user gesture
    if (this.state === 'ended') { this.seek(0); return; }
    if (this.state === 'paused') this.#startPlayback();
  }

  pause() {
    this.wantPlay = false;
    this.mseEl?.pause();
    if (this.state === 'playing') {
      this.#disengageClock();
      this.workers.video?.postMessage({ type: 'pause' });
      this.#setState('paused');
    }
  }

  // `flush` is for the session's own seeks that exist to rebuild the buffers (a changed mix setting): those never stay in the buffer.
  seek(timeUs, flush = false) {
    if (!['paused', 'playing', 'ended', 'seeking', 'buffering'].includes(this.state) || !this.workers.demux) return;
    const target = Math.max(0, Math.min(timeUs, Math.max(0, this.durationUs - 1)));
    if (!flush && this.#seekInBuffer(target)) return;
    this.netHold = null;
    this.#disengageClock();
    this.workers.video?.postMessage({ type: 'pause' });
    this.mseEl?.pause();
    this.epoch++;
    this.#startEpoch(target, null);
  }

  // A target the element already holds (js/seek-policy.js) is the element's own seek. Only where the element is the clock and
  // carries everything that plays (sound and picture, or a file without sound), and only from a settled epoch: right after a
  // seek that flushes, `buffered` still lists what the worker is about to remove.
  #seekInBuffer(targetUs) {
    const el = this.mseEl, m = this.mse;
    if (!el || !m || isWebKit || this.audio || this.mseA || this.workers.mse || this.switching || this.qs.get('softseek') === '0') return false;   // WebKit: not measured there
    if (!(this.state === 'playing' || this.state === 'paused' || (this.state === 'seeking' && m.soft))) return false;
    const t = (targetUs + (m.offsetUs || 0)) / 1e6;
    const d = this.stats.demux, reachS = this.qs.get('softreach') === '0' || d?.epoch !== this.epoch ? 0 : downloadedReachS(d.net);
    if (el.error || !holdsTarget(elementRanges(el), t, { ended: el.ended, fromT: el.currentTime, reachS })) return false;
    clearTimeout(m.soft?.timer);
    const soft = m.soft = { targetUs, timer: 0 };
    this.seekTargetUs = targetUs;
    this.#mark(`seek ${(targetUs / 1e6).toFixed(1)} in buffer`);
    this.#setState('seeking');
    const settle = () => {
      if (this.mse?.soft !== soft) return;
      clearTimeout(soft.timer); this.mse.soft = null;
      this.#setState('paused');
      if (this.wantPlay) this.#startPlayback();
    };
    el.addEventListener('seeked', settle, { once: true, signal: this.elEvents.signal });
    // The element said it holds the target; if it still has not landed (the range was evicted under it), the seek starts over there.
    soft.timer = setTimeout(() => { if (this.mse?.soft === soft && el.seeking) { this.mse.soft = null; this.#setState('paused'); this.seek(targetUs, true); } else settle(); }, SOFT_SEEK_LIMIT_MS);
    el.currentTime = t;
    // The worker fills ahead of and trims behind the position it was last told (every 250 ms): told now, it lets the next
    // packets through at once when the target lies at the end of what is appended.
    this.#mseW()?.postMessage({ type: 'position', us: Math.round(targetUs) });
    return true;
  }

  // The worker has emptied the SourceBuffers for this epoch: the element is told its target now, not when the target is
  // buffered. It then decodes from the keyframe while the last packets before the start are still on the link, instead of
  // after them (the seek and its decode came after the wait for 300 ms of media past the target: 60 to 300 ms per seek on 4K
  // with 10 s GOPs). Before the flush the element could land in what is about to be removed.
  #mseEarlySeek(m) {
    if (m.epoch !== this.epoch || !this.mse || !this.mseEl || isWebKit || this.mseA || this.workers.mse || this.audio || this.qs.get('earlyseek') === '0') return;   // WebKit: not measured there
    if (!['seeking', 'buffering'].includes(this.state) || this.netHold) return;
    this.mse.offsetUs = m.offsetUs || 0;
    this.mse.seekEpoch = this.epoch;
    this.#mark('el-set-target');
    this.#mseSeekElement(this.seekTargetUs);
  }

  currentTimeUs() {
    if (['seeking', 'loading'].includes(this.state) || (this.state === 'buffering' && !this.netHold)) return this.seekTargetUs;
    const a = this.audio;
    if (a?.ctl && a.ring) {
      const v = this.stats.video?.clockUs;
      if (Atomics.load(a.ctl, CTL.DRAINED) === 1 && v != null) return v;
      return Math.max(this.seekTargetUs, audioClockUs(a.ctl, a.baseUs, a.sampleRate));
    }
    // With no Web Audio clock the element is the clock, paused or not (stats.video.clockUs is written from this function).
    if (this.mse && this.mseEl && !this.audio) return Math.max(0, this.mseEl.currentTime * 1e6 - (this.mse.offsetUs || 0));
    return this.stats.video?.clockUs ?? this.seekTargetUs;
  }

  // ---------------------------------------------------------------- MSE path
  // SDR and HDR10 video in a codec the browser's own pipeline carries plays through Media Source Extensions: Chrome decodes and
  // composites it like any <video> (overlay, HDR10 kept). 4K HDR10 measured 26.5 % of a core through WebCodecs and a WebGPU
  // canvas against 11.6 % for a plain <video>. Dolby Vision, the compositor modes, VC-1, interlaced H.264, Annex B streams the
  // demux worker could not rewrite to avcC/hvcC, and HDR10+ keep the WebCodecs path. ?mse=0 turns it off.
  // FLAC carries up to 8 channels in MSE, at the source rate (Chrome resamples to the device); 6 ch, 8 ch at 48/96 kHz and 2 ch
  // at 192 kHz were checked to append and decode in Chrome 157. ?msea=0 keeps audio on Web Audio.
  #mseAudioFormat(t) {
    if (this.mseDisabled || this.qs.get('msea') === '0' || this.qs.get('mse') === '0' || !mseInWorker) return null;
    if (!mseAudioSupported('flac')) return null;
    let maxCh = 2;
    try { const c = new AudioContext(); maxCh = c.destination.maxChannelCount || 2; c.close().catch(() => {}); } catch {}
    const devCh = this.qs.has('maxch') ? Number(this.qs.get('maxch')) : maxCh;
    // AAC and Opus go in as they are, the way a streaming site feeds the element: no decode here and no FLAC. Only while nothing of
    // ours has to touch the samples: speakers mode (binaural needs the decode), no normalize or limiter, and no fold-down (the
    // source fits the device; our downmix applies the headroom the browser's does not).
    const mo = this.mixOptions || {};
    const norm = mo.normalize ?? (this.qs.has('norm') ? this.qs.get('norm') !== '0' : null);
    const limiter = mo.limiter ?? this.qs.get('limiter') === '1';
    if ((/^mp4a\.40\.(2|5|29)$/.test(t.codec) || t.codec === 'opus') && t.description && this.qs.get('passa') !== '0'
      && this.outputMode === 'speakers' && norm !== true && !limiter && (t.channels || 2) <= devCh
      && mseAudioSupported(t.codec)) {
      const desc = t.description instanceof ArrayBuffer ? t.description : t.description.buffer.slice(t.description.byteOffset, t.description.byteOffset + t.description.byteLength);
      return { pass: { codec: t.codec, description: desc, channels: t.channels || 2, sampleRate: t.codec === 'opus' ? 48000 : t.sampleRate }, channels: t.channels || 2, sampleRate: t.codec === 'opus' ? 48000 : t.sampleRate, maxChannelCount: maxCh };
    }
    // Dolby Digital and Digital Plus (and JOC Atmos) go to the system as they are: AVFoundation decodes, renders objects and folds down.
    if (isWebKit && t.dolbyAudio && this.outputMode === 'speakers' && norm !== true && !limiter) {
      const codec = t.dolbyAudio.box === 'dec3' ? 'ec-3' : 'ac-3';
      if (mseAudioSupported(codec)) return { pass: { codec, description: t.dolbyAudio.config.buffer.slice(0), channels: t.dolbyAudio.channels, sampleRate: t.dolbyAudio.sampleRate }, channels: t.dolbyAudio.channels, sampleRate: t.dolbyAudio.sampleRate, maxChannelCount: maxCh };
    }
    // WebKit gets every channel: the system mixes them for the device (and for spatial audio), which a stereo fold-down here would rule out.
    const channels = Math.min(8, snapChannels(t.channels || 2, isWebKit ? 8 : devCh));
    const sr = t.sampleRate >= 8000 && t.sampleRate <= 192000 ? t.sampleRate : 48000;
    return { channels, sampleRate: sr, bits: 24, maxChannelCount: maxCh };
  }

  async #setupMseAudio(aTrack, lanes, fmt) {
    this.audio = null;
    this.audioPass = !!fmt.pass;
    if (fmt.pass) {
      const ch = new MessageChannel();
      lanes.push({ trackId: aTrack.id, kind: 'audio', port: ch.port1 });
      this.#mseW().postMessage({ type: 'audio-port', port: ch.port2, pass: fmt.pass }, [ch.port2]);
      this.audioRoute = `${aTrack.codec} passthrough (MSE)`;
      this.audioInfo = { route: `${aTrack.codec} passed through to MSE (no decode)`, channels: fmt.channels, sampleRate: fmt.sampleRate };
      this.#log(`audio through MSE: ${aTrack.codec} ${fmt.channels} ch ${fmt.sampleRate} Hz passed through, device ${fmt.maxChannelCount} ch`);
      return;
    }
    const ch = new MessageChannel(), pcm = new MessageChannel();
    lanes.push({ trackId: aTrack.id, kind: 'audio', port: ch.port1 });
    this.#mseW().postMessage({ type: 'audio-port', port: pcm.port1, channels: fmt.channels, sampleRate: fmt.sampleRate, bits: fmt.bits }, [pcm.port1]);
    const w = this.#spawn('audio-worker.js', 'audio');
    const ok = this.#expect('audio', 'inited'), bad = this.#expect('audio', 'init-failed');
    w.postMessage({ type: 'init', track: aTrack, port: ch.port2, msePort: pcm.port2, epoch: this.epoch, outChannels: fmt.channels, sampleRate: fmt.sampleRate, dtsx: this.qs.has('dtsx'), mode: this.outputMode, joc: this.jocEnabled ?? this.qs.get('joc') !== '0' }, [ch.port2, pcm.port2]);
    { const mo = this.mixOptions || { normalize: this.qs.has('norm') ? this.qs.get('norm') !== '0' : null, limiter: this.qs.get('limiter') === '1' }; this.mixOptions = mo; w.postMessage({ type: 'mixopts', ...mo }); }
    const m = await Promise.race([ok, bad]);
    if (m.type === 'init-failed') throw new Error(m.message);
    this.#log('inited: ' + JSON.stringify({ ...m, type: undefined }) + ` (audio through MSE: FLAC ${fmt.channels} ch ${fmt.sampleRate} Hz, device ${fmt.maxChannelCount} ch)`);
  }

  #mseAudioRefresh() {
    // A setting that needs our own processing (binaural, normalize, limiter) moves a passed-through track to the decode path and back.
    // Changed while seeking, buffering or loading, it waits for the session to settle (#setState) instead of being lost.
    if ((this.mseAV || this.mseA) && this.aTrack && !!this.#mseAudioFormat(this.aTrack)?.pass !== !!this.audioPass) {
      if (!['playing', 'paused'].includes(this.state) || this.switching) { this.audioRefreshPending = true; return; }
      this.audioRefreshPending = false;
      this.switchAudio(this.audioTracks.indexOf(this.aTrack), true); return;
    }
    if (this.audioPass) return;   // still passed through: nothing of ours touches the samples
    if (this.mseAV && ['playing', 'paused'].includes(this.state)) { const play = this.state === 'playing'; this.seek(this.currentTimeUs(), true); if (play) this.wantPlay = true; }
  }

  // The element's pipeline has no software fallback: a stream VideoToolbox refuses (-12909 on some M2TS and on POCAWE) fails the
  // element for good. The session reopens itself once at the same position with MSE off, where WebCodecs falls back to software.
  async #mseFallback(reason) {
    if (this.mseDisabled || this.state === 'stopping' || this.state === 'stopped') { this.#fail(reason); return; }
    const at = this.state === 'loading' ? this.startUs : this.currentTimeUs(), play = this.wantPlay || this.state === 'playing';
    this.subInitial = this.subIndex;
    const onEvent = this.onEvent;
    this.onEvent = e => { if (e.type !== 'state') onEvent(e); };   // the host sees one continuous session
    await this.stop();
    this.onEvent = onEvent;
    Object.assign(this, { _stopping: null, epoch: 0, mseAV: false, mseDisabled: true, startUs: at, wantPlay: play, endedFlags: { video: false, audio: false }, pendingReady: new Set(), readyInfo: {}, stats: { video: null, audio: null, demux: null } });
    this.#warn(`MSE path failed (${reason}); reopened at ${(at / 1e6).toFixed(1)} s on the WebCodecs path`);
    try { await this.open(); } catch {}
  }

  // The worker that owns the MediaSource: the video lane's own worker on the MSE video path, a separate 'mse' worker when only
  // the audio goes through MSE next to the WebCodecs/canvas video worker.
  #mseW() { return this.workers.mse || (this.mse ? this.workers.video : null); }

  // Writes the element's position as a ring READ count, so the video worker's audio-clock code (smoothing, drain) runs unchanged.
  #startElClock() {
    const c = this.elClock;
    c.timer = setInterval(() => {
      const el = this.mseEl;
      if (!el || !this.mse) return;
      const us = el.currentTime * 1e6 - (this.mse.offsetUs || 0) - c.baseUs;
      Atomics.store(c.ctl, CTL.READ, Math.max(0, Math.round((us * c.sampleRate) / 1e6)) | 0);
      if (el.ended) Atomics.store(c.ctl, CTL.DRAINED, 1);
    }, 10);
  }

  #dropMseA() {
    if (!this.mseA) return;
    this.mseA = false; clearInterval(this.elClock?.timer); this.elClock = null;
    this.#detachElement();
    this.workers.mse?.terminate(); delete this.workers.mse; clearInterval(this.mse?.timer); this.mse = null; this.mseEl?.remove(); this.mseEl = null;
  }

  #mseEligible(t, quality = this.quality) {
    if (this.mseDisabled) return false;
    if (this.qs.get('mse') === '0' || this.qs.get('present') === 'canvas') return false;
    if (!mseInWorker) return false;
    if (isWebKit && this.#webkitNative(t)) {
      if (quality !== 'native') return false;
      t.nativeDv = nativeDolbyVision(t);
      if (t.nativeDv) return true;
    }
    else if (this.#baseLayerOnly(t)) {
      if (quality !== 'native') return false;
      if (!t.baseOnly) this.#log(`Dolby Vision profile ${t.dolbyVision.profile}: this device's decoder does not take it, the base layer plays through the system decoder`);
      t.baseOnly = true;
    }
    else if (t.dolbyVision || t.enhancementLayer || (t.hdr10plus && this.qs.get('hdr10plus') !== 'video') || quality !== 'native') return false;
    // Transport streams arrive rewritten to avcC/hvcC and length prefixes by the demux worker. Interlaced H.264 (Blu-ray 1080i) stays
    // on the WebCodecs path, which deinterlaces; MSE would show it woven.
    if (t.annexB && !t.description) return false;
    if (/^avc/.test(t.codec)) { const f = h264SpsFacts(t.description); if (!f || !f.frameMbsOnly) return false; }
    if (!/^(avc1|avc3|hvc1|hev1|av01|vp09|vp8)/.test(t.codec)) return false;
    if (!/^(vp09|vp8)/.test(t.codec) && !t.description) return false;
    return MSE.isTypeSupported(`video/mp4; codecs="${t.codec}"`);
  }

  // Safari shows Dolby Vision 5 and 8 and HDR10+ through its own pipeline, the one AVPlayer uses.
  #webkitNative(t) { return !!nativeDolbyVision(t) || (t.hdr10plus && !t.dolbyVision); }

  // A handheld plays the base layer of Dolby Vision its own pipeline refuses (platform.js); ?dvbase=1 asks for it anywhere, ?dvbase=0 never.
  #baseLayerOnly(t) {
    const want = this.qs.get('dvbase');
    return want !== '0' && (isHandheld || want === '1') && !nativeDolbyVision(t) && dolbyVisionBaseLayer(t);
  }

  #attachMse(handle) {
    const el = (isWebKit && unlockedElement()) || document.createElement('video');
    Object.assign(el, { muted: true, playsInline: true, className: 'video-canvas', disableRemotePlayback: true });
    this.elEvents?.abort(); this.elEvents = new AbortController();
    const signal = this.elEvents.signal;
    el.preload = 'auto';
    el.srcObject = handle;
    if (this.workers.mse) el.style.display = 'none';   // sound only: the canvas keeps the picture
    else if (this.canvas) this.canvas.style.display = 'none';
    if (this.videoEl) this.videoEl.style.display = 'none';
    this.host.append(el);
    this.mseEl = el;
    this.mse = { offsetUs: 0, stats: null, drift: [], seeks: 0, rateNudges: 0 };
    for (const ev of ['seeking', 'seeked', 'waiting', 'canplay', 'playing']) el.addEventListener(ev, () => this.#mark(`el-${ev}`), { signal });
    el.addEventListener('ended', () => { if (this.mse && this.state === 'playing') { this.endedFlags[this.mseA ? 'audio' : 'video'] = true; this.#checkEnded(); } }, { signal });
    el.addEventListener('error', () => { if (this.mse) this.#mseFallback(`video element: ${el.error?.message || el.error?.code}`); }, { signal });
  }

  #startMse() {
    const el = this.mseEl;
    this.mse.timer = setInterval(() => {
      if (!this.mse) return;
      const clock = this.currentTimeUs();
      const elUs = el.currentTime * 1e6 - (this.mse.offsetUs || 0);
      // The worker trims behind and fills ahead of the element's position, which can lag the clock (a stall); never past it.
      const settling = ['seeking', 'buffering', 'loading'].includes(this.state);   // the element still sits at the old position
      this.#mseW()?.postMessage({ type: 'position', us: Math.round(settling ? this.seekTargetUs : Math.min(clock, elUs)) });
      this.#mseWatchdog(clock, elUs);
      if (this.mseA) return;
      if (!this.vTrack) { this.onEvent({ type: 'stats', stats: this.snapshot() }); return; }   // sound only: no picture to count
      const q = el.getVideoPlaybackQuality?.();
      const total = q ? q.totalVideoFrames : 0, dropped = q ? q.droppedVideoFrames : 0;
      const now = performance.now(), prev = this.mse.prevQ;
      const fps = prev && now > prev.t ? Math.round(((total - prev.total) * 1000) / (now - prev.t)) : 0;
      this.mse.prevQ = { t: now, total };
      const d = this.mse.drift;
      this.stats.video = {
        type: 'stats', epoch: this.epoch, shown: total - dropped, dropped, fps, clockUs: clock, queue: 0, preroll: 0,
        droppedPct: total ? (dropped * 100) / total : 0, avSamples: d.length, vsyncMs: null,
        avP95Ms: d.length ? Math.round([...d].map(Math.abs).sort((x, y) => x - y)[Math.floor(d.length * 0.95)] * 10) / 10 : 0,
        avMeanMs: d.length ? Math.round((d.reduce((x, y) => x + y, 0) / d.length) * 10) / 10 : 0,
        decoder: { codec: this.vTrack?.codec ?? 'none', hardwareAcceleration: 'browser (MSE)' },
        hdr: { canvas: 'video element (MSE)', source: this.vTrack?.codec ?? 'none' },
        mse: { ...this.mse.stats, seeks: this.mse.seeks, rateNudges: this.mse.rateNudges, recoveries: this.mse.recoveries || 0, gapJumps: this.mse.gapJumps || 0, rate: el.playbackRate, readyState: el.readyState },
      };
      this.onEvent({ type: 'stats', stats: this.snapshot() });
    }, 250);
    // Per-frame callbacks only while the element follows the Web Audio clock; with the sound in the element there is nothing to
    // correct, and a callback per picture wakes the page's main thread 24 to 60 times a second.
    if (!this.mseAV && !this.mseA) this.#mseFrameLoop();
  }

  // Keeps the element on the Web Audio clock (#mseSync) once per picture. Started with the MSE video path when the sound plays through
  // Web Audio, and later when an MSE audio track turned out unusable and a switch moved the sound to Web Audio.
  #mseFrameLoop() {
    const el = this.mseEl;
    if (!el || this.mse?.frameLoop) return;
    this.mse.frameLoop = true;
    const onFrame = (now, md) => {
      if (!this.mse || this.mseEl !== el) return;
      if (this.state === 'playing' && !el.paused) this.#mseSync(md);
      el.requestVideoFrameCallback(onFrame);
    };
    el.requestVideoFrameCallback(onFrame);
  }

  // A stalled element (no data at its position, or a decoder hiccup) gets no frame callbacks, so #mseSync never sees it. When it
  // trails the clock by more than a second for a second, it is moved to the clock if that is buffered, else the session seeks.
  #mseWatchdog(clockUs, elUs) {
    const el = this.mseEl;
    // WebKit: an element without audio is its own clock too, so its stalls would otherwise stay hidden (Profile 5 with the lid closed sat at frame 13).
    if (this.mseAV || this.mseA || (isWebKit && !this.audio)) {
      // The element is the clock here, so a stall shows as no progress with data buffered at the position: VideoToolbox can sit on a
      // stream it cannot decode (POCAWE: 15 s at frame 0) before, or without, reporting an error.
      const now = performance.now();
      if (this.state !== 'playing' || el.paused) { this.mse.lastMove = null; return; }
      if (this.mse.lastMove == null || el.currentTime !== this.mse.lastT) { if (this.mse.lastT != null && Math.abs(el.currentTime - this.mse.lastT) > 0.1) this.mse.kicked = false; this.mse.waiting = false; this.mse.lastMove = now; this.mse.lastT = el.currentTime; return; }
      let ahead = 0, next = null;
      for (let i = 0; i < el.buffered.length; i++) {
        const a = el.buffered.start(i), b = el.buffered.end(i);
        if (a <= el.currentTime + 0.05 && el.currentTime < b) ahead = b - el.currentTime;
        else if (a > el.currentTime && next === null) next = a;
      }
      // A hole in front of the element (a track that starts later than the other, a fragment boundary) is jumped, as players do.
      if (!ahead && next !== null && next - el.currentTime < 2 && now - this.mse.lastMove > 300) { this.mse.gapJumps = (this.mse.gapJumps || 0) + 1; el.currentTime = next + 0.001; this.mse.lastMove = now; return; }
      // WebKit: with sound in the element, some starts sit on the first frame with everything buffered (AV1, E-AC-3 and DTS clips,
      // differently from run to run). A 20 ms seek released it (WKWebView, sound on: 1.0 s to 3.84 s in the next 3 s). A picture
      // without a sound track does the same on an iPhone (iOS 27 simulator: four 4K files without audio sat at the start).
      if (isWebKit && now - this.mse.lastMove > 1200 && ahead > 1 && !this.mse.kicked) { this.mse.kicked = true; this.mse.kicks = (this.mse.kicks || 0) + 1; this.#log(`element sat at ${el.currentTime.toFixed(3)} s with ${ahead.toFixed(1)} s buffered: nudging it`); el.currentTime += 0.02; return; }
      // A route change is for a decoder that does not decode, never for a link that does not deliver (a reopen cost 9 to 19 s of
      // playback on a 66 Mbit/s link and the other route starves the same way). The pipeline counts as stuck only when both
      // SourceBuffers hold STUCK_FULL_S ahead of the position, so the element has everything it can ask for, and the position
      // still has not moved for STUCK_MS. Anything less is the element waiting for data and shows as buffering.
      const st = this.mse.stats, full = !!st && [st.video, st.audio].every(l => !l || (l.aheadMs ?? 0) >= STUCK_FULL_S * 1000);
      if (now - this.mse.lastMove > 1000 && !full && !this.mse.waiting) { this.mse.waiting = true; this.mse.starves = (this.mse.starves || 0) + 1; this.#log(`buffering: element waits for data at ${el.currentTime.toFixed(2)} s (video ${st?.video?.aheadMs ?? '?'} ms, audio ${st?.audio?.aheadMs ?? '?'} ms ahead)`); }
      if (now - this.mse.lastMove > STUCK_MS && full && ahead >= STUCK_FULL_S && !this.qs.has('nofallback')) { this.mse.lastMove = null; this.#mseFallback(`no progress for ${STUCK_MS / 1000} s with ${ahead.toFixed(1)} s buffered in every track`); }
      return;
    }
    if (this.state !== 'playing' || el.paused || !this.audio || clockUs - elUs < 1e6) { this.mse.stallSince = null; return; }
    const now = performance.now();
    if (this.mse.stallSince == null) { this.mse.stallSince = now; return; }
    if (now - this.mse.stallSince < 1000) return;
    this.mse.stallSince = null;
    const t = (clockUs + 200_000 + (this.mse.offsetUs || 0)) / 1e6;
    let inBuf = false;
    for (let i = 0; i < el.buffered.length; i++) if (el.buffered.start(i) <= t && t < el.buffered.end(i) - 0.5) inBuf = true;
    this.mse.recoveries = (this.mse.recoveries || 0) + 1;
    this.#warn(`video element stalled ${Math.round((clockUs - elUs) / 1000)} ms behind the clock; ${inBuf ? 'moving it to the clock' : 'reloading from the clock position'}`);
    if (inBuf) el.currentTime = t; else this.seek(clockUs);
  }

  // Whether the element is at (or on its way to) this position already. Once it was pointed there after this epoch's flush
  // (#mseEarlySeek), a second assignment would only make it seek and decode again.
  #mseElementAt(us) {
    const el = this.mseEl;
    return !!el && Math.abs(el.currentTime * 1e6 - (this.mse.offsetUs || 0) - us) < 1000;
  }

  #mseSeekElement(us) {
    const el = this.mseEl;
    if (!el) return;
    el.currentTime = (us + (this.mse.offsetUs || 0)) / 1e6;
  }

  #msePlay() {
    const el = this.mseEl;
    if (this.mseAV || this.mseA) {
      this.clockMode = 'element';
      if (isWebKit) this.mse.audioPlay = { at: performance.now(), time: el.currentTime };
      el.muted = this.qs.has('msemuted');   // test runs with the lid closed: an unmuted element waits for an audio device that is not rendering
      el.play().catch(e => {
        // A play() cut short by a pause() or a seek (AbortError, quick track or setting switches) is not a policy block: the next
        // play() follows. Only NotAllowedError means the autoplay policy; then play muted and unmute at the next user gesture.
        if (e.name === 'AbortError') return;
        this.#warn(`audio blocked by the autoplay policy (${e.name}); playing muted until a user gesture`);
        el.muted = true; el.play().catch(() => {});
        if (!this.gestureHandler) {
          this.gestureHandler = () => { if (this.mseEl) this.mseEl.muted = false; };
          for (const ev of ['pointerdown', 'keydown', 'touchstart']) document.addEventListener(ev, this.gestureHandler, { capture: true });
        }
      });
      return;
    }
    this.#mseSeekElement(this.currentTimeUs());
    el.playbackRate = 1;
    el.play().catch(e => this.#warn('video element play: ' + e.message));
  }

  // Keeps the element on the master clock. The picture's pts at its expected display time is compared with the clock at that
  // time: a small error is pulled in by nudging playbackRate (inaudible, the element is muted), a large one by a seek.
  #mseSync(md) {
    const el = this.mseEl;
    const clockAtDisplay = this.currentTimeUs() + (md.expectedDisplayTime - performance.now()) * 1000;
    if (!this.audio) return;   // without audio the element itself is the clock
    const ptsUs = md.mediaTime * 1e6 - (this.mse.offsetUs || 0);
    const errMs = (ptsUs - clockAtDisplay) / 1000;   // positive: picture ahead of the sound
    const d = this.mse.drift; d.push(errMs); if (d.length > 240) d.shift();
    // A corrective seek needs time to land (4K, long GOPs): judging the error before it does seeked again and again.
    if (performance.now() < (this.mse.seekCooldown || 0)) return;
    if (Math.abs(errMs) > 120) {
      this.mse.seeks++;
      this.mse.seekCooldown = performance.now() + 1500;
      this.#mseSeekElement(this.currentTimeUs() + 60_000);
      el.playbackRate = 1;
      return;
    }
    // A 15 ms dead band and at most 1 % keep Chrome's renderer from dropping frames to follow rate changes (an 8 ms band and up
    // to 5 % dropped 23 of 720 frames at 1080p24).
    const rate = Math.abs(errMs) < 15 ? 1 : 1 - Math.max(-0.01, Math.min(0.01, errMs / 4000));
    if (Math.abs(el.playbackRate - rate) > 0.001) { el.playbackRate = rate; this.mse.rateNudges++; }
  }

  #median(a) { const b = [...a].sort((x, y) => x - y); return b[b.length >> 1]; }

  snapshot() {
    return {
      state: this.state, epoch: this.epoch, timeUs: this.currentTimeUs(), durationUs: this.durationUs,
      video: this.stats.video, audio: this.stats.audio, demux: this.stats.demux,
      present: this.mseEl && !this.mseA ? { mode: 'mse', avMeanMs: this.stats.video?.avMeanMs ?? null, avP95Ms: this.stats.video?.avP95Ms ?? null, seeks: this.mse?.seeks ?? 0, rateNudges: this.mse?.rateNudges ?? 0 } : this.videoEl && this.videoEl.style.display !== 'none' ? { mode: 'video', latencyMs: this.vlat?.length ? this.#median(this.vlat) : null, leadMs: this.stats.video?.presentLeadMs ?? null, samples: this.vlat?.length || 0 } : { mode: 'canvas' },
      subtitle: { index: this.subIndex, forcedOnly: this.subForced, stats: this.subRenderer?.stats?.() ?? null },
      quality: this.quality, qualityAuto: this.qualityAuto || null, bitrateMbps: this.bitrateMbps ?? null, audioIndex: this.aTrack ? this.audioTracks.indexOf(this.aTrack) : null,
      audioRoute: this.audioRoute, audioInfo: this.audioInfo || null, clockMode: this.clockMode, clockSwitches: this.clockSwitches, unsupported: this.unsupported, engine: this.engine,
      audioCtx: this.audio ? { state: this.audio.ctx.state, maxChannelCount: this.audio.maxChannelCount, outChannels: this.audio.outChannels, sampleRate: this.audio.sampleRate } : null,
      source: this.sourceInfo, warnings: this.warnings, error: this.error || null, finalAccounting: this.finalAccounting,
    };
  }

  // ---------------------------------------------------------------- subtitles
  subtitleTracks() { return this.tracks.filter(t => t.kind === 'subtitle'); }

  #onSubPacket(m) {
    this.subRenderer?.pushPacket({ trackId: m.trackId, ptsUs: m.ptsUs, durationUs: m.durationUs, data: new Uint8Array(m.data) });
  }

  #videoRect() {
    const c = this.subCanvas, v = this.vTrack;
    const W = c.width, H = c.height;
    const ar = v && v.width && v.height ? v.width / v.height : W / H;
    let w = W, h = W / ar;
    if (h > H) { h = H; w = H * ar; }
    return { x: (W - w) / 2, y: (H - h) / 2, width: w, height: h };
  }

  #subTick() {
    const c = this.subCanvas;
    if (!c || !this.subRenderer) return;
    const dpr = window.devicePixelRatio || 1;
    const w = Math.max(2, Math.round(this.host.clientWidth * dpr)), h = Math.max(2, Math.round(this.host.clientHeight * dpr));
    if (c.width !== w || c.height !== h) { c.width = w; c.height = h; }
    try { this.subRenderer.render(this.currentTimeUs(), this.#videoRect()); } catch (e) { if (!this._subErr) { this._subErr = 1; this.#warn('subtitle render: ' + e.message); } }
  }

  // index into subtitleTracks(), or null for off. Works while playing: the demuxer forwards the track's packets from its current
  // position on (text cues that started earlier and are still on screen appear at the next seek; PGS shows from the next display set).
  async setSubtitle(index) {
    const list = this.subtitleTracks();
    if (!this.subCanvas) {
      if (getComputedStyle(this.host).position === 'static') this.host.style.position = 'relative';
      this.subCanvas = document.createElement('canvas');
      this.subCanvas.className = 'sub-canvas';
      this.host.append(this.subCanvas);
      this.subTimer = setInterval(() => this.#subTick(), 33);
    }
    this.subRenderer?.dispose(); this.subRenderer = null;
    this.subIndex = index;
    this.workers.demux?.postMessage({ type: 'subs', trackIds: index == null ? [] : [list[index].id] });
    const c = this.subCanvas; c.getContext('2d').clearRect(0, 0, c.width, c.height);
    if (index == null) return;
    const hdrVideo = this.quality !== 'native' || [16, 18].includes(this.vTrack?.color?.transfer);
    const r = createSubtitleRenderer(c, { whiteLevel: hdrVideo ? 0.8 : 1, forcedOnly: this.subForced });
    await r.setTrack(list[index]);
    this.subRenderer = r;
  }

  setSubtitleForcedOnly(on) { this.subForced = !!on; this.subRenderer?.setForcedOnly(!!on); }

  // ---------------------------------------------------------------- audio track switch (no stop)
  // One switch at a time: a second one started while the first waited for its audio worker terminated that worker and left the
  // first switch hanging. Later calls run after it.
  switchAudio(index, rebuild = false) {
    const run = () => this.#switchAudio(index, rebuild);
    this.switchChain = (this.switchChain || Promise.resolve()).catch(() => {}).then(() => { this.switching = true; return run(); }).finally(() => { this.switching = false; if (this.audioRefreshPending) this.#mseAudioRefresh(); });
    return this.switchChain;
  }

  async #switchAudio(index, rebuild = false) {
    const nt = this.audioTracks?.[index];
    if (!nt || (nt === this.aTrack && !rebuild) || !this.workers.demux) return;
    const wasPlaying = this.state === 'playing' || this.wantPlay;
    const cur = this.currentTimeUs();
    this.#disengageClock();
    this.workers.video?.postMessage({ type: 'pause' });
    clearInterval(this.latencyTimer);
    if (this.gestureHandler) for (const ev of ['pointerdown', 'keydown', 'touchstart']) document.removeEventListener(ev, this.gestureHandler, { capture: true }); this.gestureHandler = null;
    const oldId = this.aTrack?.id;
    this.workers.audio?.terminate(); delete this.workers.audio;
    this.waiters = this.waiters.filter(w => w.label !== 'audio');
    const a = this.audio;
    if (a) { try { a.node.port.postMessage({ type: 'detach' }); a.node.disconnect(); } catch {} this.#closeCtx(a.ctx); }
    this.audio = null; this.stats.audio = null; this.audioRoute = undefined; this.audioInfo = null;
    this.epoch++;
    this.aTrack = nt; this.audioIndex = index;
    const lanes = [];
    this.mseEl?.pause();
    try {
      if (this.mseAV || this.mseA) { const fmt = this.#mseAudioFormat(nt); if (!fmt) throw new Error('no MSE audio format'); await this.#setupMseAudio(nt, lanes, fmt); }
      else await this.#setupAudio(nt, lanes);
    } catch (e) {
      this.#warn(`audio track unusable (${e.message}); playing video only`);
      if (this.mseAV || this.mseA) { this.#mseW()?.postMessage({ type: 'audio-off' }); this.mseAV = false; this.#dropMseA(); this.workers.audio?.terminate(); delete this.workers.audio; lanes.length = 0; }
      this.aTrack = null; this.audio = null;
    }
    if (lanes[0]) this.workers.demux.postMessage({ type: 'replaceLane', oldTrackId: oldId, trackId: nt.id, kind: 'audio', port: lanes[0].port }, [lanes[0].port]);
    this.wantPlay = wasPlaying;
    this.epoch++;
    this.#startEpoch(Math.max(0, Math.min(cur, this.durationUs - 1)), null);
  }

  // ---------------------------------------------------------------- snapshot (tests): the next drawn frame plus the subtitle overlay as a PNG data URL
  async snapshotPng() {
    const bmp = this.mseEl ? await createImageBitmap(this.mseEl).catch(() => null) : await new Promise(res => { this.#snapResolve = res; this.workers.video?.postMessage({ type: 'snap' }); setTimeout(() => { if (this.#snapResolve === res) { this.#snapResolve = null; res(null); } }, 4000); });
    if (!bmp) return null;
    const out = document.createElement('canvas');
    out.width = bmp.displayWidth ?? bmp.width; out.height = bmp.displayHeight ?? bmp.height;   // an ImageBitmap, or a VideoFrame on the <video> path
    const g = out.getContext('2d');
    g.drawImage(bmp, 0, 0, out.width, out.height);
    if (bmp.close) bmp.close();
    if (this.subCanvas && this.subRenderer) {
      this.#subTick();
      const r = this.#videoRect();
      g.drawImage(this.subCanvas, r.x, r.y, r.width, r.height, 0, 0, out.width, out.height);
    }
    return out.toDataURL('image/png');
  }

  // ---------------------------------------------------------------- stop
  stop() {
    if (this._stopping) return this._stopping;
    this._stopping = (async () => {
      this.epoch++;
      const wasState = this.state;
      this.state = 'stopping'; this.onEvent({ type: 'state', state: 'stopping' });
      clearInterval(this.latencyTimer); clearInterval(this.subTimer);
      this.subRenderer?.dispose(); this.subRenderer = null; this.subCanvas?.remove(); this.subCanvas = null;
      for (const w of this.waiters) { clearTimeout(w.timer); w.reject(new Error('stopped')); }
      this.waiters = [];
      this.#disengageClock();
      if (this.gestureHandler) for (const ev of ['pointerdown', 'keydown', 'touchstart']) document.removeEventListener(ev, this.gestureHandler, { capture: true }); this.gestureHandler = null;
      this.#detachElement();
      const closing = Object.entries(this.workers).map(([label, w]) => new Promise(res => {
        const t = setTimeout(() => res({ label, timeout: true }), 2000);
        const prev = w.onmessage;
        w.onmessage = e => { if (e.data.type === 'closed') { clearTimeout(t); res({ label, ...e.data }); } };
        try { w.postMessage({ type: 'close' }); } catch { clearTimeout(t); res({ label, failed: true }); }
      }));
      const results = await Promise.all(closing);
      this.prevideo?.terminate(); this.prevideo = null;
      for (const w of Object.values(this.workers)) w.terminate();
      this.workers = {};
      this.closedInfo = Object.fromEntries(results.map(r => [r.label, r]));
      const v = results.find(r => r.label === 'video');
      this.finalAccounting = v ? { framesCreated: v.framesCreated, framesClosed: v.framesClosed, open: v.open, timeout: !!v.timeout } : null;
      const a = this.audio;
      if (a) {
        try { a.node.port.postMessage({ type: 'detach' }); a.node.disconnect(); } catch {}
        if (a.ctx === this.unlockedCtx) this.#closeCtx(a.ctx);
        else try { await Promise.race([a.ctx.close(), new Promise(r => setTimeout(r, 2000))]); } catch {}
      }
      this.audio = null;
      this.canvas?.remove(); this.canvas = null;
      this.anchor?.remove(); this.anchor = null;
      clearInterval(this.elClock?.timer); this.elClock = null; this.mseA = false;
      clearInterval(this.mse?.timer); this.mse = null; this.elEvents?.abort(); if (this.mseEl) { this.mseEl.style.display = ''; this.mseEl.pause(); this.mseEl.removeAttribute('src'); this.mseEl.srcObject = null; this.mseEl.remove(); this.mseEl = null; }
      try { this.vwriter?.close(); } catch {} this.vwriter = null; this.videoEl?.remove(); this.videoEl = null;
      this.state = 'stopped';
      this.onEvent({ type: 'state', state: 'stopped' });
      return { wasState, accounting: this.finalAccounting };
    })();
    return this._stopping;
  }
}
