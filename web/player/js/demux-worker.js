// SPDX-License-Identifier: LGPL-3.0-only
// Demux worker: owns the Source and the Demuxer, routes packets to the video and audio workers over
// MessagePorts with credit-based backpressure (each consumer acks packets as it takes them).
import { buildDac3, buildDec3 } from './mse/dolby-audio.js';
import { wasmBytes, jsHeapBytes } from './wasm-mem.js';
import { BlobSource, openUrl } from './source.js';
import { createJsDemuxer } from './demux/index.js';
import { openDemuxer, splitDolbyVision, relabelPacketSps, convertPacketTo81, dolbyVisionElKind, P5_CARRIER } from '../../engine/demuxer.mjs';
import { sourceTrace, ReadInterrupted } from '../../engine/sources.mjs';
import { initHdr10Plus, extractHdr10Plus } from '../../engine/hdr10plus.mjs';
import { lengthSizeOfHvcc } from './dv-util.js';
import { splitAnnexB, buildAvcCAll, PpsRenumber, annexBToLengthPrefixed, avcCodecFromAvcC, buildHvcC, annexBToLengthPrefixedHevc } from './demux/annexb.js';
import { hevcCodecString } from '../../engine/demuxer.mjs';
import { VIDEO_INFLIGHT, AUDIO_INFLIGHT, AUDIO_WINDOW_US, PENDING_BYTES_CAP, yieldNow } from './protocol.js';
import { stage, stageNow, stages } from './stages.js';

let epoch = 0, gen = 0, closed = false, fillReleased = false;
let source = null, demuxer = null, engineName = '';
let engineInput = null;
let originUs = 0;           // container start offset (transport streams start at 600 s): subtracted from every timestamp
const annexB = new Set();   // trackIds of Annex B H.264 and HEVC rewritten to avcC / hvcC + length prefixes
const compTracks = new Map();   // trackId -> {track, dv, ls}: tracks going through the compositor (split layers, relabelled SPS, RPU / HDR10+ attached)
const dvTracks = new Map();   // trackId -> track, video tracks whose enhancement layer must not reach WebCodecs
// Dual-track Profile 7 (the enhancement layer and its RPU in a video track of their own, bl_present_flag 0): the base layer track is
// presented as the Dolby Vision track and gets each enhancement access unit of the same pts attached, as in a single-track file.
const dualDv = new Map();        // base layer trackId -> {elId, dolbyVision}
const dualEl = new Map();        // enhancement trackId -> base layer trackId
let elByPts = new Map(), blHold = [];
function findDualLayer(tracks) {
  dualDv.clear(); dualEl.clear();
  const vids = tracks.filter(t => t.kind === 'video');
  for (const e of vids) {
    if (!(e.dolbyVision && e.dolbyVision.blPresent === false && e.dolbyVision.elPresent)) continue;
    const b = vids.find(t => t !== e && !dualEl.has(t.id) && /^(hvc1|hev1)/.test(t.codec) && !(t.dolbyVision && t.dolbyVision.blPresent === false));
    if (!b) continue;
    dualDv.set(b.id, { elId: e.id, dolbyVision: { ...e.dolbyVision, blPresent: true } });
    dualEl.set(e.id, b.id);
  }
}
function presentTracks(tracks) {
  return tracks.map(t => dualDv.has(t.id) ? { ...t, dolbyVision: dualDv.get(t.id).dolbyVision } : dualEl.has(t.id) ? { ...t, enhancementLayer: true } : t);
}
const lanes = new Map();   // trackId -> {kind, port, pending:[], inflight, limit, eofSent}
let wake = null, eof = false, pendingBytes = 0;
let damagedSeen = 0;
let packetsRead = 0, packetsSent = 0, firstRouted = false, native81 = null, baseOnly = null, positionedAt = null;   // null: the start scan left the cursor somewhere after the first packets
const subIds = new Set();   // subtitle tracks whose packets go to the main thread (small, no credit window)

