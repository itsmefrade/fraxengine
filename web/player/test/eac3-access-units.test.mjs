// SPDX-License-Identifier: LGPL-3.0-only
import assert from 'node:assert/strict';
import { test } from 'node:test';
import { buildDec3, eac3Blocks } from '../js/mse/dolby-audio.js';
import { Fmp4Muxer } from '../js/mse/fmp4.js';

class W {
  bits = [];
  u(n, v) { for (let i = n - 1; i >= 0; i--) this.bits.push((v >> i) & 1); return this; }
  bytes(len) {
    const out = new Uint8Array(len);
    this.bits.forEach((b, i) => { out[i >> 3] |= b << (7 - (i & 7)); });
    return out;
  }
}

// One independent E-AC-3 syncframe (acmod 7, LFE, 48 kHz) of the given length; the header is real, the audio blocks are zeros.
function syncframe({ numblkscod = 3, bytes = 512, meta = false, complexity = 0 } = {}) {
  const w = new W().u(16, 0x0b77).u(2, 0).u(3, 0).u(11, bytes / 2 - 1).u(2, 0).u(2, numblkscod).u(3, 7).u(1, 1).u(5, 16).u(5, 31).u(1, 0);
  if (meta) {
    w.u(1, 1).u(2, 0).u(6, 0).u(6, 0).u(1, 0);    // mixmdate: downmix, centre levels, surround levels, no lfe mix
    w.u(1, 0).u(1, 0).u(2, 0).u(1, 0);            // no program scale, no external scale, mixdef 0, no frmmixcfginfo
    w.u(1, 1).u(3, 0).u(2, 0).u(2, 0).u(1, 0).u(1, 0);   // infomdate: bsmod, copyright and orig, dsurexmod, no audprodi, sourcefscod
  } else w.u(1, 0).u(1, 0);
  if (numblkscod !== 3) w.u(1, 1);                // convsync
  if (complexity) w.u(1, 1).u(6, 1).u(8, 1).u(8, complexity);
  else w.u(1, 0);
  return w.bytes(bytes);
}

const trun = (frag) => {
  const dv = new DataView(frag.buffer, frag.byteOffset, frag.byteLength);
  for (let i = 0; i + 8 <= frag.length; i++) {
    if (frag[i] === 0x74 && frag[i + 1] === 0x72 && frag[i + 2] === 0x75 && frag[i + 3] === 0x6e) {
      const count = dv.getUint32(i + 8);
      return { count, durations: Array.from({ length: count }, (_, k) => dv.getUint32(i + 16 + k * 16)), sizes: Array.from({ length: count }, (_, k) => dv.getUint32(i + 20 + k * 16)) };
    }
  }
  throw new Error('no trun');
};
const mux = () => new Fmp4Muxer({ codec: 'ec-3', description: new Uint8Array(5), channels: 8, sampleRate: 48000 });
const sample = (frame, n) => ({ data: frame, ptsUs: Math.round(n * 256 * 1e6 / 48000), dtsUs: Math.round(n * 256 * 1e6 / 48000), durationUs: 5333, key: true });

test('the JOC complexity is read from the additional bitstream info of the first syncframe', () => {
  const r = buildDec3(syncframe({ complexity: 16 }));
  assert.equal(r.config.length, 7);
  assert.equal(r.config[5], 1);
  assert.equal(r.config[6], 16);
});

test('the JOC complexity is found behind the mixing and informational metadata', () => {
  const r = buildDec3(syncframe({ meta: true, complexity: 12 }));
  assert.equal(r.config[5], 1);
  assert.equal(r.config[6], 12);
});

test('a demuxer complexity wins, and a stream without the extension stays plain E-AC-3', () => {
  assert.equal(buildDec3(syncframe({ complexity: 16 }), 7).config[6], 7);
  const plain = buildDec3(syncframe());
  assert.equal(plain.config.length, 5);
});

test('eac3Blocks counts six blocks per access unit of the independent substream', () => {
  assert.equal(eac3Blocks(syncframe({ numblkscod: 3 })), 6);
  assert.equal(eac3Blocks(syncframe({ numblkscod: 0 })), 1);
  const pack = (n, f) => { const out = new Uint8Array(n * f.length); for (let i = 0; i < n; i++) out.set(f, i * f.length); return out; };
  assert.equal(eac3Blocks(pack(6, syncframe({ numblkscod: 0 }))), 6);
  assert.equal(eac3Blocks(new Uint8Array(32)), 0);
});

test('short E-AC-3 syncframes are merged into six block samples across fragments', () => {
  const m = mux();
  const f = syncframe({ numblkscod: 0, bytes: 400 });
  const first = m.fragment([sample(f, 0), sample(f, 1), sample(f, 2), sample(f, 3)]);
  assert.equal(first.length, 0);   // no complete set yet
  const second = m.fragment([sample(f, 4), sample(f, 5), sample(f, 6)]);
  const t = trun(second);
  assert.equal(t.count, 1);
  assert.equal(t.sizes[0], 6 * 400);
  // the trailing block stays held for the next fragment
  assert.equal(m.ec3Held.blocks, 1);
});

test('a seek drops the blocks held from before it', () => {
  const m = mux();
  const f = syncframe({ numblkscod: 0, bytes: 400 });
  m.fragment([sample(f, 0), sample(f, 1)]);
  const after = m.fragment([0, 1, 2, 3, 4, 5].map(i => sample(f, 2000 + i)));
  assert.equal(trun(after).count, 1);
  assert.equal(trun(after).sizes[0], 6 * 400);
});

test('six block E-AC-3 samples pass through one to one', () => {
  const m = mux();
  const f = syncframe({ numblkscod: 3, bytes: 1536 });
  const frag = m.fragment([0, 1, 2].map(i => ({ data: f, ptsUs: i * 32000, dtsUs: i * 32000, durationUs: 32000, key: true })));
  const t = trun(frag);
  assert.equal(t.count, 3);
  assert.deepEqual(t.durations, [1536, 1536, 1536]);
});
