// SPDX-License-Identifier: LGPL-3.0-only
// MSE worker: video packets (and, when the page asks for it, the audio worker's decoded PCM as FLAC) -> fragmented MP4 -> a
// MediaSource that lives in this worker and plays in a <video> on the page. Chrome then decodes, composites and plays it itself
// (hardware decode, overlay, HDR10 signalling kept, its own A/V sync). With audio here the element is the master clock; with
// video only the page keeps the element on the Web Audio clock (session.js).
// Speaks the video worker's protocol (init, seek, close / inited, ready, stats), so session.js only chooses which worker to start.
import { Fmp4Muxer } from './mse/fmp4.js';
import { FlacEncoder } from './mse/flac-enc.js';
import { hdrSignal, hdrSignalNeedsKeyframe } from './mse/hdr-signal.js';
import { isWebKit, MSE, mseInWorker } from './platform.js';
import { jsHeapBytes } from './wasm-mem.js';
import { stage, stageNow, stages } from './stages.js';

let FRAGMENT_US = 500_000;     // a fragment closes at the next keyframe or after this much media
let AHEAD_US = 20_000_000;       // stop taking video packets this far ahead of the playback position
// ...or this many bytes ahead, whichever comes first: Chrome's SourceBuffer quota is about 150 MB for video, and 20 s of a 90 Mbit/s
// remux is 225 MB (QuotaExceededError on every append, playback stalled). Halved after a quota hit.
const VIDEO_AHEAD_BYTES = 48 << 20;
// Everything the video SourceBuffer holds, behind the element included: it may not be cut inside the GOP being played, nor in the
// GOP before it (see trim), so 10 s GOPs at 90 Mbit/s (112 MB each) leave little of the ~150 MB quota for the ahead budget. Without
// this the append failed while the element was still in the first GOP, where Chrome could not free anything either, and playback
// stalled for good (4 MB source blocks made the fragments arrive early enough to hit it). Lowered to 85% of the buffer after a quota hit.
const VIDEO_TOTAL_BYTES = 120 << 20;
const MIN_AHEAD_US = 2_000_000;   // below this the element needs the data whatever the budget says
let BEHIND_US = 10_000_000;      // trim the buffer this far behind it...
const BEHIND_BYTES = 24 << 20;   // ...or to this many bytes: Chrome evicts on its own above the quota, and may cut a gap ahead of the element
// Both tracks share one timeline shift (the muxer keeps a preset offset), so the element time is source time + 1 s for video and
// audio alike. 1 s covers B-frame decode delay and AAC priming.
const OFFSET_US = 1_000_000;
const SEEKABLE_S = 1e6;   // longer than any file (11 days)

const post = (m, t) => self.postMessage(m, t || []);
const log = text => post({ type: 'log', text });
// Named points of a start or a seek, once per epoch each (the session lists them with its own and the other workers').
let marked = new Set();
const mark = name => { if (marked.has(name)) return; marked.add(name); post({ type: 'mark', name, at: performance.timeOrigin + performance.now() }); };

let epoch = 0, closed = false, ms = null;
let targetUs = 0, playUs = 0, readySent = false;
let video = null, audio = null;   // Buf
let vport = null, held = 0, pend = [], needKey = true, lastDurUs = 41_708, bytesIn = 0, cutOn = !isWebKit, cutAtTarget = cutOn;   // cutOn: the fragments are cut for a seek (below). Not on WebKit, where it is not measured; ?msecut=0 turns it off for A/B
let apcm = null;                   // {pass, port, enc, channels, sampleRate, baseUs, samples, pend, pendUs, held, lastDurUs}
// pass: the source's own AAC or Opus packets straight from the demux lane (no decode, no FLAC), acknowledged like video packets.
// Otherwise decoded PCM from the audio worker, encoded to FLAC here.
// Packets and PCM of an epoch this worker has not seen yet: they come over their own ports, the 'seek' over the page's, with no
// ordering between them. Dropping them lost the keyframe and the lane credit, and the seek hung.
let early = [];