const post = m => self.postMessage(m);
const mark = (name, at = performance.timeOrigin + performance.now()) => post({ type: 'mark', name, at });
sourceTrace.mark = mark;
mark('boot');
const wakeUp = () => { const w = wake; wake = null; w?.(); };

function onAck(lane, m) {
  if (m.epoch !== epoch) return;
  lane.inflight = Math.max(0, lane.inflight - m.n);
  wakeUp();
}

// The download that served the first packet after a start or a seek: when it was asked for, answered and delivered its first bytes.
// Only when it began after the seek did; otherwise the packet came out of what the ring already held.
let seekAt = 0;
function netMarks() {
  const t = demuxer.source?.stats?.().times;
  if (!t || !(t[0] >= seekAt)) return;
  mark('net-request', t[0]);
  if (t[1]) mark('net-response', t[1]);
  if (t[2]) mark('net-first-byte', t[2]);
}

// Bytes per second of media over the last 20 s of demuxed video, so the buffered bytes can be stated in seconds of playback (a remux
// varies too much, and a size / duration guess is useless when only part of the file is known).
let mediaBytes = 0, rateMarks = [];
function noteMedia(p) {
  mediaBytes += p.data.byteLength;
  if (lanes.get(p.trackId)?.kind !== 'video') return;
  const last = rateMarks[rateMarks.length - 1];
  if (last && p.ptsUs < last.pts + 500000) return;
  rateMarks.push({ pts: p.ptsUs, bytes: mediaBytes });
  while (rateMarks.length > 2 && p.ptsUs - rateMarks[0].pts > 20e6) rateMarks.shift();
}
function mediaRate() {
  const a = rateMarks[0], b = rateMarks[rateMarks.length - 1];
  return a && b.pts - a.pts >= 1e6 ? ((b.bytes - a.bytes) * 1e6) / (b.pts - a.pts) : null;
}
let netPrev = null;
function netStats() {
  const s = demuxer?.source?.stats?.();
  if (!s) return null;
  const now = performance.now(), rate = mediaRate();
  const linkBps = s.linkKbps ? s.linkKbps * 1000 : null;   // measured by the download itself: this thread may be blocked in a read for a second
  let waitFrac = null;
  if (netPrev && now > netPrev.t) {
    waitFrac = Math.min(1, (s.waitMs - netPrev.waitMs) / (now - netPrev.t));
  }
  netPrev = { t: now, end: s.end, waitMs: s.waitMs };
  return { ...s, bytesPerMediaS: rate && Math.round(rate), aheadS: rate ? (s.ahead + pendingBytes) / rate : null, linkBps, waitFrac };
}

function resetLanes() {
  rateMarks = []; netPrev = null;
  elByPts = new Map(); blHold = [];
  for (const l of lanes.values()) { l.pending = []; l.inflight = 0; l.eofSent = false; l.gate = null; }
  pendingBytes = 0; eof = false;
}

function flushLanes() {
  const tPost = stageNow();
  for (const [id, l] of lanes) {
    // Audio goes out as one message per pass, not one per packet: TrueHD is 1200 access units a second, and a post plus an ack
    // for each one made the lane's throughput depend on how often this loop came back from a blocking read (measured 0.5 s of
    // audio per second while the video buffer was filling, on a link three times the bitrate).
    const list = l.kind === 'audio' ? [] : null, bufs = [];
    while (l.pending.length && l.inflight < l.limit) {
      const p = l.pending.shift();
      pendingBytes -= p.data.byteLength;
      l.inflight++; packetsSent++;
      const buf = p.data.buffer;
      if (list) {
        if (p.durationUs > 0) l.limit = Math.max(AUDIO_INFLIGHT, Math.min(4096, Math.ceil(AUDIO_WINDOW_US / p.durationUs)));
        list.push({ ptsUs: p.ptsUs, dtsUs: p.dtsUs, durationUs: p.durationUs, key: p.key, data: buf }); bufs.push(buf);
        continue;
      }
      const msg = { type: 'pkt', epoch, trackId: id, ptsUs: p.ptsUs, dtsUs: p.dtsUs, durationUs: p.durationUs, key: p.key, data: buf };
      const tr = [buf];
      if (p.el) { msg.el = p.el.buffer; tr.push(p.el.buffer); pendingBytes -= p.el.byteLength; }
      if (p.rpu) msg.rpu = p.rpu;
      if (p.h10p) msg.h10p = p.h10p;
      l.port.postMessage(msg, tr);
    }
    if (list && list.length) l.port.postMessage({ type: 'pkts', epoch, trackId: id, list }, bufs);
    if (eof && !l.pending.length && !l.eofSent) { l.eofSent = true; l.port.postMessage({ type: 'eof', epoch }); }
  }
  stage('demux.post', tPost);
}

