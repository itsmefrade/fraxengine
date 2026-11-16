// SPDX-License-Identifier: LGPL-3.0-only
import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { openDemuxer } from '../demuxer.mjs';
import { createAudioPlugin } from '../audio-plugins.mjs';
import { loadModule } from '../aether-wasm.mjs';
import { createOutput } from '../output.mjs';
import { fixtureFiles, labFiles, have, decodeWindow, ROOT } from './_helpers.mjs';

if (process.env.AETHER_WASM) await loadModule(process.env.AETHER_WASM);

const H = Math.SQRT1_2;
const L714 = ['FL', 'FR', 'FC', 'LFE', 'BL', 'BR', 'SL', 'SR', 'TFL', 'TFR', 'TBL', 'TBR'];
const L51 = ['FL', 'FR', 'FC', 'LFE', 'BL', 'BR'];

function lcg(seed) {
  let s = BigInt(seed);
  return () => {
    s = (s * 6364136223846793005n + 1442695040888963407n) & 0xffffffffffffffffn;
    return Number((s >> 40n)) / 2 ** 24 * 2 - 1;
  };
}
const noise = (seed, n) => { const r = lcg(seed); return Float32Array.from({ length: n }, r); };

// ---- the committed HRTF table, parsed independently from the generated Rust source -------------
const hrtfSrc = fs.readFileSync(path.join(ROOT, 'rust/aether-audio/src/output/hrtf_data.rs'), 'utf8');
const slots = {};
for (const m of hrtfSrc.matchAll(/name: "(\w+)",[\s\S]*?left: \[([^\]]*)\],\s*right: \[([^\]]*)\]/g)) {
  const nums = s => s.split(',').map(v => Number(v.trim()) / 32768);
  slots[m[1]] = { left: nums(m[2]), right: nums(m[3]) };
}
// label -> [slot, mirrored] for a 7.1.4 source (sides 90, rears 135, heights at elevation 40)
const DIR = {
  FL: ['R30', true], FR: ['R30', false], FC: ['C0', false], BL: ['R135', true], BR: ['R135', false],
  SL: ['R90', true], SR: ['R90', false], TFL: ['U45', true], TFR: ['U45', false], TBL: ['U135', true], TBR: ['U135', false],
};
const pair = label => {
  const [s, mirror] = DIR[label];
  const { left, right } = slots[s];
  return mirror ? [right, left] : [left, right];
};

test('the table has all 14 measured directions', () => {
  assert.equal(Object.keys(slots).length, 14);
  for (const s of Object.values(slots)) assert.equal(s.left.length, 128);
});

test('identity passes the planes through bit-exact, with and without the limiter', async () => {
  const input = L51.map((_, c) => noise(c + 1, 1024));
  input[0][0] = -0.0;
  input[1][0] = 8388607 / 8388608;
  input[2][0] = -8388608 / 8388608;
  for (const device of [6, 8]) {
    const out = await createOutput({ sourceLayout: L51, deviceChannels: device });
    assert.equal(out.kind, 'identity');
    assert.deepEqual(out.outLayout, L51);
    assert.match(out.description, /passthrough, bit-exact/);
    assert.equal(out.process(input), input, 'zero-copy');
    out.close();
  }
  // Through WASM (the limiter forces the copy path): quiet material is still bit-exact.
  const quiet = input.map(p => p.map(v => v * 0.5));
  const out = await createOutput({ sourceLayout: L51, deviceChannels: 6, limiter: true });
  const res = out.process(quiet);
  for (let c = 0; c < 6; c++) {
    assert.ok(res[c].every((v, i) => Object.is(v, quiet[c][i])), `plane ${c}`);
  }
  out.close();
});

