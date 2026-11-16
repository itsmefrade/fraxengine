// SPDX-License-Identifier: LGPL-3.0-only
import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { openDemuxer } from '../../engine/demuxer.mjs';
import { createSubtitleRenderer, layoutCue } from '../renderer.mjs';
import { openSubtitleDecoder } from '../subs-wasm.mjs';

const here = path.dirname(fileURLToPath(import.meta.url));
const root = path.resolve(here, '../../..');
const FIX = path.join(root, 'rust/aether-subs/tests/fixtures');
const WASM = [process.env.AETHER_WASM, path.join(root, 'web/wasm/aether_wasm.wasm')].filter(Boolean).find(p => fs.existsSync(p));

const hasFfmpeg = spawnSync('ffmpeg', ['-version']).status === 0;
let wasmOk = false;
if (WASM) {
  const mod = await WebAssembly.compile(fs.readFileSync(WASM));
  wasmOk = WebAssembly.Module.exports(mod).some(e => e.name === 'aether_subs_open');
}
const skipReason = !wasmOk ? 'wasm without subtitle exports (run sh rust/build.sh)' : false;

/** Canvas 2D stand-in that records draw calls with the state they were made in. */
function fakeCanvas(width = 1280, height = 720) {
  const calls = [];
  const state = { font: '', fillStyle: '', strokeStyle: '', lineWidth: 1, globalAlpha: 1 };
  const rec = op => (...args) => { calls.push({ op, args, ...state }); };
  const ctx = new Proxy(state, {
    get(t, k) {
      if (k === 'measureText') return text => ({ width: text.length * parseFloat(/([\d.]+)px/.exec(state.font)[1]) * 0.5 });
      if (k in t) return t[k];
      return rec(String(k));
    },
    set(t, k, v) { t[k] = v; return true; },
  });
  return { width, height, getContext: () => ctx, calls, texts: () => calls.filter(c => c.op === 'fillText') };
}

const surfaces = [];
const createSurface = (w, h, px) => { const s = { w, h, px }; surfaces.push(s); return s; };

function tmp(name) {
  const d = path.join(os.tmpdir(), `aether-subs-js-${process.pid}`);
  fs.mkdirSync(d, { recursive: true });
  return path.join(d, name);
}

async function textTrackFromMkv(codecArgs, name) {
  const out = tmp(name);
  const r = spawnSync('ffmpeg', ['-v', 'error', '-y', '-i', path.join(FIX, 'sample.srt'), ...codecArgs, out]);
  assert.equal(r.status, 0, String(r.stderr));
  const d = await openDemuxer({ path: out });
  const track = d.tracks().find(t => t.kind === 'subtitle');
  const packets = [];
  for (let p; (p = d.nextPacket());) packets.push(p);
  d.close();
  return { track, packets };
}

for (const [label, args, mkv] of [['ass', ['-c:s', 'ass'], 'a.mkv'], ['srt', ['-c:s', 'srt'], 's.mkv'], ['webvtt', ['-c:s', 'webvtt'], 'w.mkv']]) {
  test(`${label} track from Matroska draws the sample cues`, { skip: skipReason || (!hasFfmpeg && 'no ffmpeg') }, async () => {
    const { track, packets } = await textTrackFromMkv(args, mkv);
    assert.equal(packets.length, 4);
    const canvas = fakeCanvas();
    const r = createSubtitleRenderer(canvas, { wasm: WASM });
    r.pushPacket(packets[0]); // queued before the decoder is ready
    await r.setTrack(track);
    r.pushPacket(packets[0]);
    for (const p of packets) r.pushPacket(p);
    assert.equal(r.stats().cues, 4, 'repeated packet is ignored');
    const rect = { x: 0, y: 0, width: 1280, height: 720 };

    assert.equal(r.render(500_000, rect), false);
    assert.equal(canvas.texts().length, 0);

    assert.equal(r.render(1_500_000, rect), true);
    const t1 = canvas.texts();
    // whitespace runs take room in the layout but are not painted
    assert.equal(t1.map(c => c.args[0]).join(''), 'Hello,world!');
    assert.ok(/italic/.test(t1.find(c => c.args[0] === 'world').font));
    assert.ok(t1.every(c => /"Arial"/.test(c.font)));
    // bottom centre: baseline in the lower part, text block centred around x = 640
    const first = t1[0], lastT = t1[t1.length - 1];
    assert.ok(first.args[2] > 720 * 0.8);
    const left = first.args[1], right = lastT.args[1] + 0.5 * lastT.args[0].length * parseFloat(/([\d.]+)px/.exec(lastT.font)[1]);
    assert.ok(Math.abs((left + right) / 2 - 640) < 2);
    // outline pass exists (stroke) and fill colour is white scaled to the SDR level
    assert.ok(canvas.calls.some(c => c.op === 'strokeText'));
    assert.equal(first.fillStyle, 'rgba(204,204,204,1.000)');

    // two lines, one coloured span, white level applied to every channel
    canvas.calls.length = 0;
    r.render(4_500_000, rect);
    const t2 = canvas.texts();
    assert.equal(t2.map(c => c.args[0]).join(''), 'Twolinessecondlinewithcolour');
    const ys = new Set(t2.map(c => c.args[2]));
    assert.equal(ys.size, 2, 'two baselines');
    if (label !== 'webvtt') assert.ok(t2.some(c => c.args[0] === 'colour' && c.fillStyle === 'rgba(204,102,0,1.000)'), 'orange at 0.8');
    assert.ok(t2.some(c => c.args[0] === 'Two' && /700/.test(c.font)));

    // {\an8}: top aligned (not for webvtt, ffmpeg's muxer drops it)
    if (label !== 'webvtt') {
      canvas.calls.length = 0;
      r.render(7_500_000, rect);
      assert.ok(canvas.texts()[0].args[2] < 720 / 3);
    }
    r.dispose();
  });
}