function wantsMore() {
  if (eof || pendingBytes >= PENDING_BYTES_CAP) return false;
  for (const l of lanes.values()) if (!l.pending.length && l.inflight < l.limit) return true;
  return false;
}

// A seek must not wait behind the read-ahead: the loop's reads block for as long as the link takes (one read is a megabyte of
// Matroska indexing or a keyframe: 100 to 200 ms at 85 Mbit/s, and the seek waited 55 to 280 ms for its turn, 0 to 3 ms with this). The page counts
// its seeks in a shared word before it posts each one; while that count is ahead of the seeks handled here, a read of the loop
// that is waiting for data gives up (ReadInterrupted) and the loop ends. The seek that follows positions the demuxer anew, so
// what the abandoned read left half done does not matter. Only the loop's own reads are interruptible, never a seek's.
let seekWord = null, seeksHandled = 0;
const seekPending = () => seekWord !== null && Atomics.load(seekWord, 0) !== seeksHandled;
async function loop() {
  try { await loopBody(); }
  catch (err) { if (!closed && !(err instanceof ReadInterrupted)) post({ type: 'error', where: 'demux', message: String(err && err.message || err) }); }
}
// The loop reads synchronously, and a read that waits for the network blocks this thread: acknowledgements and a seek sit in its
// queue meanwhile. With a yield only every 16 packets, a lane out of credit got its next 12 packets once per 16 read (each a
// wait for the link after a seek), so video reached the consumer at about half the rate it was downloaded: on a 85 Mbit/s link
// the packet at the seek target was read 5.0 s after a seek and posted at 8.7 s, with 38 MB waiting in the lane. So the loop
// gives the queue a turn after every read that waited and while a lane is held.
function laneHeld() {
  for (const l of lanes.values()) if (l.pending.length && l.inflight >= l.limit) return true;
  return false;
}
async function loopBody() {
  const my = ++gen;
  let n = 0;
  while (my === gen && !closed) {
    flushLanes();
    if (wantsMore()) {
      const tRead = stageNow();
      const src = demuxer.source, waited = src?.waitMs;
      if (src && seekWord) src.interrupt = seekPending;
      let p;
      try { p = demuxer.nextPacket(); } finally { if (src) src.interrupt = null; }
      const turn = (src && src.waitMs !== waited) || laneHeld();
      stage('demux.read', tRead, p ? p.data.byteLength : 0);
      if (!p) { releaseHeld(true); for (const l of lanes.values()) openGate(l); eof = true; continue; }
      packetsRead++;
      noteMedia(p);
      if (dualEl.has(p.trackId)) {
        if (lanes.has(dualEl.get(p.trackId))) { elByPts.set(p.ptsUs, p.data.slice()); releaseHeld(false); }
        if ((++n & 15) === 0 || turn) await yieldNow();
        continue;
      }
      if (dualDv.has(p.trackId) && lanes.has(p.trackId)) {
        blHold.push(p); releaseHeld(false);
        if ((++n & 15) === 0 || turn) await yieldNow();
        continue;
      }
      routePacket(p);
      if ((++n & 15) === 0 || turn) await yieldNow();
      continue;
    }
    await new Promise(r => { wake = r; });
  }
}