// One SourceBuffer with its own operation queue. One operation runs at a time; `current` is it. An abort() (seek during an
// append) queues one more updateend for the cancelled append, so nothing starts until that event is consumed; attributing it
// to the next operation lost fragments (a seek on a 4K MKV hung in 'seeking').
class Buf {
  constructor(kind, muxer, aheadBytes, behindBytes) {
    Object.assign(this, { kind, muxer, aheadCap: aheadBytes, behindBytes, totalCap: kind === 'video' ? VIDEO_TOTAL_BYTES : Infinity, sb: null, ops: [], current: null, skip: false, frags: [],
      appendedUs: -Infinity, firstPtsUs: null, eof: false, fragments: 0, quotaHits: 0, lastError: null });
  }
  attach() {
    this.sb = ms.addSourceBuffer(this.muxer.mimeType);
    this.sb.mode = 'segments';
    this.sb.addEventListener('updateend', () => this.onUpdateEnd());
    this.sb.addEventListener('error', () => { this.lastError = 'SourceBuffer error'; post({ type: 'error', where: 'mse', message: `${this.kind} SourceBuffer error (the element rejected a fragment)` }); });
    if (!this.waitKeyframe) this.ops.push({ kind: 'append', data: this.muxer.init(), end: -Infinity });
  }
  // WebKit, a track whose HDR description is only in band: the init segment waits for the first keyframe (au null: the track has none).
  finishInit(au) {
    const t = this.waitKeyframe;
    if (!t) return;
    this.waitKeyframe = null;
    if (au) Object.assign(this.muxer, hdrSignal(t, au));
    this.ops.unshift({ kind: 'append', data: this.muxer.init(), end: -Infinity });
    this.run();
  }
  fragment(samples) {
    if (!samples.length) return;
    if (this.firstPtsUs === null) this.firstPtsUs = Math.min(...samples.map(s => s.ptsUs));
    const tMux = stageNow();
    const data = this.muxer.fragment(samples);
    stage(`mse.mux-${this.kind}`, tMux, data.length);
    if (!data.length) return;   // E-AC-3 short syncframes the muxer is still holding for a full access unit
    const end = Math.max(...samples.map(s => s.ptsUs + s.durationUs));
    // A fragment that opens with a keyframe is a place where the buffer may be cut (see trim).
    const keyStart = samples[0].key ? Math.min(...samples.map(s => s.ptsUs)) : null;
    this.ops.push({ kind: 'append', data, end, keyStart, epoch });
    this.fragments++;
    this.run();
  }
  run() {
    const sb = this.sb;
    if (this.retry || this.current || this.skip || !sb || !this.ops.length || closed || sb.updating) { if (!this.ops.length) maybeEos(); return; }
    const op = this.ops[0];
    if (op.kind === 'type') {
      if (self.MSE_DEBUG) log(`dbg type ${op.mime}`);   // synchronous, no updateend: a track switch between FLAC and a passed-through codec
      this.ops.shift();
      try { sb.changeType(op.mime); } catch (e) { this.lastError = String(e); post({ type: 'error', where: 'mse-append', message: `${this.kind} changeType ${op.mime}: ${e}` }); }
      this.run(); return;
    }
    if (self.MSE_DEBUG && (op.kind !== 'append' || op.end === -Infinity || op.epoch !== this.dbgEp)) log(`dbg ${this.kind} ${op.kind} ${op.data?.byteLength ?? ''} end=${op.end} ep=${op.epoch} cur=${epoch} mux=${this.muxer.codec} head=${op.data ? Array.from(op.data.slice(4, 8)).map(c => String.fromCharCode(c)).join('') : ''}`);
    this.dbgEp = op.epoch ?? this.dbgEp;
    try {
      const tApp = stageNow();
      if (op.kind === 'append') sb.appendBuffer(op.data);
      else sb.remove(op.start, op.end);
      this.ops.shift();
      this.current = op;
      stage(`mse.${op.kind}-${this.kind}`, tApp, op.data ? op.data.byteLength : 0);
    } catch (e) {
      if (e.name === 'QuotaExceededError' && op.kind === 'append') {
        if (this.retry) return;
        this.quotaHits++;
        if (this.kind === 'video') this.totalCap = Math.max(32 << 20, Math.min(this.totalCap, Math.round(this.totalBytes() * 0.85)));
        if (this.quotaHits === 1 || this.quotaHits % 50 === 0) { this.aheadCap = Math.max(4 << 20, this.aheadCap >> 1); log(`${this.kind} SourceBuffer full: ahead budget now ${this.aheadCap >> 20} MB`); }
        this.trim(true);
        // One retry timer per buffer: every run() caller (fragments, positions, updateend) hit the same error and started its own chain.
        if (!this.retry) this.retry = setTimeout(() => { this.retry = 0; this.run(); }, 100);
        return;
      }
      this.lastError = String(e);
      post({ type: 'error', where: 'mse-append', message: `${this.kind} ${op.kind}: ${e}` });
      this.ops.shift();
    }
  }
  onUpdateEnd() {
    if (this.skip) { this.skip = false; this.run(); return; }
    const op = this.current; this.current = null;
    if (op && op.flush === epoch) { this.flushed = epoch; flushedAll(); }
    if (op && op.kind === 'append' && op.epoch === epoch) {
      this.appendedUs = Math.max(this.appendedUs, op.end);
      if (Number.isFinite(op.end)) { mark(`${this.kind}-first-append`); if (op.end >= targetUs) mark(`${this.kind}-at-target`); }
      if (Number.isFinite(op.end)) { this.frags.push({ end: op.end, bytes: op.data.byteLength, keyStart: op.keyStart }); if (this.frags.length > 8000) this.frags.shift(); }
      checkReady();
      if (this === video) releaseCredit(); else releaseAudioCredit();
    }
    this.run();
  }
  from() { return readySent ? playUs : targetUs; }
  aheadUs() { return this.appendedUs - this.from(); }
  // Until the seek target is buffered, the fragments from the keyframe up to the target are needed, not 'ahead' (a 90 Mbit/s clip
  // with 10 s GOPs needs ~110 MB before the target, and counting it against the budget stalled the seek for good).
  aheadBytes() { const from = this.from(); let n = 0; for (const f of this.frags) if (f.end > from) n += f.bytes; return n; }
  // Bytes the SourceBuffer holds for this track, as far as this worker knows. Chrome's own eviction (it frees the GOPs behind the
  // element when an append would not fit) leaves no trace here, so what no longer starts inside a buffered range is dropped first.
  totalBytes() {
    const sb = this.sb;
    if (sb && sb.buffered.length && this.frags.length) {
      const startUs = sb.buffered.start(0) * 1e6 - OFFSET_US;
      let i = 0; while (i < this.frags.length && this.frags[i].end <= startUs) i++;
      if (i) this.frags = this.frags.slice(i);
    }
    let n = 0; for (const f of this.frags) n += f.bytes; return n;
  }
  trim(force) {
    const sb = this.sb;
    if (!sb || !sb.buffered.length) return;
    let keepUs = BEHIND_US, bytes = 0;
    for (let i = this.frags.length - 1; i >= 0; i--) {
      const f = this.frags[i];
      if (f.end > playUs) continue;
      bytes += f.bytes;
      if (bytes > this.behindBytes) { keepUs = Math.min(keepUs, Math.max(1_000_000, playUs - f.end)); break; }
    }
    // MSE removes every frame that depends on a removed one up to the next random access point, so a cut inside the GOP being
    // played takes the rest of that GOP with it and the element stalls (a 90 Mbit/s clip with 10 s GOPs did). Cut only at a
    // keyframe, the last one at or before `want`.
    // With open GOPs (x265's default) the leading pictures of a GOP reference the GOP before it, so that one is kept whole too:
    // cutting at the start of the GOP being played stalled a 4K HEVC clip as soon as the behind window was 3 s.
    const want = playUs - (force ? Math.min(keepUs, 2_000_000) : keepUs);
    const starts = this.frags.filter(f => f.keyStart != null).map(f => f.keyStart);
    let g = -1;
    for (let i = 0; i < starts.length; i++) if (starts[i] <= playUs) g = i;
    // Cutting at the start of the GOP being played took that whole GOP with it on a 90 Mbit/s clip even 7 s into it (Chrome
    // deletes to the next keyframe after the leading pictures), so the GOP before it stays until the next GOP starts.
    let cutUs = null;
    for (let i = 0; i < g; i++) if (starts[i] <= want) cutUs = starts[i];
    if (cutUs == null) return;
    const cut = (cutUs + OFFSET_US) / 1e6;
    if (cut > sb.buffered.start(0) + (force ? 0 : 1)) { this.ops.unshift({ kind: 'remove', start: 0, end: cut }); this.frags = this.frags.filter(f => f.end > cutUs); }
  }
  reset() {
    clearTimeout(this.retry); this.retry = 0;
    this.appendedUs = -Infinity; this.firstPtsUs = null; this.eof = false; this.frags = [];
    // Drop queued fragments of the old epoch and everything buffered: segments mode places the new fragments by their own
    // timestamps, and a clean buffer keeps the element from showing old pictures or playing old sound around the seek target.
    this.ops = this.ops.filter(o => o.kind === 'type' || (o.kind === 'append' && o.end === -Infinity));
    if (this.sb) {
      if (this.sb.updating && this.current?.kind === 'append') {
        // An init segment cut off here would be lost, and after a track switch (changeType) the next fragment then fails to parse.
        if (this.current.end === -Infinity) this.ops.unshift(this.current);
        this.sb.abort(); this.current = null; this.skip = true;
      }
      this.ops.push({ kind: 'remove', start: 0, end: Infinity, flush: epoch });
    } else this.flushed = epoch;
    this.run();
  }
  ranges() {
    const r = [];
    if (this.sb) for (let i = 0; i < this.sb.buffered.length; i++) r.push([Math.round(this.sb.buffered.start(i) * 1000), Math.round(this.sb.buffered.end(i) * 1000)]);
    return r;
  }
  stats() { return { fragments: this.fragments, buffered: this.ranges(), aheadMs: Number.isFinite(this.appendedUs) ? Math.round(this.aheadUs() / 1000) : null, aheadMB: +(this.aheadBytes() / 1048576).toFixed(1), ops: this.ops.length, quotaHits: this.quotaHits, lastError: this.lastError }; }
}

