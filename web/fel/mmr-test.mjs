// SPDX-License-Identifier: LGPL-3.0-only
// Independent check of the WGSL MMR / multi piece polynomial prediction on real MMR streams (woman, rangers).
// COMPOSE runs on FFmpeg's exact BL planes with an EL whose codes give residual 0, so raw16 equals the prediction. The expected value
// is computed in BigInt from the table words with the integer formula of rust/aether-dovi/src/fel/mapping.rs (predict_reference16)
// and the [1 2 1] / 2 row luma downsample of sample.rs (chroma_site_luma). Samples are random positions, every checkpoint frame.
// window.runMmrTest({clip, samples})
import { FelCompositor } from './fel-compositor.mjs';
import { FelTables } from './fel-tables.mjs';
import { getDevice } from './fel-test.mjs';
import { prepareClip } from './hybrid-test.mjs';

const log = s => { const el = document.getElementById('log'); if (el) el.textContent += s + '\n'; console.log(s); };
const W = 3840, H = 2160, EW = 1920, EH = 1080;
const B = BigInt;
const DESC = 64, COEF = 160, PRED = 688, NLQ = 3760;

function curveOf(words, c) {
  const d = DESC + c * 32, n = words[d];
  const pivots = Array.from({ length: n }, (_, i) => words[d + 2 + i]);
  return { pivots, lo: words[d + 27], hi: words[d + 28], pieces: n - 1, kind: p => words[d + 11 + p], order: p => words[d + 19 + p] };
}
function select(cv, code) { let k = 0; for (let p = 0; p < cv.pieces; p++) if (code >= cv.pivots[p]) k = p; return k; }
function predict(words, c, code, input) {
  const denom = words[3], cv = curveOf(words, c), p = select(cv, code);
  if (cv.kind(p) === 0) return words[PRED + c * 1024 + code];                       // polynomial: table from Rust (verified by the 12 gold hashes)
  const base = COEF + (c * 8 + p) * 22, order = cv.order(p);
  const cl = i => { const k = curveOf(words, i); return B(Math.min(Math.max(input[i], k.lo), k.hi)); };
  const [y, u, v] = [cl(0), cl(1), cl(2)];
  const first = [y << 10n, u << 10n, v << 10n, y * u, y * v, u * v, (y * u * (v << 10n)) >> 20n];
  const second = [y * y, u * u, v * v, (first[3] * first[3]) >> 20n, (first[4] * first[4]) >> 20n, (first[5] * first[5]) >> 20n, (first[6] * first[6]) >> 20n];
  const third = first.map((f, i) => (f * second[i]) >> 20n);
  let sum = B(words[base]) * (1n << 20n);
  [first, second, third].slice(0, order).forEach((terms, o) => { for (let i = 0; i < 7; i++) sum += B(words[base + 1 + o * 7 + i]) * terms[i]; });
  if (sum < 0n) sum = 0n;
  const r = sum >> B(denom + 4);
  return Number(r > 65535n ? 65535n : r);
}

export async function runMmrTest(opts = {}) {
  const { device } = await getDevice();
  const clip = opts.clip ?? 'woman', samples = opts.samples ?? 6000;
  const target = device.createTexture({ size: [64, 64], format: 'rgba16float', usage: GPUTextureUsage.RENDER_ATTACHMENT });
  const comp = await FelCompositor.create(device, null, { target });
  const tables = await FelTables.create(new URL('../wasm/aether_wasm.wasm', import.meta.url));
  const cl = await prepareClip(clip, comp, tables, log);
  const out = { clip, info: cl.info, rows: [] };
  for (const k of cl.checkpoints) {
    const { bl, words } = await cl.gold(k);
    const blU = new Uint16Array(bl.buffer, bl.byteOffset, bl.byteLength / 2);
    // EL codes with residual 0 per component (the NLQ table entry that is 0, closest to mid scale)
    const zero = c => { let best = 512, bd = 1e9; for (let code = 0; code < 1024; code++) if (words[NLQ + c * 1024 + code] === 0 && Math.abs(code - 512) < bd) { best = code; bd = Math.abs(code - 512); } return best; };
    const z = [0, 1, 2].map(zero);
    const el = new Uint16Array(EW * EH * 3 / 2);
    el.fill(z[0], 0, EW * EH); el.fill(z[1], EW * EH, EW * EH * 5 / 4); el.fill(z[2], EW * EH * 5 / 4);
    comp.composeExact(new Uint8Array(blU.buffer, blU.byteOffset, blU.byteLength), new Uint8Array(el.buffer), words);
    const { raw16 } = await comp.readComposed();
    const Y = blU.subarray(0, W * H), U = blU.subarray(W * H, W * H * 5 / 4), V = blU.subarray(W * H * 5 / 4);
    const row = { frame: k, mmrMask: words[6], zeroCodes: z, luma: { n: 0, bad: 0, maxDiff: 0 }, cb: { n: 0, bad: 0, maxDiff: 0 }, cr: { n: 0, bad: 0, maxDiff: 0 } };
    const edge = (x, y) => Y[Math.min(Math.max(y, 0), H - 1) * W + Math.min(Math.max(x, 0), W - 1)];
    let seed = k * 7919 + 13; const rnd = n => { seed = (Math.imul(seed, 1664525) + 1013904223) >>> 0; return seed % n; };
    for (let s = 0; s < samples; s++) {
      const x = rnd(W / 2), y = rnd(H / 2);
      const rowL = yy => (edge(2 * x - 1, yy) + 2 * edge(2 * x, yy) + edge(2 * x + 1, yy) + 2) >> 2;
      const ys = (rowL(2 * y) + rowL(2 * y + 1) + 1) >> 1;
      const ci = y * (W / 2) + x, cu = U[ci], cv = V[ci];
      for (const [name, c, got, want] of [['cb', 1, raw16[W * H + ci], predict(words, 1, cu, [ys, cu, cv])], ['cr', 2, raw16[W * H + W * H / 4 + ci], predict(words, 2, cv, [ys, cu, cv])]]) {
        const r = row[name]; r.n++; const d = Math.abs(got - want); if (d) r.bad++; if (Math.abs(want - (name === 'cb' ? cu : cv) * 64) > 64) r.nonIdentity = (r.nonIdentity ?? 0) + 1; r.maxDiff = Math.max(r.maxDiff, d);
      }
      const lx = rnd(W), ly = rnd(H), lc = Y[ly * W + lx], lw = predict(words, 0, lc, [0, 0, 0]);
      const lg = raw16[ly * W + lx], r = row.luma; r.n++; const d = Math.abs(lg - lw); if (d) r.bad++; r.maxDiff = Math.max(r.maxDiff, d);
    }
    out.rows.push(row);
    log(`mmr ${clip} frame ${k}: luma bad ${row.luma.bad}/${row.luma.n} cb ${row.cb.bad}/${row.cb.n} (max ${row.cb.maxDiff}) cr ${row.cr.bad}/${row.cr.n} (max ${row.cr.maxDiff})`);
  }
  out.summary = { totalSamples: out.rows.reduce((a, r) => a + r.luma.n + r.cb.n + r.cr.n, 0), totalBad: out.rows.reduce((a, r) => a + r.luma.bad + r.cb.bad + r.cr.bad, 0) };
  out.summary.pass = out.summary.totalBad === 0;
  log('mmr summary ' + JSON.stringify(out.summary));
  comp.dispose(); target.destroy();
  return out;
}