// Base layer packets wait (in decode order) for the enhancement access unit of the same pts; 'force' lets them go without one
// (end of stream, or an enhancement layer that skips a picture: 48 packets later it is not coming).
function releaseHeld(force) {
  while (blHold.length) {
    const p = blHold[0], el = elByPts.get(p.ptsUs);
    if (!el && !force && blHold.length < 48) break;
    blHold.shift();
    if (el) { elByPts.delete(p.ptsUs); p.dualEl = el; }
    routePacket(p);
  }
  if (elByPts.size > 96) { const keep = new Set(blHold.map(p => p.ptsUs)); for (const k of elByPts.keys()) if (!keep.has(k) && elByPts.size > 64) elByPts.delete(k); }
}

function routePacket(p) {
  {
    {
      const lane = lanes.get(p.trackId);
      if (!lane && subIds.has(p.trackId)) {
        const d = p.data.slice();
        post({ type: 'subpkt', epoch, trackId: p.trackId, ptsUs: p.ptsUs - originUs, durationUs: p.durationUs, data: d.buffer }, [d.buffer]);
      } else if (lane) {
        if (originUs) { p.ptsUs -= originUs; p.dtsUs -= originUs; }
        if (annexB.has(p.trackId)) p.data = hevcAnnexB.has(p.trackId) ? annexBToLengthPrefixedHevc(p.data) : annexBToLengthPrefixed(p.data, ppsRenumber.get(p.trackId));
        const pr = ppsRenumber.get(p.trackId);
        if (pr?.failed && !pr.reported) {   // visible, not silent: from here the hardware decoder may refuse the stream (-12909)
          pr.reported = true;
          post({ type: 'log', text: `pps-renumber-overflow track ${p.trackId} at ${(p.ptsUs / 1e6).toFixed(1)} s: more than 18 picture parameter set contents in use inside one GOP (${pr.recycled} ids recycled)` });
        }
        if (native81?.id === p.trackId) p.data = convertPacketTo81(p, native81.ls);
        else if (baseOnly?.id === p.trackId) { const bl = splitDolbyVision(p, baseOnly.ls).bl; if (bl.length) p.data = bl; }   // the element gets plain HEVC: no RPU, no enhancement layer
        else if (compTracks.has(p.trackId)) {
          const c = compTracks.get(p.trackId);
          let bl = p.data;
          if (c.dv) {
            const sp = splitDolbyVision(p, c.ls);
            if (sp.bl.length) bl = sp.bl;
            if (sp.el.length) p.el = relabelPacketSps(sp.el, c.ls).data.slice();
            if (sp.rpu) p.rpu = sp.rpu;
            if (p.dualEl) {
              // the enhancement track carries plain HEVC NALs (no type 63 wrapper) and the RPU (type 62)
              const se = splitDolbyVision(p.dualEl, c.ls);
              if (se.bl.length) p.el = relabelPacketSps(se.bl, c.ls).data.slice();
              if (se.rpu && !p.rpu) p.rpu = se.rpu;
              p.dualEl = null;
            }
          } else { const h = extractHdr10Plus(p, c.track); if (h) p.h10p = h; }
          p.data = (c.p5 ? relabelPacketSps(bl, c.ls, P5_CARRIER) : relabelPacketSps(bl, c.ls)).data;
        } else if (dvTracks.has(p.trackId)) {
          // Dolby Vision P7 (BL + EL + RPU in one access unit): the decoder gets the HEVC base layer only (plays as HDR10).
          const bl = splitDolbyVision(p, dvTracks.get(p.trackId)).bl;
          if (bl.length) p.data = bl;
        }
        if (!firstRouted) { firstRouted = true; netMarks(); mark('first-packet'); demuxer.source?.preloadTail?.(); }
        let d = p.data;
        if (d.byteOffset !== 0 || d.byteLength !== d.buffer.byteLength) d = d.slice();   // never transfer a shared buffer
        p.data = d;
        if (lane.gate && compTracks.has(p.trackId)) gatePacket(lane, p, compTracks.get(p.trackId).ls);
        else enqueue(lane, p);
      }
    }
  }
}