// Both SourceBuffers are empty for this epoch: the page may point the element at the target (see session.js #mseEarlySeek).
function flushedAll() {
  for (const b of [video, audio]) if (b && b.flushed !== epoch) return;
  post({ type: 'flushed', epoch, offsetUs: OFFSET_US });
}

function maybeEos() {
  if (!ms || ms.readyState !== 'open') return;
  for (const b of [video, audio]) if (b && (!b.eof || b.ops.length || b.current || b.sb?.updating)) return;
  if (!video && !audio) return;
  try { ms.endOfStream(); } catch {}
}

function checkReady() {
  if (readySent) return;
  for (const b of [video, audio]) if (b && !b.eof && b.appendedUs - targetUs < 300_000) return;
  readySent = true;
  post({ type: 'ready', epoch, firstPtsUs: video?.firstPtsUs ?? audio?.firstPtsUs ?? targetUs, offsetUs: OFFSET_US });
}

// ---------------------------------------------------------------- video
function ack(n) { vport.postMessage({ type: 'ack', epoch, n }); }

// Packets are acknowledged as soon as they are in a fragment, unless the buffer is already far enough ahead; then the
// acknowledgement waits, and the demuxer's credit window holds the rest back.
// Fragments still queued count too: while the SourceBuffer refuses appends (quota), appendedUs stands still, and acknowledging
// on it alone let a 90 Mbit/s file pour into the queue whole (1.35 GB) until the page froze.
const QUEUE_CAP = 16 << 20;
function queuedBytes(b) { let n = 0; for (const o of b.ops) if (o.kind === 'append' && o.data) n += o.data.byteLength; return n; }
function releaseCredit() {
  if (!held || video.aheadUs() > AHEAD_US || video.aheadBytes() > video.aheadCap || queuedBytes(video) > QUEUE_CAP) return;
  if (video.aheadUs() >= MIN_AHEAD_US && video.totalBytes() + queuedBytes(video) > video.totalCap) return;
  ack(held); held = 0;
}

