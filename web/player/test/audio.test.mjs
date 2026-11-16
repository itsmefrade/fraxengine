// node --test web/player/test/
import test from 'node:test';
import assert from 'node:assert/strict';
import { SincResampler } from '../js/audio/resampler.js';
import { buildMixMatrix, OUT_LAYOUTS, snapChannels, applyMix } from '../js/audio/mix.js';

function sine(freq, rate, n, off = 0) { const a = new Float32Array(n); for (let i = 0; i < n; i++) a[i] = 0.5 * Math.sin(2 * Math.PI * freq * (i + off) / rate); return a; }

for (const [inR, outR] of [[44100, 48000], [48000, 44100], [96000, 48000], [48000, 96000], [88200, 48000]]) {
  test(`resampler ${inR}->${outR}: tone fidelity and chunk independence`, () => {
    const n = inR; // 1 s
    const x = sine(1000, inR, n);
    const r = new SincResampler(inR, outR, 1);
    const parts = [];
    for (let o = 0, step = 997; o < n; o += step) parts.push(r.process([x.subarray(o, Math.min(n, o + step))], Math.min(step, n - o)).planes[0].slice());
    const y = Float32Array.from(parts.flatMap(p => Array.from(p)));
    const whole = new SincResampler(inR, outR, 1).process([x], n).planes[0];
    assert.equal(y.length, whole.length);
    let diff = 0; for (let i = 0; i < y.length; i++) diff = Math.max(diff, Math.abs(y[i] - whole[i]));
    assert.ok(diff < 1e-6, `chunked differs ${diff}`);
    // compare with the analytic tone away from the edges
    let err = 0, sig = 0;
    const lead = 64 * Math.max(1, inR / outR) * (outR / inR);
    for (let k = Math.ceil(lead) + 64; k < y.length - 64; k++) {
      const ref = 0.5 * Math.sin(2 * Math.PI * 1000 * (k * inR / outR + (-(r.half - 1) + (r.half - 1))) / inR);
      err += (y[k] - ref) ** 2; sig += ref ** 2;
    }
    const snr = 10 * Math.log10(sig / err);
    assert.ok(snr > 90, `SNR ${snr.toFixed(1)} dB`);
    assert.ok(Math.abs(y.length - n * outR / inR) <= (r.half + 2) * Math.max(1, outR / inR));
  });
}

test('resampler rejects aliasing: 30 kHz tone at 96k -> 48k is attenuated', () => {
  const r = new SincResampler(96000, 48000, 1);
  const x = sine(30000, 96000, 96000);
  const y = r.process([x], x.length).planes[0];
  let rms = 0; for (let i = 100; i < y.length; i++) rms += y[i] ** 2; rms = Math.sqrt(rms / (y.length - 100));
  assert.ok(rms < 0.5 * 0.7071 * 1e-3, `alias rms ${rms}`);
});

test('mix: identity and 7.1.4 -> 7.1 / 5.1 / 2.0', () => {
  const l714 = OUT_LAYOUTS[12];
  assert.equal(buildMixMatrix(OUT_LAYOUTS[8], OUT_LAYOUTS[8]).identity, true);
  const m71 = buildMixMatrix(l714, OUT_LAYOUTS[8]);
  assert.ok(Math.abs(m71.gains[0][8] - Math.SQRT1_2) < 1e-6);      // TFL -> FL
  assert.ok(Math.abs(m71.gains[6][10] - Math.SQRT1_2) < 1e-6);     // TBL -> BL
  const m51 = buildMixMatrix(l714, OUT_LAYOUTS[6]);
  assert.ok(Math.abs(m51.gains[4][6] - Math.SQRT1_2) < 1e-6);      // BL -> SL
  assert.ok(Math.abs(m51.gains[4][10] - Math.SQRT1_2) < 1e-6);     // TBL -> SL
  const m20 = buildMixMatrix(['FL', 'FR', 'FC', 'LFE', 'BL', 'BR'], OUT_LAYOUTS[2]);
  assert.equal(m20.lfeDropped, true);
  const g = m20.normGain;
  assert.ok(Math.abs(m20.gains[0][0] - g) < 1e-6 && Math.abs(m20.gains[0][2] - g * Math.SQRT1_2) < 1e-6 && Math.abs(m20.gains[0][4] - g * Math.SQRT1_2) < 1e-6);
  assert.equal(m20.gains[0][1], 0);
});

test('mix: 5.1 with rear labels plays on the surround pair of a 7.1 output', () => {
  const m = buildMixMatrix(['FL', 'FR', 'FC', 'LFE', 'BL', 'BR'], OUT_LAYOUTS[8]);
  assert.equal(m.gains[4][4], 1); assert.equal(m.gains[5][5], 1); assert.equal(m.gains[6].every(v => v === 0), true);
});

test('mix: permutation (TrueHD SMPTE order to WAVE) and applyMix', () => {
  const m = buildMixMatrix(['FL', 'FR', 'FC', 'LFE', 'SL', 'SR', 'BL', 'BR'], OUT_LAYOUTS[8]);
  assert.equal(m.identity, true);
  const m2 = buildMixMatrix(['FC', 'FL', 'FR', 'SL', 'SR', 'LFE'], OUT_LAYOUTS[6]);
  const inp = [1, 2, 3, 4, 5, 6].map(v => Float32Array.of(v));
  const out = applyMix(m2.gains, inp, 1, OUT_LAYOUTS[6].map(() => new Float32Array(1)));
  assert.deepEqual(out.map(p => p[0]), [2, 3, 1, 6, 4, 5]);
});

test('snapChannels', () => {
  assert.equal(snapChannels(8, 2), 2); assert.equal(snapChannels(8, 8), 8); assert.equal(snapChannels(12, 8), 8);
  assert.equal(snapChannels(6, 16), 6); assert.equal(snapChannels(7, 8), 6); assert.equal(snapChannels(1, 2), 1); assert.equal(snapChannels(16, 16), 12);
});
