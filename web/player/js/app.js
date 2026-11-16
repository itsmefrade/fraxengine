// SPDX-License-Identifier: LGPL-3.0-only
// UI glue: open a File (picker or drop) or ?src=URL, transport controls, seek bar, stats overlay.
import { Session } from './session.js';
import { armMediaUnlock } from './media-unlock.js';

armMediaUnlock();

const $ = id => document.getElementById(id);
const stage = $('stage'), statusEl = $('status'), statsEl = $('stats'), logEl = $('log');
const playBtn = $('play'), stopBtn = $('stop'), seekEl = $('seek'), timeEl = $('time');

let session = null;
let loadToken = 0;
let scrubbing = false;
let lastSnap = null;
let lastSource = null;

const fmtTime = us => { const s = Math.max(0, Math.floor(us / 1e6)); return `${Math.floor(s / 60)}:${String(s % 60).padStart(2, '0')}`; };

function log(text) {
  const line = `${(performance.now() / 1000).toFixed(2)}  ${text}`;
  logEl.textContent += line + '\n';
  if (logEl.textContent.length > 20000) logEl.textContent = logEl.textContent.slice(-15000);
  logEl.scrollTop = logEl.scrollHeight;
  console.log('[player]', text);
}

function setUi(state) {
  statusEl.textContent = state;
  statusEl.classList.toggle('error', state === 'error');
  const live = ['paused', 'playing', 'ended', 'seeking', 'buffering'].includes(state);
  playBtn.disabled = !['paused', 'playing', 'ended'].includes(state);
  playBtn.textContent = state === 'playing' ? 'Pause' : state === 'ended' ? 'Replay' : 'Play';
  stopBtn.disabled = !(live || state === 'loading' || state === 'error');
  seekEl.disabled = !live;
  stage.classList.toggle('has-video', live);
  if (!live && state !== 'loading') { seekEl.value = 0; }
}