// How much media a fragment collects: FRAGMENT_US, but between the seek target and the start of playback SEEK_FRAGMENT_US.
const SEEK_FRAGMENT_US = 80_000;
const seekSpan = beforeTarget => (cutOn && !beforeTarget && !readySent ? SEEK_FRAGMENT_US : FRAGMENT_US);

function flushVideo() { const s = pend; pend = []; video.fragment(s); }

function acceptVideo(m) {
  if (m.epoch > epoch) { early.push(['v', m]); return; }
  if (m.type === 'pkt') {
    if (m.epoch !== epoch) return;   // an older epoch: the demux worker reset that lane's credit on the seek
    bytesIn += m.data.byteLength;
    if (needKey && !m.key) { ack(1); return; }
    needKey = false;
    mark('video-first-key');
    if (m.key) video.finishInit(new Uint8Array(m.data));
    if (m.key && pend.length) flushVideo();
    const s = { data: new Uint8Array(m.data), ptsUs: m.ptsUs, dtsUs: m.dtsUs ?? m.ptsUs, durationUs: m.durationUs || 0, key: !!m.key };
    if (pend.length) { const d = s.dtsUs - pend[pend.length - 1].dtsUs; if (d > 0 && d < 1e6) lastDurUs = d; }
    if (!s.durationUs) s.durationUs = lastDurUs;
    // The picture at the target is decodable as soon as every packet up to its decode time is appended, and the first packet with
    // a later decode time says they all are. The fragment closes there, so the element starts decoding from the keyframe while
    // the next packets are still on the link. The decoder gives the target picture out only after a few pictures more (its
    // reorder delay), so from there until the start the fragments stay short (seekSpan): with FRAGMENT_US the element got them
    // half a second of media later (230 ms of an 85 Mbit/s link at 39 Mbit/s).
    if (cutAtTarget && s.dtsUs > targetUs) { cutAtTarget = false; if (pend.length) flushVideo(); }
    pend.push(s);
    held++;
    if (s.dtsUs - pend[0].dtsUs >= seekSpan(cutAtTarget)) flushVideo();
    releaseCredit();
  } else if (m.type === 'eof') {
    if (m.epoch !== epoch) return;
    video.finishInit(null);
    flushVideo();
    video.eof = true;
    checkReady();
    video.run();
  }
}