test('downmix coefficients match hand-computed values', async () => {
  const n = 1024;
  // 5.1 -> 2.0: Lo = L + 0.7071 C + 0.7071 Ls, Ro = R + 0.7071 C + 0.7071 Rs, LFE dropped.
  const out = await createOutput({ sourceLayout: L51, deviceChannels: 2 });
  assert.equal(out.kind, 'mix');
  assert.deepEqual(out.outLayout, ['FL', 'FR']);
  assert.match(out.description, /LFE dropped/);
  assert.match(out.description, /limiter off/);
  const input = L51.map((_, c) => noise(10 + c, n));
  const res = out.process(input);
  for (let i = 0; i < n; i++) {
    const l = input[0][i] + H * input[2][i] + H * input[4][i];
    const r = input[1][i] + H * input[2][i] + H * input[5][i];
    assert.ok(Math.abs(res[0][i] - l) < 1e-5 && Math.abs(res[1][i] - r) < 1e-5);
  }
  out.close();
  // 7.1.4 -> 2.0 composed: TBL reaches FL at 0.5, TFL at 0.7071, the bed at 0.7071.
  const o2 = await createOutput({ sourceLayout: L714, deviceChannels: 2 });
  const impulse = (c) => L714.map((_, k) => { const a = new Float32Array(n); if (k === c) a[0] = 1; return a; });
  const want = { FL: 1, FC: H, SL: H, BL: H, TFL: H, TBL: 0.5, LFE: 0, FR: 0, TFR: 0 };
  for (const [label, g] of Object.entries(want)) {
    const r = o2.process(impulse(L714.indexOf(label)));
    assert.ok(Math.abs(r[0][0] - g) < 1e-6, `FL <- ${label}: ${r[0][0]} vs ${g}`);
  }
  o2.close();
  // 7.1.4 on an 8 channel device is 7.1 with the heights folded at -3 dB; on 6 channels 5.1.
  const o8 = await createOutput({ sourceLayout: L714, deviceChannels: 8 });
  assert.deepEqual(o8.outLayout, ['FL', 'FR', 'FC', 'LFE', 'SL', 'SR', 'BL', 'BR']);
  const r8 = o8.process(impulse(L714.indexOf('TBL')));
  assert.ok(Math.abs(r8[o8.outLayout.indexOf('BL')][0] - H) < 1e-6);
  const o6 = await createOutput({ sourceLayout: L714, deviceChannels: 6 });
  assert.equal(o6.outChannels, 6);
  assert.ok(Math.abs(o6.process(impulse(L714.indexOf('TBL')))[4][0] - 0.5) < 1e-6);
  [o8, o6].forEach(o => o.close());
});

test('limiter is only on when asked for and is reported', async () => {
  const loud = L51.map(() => new Float32Array(512).fill(1));
  const off = await createOutput({ sourceLayout: L51, deviceChannels: 2 });
  assert.ok(off.process(loud)[0][0] > 2, 'no implicit limiting');
  assert.ok(off.peakGain > 2.4);
  const on = await createOutput({ sourceLayout: L51, deviceChannels: 2, limiter: true });
  const res = on.process(loud);
  assert.ok(res[0].every(v => Math.abs(v) <= 0.98 + 1e-6));
  const stats = on.limiterStats();
  assert.equal(stats.frames, 512);
  assert.ok(stats.deepestDb < -6);
  assert.match(on.description, /limiter on/);
  [off, on].forEach(o => o.close());
});

