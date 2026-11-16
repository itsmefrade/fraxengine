// SPDX-License-Identifier: LGPL-3.0-only
// Blu-ray H.264 redefines one PPS id per picture; every distinct content gets its own id by a whole-byte rewrite.
import test from 'node:test';
import assert from 'node:assert/strict';
import { PpsRenumber, buildAvcCAll, annexBToLengthPrefixed } from '../js/demux/annexb.js';

const hex = s => Uint8Array.from(s.match(/../g).map(h => parseInt(h, 16)));
const bitsOf = (b, from = 0) => { let s = ''; for (const v of b.subarray(from)) s += v.toString(2).padStart(8, '0'); return s; };
const unescape = x => { const o = []; for (let i = 0; i < x.length; i++) { if (i >= 2 && x[i] === 3 && x[i - 1] === 0 && x[i - 2] === 0) continue; o.push(x[i]); } return Uint8Array.from(o); };
function ue(bits, p) { let z = 0; while (bits[p + z] === '0') z++; return [parseInt(bits.slice(p + z, p + 2 * z + 1), 2) - 1, p + 2 * z + 1]; }

// the four PPS contents of one sample (id 0, num_ref_idx_l0_default_active_minus1 0 to 3)
const PPS = ['68ef3525', '68eacd4940', '68ebcd4940', '68e9335250'].map(hex);

test('the first content keeps its id, later contents get ids whose code is one byte longer', () => {
  const r = new PpsRenumber();
  assert.deepEqual(r.nal(PPS[0]), PPS[0]);
  const ids = PPS.slice(1).map(p => {
    const out = r.nal(p), a = bitsOf(p, 1), b = bitsOf(out, 1);
    assert.equal(out.length, p.length + 1);
    const [id, end] = ue(b, 0);
    assert.equal(b.slice(end), a.slice(1), 'everything behind the id is unchanged and still byte aligned');
    return id;
  });
  assert.deepEqual(ids, [15, 16, 17]);
  assert.deepEqual(r.nal(PPS[1]), r.nal(PPS[1]), 'a repeated content maps to the same id');
  assert.equal(r.failed, false);
});

test('slices follow the content their id currently names', () => {
  const r = new PpsRenumber();
  // first_mb 0, slice_type 7, pps 0, then payload bytes (with a 00 00 run that needs emulation prevention after the shift)
  const slice = hex('6588' + '80000003000102030405060708090a0b0c0d0e0f101112');
  r.nal(PPS[0]);
  assert.equal(r.nal(slice), slice, 'id 0 with its first content is left alone');
  r.nal(PPS[2]);
  const out = r.nal(slice), a = bitsOf(unescape(slice), 1), b = bitsOf(unescape(out), 1);
  let p = 0, v;
  [v, p] = ue(b, p); assert.equal(v, 0);
  [v, p] = ue(b, p); assert.equal(v, 7);
  const idAt = p;
  [v, p] = ue(b, p); assert.equal(v, 15);
  assert.equal(b.slice(p), a.slice(idAt + 1));
  for (let i = 2; i < out.length; i++) assert.ok(!(out[i - 2] === 0 && out[i - 1] === 0 && out[i] < 3), 'no start code emulation');
  r.nal(PPS[0]);
  assert.equal(r.nal(slice), slice, 'back to the first content');
});

test('avcC lists every distinct PPS and the packet rewrite applies the renumbering', () => {
  const r = new PpsRenumber();
  const sps = hex('67640029ac2ca501e0089f97015202020280');
  const sets = [sps, ...PPS, PPS[1]].map(x => r.nal(x));
  const avcc = buildAvcCAll(sets);
  assert.equal(avcc[5] & 31, 1);
  assert.equal(avcc[8 + sps.length], 4);
  const au = Uint8Array.from([0, 0, 1, 9, 0x10, 0, 0, 1, ...PPS[3], 0, 0, 1, ...hex('4188' + '8011223344556677889900aabbccddeeff00112233')]);
  const lp = annexBToLengthPrefixed(au, r);
  const n0 = (lp[2] << 8) | lp[3];
  assert.equal(n0, PPS[3].length + 1);
  assert.equal(lp.length, 4 + n0 + 4 + 23 + 1);
});

test('past 18 contents under one id, a content unused since the last IDR picture gives its id back (review note on e36787e)', () => {
  const r = new PpsRenumber();
  // pps id 0, then 31 distinct tails: more contents than the 16 nine-bit ids and id 255 can hold
  const content = i => hex('68ef' + (0x1000 + i).toString(16) + '80');
  const idr = hex('6588' + '8011223344556677889900aabbccddeeff'), p = hex('4188' + '8011223344556677889900aabbccddeeff');
  const idOf = out => ue(bitsOf(unescape(out), 1), 0)[0];
  const sliceId = out => { const b = bitsOf(unescape(out), 1); let q = 0, v; [v, q] = ue(b, q); [v, q] = ue(b, q); return ue(b, q)[0]; };
  const ids = [];
  for (let i = 0; i < 31; i++) {
    if (i % 6 === 0) { r.nal(content(0)); r.nal(idr); }   // an IDR picture on the first content every six pictures
    const out = r.nal(content(i));
    ids.push(idOf(out));
    assert.equal(sliceId(r.nal(p)), ids[i], 'the slice names the id its PPS was just given');
  }
  assert.equal(r.failed, false);
  assert.ok(r.recycled >= 12, `ids were reused (${r.recycled})`);
  assert.equal(new Set(ids.slice(-6)).size, 6, 'contents inside one GOP never share an id');
  assert.ok(ids.every(id => id === 0 || (id >= 15 && id <= 30) || id === 255));
});

test('contents that are all in use inside one GOP and do not fit are reported, not hidden', () => {
  const r = new PpsRenumber();
  const content = i => hex('68ef' + (0x1000 + i).toString(16) + '80');
  const p = hex('4188' + '8011223344556677889900aabbccddeeff');
  for (let i = 0; i < 19; i++) { r.nal(content(i)); r.nal(p); }
  assert.equal(r.failed, true);
});