// ---------------------------------------------------------------- audio (PCM from the audio worker, encoded to FLAC here)
function flushAudio() {
  const a = apcm;
  if (!a.pend.length) return;
  const s = a.pend; a.pend = []; a.pendUs = 0;
  audio.fragment(s);
}

function pushFlacFrames(frames) {
  const a = apcm;
  for (const f of frames) {
    const ptsUs = a.baseUs + (a.samples * 1e6) / a.sampleRate;
    a.samples += f.samples;
    const durationUs = (f.samples * 1e6) / a.sampleRate;
    a.pend.push({ data: f.data, ptsUs, dtsUs: ptsUs, durationUs, key: true });
    a.pendUs += durationUs;
    if (a.cut && ptsUs + durationUs > targetUs) { a.cut = false; flushAudio(); }   // the sound at the target, as for video
  }
  if (a.pendUs >= seekSpan(a.cut)) flushAudio();
}

function acceptPcm(m) {
  const a = apcm;
  if (m.epoch > epoch) { early.push(['a', m]); return; }
  if (m.epoch !== epoch || !audio) return;
  if (m.type === 'pcm') {
    if (a.baseUs === null) { a.baseUs = m.ptsUs; a.samples = 0; mark('audio-first-pcm'); }
    const tEnc = stageNow();
    const enc = a.enc.encode(m.planes);
    stage('mse.flac-encode', tEnc);
    pushFlacFrames(enc);
  } else if (m.type === 'pcm-eof') {
    pushFlacFrames(a.enc.flush());
    flushAudio();
    audio.eof = true;
    checkReady();
    audio.run();
  }
}