test('binaural impulse responses match the dataset for every speaker direction', async () => {
  // 44.1 kHz is the dataset rate, so the responses are used as measured (x one common gain).
  const n = 1024;
  const out = await createOutput({ sourceLayout: L714, deviceChannels: 2, mode: 'headphones', sampleRate: 44100 });
  assert.equal(out.kind, 'binaural');
  assert.deepEqual(out.outLayout, ['FL', 'FR']);
  assert.match(out.description, /binaural stereo \(MIT KEMAR/);
  const [fl, fr] = pair('FL');
  const scale = 1 / Math.sqrt([...fl, ...fr].reduce((a, v) => a + v * v, 0));
  let worst = 0;
  for (let c = 0; c < L714.length; c++) {
    const label = L714[c];
    const input = L714.map((_, k) => { const a = new Float32Array(n); if (k === c) a[0] = 1; return a; });
    out.reset();
    const res = out.process(input);
    for (let ear = 0; ear < 2; ear++) {
      for (let i = 0; i < n; i++) {
        let want;
        if (label === 'LFE') want = i === 22 ? 0.5 : 0; // delta at 0.5 ms, gain 0.5
        else want = (pair(label)[ear][i] ?? 0) * scale;
        const e = Math.abs(res[ear][i] - want);
        worst = Math.max(worst, e);
        assert.ok(e < 2e-6, `${label} ear ${ear} tap ${i}: ${res[ear][i]} vs ${want}`);
      }
    }
  }
  console.log(`  binaural IR max abs error vs dataset: ${worst.toExponential(2)}`);
  out.close();
});

test('binaural keeps the stream continuous across block sizes and reset', async () => {
  const total = 4096;
  const input = L714.map((_, c) => noise(50 + c, total));
  const run = async chunk => {
    const out = await createOutput({ sourceLayout: L714, deviceChannels: 2, mode: 'headphones' });
    const got = [[], []];
    for (let s = 0; s < total; s += chunk) {
      const r = out.process(input.map(p => p.subarray(s, s + chunk)));
      got[0].push(...r[0]); got[1].push(...r[1]);
    }
    out.close();
    return got;
  };
  const a = await run(1024), b = await run(512);
  for (let e = 0; e < 2; e++) assert.ok(a[e].every((v, i) => Math.abs(v - b[e][i]) < 1e-6));
});

test('argument errors are explicit', async () => {
  await assert.rejects(createOutput({ sourceLayout: ['FL', 'XX'], deviceChannels: 2 }), /XX/);
  await assert.rejects(createOutput({ sourceLayout: ['FL', 'FR'], deviceChannels: 0 }), /device channel/);
  await assert.rejects(createOutput({ sourceLayout: ['FL', 'FR'], deviceChannels: 2, mode: 'tv' }), /mode/);
  const out = await createOutput({ sourceLayout: L51, deviceChannels: 2 });
  assert.throws(() => out.process([new Float32Array(8)]), /expected 6 planes/);
  assert.throws(() => out.process(L51.map(() => new Float32Array(out.maxFrames + 1))), /exceed maxFrames/);
  out.close();
});

// ---- real material -------------------------------------------------------------------------

/**
 * 3 s of real audio as a 12 channel 7.1.4 source. First choice: the DTS:X presentation of the lab
 * file "DTSX Gravity.mkv" (a genuine 7.1.4 render). Otherwise the TrueHD Atmos 7.1 bed of the
 * repository fixture with four SYNTHETIC height channels derived from it (marked `synthetic`).
 * The window starts at the first second that is not silent.
 */
async function realSource(t) {
  const take = (dec, seconds = 3) => {
    const sr = dec.sampleRate, n = dec.planar[0].length;
    let start = 0;
    for (; start + seconds * sr <= n; start += sr) {
      let pk = 0;
      for (const p of dec.planar) for (let i = start; i < start + sr; i++) pk = Math.max(pk, Math.abs(p[i]));
      if (pk > 0.01) break;
    }
    const len = Math.min(seconds * sr, n - start);
    return { ...dec, planar: dec.planar.map(p => p.slice(start, start + len)) };
  };
  if (have(labFiles.dtsxGravity) && !process.env.AETHER_OUTPUT_SYNTHETIC) {
    const d = await openDemuxer({ path: labFiles.dtsxGravity });
    const track = d.tracks().find(x => x.kind === 'audio' && x.codec === 'dts');
    const plugin = createAudioPlugin(track, { dtsx: true });
    plugin.configure(track);
    const dec = decodeWindow(d, plugin, track.id, 8);
    plugin.close(); d.close();
    if (dec?.planar.length === 12) return { ...take(dec), name: 'DTSX Gravity.mkv (DTS:X 7.1.4)', synthetic: false };
  }
  if (!have(fixtureFiles.atmos)) return t.skip('no fixture'), null;
  const d = await openDemuxer({ path: fixtureFiles.atmos });
  const track = d.tracks().find(x => x.kind === 'audio' && x.codec === 'truehd');
  const plugin = createAudioPlugin(track, { presentation: 2 });
  plugin.configure(track);
  const dec = decodeWindow(d, plugin, track.id, 7);
  plugin.close(); d.close();
  const bed = take(dec);
  const at = l => bed.planar[bed.layout.indexOf(l)];
  const mix = (a, b) => Float32Array.from(at(a), (v, i) => 0.5 * (v + at(b)[i]));
  const heights = { TFL: mix('FL', 'SL'), TFR: mix('FR', 'SR'), TBL: mix('SL', 'BL'), TBR: mix('SR', 'BR') };
  const layout = [...bed.layout, ...Object.keys(heights)];
  return { ...bed, layout, planar: [...bed.planar, ...Object.values(heights)], name: 'Atmos 7.1 bed + synthetic heights', synthetic: true };
}

for (const mode of ['headphones', 'speakers']) {
  test(`3 s of real 7.1.4 audio -> ${mode === 'headphones' ? 'binaural' : 'stereo downmix'}: finite, no clipping, timing`, async t => {
    const src = await realSource(t);
    if (!src) return;
    assert.equal(src.planar.length, 12, `expected a 12 channel 7.1.4 render, got ${src.layout}`);
    const out = await createOutput({ sourceLayout: src.layout, deviceChannels: 2, mode, sampleRate: src.sampleRate });
    const total = Math.min(src.planar[0].length, 3 * src.sampleRate);
    const block = 1024;
    const rendered = [new Float32Array(total), new Float32Array(total)];
    const times = [];
    let peakIn = 0;
    for (const p of src.planar) for (let i = 0; i < total; i++) peakIn = Math.max(peakIn, Math.abs(p[i]));
    for (let s = 0; s + block <= total; s += block) {
      const t0 = performance.now();
      const r = out.process(src.planar.map(p => p.subarray(s, s + block)));
      times.push(performance.now() - t0);
      rendered[0].set(r[0], s);
      rendered[1].set(r[1], s);
    }
    let peak = 0, clipped = 0, bad = 0, energy = 0;
    for (const p of rendered) for (const v of p) {
      if (!Number.isFinite(v)) bad++;
      peak = Math.max(peak, Math.abs(v));
      if (Math.abs(v) > 1) clipped++;
      energy += v * v;
    }
    times.sort((a, b) => a - b);
    const mean = times.reduce((a, b) => a + b, 0) / times.length;
    console.log(`  ${src.name} ${src.layout.length}ch @${src.sampleRate}: ${out.description}`);
    console.log(`  ${mode}: input peak ${peakIn.toFixed(3)}, output peak ${peak.toFixed(3)}, samples over full scale ${clipped}, rms ${Math.sqrt(energy / (2 * total)).toFixed(4)}`);
    console.log(`  ${times.length} blocks of ${block}: mean ${mean.toFixed(3)} ms, median ${times[times.length >> 1].toFixed(3)} ms, p99 ${times[Math.floor(times.length * 0.99)].toFixed(3)} ms, max ${times.at(-1).toFixed(3)} ms`);
    assert.equal(bad, 0, 'no NaN or infinity');
    assert.ok(peak > 0.001, 'not silent');
    assert.equal(clipped, 0, 'no sample over full scale');
    // A mix of 12 correlated channels can exceed full scale; a limiter is not applied silently.
    // Report it, but a binaural render of normal content stays inside a sane range.
    assert.ok(peak < 8, `peak ${peak}`);
    assert.ok(mean < 2, `mean block cost ${mean} ms must stay well under 2 ms`);
    out.close();
  });
}

test('block cost: 7.1.4 -> binaural in WASM, 1024 frames at 48 kHz', async () => {
  const out = await createOutput({ sourceLayout: L714, deviceChannels: 2, mode: 'headphones' });
  const input = L714.map((_, c) => noise(700 + c, 1024));
  for (let i = 0; i < 200; i++) out.process(input);
  const times = [];
  for (let i = 0; i < 3000; i++) {
    const t0 = performance.now();
    out.process(input);
    times.push(performance.now() - t0);
  }
  times.sort((a, b) => a - b);
  const mean = times.reduce((a, b) => a + b, 0) / times.length;
  console.log(`  7.1.4 -> binaural: mean ${mean.toFixed(3)} ms, median ${times[1500].toFixed(3)} ms, p99 ${times[2970].toFixed(3)} ms, max ${times.at(-1).toFixed(3)} ms per 1024 frame block (21.3 ms of audio)`);
  assert.ok(mean < 1, `mean ${mean} ms`);
  out.close();
});
