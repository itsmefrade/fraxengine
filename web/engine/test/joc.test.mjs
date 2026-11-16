// SPDX-License-Identifier: LGPL-3.0-only
// E-AC-3 JOC (Dolby Atmos in DD+): objects rendered to 7.1.4 through the engine plugin.
import test from 'node:test';
import assert from 'node:assert/strict';
import { openDemuxer } from '../demuxer.mjs';
import { createAudioPlugin, engineCodec } from '../audio-plugins.mjs';
import { labFiles, fixtureFiles, ROOT, have, hasFfmpeg, ffmpegReference, ffmpegAudioIndex, comparePlanar, decodeWindow } from './_helpers.mjs';
import path from 'node:path';

const CHID = path.join(ROOT, 'test-media/audio-official-joc-channel-id.mp4');
const SPEAKERS = ['FL', 'FR', 'FC', 'LFE', 'SL', 'SR', 'BL', 'BR', 'TFL', 'TFR', 'TBL', 'TBR'];

async function open(file, opts) {
  const d = await openDemuxer({ path: file });
  const track = d.tracks().find(x => x.kind === 'audio' && engineCodec(x.codec) === 'eac3');
  const plugin = createAudioPlugin(track, opts).configure(track);
  return { d, track, plugin };
}

/** Decodes up to `seconds` (all when omitted); returns blocks plus the decode time in ms (demux excluded). */
function decode(h, seconds = Infinity) {
  const blocks = [];
  let ms = 0, info = null;
  const startUs = h.d.startTimeUs ?? 0;
  for (let p; (p = h.d.nextPacket());) {
    if (p.trackId !== h.track.id) continue;
    if (p.ptsUs - startUs > seconds * 1e6) break;
    const t = performance.now();
    const r = h.plugin.decode(p);
    ms += performance.now() - t;
    if (r.frames) { blocks.push(r.planar); info = r; }
  }
  const frames = blocks.reduce((n, b) => n + b[0].length, 0);
  return { blocks, frames, ms, info };
}

test('jocs: Atmos sample renders 15 objects to 7.1.4, finite, within full scale, cheap in wasm', async t => {
  if (!have(labFiles.eac3Joc)) return t.skip('missing fixture');
  const h = await open(labFiles.eac3Joc);
  const r = decode(h, 10);
  assert.deepEqual(r.info.joc, { objects: 15, rendered: true });
  assert.equal(r.info.channels, 12);
  assert.deepEqual(r.info.layout, SPEAKERS);
  let peak = 0, nonfinite = 0;
  const energy = new Float64Array(12);
  for (const b of r.blocks) b.forEach((pl, c) => { for (const v of pl) { if (!Number.isFinite(v)) nonfinite++; else { peak = Math.max(peak, Math.abs(v)); energy[c] += v * v; } } });
  const seconds = r.frames / 48000;
  const msPerSecond = r.ms / seconds;
  const bedOnly = await open(labFiles.eac3Joc, { joc: false });
  const rb = decode(bedOnly, 10);
  console.log(`  bed only: ${(rb.ms / (rb.frames / 48000)).toFixed(1)} ms CPU per second of audio; JOC objects + render add ${(msPerSecond - rb.ms / (rb.frames / 48000)).toFixed(1)}`);
  bedOnly.plugin.close();
  console.log(`  atmos 10 s: ${r.frames} frames, peak ${peak.toFixed(4)}, ${msPerSecond.toFixed(1)} ms CPU per second of audio (${(1000 / msPerSecond).toFixed(0)}x realtime)`);
  console.log(`  energy per speaker: ${SPEAKERS.map((s, c) => `${s} ${(10 * Math.log10(energy[c] / r.frames + 1e-20)).toFixed(1)}dB`).join(' ')}`);
  assert.equal(nonfinite, 0, 'no NaN or infinity');
  assert.ok(peak <= 1, `no clipping: peak ${peak}`);
  assert.ok(peak > 0.05, 'not silent');
  assert.ok(seconds > 9.9);
  assert.ok(msPerSecond < 100, `decode cost ${msPerSecond} ms per second of audio`);
  h.plugin.close();
});

