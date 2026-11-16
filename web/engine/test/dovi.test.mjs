// SPDX-License-Identifier: LGPL-3.0-only
import test from 'node:test';
import assert from 'node:assert/strict';
import { openDemuxer, splitDolbyVision, relabelForChrome, relabelPacketSps, SDR_CARRIER } from '../demuxer.mjs';
import { AetherWasm, moduleSync } from '../aether-wasm.mjs';
import { fixtureFiles, labFiles, have } from './_helpers.mjs';

function nalTypes(data, size = 4) {
  const out = [];
  for (let p = 0; p + size <= data.length;) {
    let len = 0;
    for (let i = 0; i < size; i++) len = len * 256 + data[p + i];
    out.push((data[p + size] >> 1) & 63);
    p += size + len;
  }
  return out;
}

test('splitDolbyVision on a P7 keyframe', async t => {
  if (!have(fixtureFiles.dtsTruehd)) return t.skip('missing fixture');
  const d = await openDemuxer({ path: fixtureFiles.dtsTruehd });
  const video = d.tracks().find(x => x.kind === 'video');
  let p;
  while ((p = d.nextPacket()).trackId !== video.id);
  const s = splitDolbyVision(p, video);
  assert.ok(s.bl.length > 0 && s.el.length > 0, 'both layers present');
  assert.ok(s.rpu && ((s.rpu[0] >> 1) & 63) === 62, 'type 62 RPU');
  assert.ok(nalTypes(s.bl).every(x => x < 62), 'BL has no wrapper NALs');
  assert.ok(nalTypes(s.el).every(x => x < 62), 'EL NALs are unwrapped');
  assert.ok(nalTypes(s.bl).some(x => x >= 16 && x <= 21), 'BL carries an IRAP slice');
  // BL + EL + RPU account for the whole access unit payload
  assert.equal(s.bl.length + s.el.length + 2 * nalTypes(s.el).length + s.rpus.reduce((n, r) => n + 4 + r.length, 0), p.data.length);
  const w = new AetherWasm(moduleSync());
  const info = w.doviInspectRpu(s.rpu);
  assert.equal(info.profile, 7);
});

test('relabel in-band SPS to the SDR carrier and back', async t => {
  if (!have(fixtureFiles.dtsTruehd)) return t.skip('missing fixture');
  const d = await openDemuxer({ path: fixtureFiles.dtsTruehd });
  const video = d.tracks().find(x => x.kind === 'video');
  let p;
  while ((p = d.nextPacket()).trackId !== video.id);
  const { bl } = splitDolbyVision(p, video);
  const r = relabelPacketSps(bl, video);
  assert.ok(r.original, 'an SPS was found');
  assert.deepEqual([r.original.primaries, r.original.transfer], [9, 16], 'original is BT.2020 PQ');
  const again = relabelPacketSps(r.data, video, { primaries: -1, transfer: -1, matrix: -1 });
  assert.deepEqual([again.original.primaries, again.original.transfer, again.original.matrix], [1, 13, r.original.matrix]);
  // restoring the original colour round-trips to the same bytes
  const back = relabelPacketSps(r.data, video, r.original);
  assert.deepEqual(Buffer.from(back.data), Buffer.from(bl));
  // a packet without SPS comes back untouched
  let q;
  while ((q = d.nextPacket()).trackId !== video.id || q.key);
  const bl2 = splitDolbyVision(q, video).bl;
  assert.equal(relabelPacketSps(bl2, video).data, bl2);
});