function enqueue(lane, p) {
  lane.pending.push(p); pendingBytes += p.data.byteLength + (p.el ? p.el.byteLength : 0);
}

// Seek gate of a video lane we decode ourselves (canvas paths: exact / GPU Dolby Vision, WebCodecs). A seek with audio starts the
// cursor lookbackUs (3 s) early, and every video packet from there would be decoded: on the 91 Mbit/s awaken stream (keyframe
// every second) a seek decoded 84 pictures before showing the target. The gate holds video packets from the seek on and drops
// them whenever a random access picture (NAL type checked: Matroska key flags are not sync points) at or before the target
// arrives; it opens once the decode time passes the target, at the end of the stream, or past GATE_BYTES (then all held packets go on).
const GATE_BYTES = 64 << 20;
function isIrapPacket(data, ls) {
  for (let i = 0; i + ls + 2 <= data.length;) {
    let n = 0; for (let k = 0; k < ls; k++) n = n * 256 + data[i + k];
    const t = (data[i + ls] >> 1) & 63;
    if (t < 32) return t >= 16 && t <= 23;   // first VCL NAL
    i += ls + n;
  }
  return false;
}
function gatePacket(lane, p, ls) {
  const g = lane.gate;
  if (p.ptsUs <= g.targetUs && isIrapPacket(p.data, ls)) { g.buf = []; g.bytes = 0; }
  g.buf.push(p); g.bytes += p.data.byteLength + (p.el ? p.el.byteLength : 0);
  if (p.dtsUs > g.targetUs || g.bytes > GATE_BYTES) openGate(lane);
}
function openGate(lane) {
  const g = lane.gate; if (!g) return;
  lane.gate = null;
  for (const q of g.buf) enqueue(lane, q);
}