function releaseAudioCredit() {
  const a = apcm;
  if (!a?.pass || !a.held || !a.port || audio.aheadUs() > AHEAD_US || queuedBytes(audio) > QUEUE_CAP) return;
  a.port.postMessage({ type: 'ack', epoch, n: a.held }); a.held = 0;
}

function acceptPass(m) {
  const a = apcm;
  if (m.epoch > epoch) { early.push(['p', m]); return; }
  if (m.epoch !== epoch || !audio || !a?.pass) return;
  if (m.type === 'pkts') { for (const x of m.list) acceptPass({ type: 'pkt', epoch: m.epoch, trackId: m.trackId, ptsUs: x.ptsUs, dtsUs: x.dtsUs, durationUs: x.durationUs, key: x.key, data: x.data }); return; }
  if (m.type === 'pkt') {
    bytesIn += m.data.byteLength;
    if (a.pend.length) { const d = m.ptsUs - a.pend[a.pend.length - 1].ptsUs; if (d > 0 && d < 500_000) a.lastDurUs = d; }
    const durationUs = m.durationUs || a.lastDurUs;
    a.pend.push({ data: new Uint8Array(m.data), ptsUs: m.ptsUs, dtsUs: m.ptsUs, durationUs, key: true });
    a.pendUs += durationUs;
    a.held++;
    if (a.cut && m.ptsUs + durationUs > targetUs) { a.cut = false; flushAudio(); }
    if (a.pendUs >= seekSpan(a.cut)) flushAudio();
    releaseAudioCredit();
  } else if (m.type === 'eof') {
    flushAudio();
    audio.eof = true;
    checkReady();
    audio.run();
  }
}

// The muxer (and FLAC encoder) for one audio format: {pass: {codec, description, channels, sampleRate}} or a PCM format.
function audioMux(f) {
  if (f.pass) {
    const p = f.pass;
    const mux = new Fmp4Muxer({ codec: p.codec, description: new Uint8Array(p.description), channels: p.channels, sampleRate: p.sampleRate });
    return { mux, state: { pass: true, enc: null, channels: p.channels, sampleRate: p.sampleRate, codec: p.codec } };
  }
  // Verbatim frames cost a third of the predictor's CPU, but they are raw size and Chrome's audio SourceBuffer quota is about 12 MB:
  // above 1.5 MB/s (8 ch 48 kHz 24 bit is 1.15) the compressed frames keep several seconds buffered.
  const compress = f.channels * f.sampleRate * ((f.bits || 24) / 8) > 1_500_000;
  const enc = new FlacEncoder({ channels: f.channels, sampleRate: f.sampleRate, bitsPerSample: f.bits || 24, compress });
  const mux = new Fmp4Muxer({ codec: 'flac', description: enc.streamInfo(), channels: f.channels, sampleRate: f.sampleRate });
  return { mux, state: { pass: false, enc, channels: f.channels, sampleRate: f.sampleRate, codec: 'flac' } };
}