test('relabelForChrome patches hvcC parameter sets when present', async t => {
  const file = [labFiles.felAllLayers, labFiles.felPowerRangers, labFiles.awakenGirl, labFiles.eac3Joc].find(have);
  if (!file) return t.skip('no lab media');
  for (const f of [labFiles.felAllLayers, labFiles.felPowerRangers, labFiles.awakenGirl, labFiles.eac3Joc].filter(have)) {
    const d = await openDemuxer({ path: f });
    const video = d.tracks().find(x => x.kind === 'video');
    const { track, original } = relabelForChrome(video);
    assert.equal(track.relabelled, true);
    assert.equal(track.codec, video.codec);
    if (original) {
      assert.deepEqual([original.primaries, original.transfer], [9, 16]);
      assert.equal(track.description.length >= video.description.length - 8, true);
      const check = relabelForChrome(track, { primaries: -1, transfer: -1, matrix: -1 });
      assert.deepEqual([check.original.primaries, check.original.transfer], [1, 13]);
    }
    console.log(`  ${f.split('/').pop()}: hvcC SPS ${original ? 'patched from ' + original.primaries + '/' + original.transfer : 'absent (in-band)'}`);
  }
});

test('SDR_CARRIER constant matches vui::SDR_CARRIER', () => {
  assert.deepEqual({ ...SDR_CARRIER }, { primaries: 1, transfer: 13, matrix: -1 });
});

// Main10 SPS, 64x64, whose VUI carries no video signal type (what streaming profile 5 encodes look like).
function spsWithoutSignalType() {
  const b = [];
  const u = (v, n) => { for (let i = n - 1; i >= 0; i--) b.push(Math.floor(v / 2 ** i) % 2); };
  const ue = v => { const x = v + 1, n = 2 * Math.floor(Math.log2(x)) + 1; u(x, n); };
  u(0, 4); u(0, 3); u(1, 1); u(2, 8); u(0x20000000, 32); u(0b1001, 4); u(0, 32); u(0, 12); u(93, 8);
  ue(0); ue(1); ue(64); ue(64); u(0, 1); ue(2); ue(2); ue(4); u(1, 1);
  for (const v of [4, 4, 4, 0, 3, 0, 3, 1, 1]) ue(v);
  u(0, 1); u(0, 3); ue(0); u(0, 1); u(0, 2);
  u(1, 1); u(0, 1); u(0, 1); u(0, 1);   // VUI present; no aspect ratio, no overscan, no video signal type
  u(0, 7);                              // the rest of the VUI absent
  u(0, 1);                              // no SPS extension
  u(1, 1); while (b.length % 8) b.push(0);
  const bytes = [0x42, 0x01];
  for (let i = 0, zeros = 0; i < b.length; i += 8) {
    const v = b.slice(i, i + 8).reduce((a, x) => a * 2 + x, 0);
    if (zeros >= 2 && v <= 3) { bytes.push(3); zeros = 0; }
    zeros = v === 0 ? zeros + 1 : 0; bytes.push(v);
  }
  return Uint8Array.from(bytes);
}

test('an SPS that states no colour is relabelled anyway, full range for the profile 5 carrier', async () => {
  const { initEngine } = await import('../demuxer.mjs');
  await initEngine();
  const { packetFullRange } = await import('../../fel/sps-range.mjs');
  const { P5_CARRIER } = await import('../aether-wasm.mjs');
  const sps = spsWithoutSignalType();
  const packet = new Uint8Array(4 + sps.length); packet.set([0, 0, 0, sps.length]); packet.set(sps, 4);
  assert.equal(packetFullRange(packet, 4, { primaries: 1, transfer: 13 }), null);
  const p5 = relabelPacketSps(packet, 4, P5_CARRIER);
  assert.deepEqual(p5.original, { primaries: 2, transfer: 2, matrix: 2 });
  assert.equal(packetFullRange(p5.data, 4, { primaries: 1, transfer: 13 }), true);
  const sdr = relabelPacketSps(packet, 4, SDR_CARRIER);
  // the SDR carrier keeps the matrix, here unspecified, so the scan names no range: read as limited, which is what was written
  assert.equal(packetFullRange(sdr.data, 4, { primaries: 1, transfer: 13 }), null);
  assert.equal(packetFullRange(relabelPacketSps(packet, 4, { primaries: 1, transfer: 13, matrix: 9 }).data, 4, { primaries: 1, transfer: 13 }), false);
});