// Reads the first packets once to (a) find the stream start (transport streams carry a large PCR offset) and (b) build an
// avcC for Annex B H.264 without a description. The cursor is left where the scan stopped (positionedAt null); 'select' seeks.
const ppsRenumber = new Map();   // trackId -> PpsRenumber of an Annex B H.264 track (Blu-ray redefines one PPS id per picture)
const hevcAnnexB = new Set();   // trackIds of Annex B HEVC rewritten to hvcC + length prefixes
const hdr10plusTracks = new Set();
const DOLBY_AUDIO = /^(eac3|ec-3|ac3|ac-3)$/;
const SCAN_BYTES = 2 << 20;   // start scan: stop here once every track has been seen (see scanStart). 6 MiB took 800 ms over a 50 Mbit/s CDN
const trackById = id => demuxer.tracks().find(t => t.id === id);
function scanStart() {
  demuxer.seek(0);
  const firstPts = new Map(), firstKey = new Map(), firstDolby = new Map();
  hdr10plusTracks.clear(); ppsRenumber.clear();
  const scanSets = new Map();
  // The scan is bounded by bytes as well as packets. 600 packets of a 90 Mbit/s 4K file are ~280 MB, read twice (the Rust demuxer is
  // reopened for the cursor at 0 right after): measured over a 45 Mbit/s link, a 40 Mbit/s file opened after 9.5 s and a 90 Mbit/s one
  // after 9 to 10 s even at 150 Mbit/s, with nothing but this scan to read. Once the first pts of every track (and the keyframe an
  // Annex B track needs) are in, only the HDR10+ detection is left, and it reads the first frames only.
  // A transport stream finds its tracks while reading, so "every track seen" means nothing before SCAN_BYTES there. Matroska and MP4
  // name their tracks in the header: the scan ends with the packet that completes the set (3 to 8 MiB less to wait for before the first frame).
  const scanFloor = demuxer.container === 'matroska' || demuxer.container === 'mp4' ? 0 : SCAN_BYTES;
  let scannedBytes = 0, enough = false;
  for (let p, scanned = 0; scanned < 600 && !enough && (p = demuxer.nextPacket()); scanned++) {
    scannedBytes += p.data.byteLength;
    if (scanned < 240 && /^(hvc1|hev1)/.test(trackById(p.trackId)?.codec || '') && !trackById(p.trackId).dolbyVision && !hdr10plusTracks.has(p.trackId) && extractHdr10Plus(p, trackById(p.trackId))) hdr10plusTracks.add(p.trackId);
    if (!firstPts.has(p.trackId)) firstPts.set(p.trackId, p.ptsUs);
    if (p.key && !firstKey.has(p.trackId) && firstKey.size < 4) firstKey.set(p.trackId, p.data.slice());
    if (firstKey.has(p.trackId) && trackById(p.trackId)?.annexB && /^avc1/.test(trackById(p.trackId).codec)) {
      // every parameter set of the scanned start goes into the avcC, each PPS content under its own id
      let r = ppsRenumber.get(p.trackId);
      if (!r) { ppsRenumber.set(p.trackId, r = new PpsRenumber()); scanSets.set(p.trackId, []); }
      for (const x of splitAnnexB(p.data)) if ((x[0] & 31) === 7 || (x[0] & 31) === 8) scanSets.get(p.trackId).push(r.nal(x).slice());
    }
    const dvt = trackById(p.trackId);
    if (dvt?.dolbyVision?.profile === 7 && !dvt.dolbyVision.elKind) { try { const k = dolbyVisionElKind(p, lengthSizeOfHvcc(dvt.description)); if (k !== 'unknown') dvt.dolbyVision.elKind = k; } catch {} }
    if (DOLBY_AUDIO.test(trackById(p.trackId)?.codec || '') && !firstDolby.has(p.trackId)) firstDolby.set(p.trackId, p.data.slice());
    if (scannedBytes >= scanFloor) {
      const ts = demuxer.tracks();
      enough = ts.every(t => t.kind === 'subtitle' || firstPts.has(t.id)) && ts.every(t => t.kind !== 'video' || t.description || !(t.annexB || /^avc1/.test(t.codec)) || firstKey.has(t.id));
    }
  }
  const tracks = demuxer.tracks();   // transport streams discover tracks while reading
  for (const t of tracks) {
    const pkt = firstDolby.get(t.id);
    if (pkt) t.dolbyAudio = /^(eac3|ec-3)/.test(t.codec) ? buildDec3(pkt, t.jocComplexity || 0) : buildDac3(pkt);
  }
  const starts = tracks.filter(t => t.kind !== 'subtitle' && firstPts.has(t.id)).map(t => firstPts.get(t.id));
  const min = starts.length ? Math.min(...starts) : 0;
  originUs = min >= 500000 ? min : 0;
  for (const t of tracks) {
    if (t.kind === 'video' && !t.description && /^(hev1|hvc1)/.test(t.codec) && t.annexB && firstKey.has(t.id)) {
      // MPEG-TS HEVC: hvcC from the in-band VPS/SPS/PPS, packets rewritten to length prefixes (same as the H.264 path below)
      const hvcc = buildHvcC(splitAnnexB(firstKey.get(t.id)));
      if (hvcc) { t.description = hvcc; t.codec = hevcCodecString(hvcc); annexB.add(t.id); hevcAnnexB.add(t.id); }
      continue;
    }
    if (t.kind !== 'video' || t.description || !/^avc1/.test(t.codec) || !firstKey.has(t.id)) continue;
    const avcc = buildAvcCAll(scanSets.get(t.id) || []);
    if (avcc) { t.description = avcc; t.codec = avcCodecFromAvcC(avcc); annexB.add(t.id); }
  }
  return tracks;
}