// ---------------------------------------------------------------- lifecycle
async function openSource(audioFmt) {
  ms = new MSE();
  const handle = ms.handle;
  post({ type: 'mse-handle', handle }, [handle]);
  await new Promise((res, rej) => { ms.addEventListener('sourceopen', res, { once: true }); setTimeout(() => rej(new Error('MediaSource did not open within 5 s')), 5000); });
  // Every SourceBuffer is added before the first init segment is appended (Chrome refuses a new one once it has metadata).
  if (video) video.attach();
  if (audioFmt) {
    try {
      const { mux, state } = audioMux(audioFmt);
      mux.offsetUs = OFFSET_US;
      if (!MSE.isTypeSupported(mux.mimeType)) throw new Error(`MSE does not accept ${mux.mimeType}`);
      audio = new Buf('audio', mux, 8 << 20, 4 << 20);
      audio.attach();
      apcm = { port: null, ...state, baseUs: null, samples: 0, pend: [], pendUs: 0, held: 0, lastDurUs: 21_333, cut: cutOn };
    } catch (e) { audio = null; apcm = null; openSource.audioError = String(e && e.message || e); }
  }
  ms.duration = Infinity;
  // With an unbounded duration the element may only seek inside what is buffered (seekable is [0, end of the buffered data], and
  // empty while nothing is buffered: a seek is then dropped without an event). The page points the element at a target before
  // its data is appended, so the whole timeline is declared seekable.
  if (!isWebKit) try { ms.setLiveSeekableRange(0, SEEKABLE_S); } catch {}   // WebKit keeps seeking once the target is buffered (session.js)
  video?.run(); audio?.run();
}

function resetEpoch(newEpoch, target) {
  epoch = newEpoch; targetUs = target; playUs = target; readySent = false; marked = new Set();
  pend = []; needKey = true; held = 0; cutAtTarget = cutOn;
  if (apcm) { apcm.enc?.reset(); apcm.baseUs = null; apcm.samples = 0; apcm.pend = []; apcm.pendUs = 0; apcm.held = 0; apcm.cut = cutOn; }
  // A seek gives the budgets back: one quota hit in a heavy scene otherwise kept a 90 Mbit/s stream at the 2 s floor for the session.
  if (video) { video.aheadCap = VIDEO_AHEAD_BYTES; video.totalCap = VIDEO_TOTAL_BYTES; }
  video?.reset(); audio?.reset();
  flushedAll();   // nothing was attached yet, so nothing had to be removed
  const keep = early.filter(([, m]) => m.epoch >= epoch); early = [];
  for (const [k, m] of keep) (k === 'v' ? acceptVideo : k === 'p' ? acceptPass : acceptPcm)(m);
}