test('render skips identical frames and follows the video rect', { skip: skipReason || (!hasFfmpeg && 'no ffmpeg') }, async () => {
  const { track, packets } = await textTrackFromMkv(['-c:s', 'ass'], 'a2.mkv');
  const canvas = fakeCanvas();
  const r = createSubtitleRenderer(canvas, { wasm: WASM, whiteLevel: 1 });
  await r.setTrack(track);
  packets.forEach(p => r.pushPacket(p));
  const rect = { x: 0, y: 90, width: 1280, height: 540 }; // letterboxed 21:9 inside 16:9
  r.render(1_500_000, rect);
  const n = canvas.calls.length;
  r.render(1_600_000, rect);
  assert.equal(canvas.calls.length, n, 'same cue, same rect: nothing redrawn');
  r.render(1_600_000, { ...rect, height: 500 });
  assert.ok(canvas.calls.length > n, 'rect change redraws');
  canvas.calls.length = 0;
  r.render(1_600_000, rect);
  const ys = canvas.texts().map(c => c.args[2]);
  assert.ok(ys.every(y => y >= 90 && y <= 630), 'drawn inside the video rect');
  assert.equal(canvas.texts()[0].fillStyle, 'rgba(255,255,255,1.000)', 'whiteLevel 1');
  r.dispose();
});

test('other tracks are ignored and a vobsub track is rejected', { skip: skipReason }, async () => {
  const r = createSubtitleRenderer(fakeCanvas(), { wasm: WASM });
  await assert.rejects(r.setTrack({ id: 9, kind: 'subtitle', codec: 'vobsub' }), /unsupported/);
  await r.setTrack({ id: 3, kind: 'subtitle', codec: 'srt' });
  r.pushPacket({ trackId: 4, ptsUs: 0, durationUs: 1e6, data: new TextEncoder().encode('nope') });
  r.pushPacket({ trackId: 3, ptsUs: 0, durationUs: 1e6, data: new TextEncoder().encode('yes') });
  assert.equal(r.stats().cues, 1);
  r.dispose();
});

test('styled ASS script keeps pos, fad, layers and colours through the wasm boundary', { skip: skipReason }, async () => {
  const dec = await openSubtitleDecoder({ codec: 'ass', description: new Uint8Array(0) }, WASM);
  const { info, cues } = dec.loadFile(fs.readFileSync(path.join(FIX, 'styled.ass')));
  assert.deepEqual([info.playResX, info.playResY], [1280, 720]);
  assert.equal(cues.length, 3);
  const sign = cues[1];
  assert.deepEqual(sign.pos, [640, 100]);
  assert.deepEqual(sign.fad, [300, 500]);
  assert.equal(sign.layer, 1);
  assert.deepEqual(sign.spans[0].c, [0, 255, 0, 255]);
  assert.equal(sign.spans[0].bord, 5);
  assert.equal(cues[2].an, 9);
  assert.equal(cues[2].spans[0].c[3], 127);
  dec.close();
});