// Position the demuxer. Engine seek() lands on the video keyframe's storage position, which drops interleaved audio stored
// before it (MOV: the first 2 s of audio in the A/V sync file). Near the start the demuxer is therefore reopened (cursor at
// storage position 0) instead; the engine has no rewind().
async function seekTo(timeUs) {
  if (timeUs <= 0 && engineName === 'rust-engine' && engineInput) {
    // The source (and its cached blocks) carries over: without it the reopen after the start scan read the first 6 MB again over
    // HTTP, about 2 s of a 45 Mbit/s link.
    const src = demuxer.source;
    demuxer.close({ keepSource: !!src });
    demuxer = await openDemuxer(engineInput, src ? { reuse: src } : {});
    return;
  }
  demuxer.seek(timeUs + originUs);
}

self.onmessage = async e => {
  const m = e.data;
  try {
    if (m.type === 'open') {
      if (m.demux === 'js') {
        source = m.file ? new BlobSource(m.file) : await openUrl(m.url, m.headers || {}, m.sourceOpts || {});
        demuxer = createJsDemuxer(source);
        engineName = 'js-mp4 (interim)';
      } else {
        // The engine reads synchronously: Blob via FileReaderSync, URL via its own HTTP sources (engine/sources.mjs). A server
        // without Range support gets the whole file downloaded into a Blob first (source.js openUrl).
        let input = m.file;
        if (!input) {
          mark('probe-start');
          source = await openUrl(m.url, m.headers || {}, { ...(m.sourceOpts || {}), describeOnly: true });
          mark('probe-done');
          input = source.mode === 'http-range' ? { url: source.url, length: source.size, headers: source.headers, stream: source.far, prebuilt: source.prebuilt } : source.blob;
        } else source = { mode: 'blob', name: m.file.name || 'blob', size: m.file.size };
        engineInput = input;
        demuxer = await openDemuxer(input);
        if (fillReleased) demuxer.source?.releaseFill?.();   // adopted while the opening was still running
        mark(`header-parsed wait ${Math.round(demuxer.source?.stats?.().waitMs ?? 0)}ms side ${demuxer.source?.stats?.().sideReads ?? 0}`);
        engineName = 'rust-engine';
      }
      let tracks = demuxer.tracks();
      if (engineName === 'rust-engine') { await initHdr10Plus().catch(() => {}); tracks = scanStart(); for (const t of tracks) if (hdr10plusTracks.has(t.id)) t.hdr10plus = true; mark('start-scan-done'); }
      findDualLayer(tracks); tracks = presentTracks(tracks);
      post({ type: 'tracks', tracks, durationUs: demuxer.durationUs || 0, engine: engineName, source: { mode: source.mode, name: source.name, size: source.size } });
      // A resume: the worker and GPU setup that follows on the main thread takes some 100 ms, and the download at the start position is the longest part of the opening.
      if (m.startUs > 0 && engineName === 'rust-engine') { mark('start-seek'); await seekTo(m.startUs); positionedAt = m.startUs; mark('start-position'); }
    } else if (m.type === 'select') {
      // m.lanes: [{trackId, kind, port}]
      epoch = m.epoch;
      if (m.seekWord) seekWord = new Int32Array(m.seekWord);
      await initHdr10Plus().catch(() => {});
      native81 = null; baseOnly = null;
      for (const t of presentTracks(demuxer.tracks())) {
        if (t.kind !== 'video' || !m.lanes.some(l => l.trackId === t.id)) continue;
        if (m.native81 === t.id) { native81 = { id: t.id, ls: lengthSizeOfHvcc(t.description) }; continue; }
        if (m.baseOnly === t.id) { baseOnly = { id: t.id, ls: lengthSizeOfHvcc(t.description) }; continue; }
        if (m.compositor && /^(hvc1|hev1|dvh1|dvhe)/.test(t.codec) && (t.dolbyVision || m.compositor === 'all')) compTracks.set(t.id, { track: t, dv: !!t.dolbyVision, p5: t.dolbyVision?.profile === 5, ls: lengthSizeOfHvcc(t.description) });
        else if (t.dolbyVision?.elPresent) dvTracks.set(t.id, t);
      }
      for (const l of m.lanes) {
        const lane = { kind: l.kind, port: l.port, pending: [], inflight: 0, limit: l.kind === 'video' ? VIDEO_INFLIGHT : AUDIO_INFLIGHT, eofSent: false };
        l.port.onmessage = ev => { if (ev.data.type === 'ack') onAck(lane, ev.data); };
        lanes.set(l.trackId, lane);
      }
      mark('select');
      if (positionedAt !== (m.startUs || 0)) { await seekTo(m.startUs || 0); mark('start-position'); }
      resetLanes();
      loop();
    } else if (m.type === 'release-fill') {
      fillReleased = true;
      demuxer?.source?.releaseFill?.();
    } else if (m.type === 'subs') {
      subIds.clear(); for (const id of m.trackIds) subIds.add(id);
    } else if (m.type === 'replaceLane') {
      // audio track switch: a new port for another track; the caller seeks afterwards, which resets every lane
      lanes.delete(m.oldTrackId);
      const lane = { kind: m.kind, port: m.port, pending: [], inflight: 0, limit: AUDIO_INFLIGHT, eofSent: false };
      m.port.onmessage = ev => { if (ev.data.type === 'ack') onAck(lane, ev.data); };
      lanes.set(m.trackId, lane);
    } else if (m.type === 'seek') {
      if (m.seq != null) seeksHandled = m.seq;
      gen++; wakeUp();
      epoch = m.epoch;
      // Audio is interleaved after the video keyframe it follows, so a cold seek can start the audio later than the target
      // (LPCM chunks in MOV are seconds long). Start the cursor earlier; both workers drop what precedes the target.
      firstRouted = false;
      // Matroska interleaves audio right after the video it plays with, so the keyframe at or before the target already has the audio
      // in front of it, and for MP4 and MOV the engine's seek starts at the sound stored before the keyframe. The 3 s lookback is left
      // for transport streams and older engine builds: on a 30 Mbit/s remux it downloaded 11 MB more before each seek, and where the
      // target lay within 3 s after a keyframe it started a whole GOP early (4K MP4 with 10 s GOPs: 22 MB, 2 s of an 85 Mbit/s link).
      const lookbackUs = (demuxer.seekKeepsAudio ?? demuxer.mkv) && !m.forceLookback ? 0 : (m.lookbackUs || 0);   // forceLookback: ?lookback=1, for A/B
      seekAt = performance.timeOrigin + performance.now();
      mark('seek-start');
      await seekTo(Math.max(0, m.timeUs - lookbackUs));
      mark('seek-positioned');
      resetLanes();
      if (lookbackUs > 0 && m.gate !== false) for (const [id, l] of lanes) if (compTracks.has(id)) l.gate = { targetUs: m.timeUs, buf: [], bytes: 0 };
      loop();
    } else if (m.type === 'close') {
      closed = true; gen++; wakeUp();
      for (const l of lanes.values()) l.port.close();
      lanes.clear();
      demuxer?.close?.(); demuxer = null; source = null;
      post({ type: 'closed' });
    }
  } catch (err) {
    post({ type: 'error', where: 'demux', message: String(err && err.message || err) });
  }
};

setInterval(() => {
  if (closed) return;
  // Data the Matroska index had to skip (a bad block, the rest of a corrupt cluster) is missing from playback, video included:
  // one log line each time the count grows, and the count in the stats.
  const damaged = demuxer?.damaged || 0;
  if (damaged > damagedSeen) { damagedSeen = damaged; post({ type: 'log', text: `WARNING: Matroska data skipped (${damaged} place${damaged > 1 ? 's' : ''} so far): a block or the rest of a cluster could not be read` }); }
  const s = { type: 'stats', epoch, packetsRead, packetsSent, damaged, pendingBytes, heap: jsHeapBytes(), wasm: wasmBytes(), lanes: {}, net: netStats(), stages: stages() };
  for (const [id, l] of lanes) s.lanes[id] = { kind: l.kind, inflight: l.inflight, pending: l.pending.length };
  post(s);
}, 125);