self.onmessage = async e => {
  const m = e.data;
  try {
    if (m.type === 'init') {
      epoch = m.epoch;
      self.MSE_DEBUG = !!m.debug;
      if (m.cutAtTarget === false) cutOn = cutAtTarget = false;
      if (m.aheadS) AHEAD_US = m.aheadS * 1e6;
      if (m.fragMs) FRAGMENT_US = m.fragMs * 1000;   // ?msefrag=<ms>, for A/B
      if (m.behindS) BEHIND_US = m.behindS * 1e6;
      if (!mseInWorker) throw new Error('MediaSource is not available in a worker');
      const track = m.track;
      if (track) {
        // WebKit only: Chrome's MP4 parser reads colr, mdcv and clli too, into the colour space and HDR metadata of its decoder config, so
        // they could change its picture, and none of its HDR paths have been measured with them.
        const hdr = isWebKit ? hdrSignal(track) : null;
        const vm = new Fmp4Muxer({ codec: track.nativeDv?.codec || track.codec, description: track.description || null, width: track.width, height: track.height, dolbyVision: track.nativeDv?.record || null, ...hdr });
        vm.offsetUs = OFFSET_US;
        if (!MSE.isTypeSupported(vm.mimeType)) throw new Error(`MSE does not accept ${vm.mimeType}`);
        video = new Buf('video', vm, VIDEO_AHEAD_BYTES, BEHIND_BYTES);
        if (hdr && hdrSignalNeedsKeyframe(track, hdr)) video.waitKeyframe = track;
        vport = m.port;
      }
      await openSource(m.audio);
      if (vport) vport.onmessage = ev => acceptVideo(ev.data);
      post({ type: 'inited', decoder: track ? { codec: track.codec, hardwareAcceleration: 'browser (MSE)', mime: video.muxer.mimeType } : null, codec: track?.codec || null, size: track ? `${track.width}x${track.height}` : null, audio: !!audio, audioError: openSource.audioError || null });
    } else if (m.type === 'audio-port') {
      // A new audio worker (first start or a track switch); its blocks carry their own epoch. A switch to another channel count or
      // rate gets a new encoder and a new init segment, queued ahead of the next epoch's fragments (reset keeps init appends).
      // A passed-through track (m.pass) brings the demux lane itself; its packets are acknowledged here.
      if (!apcm) { m.port.close(); return; }
      const f = m.pass ? { pass: m.pass } : { channels: m.channels || apcm.channels, sampleRate: m.sampleRate || apcm.sampleRate, bits: m.bits };
      const codec = f.pass ? f.pass.codec : 'flac';
      if (codec !== apcm.codec || f.pass || f.channels !== apcm.channels || f.sampleRate !== apcm.sampleRate) {
        const { mux, state } = audioMux(f);
        mux.offsetUs = OFFSET_US;
        if (mux.mimeType !== audio.muxer.mimeType) audio.ops.push({ kind: 'type', mime: mux.mimeType });
        Object.assign(apcm, state, { baseUs: null, samples: 0, pend: [], pendUs: 0, held: 0 });
        audio.muxer = mux;
        audio.ops.push({ kind: 'append', data: mux.init(), end: -Infinity });
        audio.run();
        log(`audio format now ${f.pass ? `${codec} passed through` : 'FLAC'} ${apcm.channels} ch ${apcm.sampleRate} Hz`);
      }
      apcm.port?.close();
      apcm.port = m.port;
      apcm.port.onmessage = ev => (apcm.pass ? acceptPass : acceptPcm)(ev.data);
    } else if (m.type === 'audio-off') {
      // The audio track turned out unusable: without its SourceBuffer the element plays the video alone.
      if (audio?.sb && ms.readyState === 'open') { try { if (audio.sb.updating) audio.sb.abort(); ms.removeSourceBuffer(audio.sb); } catch {} }
      apcm?.port?.close(); audio = null; apcm = null;
      checkReady();
    } else if (m.type === 'seek') {
      resetEpoch(m.epoch, m.targetUs);
    } else if (m.type === 'position') {
      playUs = m.us;
      for (const b of [video, audio]) if (b?.sb && !b.sb.updating && !b.ops.length) b.trim(false);
      if (video) releaseCredit();
      if (apcm?.pass) releaseAudioCredit(); else apcm?.port?.postMessage({ type: 'position', epoch, us: m.us });
      video?.run(); audio?.run();
    } else if (m.type === 'close') {
      closed = true;
      try { if (ms && ms.readyState === 'open') ms.endOfStream(); } catch {}
      vport?.close(); apcm?.port?.close();
      post({ type: 'closed', framesCreated: 0, framesClosed: 0, open: 0 });
    }
    // clock, play, pause, snap and lead are the page's business on this path (it owns the element).
  } catch (err) {
    post({ type: m.type === 'init' ? 'init-failed' : 'error', where: 'mse', message: String(err && err.message || err) });
  }
};

setInterval(() => {
  if (closed || !ms) return;
  post({ type: 'mse-stats', epoch, bytesIn, held, offsetUs: OFFSET_US, video: video?.stats() || null, audio: audio?.stats() || null, heap: jsHeapBytes(), stages: stages() });
}, 500);