test('jocs: rendered output is sample aligned with the bed', async t => {
  if (!have(labFiles.eac3Joc)) return t.skip('missing fixture');
  const a = await open(labFiles.eac3Joc), b = await open(labFiles.eac3Joc, { joc: false });
  const ra = decode(a, 4), rb = decode(b, 4);
  const cat = r => r.blocks.reduce((o, bl) => bl.map((pl, c) => { const x = new Float32Array((o[c]?.length ?? 0) + pl.length); if (o[c]) x.set(o[c]); x.set(pl, o[c]?.length ?? 0); return x; }), []);
  const A = cat(ra), B = cat(rb);
  // Front L+R+C energy of the render and of the bed, compared over lags: the best lag must be 0.
  const front = (P, idx) => idx.map(c => P[c]);
  const fa = front(A, [0, 1, 2]), fb = front(B, [0, 1, 2]);
  let best = [Infinity, 0];
  for (const lag of [-577, -64, -1, 0, 1, 64, 577]) {
    let err = 0;
    for (let c = 0; c < 3; c++) for (let i = 2000; i < 2000 + 48000; i++) err += (fa[c][i + lag] - fb[c][i]) ** 2;
    if (err < best[0]) best = [err, lag];
  }
  console.log(`  render vs bed front channels: best lag ${best[1]}`);
  assert.equal(best[1], 0);
  a.plugin.close(); b.plugin.close();
});

test('jocs: channel-ID sample, each announced position lands in its 7.1.4 speaker(s)', async t => {
  if (!have(CHID)) return t.skip('missing fixture');
  const h = await open(CHID);
  const r = decode(h);
  assert.deepEqual(r.info.joc.rendered, true);
  assert.equal(r.info.channels, 12);
  const W = 4; // blocks of ~32 ms per window
  const groups = { L: [0], C: [2], R: [1], Rb: [7], Lb: [6], Tsl: [8, 10], Tsr: [9, 11] }; // FL FR FC ... BL BR TFL TFR TBL TBR
  const seen = Object.fromEntries(Object.keys(groups).map(k => [k, 0]));
  let loud = 0, bad = [];
  for (let i = 0; i + W <= r.blocks.length; i += W) {
    const e = new Float64Array(12);
    for (let b = i; b < i + W; b++) r.blocks[b].forEach((pl, c) => { for (const v of pl) e[c] += v * v; });
    const total = e.reduce((s, v, c) => s + (c === 3 ? 0 : v), 0);
    const tSec = (i * 1536) / 48000;
    if (tSec < 0.8 || tSec > 5.7 || total / (W * 1536) < 1e-3) continue; // the announcements; the intro and the moving section are excluded
    loud++;
    let hit = null;
    for (const [name, idx] of Object.entries(groups)) if (idx.reduce((s, c) => s + e[c], 0) / total > 0.95) hit = name;
    if (hit) seen[hit]++; else bad.push(tSec.toFixed(1));
  }
  console.log(`  channel-ID sample: ${(r.ms / (r.frames / 48000)).toFixed(1)} ms CPU per second of audio`);
  console.log(`  announced windows: ${JSON.stringify(seen)} of ${loud} loud, off-position ${bad.join(',') || 'none'}`);
  assert.deepEqual(bad, [], 'every loud announcement window is >95% in one expected speaker group');
  for (const [name, n] of Object.entries(seen)) assert.ok(n >= 2, `${name}: ${n} windows`);
  h.plugin.close();
});

test('jocs: bed fallback (joc: false) matches ffmpeg to 1e-6 over 10 s', async t => {
  if (!have(labFiles.eac3Joc)) return t.skip('missing fixture');
  if (!hasFfmpeg) return t.skip('ffmpeg not installed');
  const { d, track, plugin } = await open(labFiles.eac3Joc, { joc: false });
  const out = decodeWindow(d, plugin, track.id, 10);
  const idx = d.tracks().filter(x => x.kind === 'audio').findIndex(x => x.id === track.id);
  const ref = ffmpegReference(labFiles.eac3Joc, ffmpegAudioIndex(labFiles.eac3Joc, idx), 10, ['-drc_scale', '0']);
  assert.ok(out.layout.length === 6 || out.layout.length === 8);
  const c = comparePlanar(out.planar, out.layout, ref);
  console.log(`  bed fallback: ${out.layout.join(',')} frames=${c.frames} maxdiff=${c.max}`);
  assert.ok(c.frames >= 9.8 * 48000);
  assert.ok(c.max <= 1e-6, `max abs diff ${c.max}`);
  plugin.close();
});

test('jocs: setJoc(false) switches to the bed, setJoc(true) back to objects', async t => {
  if (!have(labFiles.eac3Joc)) return t.skip('missing fixture');
  const h = await open(labFiles.eac3Joc);
  const seq = [];
  let n = 0;
  for (let p; (p = h.d.nextPacket()) && n < 60;) {
    if (p.trackId !== h.track.id) continue;
    if (n === 20) h.plugin.setJoc(false);
    if (n === 40) h.plugin.setJoc(true);
    const r = h.plugin.decode(p);
    if (r.frames) seq.push(`${r.channels}${r.joc.rendered ? 'r' : 'b'}`);
    n++;
  }
  assert.equal(seq[10], '12r');
  assert.equal(seq[30], '6b');
  assert.equal(seq[50], '12r');
  h.plugin.close();
});