function renderStats(s) {
  lastSnap = s;
  // A visible overlay over the picture keeps Chrome from handing the video to a CoreAnimation overlay and rasterises 4 times a
  // second; hidden, it is not even built.
  if (statsEl.classList.contains('hidden')) return;
  const v = s.video, a = s.audio, d = s.demux;
  const L = [];
  L.push(`state ${s.state}  epoch ${s.epoch}  t ${fmtTime(s.timeUs)} / ${fmtTime(s.durationUs)}`);
  if (v && v.mse) {
    // MSE path: Chrome decodes and presents; the element's own counters and the buffer state are what there is to show.
    const m = v.mse;
    L.push(`video ${v.fps} fps  shown ${v.shown}  dropped ${v.dropped} (${v.droppedPct.toFixed(3)}%)  decoder ${v.decoder.codec} browser (MSE)`);
    L.push(s.clockMode === 'element' ? 'clock: the element (audio and video through MSE, Chrome keeps them in sync)' : `A/V vs Web Audio clock: mean ${v.avMeanMs} ms  p95 ${v.avP95Ms} ms  rate nudges ${m.rateNudges}  corrective seeks ${m.seeks}  recoveries ${m.recoveries}`);
    for (const k of ['video', 'audio']) if (m[k]) L.push(`MSE ${k}: buffered ${m[k].buffered.map(r => `${(r[0] / 1000 - 1).toFixed(1)}-${(r[1] / 1000 - 1).toFixed(1)} s`).join(', ')}  ahead ${m[k].aheadMs} ms / ${m[k].aheadMB} MB  fragments ${m[k].fragments}  quota hits ${m[k].quotaHits}${m[k].lastError ? '  ERROR ' + m[k].lastError : ''}`);
  } else if (v) {
    L.push(`video ${v.fps} fps  shown ${v.shown}  dropped ${v.dropped} (${v.droppedPct.toFixed(3)}%)  preroll-skipped ${v.preroll}`);
    L.push(`A/V offset p95 ${v.avP95Ms.toFixed(1)} ms  mean ${v.avMeanMs.toFixed(1)} ms  (n=${v.avSamples})  vsync ${v.vsyncMs} ms`);
    L.push(`queues: frames ${v.queue}  packets ${v.inq}  decoder ${v.decodeQueue}  open VideoFrames ${v.openFrames}  errors ${v.decodeErrors}`);
    L.push(`${v.dv ? 'BL ' : ''}decoder ${v.decoder.codec} ${v.decoder.hardwareAcceleration}${v.decoder.fallbackFrom ? ` (FALLBACK from ${v.decoder.fallbackFrom}: ${v.decoder.fallbackReason}${v.decoder.fallbackAtPacket != null ? ' at packet ' + v.decoder.fallbackAtPacket : ''})` : ''}  frame ${v.frameFormat}  gpu ${v.gpu || '?'}`);
    L.push(`HDR: source ${v.hdr.source}; canvas ${v.hdr.canvas} ${v.hdr.canvasSize || ''}; display HDR ${v.hdr.displayHdr ? 'yes' : 'no'}`);
  }
  if (v && v.dv) {
    const d = v.dv;
    const prof = d.dvProfile ? `Dolby Vision profile ${d.dvProfile}.${d.dvLevel ?? '?'}${d.elKind === 'fel' ? ' FEL' : d.elKind === 'mel' ? ' MEL' : ''}` : d.kind;
    L.push(`${prof}; mode ${d.qualityLabel}${s.qualityAuto ? ' (auto' + (s.bitrateMbps ? `, ${s.bitrateMbps.toFixed(1)} Mbps` : '') + ')' : ''}; quality ${d.psnr}`);
    L.push(`decode path: BL ${d.blPath}; EL ${d.elPath}; RPU tables ${d.tables}${d.tablesErr ? ' ERROR ' + d.tablesErr : ''}`);
    L.push(`compositor: ${d.last ? d.last.mode + ' ' + d.last.input : '-'}  ${d.composed} presented, ${d.presentCpuMsPerFrame} ms CPU/frame${d.elMissing ? `  EL missing ${d.elMissing}` : ''}${d.exact ? `  exact pool: ${d.exact.decoded} frames, peak buffer ${d.exact.peakBufferedMB} MB, wasm ${d.exact.wasmMB} MB/worker` : ''}`);
  } else if (s.quality && s.quality !== 'native') L.push(`HDR mode ${s.quality}`);
  if (s.unsupported?.video) L.push(`VIDEO: ${s.unsupported.video}`);
  if (s.audioInfo) {
    const i = s.audioInfo;
    L.push(`audio source: ${i.srcCodec} ${i.srcChannels}ch (${i.srcLayout}) ${i.srcRate} Hz  ${i.lossless ? 'lossless' : 'lossy'}`);
    if (i.joc) L.push(i.joc.rendered ? `E-AC-3 JOC: ${i.joc.objects} objects rendered to 7.1.4 -> ${i.outChannels}ch ${i.mode}/${i.kind}: ${i.downmix}` : `E-AC-3 JOC: Atmos signalled, objects off (${i.jocSetting ? 'not decoded' : 'JOC toggle off'}), 5.1/7.1 bed only`);
    L.push(`audio decoder: ${i.decoder}; output ${i.outChannels}ch (${i.outLayout}) ${i.outRate} Hz; output ${i.mode}/${i.kind} [${i.renderer}]: ${i.downmix}; resample: ${i.resample}`);
    if (i.peakGain > 1 || i.limiter?.enabled) L.push(`downmix headroom: worst-case peak +${i.peakGainDb} dB; normalize ${i.normalize?.setting}${i.normalize?.active ? ` (active, ${i.normalize.gainDb} dB)` : ' (not applied)'}; limiter ${i.limiter?.enabled ? 'ON' + (s.audio?.limiter ? `, reduced ${s.audio.limiter.reducedFrames} frames, deepest ${s.audio.limiter.deepestReductionDb} dB` : '') : 'off'}${i.peakGain > 1 && !i.normalize?.active && !i.limiter?.enabled ? '  (MAY CLIP)' : ''}`);
  }
  L.push(`clock ${s.clockMode}  demuxer ${s.engine || '?'}  present ${s.present?.mode || '?'}${s.present?.latencyMs != null ? ` (write to display ${s.present.latencyMs.toFixed(1)} ms, lead ${s.present.leadMs?.toFixed(1)} ms, picture ${(s.present.latencyMs - (s.present.leadMs || 0)).toFixed(1)} ms late; medians of ${s.present.samples})` : ''}`);
  if (a) {
    L.push(`audio ring ${a.ringMs} ms  pending ${a.outqMs} ms  packets ${a.inq}  underruns ${a.underruns}`);
    L.push(`audio route: ${s.audioRoute || '?'}; ctx ${s.audioCtx ? `${s.audioCtx.state} ${s.audioCtx.sampleRate} Hz maxCh ${s.audioCtx.maxChannelCount}` : '?'}`);
  } else if (!s.audioCtx) L.push('audio: none (wall clock master)');
  if (d) L.push(`demux read ${d.packetsRead} sent ${d.packetsSent} buffered ${(d.pendingBytes / 1048576).toFixed(1)} MB ${Object.entries(d.lanes).map(([k, l]) => `#${k} ${l.kind} inflight ${l.inflight} pending ${l.pending}`).join(' | ')}`);
  if (s.source) L.push(`source ${s.source.mode} ${(s.source.size / 1048576).toFixed(1)} MB`);
  for (const w of s.warnings) L.push('WARNING ' + w);
  statsEl.textContent = L.join('\n');
}

let lastSeekS = -1;
function tickUi() {
  if (session && ['playing', 'paused', 'seeking', 'buffering'].includes(session.state)) {
    const t = session.currentTimeUs();
    // Once a second: every change of the range input restyles and repaints the bar (4 a second was half a percent of a core).
    if (!scrubbing && session.durationUs && Math.floor(t / 1e6) !== lastSeekS) { lastSeekS = Math.floor(t / 1e6); seekEl.value = Math.round((t / session.durationUs) * 1000); }
    const text = `${fmtTime(t)} / ${fmtTime(session.durationUs)}`;
    if (timeEl.textContent !== text) timeEl.textContent = text;
  }
}
// Four updates a second are enough for a clock and a seek bar; a per-vsync loop made the page restyle 120 times a second.
setInterval(tickUi, 250);

function fillTracks(s) {
  const aud = $('audsel'), sub = $('subsel');
  aud.innerHTML = ''; (s.audioTracks || []).forEach((t, i) => aud.append(new Option(`${t.language || '?'} ${t.name ? t.name + ' ' : ''}${t.codec} ${t.channels || '?'}ch`, String(i))));
  aud.value = String(s.audioIndex ?? 0); aud.disabled = (s.audioTracks || []).length < 2;
  sub.innerHTML = ''; sub.append(new Option('Subtitles: off', ''));
  s.subtitleTracks().forEach((t, i) => sub.append(new Option(`${t.language || '?'} ${t.name ? t.name + ' ' : ''}${t.codec}`, String(i))));
  sub.disabled = s.subtitleTracks().length === 0;
}

async function load({ file, url, startUs = 0 }) {
  lastSource = { file, url };
  const token = ++loadToken;
  const old = session; session = null;
  if (old) { try { await old.stop(); } catch (e) { log('stop failed: ' + e); } }
  if (token !== loadToken) return;
  setUi('loading'); statsEl.textContent = ''; logEl.textContent = '';
  const s = new Session({
    file, url, host: stage, wantPlay: true, startUs, quality: $('quality').value === 'auto' ? null : $('quality').value,
    onEvent: ev => {
      if (s !== session && !(session === null && token === loadToken)) return;
      if (ev.type === 'state') setUi(ev.state);
      else if (ev.type === 'stats') renderStats(ev.stats);
      else if (ev.type === 'log') log(ev.text);
      else if (ev.type === 'error') { log('ERROR ' + ev.message); statsEl.textContent += '\nERROR ' + ev.message; }
    },
  });
  s.outputMode = $('outmode').value;
  session = s;
  window.session = s;
  try { s.jocEnabled = $('joc').checked; await s.open(); fillTracks(s); s.setMixOptions(mixOpts()); } catch (e) { log('open failed: ' + e.message); }
}

async function stopCurrent() {
  ++loadToken;
  const old = session; session = null;
  if (old) await old.stop();
  setUi('idle'); timeEl.textContent = '0:00 / 0:00';
}

$('file').addEventListener('change', e => { const f = e.target.files[0]; e.target.value = ''; if (f) load({ file: f }); });
$('url').addEventListener('keydown', e => { if (e.key === 'Enter' && e.target.value.trim()) load({ url: e.target.value.trim() }); });
playBtn.addEventListener('click', () => { if (!session) return; session.state === 'playing' ? session.pause() : session.play(); });
stopBtn.addEventListener('click', () => stopCurrent());
seekEl.addEventListener('input', () => { scrubbing = true; if (session) timeEl.textContent = `${fmtTime((seekEl.value / 1000) * session.durationUs)} / ${fmtTime(session.durationUs)}`; });
seekEl.addEventListener('change', () => { scrubbing = false; if (session) session.seek((seekEl.value / 1000) * session.durationUs); });
const mixOpts = () => ({ normalize: $('norm').value === 'auto' ? null : $('norm').value === 'on', limiter: $('limiter').checked });
$('norm').addEventListener('change', () => session?.setMixOptions(mixOpts()));
$('limiter').addEventListener('change', () => session?.setMixOptions(mixOpts()));
$('audsel').addEventListener('change', e => session?.switchAudio(Number(e.target.value)));
$('subsel').addEventListener('change', e => session?.setSubtitle(e.target.value === '' ? null : Number(e.target.value)));
$('forced').addEventListener('change', e => session?.setSubtitleForcedOnly(e.target.checked));
$('quality').addEventListener('change', () => { if (session && lastSource) load({ ...lastSource, startUs: session.currentTimeUs() }); });
$('joc').checked = new URLSearchParams(location.search).get('joc') !== '0';
$('joc').addEventListener('change', e => session?.setJoc(e.target.checked));
const modeSel = $('outmode');
modeSel.value = new URLSearchParams(location.search).get('outmode') === 'headphones' ? 'headphones' : 'speakers';
modeSel.addEventListener('change', () => session?.setOutputMode(modeSel.value));
if (new URLSearchParams(location.search).has('stats')) statsEl.classList.remove('hidden');
$('info').addEventListener('click', () => { statsEl.classList.toggle('hidden'); if (lastSnap) renderStats(lastSnap); });

window.addEventListener('keydown', e => {
  if (e.target.tagName === 'INPUT' && e.target.type === 'text') return;
  if (e.key === 'i') { statsEl.classList.toggle('hidden'); if (lastSnap) renderStats(lastSnap); }
  else if (!session) return;
  else if (e.key === ' ') { e.preventDefault(); playBtn.click(); }
  else if (e.key === 'ArrowRight') session.seek(session.currentTimeUs() + 5e6);
  else if (e.key === 'ArrowLeft') session.seek(session.currentTimeUs() - 5e6);
});

for (const ev of ['dragenter', 'dragover']) stage.addEventListener(ev, e => { e.preventDefault(); stage.classList.add('drag'); });
for (const ev of ['dragleave', 'drop']) stage.addEventListener(ev, e => { e.preventDefault(); stage.classList.remove('drag'); });
stage.addEventListener('drop', e => { const f = e.dataTransfer.files[0]; if (f) load({ file: f }); });

window.playerLoad = load;
window.playerStop = stopCurrent;
window.playerSnapshot = () => session?.snapshot() ?? lastSnap;

const src = new URLSearchParams(location.search).get('src');
if (src) { $('url').value = src; load({ url: src }); }
setUi('idle');