test('fade scales the drawn alpha and wrapping respects the margins', { skip: skipReason }, async () => {
  const dec = await openSubtitleDecoder({ codec: 'ass', description: new Uint8Array(0) }, WASM);
  const { cues } = dec.loadFile(fs.readFileSync(path.join(FIX, 'styled.ass')));
  const canvas = fakeCanvas();
  const r = createSubtitleRenderer(canvas, { wasm: WASM });
  // feed the sign cue as a packet of an ASS track with the same header semantics
  await r.setTrack({ id: 1, kind: 'subtitle', codec: 'ass', description: new TextEncoder().encode(fs.readFileSync(path.join(FIX, 'styled.ass'), 'utf8').split('[Events]')[0]) });
  r.pushPacket({ trackId: 1, ptsUs: 2_500_000, durationUs: 2_500_000, data: new TextEncoder().encode('0,1,Sign,,0,0,0,,{\\pos(640,100)\\fad(300,500)}Positioned sign') });
  const rect = { x: 0, y: 0, width: 1280, height: 720 };
  r.render(2_650_000, rect); // halfway through the 300 ms fade in
  const half = canvas.texts()[0].globalAlpha;
  assert.ok(Math.abs(half - 0.5) < 0.01, `alpha ${half}`);
  canvas.calls.length = 0;
  r.render(3_500_000, rect);
  assert.equal(canvas.texts()[0].globalAlpha, 1);
  canvas.calls.length = 0;
  r.render(4_900_000, rect); // 100 ms before the end of a 500 ms fade out
  assert.ok(Math.abs(canvas.texts()[0].globalAlpha - 0.2) < 0.01);

  // layout: a long line wraps inside PlayRes width minus margins
  const ctx = canvas.getContext('2d');
  const span = { t: 'word '.repeat(40).trim(), fn: 'Arial', fs: 48, b: 0, i: 0, u: 0, st: 0, c: [255, 255, 255, 255], oc: [0, 0, 0, 255], bc: [0, 0, 0, 255], bord: 2, shad: 1 };
  const cue = { s: 0, e: 1, layer: 0, an: 2, ml: 60, mr: 60, mv: 40, bs: 1, pos: null, fad: null, spans: [span] };
  const lay = layoutCue(ctx, cue, { playResX: 1280, playResY: 720, wrapStyle: 0 }, { x: 0, y: 0, width: 1280, height: 720 });
  assert.ok(lay.lines.length > 1);
  assert.ok(lay.lines.every(l => l.width <= (1280 - 120) + 1e-6));
  const bottom = lay.lines.at(-1).top + lay.lines.at(-1).asc + lay.lines.at(-1).desc;
  assert.ok(Math.abs(bottom - (720 - 40)) < 1e-6, 'bottom margin respected');
  void cues;
  r.dispose();
  dec.close();
});

test('PGS from Matroska: bitmaps scaled into the video rect, SDR level applied', { skip: skipReason || (!hasFfmpeg && 'no ffmpeg') || (!fs.existsSync(path.join(FIX, 'pocawe.sup')) && 'pocawe.sup absent (git-ignored, derived from lab media)') }, async () => {
  const out = tmp('p.mkv');
  const r0 = spawnSync('ffmpeg', ['-v', 'error', '-y', '-f', 'sup', '-i', path.join(FIX, 'pocawe.sup'), '-map', '0', '-c', 'copy', out]);
  assert.equal(r0.status, 0, String(r0.stderr));
  const d = await openDemuxer({ path: out });
  const track = d.tracks().find(t => t.kind === 'subtitle');
  assert.equal(track.codec, 'pgs');
  const packets = [];
  for (let p; (p = d.nextPacket());) packets.push(p);
  d.close();
  assert.equal(packets.length, 21);

  const canvas = fakeCanvas(1920, 1080);
  const r = createSubtitleRenderer(canvas, { wasm: WASM, createSurface });
  await r.setTrack(track);
  packets.forEach(p => r.pushPacket(p));
  assert.equal(r.stats().displays, 21);

  const rect = { x: 100, y: 50, width: 960, height: 540 };
  assert.equal(r.render(packets[0].ptsUs + 600_000, rect), true);
  const draws = canvas.calls.filter(c => c.op === 'drawImage');
  assert.equal(draws.length, 1);
  const [surf, dx, dy, dw, dh] = draws[0].args;
  assert.deepEqual([surf.w, surf.h], [252, 59]);
  assert.equal(dx, 100 + 834 * 0.5);
  assert.equal(dy, 50 + 871 * 0.5);
  assert.equal(dw, 126);
  assert.equal(dh, 29.5);
  // yellow (190,189,0) at 0.8 -> (152,151,0), alpha untouched, transparent stays transparent
  const px = surf.px;
  const find = (rr, g, b, a) => { for (let i = 0; i < px.length; i += 4) if (px[i] === rr && px[i + 1] === g && px[i + 2] === b && px[i + 3] === a) return true; return false; };
  assert.ok(find(152, 151, 0, 255));
  assert.ok(find(0, 0, 0, 0));
  assert.ok(!find(190, 189, 0, 255));

  // clear display set between the two cues, then a later cue
  canvas.calls.length = 0;
  assert.equal(r.render(packets[1].ptsUs + 100_000, rect), false);
  assert.equal(canvas.calls.filter(c => c.op === 'drawImage').length, 0);
  assert.equal(r.render(packets[2].ptsUs + 100_000, rect), true);

  // forced-only hides objects that are not forced; seek/clear drops state
  r.setForcedOnly(true);
  canvas.calls.length = 0;
  assert.equal(r.render(packets[2].ptsUs + 100_000, rect), false);
  r.setForcedOnly(false);
  r.clear();
  assert.equal(r.stats().displays, 0);
  assert.equal(r.render(packets[2].ptsUs + 100_000, rect), false);
  // after the reset the stream continues from a non-epoch-start point only if the epoch is replayed
  packets.slice(2).forEach(p => r.pushPacket(p));
  assert.equal(r.render(packets[2].ptsUs + 100_000, rect), true);
  r.dispose();
});
