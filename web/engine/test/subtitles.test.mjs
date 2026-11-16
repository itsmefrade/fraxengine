// SPDX-License-Identifier: LGPL-3.0-only
// Subtitle tracks from the demuxer feed the subtitle renderer (web/subs) without errors.
import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { openDemuxer, subtitleCodec } from '../demuxer.mjs';
import { labFiles, have } from './_helpers.mjs';

const here = path.dirname(fileURLToPath(import.meta.url));
const WASM = [process.env.AETHER_WASM, path.join(here, '../../wasm/aether_wasm.wasm')].filter(Boolean).find(p => fs.existsSync(p));
let wasmOk = false;
if (WASM) {
  const mod = await WebAssembly.compile(fs.readFileSync(WASM));
  wasmOk = WebAssembly.Module.exports(mod).some(e => e.name === 'aether_subs_open');
}

/** Canvas 2D stand-in that records draw calls (as in web/subs/test/renderer.test.mjs). */
function fakeCanvas(width = 1920, height = 1080) {
  const calls = [];
  const state = { font: '', fillStyle: '', strokeStyle: '', lineWidth: 1, globalAlpha: 1 };
  const rec = op => (...args) => { calls.push({ op, args, ...state }); };
  const ctx = new Proxy(state, {
    get(t, k) {
      if (k === 'measureText') return text => ({ width: text.length * 8 });
      if (k in t) return t[k];
      return rec(String(k));
    },
    set(t, k, v) { t[k] = v; return true; },
  });
  return { width, height, getContext: () => ctx, calls };
}

test('subtitle codec names', () => {
  const m = {
    'S_HDMV/PGS': 'pgs', 'S_TEXT/ASS': 'ass', 'S_TEXT/SSA': 'ass', 'S_TEXT/UTF8': 'srt', 'D_WEBVTT/SUBTITLES': 'webvtt',
    'S_TEXT/WEBVTT': 'webvtt', hdmv_pgs_subtitle: 'pgs', tx3g: 'srt', wvtt: 'webvtt', 'S_VOBSUB': 'vobsub',
  };
  for (const [raw, name] of Object.entries(m)) assert.equal(subtitleCodec(raw), name, raw);
});

test('POCAWE_Sample.mkv exposes a pgs track whose packets feed the renderer', { skip: !wasmOk && 'wasm without subtitle exports (run sh rust/build.sh)' }, async t => {
  if (!have(labFiles.pocawe)) return t.skip('missing POCAWE_Sample.mkv');
  const { createSubtitleRenderer } = await import('../../subs/renderer.mjs');
  const d = await openDemuxer({ path: labFiles.pocawe });
  const track = d.tracks().find(x => x.kind === 'subtitle');
  assert.equal(track.codec, 'pgs');
  assert.equal(track.containerCodec, 'S_HDMV/PGS');
  const packets = [];
  for (let p; (p = d.nextPacket()) && packets.length < 8;) if (p.trackId === track.id) packets.push(p);
  d.close();
  assert.ok(packets.length >= 3, `${packets.length} subtitle packets`);
  const canvas = fakeCanvas();
  const r = createSubtitleRenderer(canvas, { wasm: WASM, createSurface: (w, h, px) => ({ w, h, px }) });
  await r.setTrack(track);
  for (const p of packets) r.pushPacket(p);
  for (const p of packets) r.render(p.ptsUs, { x: 0, y: 0, width: 1920, height: 1080 });
  console.log(`  pgs: ${packets.length} packets, stats ${JSON.stringify(r.stats())}`);
  r.dispose();
});
