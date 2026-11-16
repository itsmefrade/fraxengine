// SPDX-License-Identifier: LGPL-3.0-only
import test from 'node:test';
import assert from 'node:assert/strict';
import { openDemuxer, initEngine } from '../demuxer.mjs';
import { createAudioPlugin, pcmParams, engineCodec } from '../audio-plugins.mjs';
import { AetherWasm, moduleSync, AetherError } from '../aether-wasm.mjs';
import { fixtureFiles, labFiles, have, hasFfmpeg, ffmpegReference, ffmpegAudioIndex, comparePlanar, decodeWindow, sha256Planar } from './_helpers.mjs';

const SECONDS = 3;

// [name, file, codec, ffmpegInputArgs]  (lossy DTS core: tolerance 1e-5, lossless: 1e-6, AC-3 family 1e-6)
const cases = [
  ['truehd 7.1 (dts+truehd file)', fixtureFiles.dtsTruehd, 'truehd'],
  ['dts core 5.1 (dts+truehd file)', fixtureFiles.dtsTruehd, 'dts'],
  ['truehd atmos bed', fixtureFiles.atmos, 'truehd'],
  ['dts-hd ma + dts:x', fixtureFiles.dtsx, 'dts'],
  ['dtsx gravity', labFiles.dtsxGravity, 'dts'],
  ['dts-hd ma 5.1 lossless', labFiles.dtsMa51, 'dts'],
  ['dv p7 + dts (awaken girl)', labFiles.awakenGirl, 'dts'],
  ['dd+ 7.1', labFiles.ddp71, 'eac3', ['-drc_scale', '0']],
  ['e-ac-3 joc atmos bed', labFiles.eac3Joc, 'eac3', ['-drc_scale', '0'], { joc: false }],
  ['lpcm 7.1 s16le', labFiles.vc1Lpcm, 'pcm'],
];

for (const [name, file, codec, ffArgs = [], popts = {}] of cases) {
  test(`decode ${name} vs ffmpeg`, async t => {
    if (!have(file)) return t.skip(`missing fixture ${file}`);
    if (!hasFfmpeg) return t.skip('ffmpeg not installed');
    const d = await openDemuxer({ path: file });
    const tracks = d.tracks().filter(x => x.kind === 'audio');
    const idx = tracks.findIndex(x => x.codec === codec);
    assert.ok(idx >= 0);
    const track = tracks[idx];
    const plugin = createAudioPlugin(track, { presentation: 2, ...popts });
    plugin.configure(track);
    const out = decodeWindow(d, plugin, track.id, SECONDS);
    assert.ok(out, 'decoded something');
    const ref = ffmpegReference(file, ffmpegAudioIndex(file, idx), SECONDS, ffArgs);
    const c = comparePlanar(out.planar, out.layout, ref);
    const tol = out.lossless || codec !== 'dts' ? 1e-6 : 1e-5;
    console.log(`  ${name}: ${out.layout.join(',')} frames=${out.planar[0].length} ref=${c.refFrames} common=${c.frames} maxdiff=${c.max} lossless=${out.lossless} sha256=${sha256Planar(out.planar).slice(0, 16)}`);
    const window = Math.min(SECONDS, d.durationUs / 1e6);
    assert.ok(c.frames >= (window - 0.15) * out.sampleRate, 'covers the window');
    assert.ok(c.max <= tol, `max abs diff ${c.max} <= ${tol}`);
    plugin.close();
  });
}

test('decode is deterministic and flush restarts cleanly', async t => {
  if (!have(fixtureFiles.atmos)) return t.skip('missing fixture');
  const run = async () => {
    const d = await openDemuxer({ path: fixtureFiles.atmos });
    const track = d.tracks().find(x => x.codec === 'truehd');
    const plugin = createAudioPlugin(track);
    plugin.configure(track);
    const r = decodeWindow(d, plugin, track.id, 1);
    return { r, plugin, d, track };
  };
  const a = await run(), b = await run();
  assert.equal(sha256Planar(a.r.planar), sha256Planar(b.r.planar));
  // flush then feed the same start of stream again: same PCM
  a.plugin.flush();
  a.d.seek(0);
  const again = decodeWindow(a.d, a.plugin, a.track.id, 1);
  assert.equal(sha256Planar(again.planar), sha256Planar(a.r.planar));
});

test('createAudioPlugin routing', async () => {
  await initEngine();
  assert.equal(createAudioPlugin({ codec: 'mp4a.40.2' }), null);
  assert.equal(createAudioPlugin({ codec: 'opus' }), null);
  assert.equal(createAudioPlugin({ codec: 'flac' }), null);
  assert.ok(createAudioPlugin({ codec: 'truehd' }));
  assert.ok(createAudioPlugin({ codec: 'dts' }));
  assert.ok(createAudioPlugin({ codec: 'eac3' }));
  assert.ok(createAudioPlugin({ codec: 'ac-3' }));
  assert.ok(createAudioPlugin({ codec: 'pcm' }));
  assert.ok(createAudioPlugin({ codec: 'mlpa' }));
});

// DTS-HD High Resolution Audio: core + XXCH (+ X96 oversampling). The XBR/X96 residual is not decoded yet,
// so the signal differs from ffmpeg by the high band content: assert format and a bounded error.
for (const [name, file, channels] of [
  ['dts-hra 7.1 (xxch + x96)', labFiles.dtsHraOrchestra, 8],
  ['dts-hra 5.1 (x96)', labFiles.dtsHra51, 6],
]) {
  test(`decode ${name} vs ffmpeg (format exact, residual bounded)`, async t => {
    if (!have(file)) return t.skip(`missing fixture ${file}`);
    if (!hasFfmpeg) return t.skip('ffmpeg not installed');
    const d = await openDemuxer({ path: file });
    const track = d.tracks().find(x => x.kind === 'audio');
    const plugin = createAudioPlugin(track);
    plugin.configure(track);
    const out = decodeWindow(d, plugin, track.id, SECONDS);
    const ref = ffmpegReference(file, ffmpegAudioIndex(file, 0), SECONDS);
    const c = comparePlanar(out.planar, out.layout, ref);
    console.log(`  ${name}: ${out.layout.join(',')} rate=${out.sampleRate} frames=${out.planar[0].length} ref=${c.refFrames} maxdiff=${c.max}`);
    assert.equal(out.planar.length, channels);
    assert.equal(out.sampleRate, 96000);
    assert.ok(c.frames >= (SECONDS - 0.15) * 96000, 'covers the window at 96 kHz');
    assert.ok(c.max < 0.25, `max abs diff ${c.max} stays a high band residual`);
    plugin.close();
  });
}

test('engine errors carry a message (aether_last_error)', async () => {
  await initEngine();
  const plugin = createAudioPlugin({ codec: 'dts' }).configure({ codec: 'dts' });
  assert.throws(() => plugin.decode(new Uint8Array(16).fill(0xff)), e => e instanceof AetherError && /core header/.test(e.message) && e.detail.length > 0 && e.code === -200);
  const pcm = createAudioPlugin({ codec: 'pcm', containerCodec: 'A_PCM/INT/LIT', bitDepth: 16, channels: 12, sampleRate: 48000 });
  assert.throws(() => pcm.configure({ codec: 'pcm', containerCodec: 'A_PCM/INT/LIT', bitDepth: 16, channels: 12, sampleRate: 48000 }), /default layout|channel mask/);
  const w = new AetherWasm(moduleSync());
  assert.equal(w.lastError(), '');
  assert.throws(() => w.demuxInfo(), /no demuxer open/);
  assert.throws(() => w.audioPush(new Uint8Array(4)), /no audio decoder open/);
});

test('pcm parameters from container sample entries', async () => {
  await initEngine();
  const p = (containerCodec, extra = {}) => pcmParams({ codec: 'pcm', containerCodec, channels: 2, sampleRate: 48000, ...extra });
  assert.equal(p('A_PCM/INT/LIT', { bitDepth: 24 }).format, 0);
  assert.equal(p('A_PCM/INT/LIT', { bitDepth: 8 }).format, 4);
  assert.equal(p('A_PCM/INT/BIG', { bitDepth: 16 }).format, 1);
  assert.deepEqual([p('A_PCM/FLOAT/IEEE', { bitDepth: 32 }).format, p('A_PCM/FLOAT/IEEE', { bitDepth: 32 }).bits], [2, 32]);
  assert.deepEqual([p('sowt').format, p('sowt').bits], [0, 16]);
  assert.deepEqual([p('twos').format, p('in24').bits, p('in32').format, p('fl32').format, p('fl64').bits, p('raw ').format], [1, 24, 1, 3, 64, 4]);
  assert.deepEqual([p('ipcm', { description: Uint8Array.of(0, 0, 0, 0, 1, 24) }).format, p('ipcm', { description: Uint8Array.of(0, 0, 0, 0, 0, 24) }).format], [0, 1]);
  assert.equal(p('fpcm', { description: Uint8Array.of(0, 0, 0, 0, 1, 32) }).format, 2);
  assert.equal(pcmParams({ codec: 'pcm-s24be', channels: 6, sampleRate: 48000 }).format, 1);
  assert.equal(pcmParams({ codec: 'pcm', pcm: { format: 'hdmv' } }).format, 5);
  assert.throws(() => p('lpcm'), /pass track\.pcm/);
  assert.equal(p('lpcm', { pcm: { format: 'float', bigEndian: true, bits: 32 } }).format, 3);
  assert.equal(engineCodec('ec-3'), 'eac3');
  assert.equal(engineCodec('mlpa'), 'truehd');
  assert.equal(engineCodec('dtsh'), 'dts');
});

/** s16le interleaved reference, pcm plugin result must match bit for bit. */
test('pcm decoder: formats, partial frames and Blu-ray LPCM', async () => {
  await initEngine();
  const run = (track, bytes) => {
    const pl = createAudioPlugin(track).configure(track);
    const r = pl.decode(bytes);
    pl.close();
    return r;
  };
  const s24be = Uint8Array.of(0x80, 0, 0, 0x40, 0, 0, 0, 0, 0, 0xff, 0xff, 0xff);
  const r = run({ codec: 'pcm', containerCodec: 'in24', channels: 2, sampleRate: 44100 }, s24be);
  assert.deepEqual(r.layout, ['FL', 'FR']);
  assert.deepEqual([...r.planar[0]], [-1, 0]);
  assert.deepEqual([...r.planar[1]], [0.5, -1 / 8388608]);
  assert.equal(r.sampleRate, 44100);
  assert.equal(r.lossless, true);
  // HDMV 7.1 24 bit, 2 frames
  const body = [];
  for (let f = 0; f < 2; f++) for (let c = 0; c < 8; c++) body.push(0, c + 1, f);
  const bd = run({ codec: 'pcm', pcm: { format: 'hdmv' } }, Uint8Array.of(0, body.length, 0xb1, 0xc0, ...body));
  assert.deepEqual(bd.layout, ['FL', 'FR', 'FC', 'LFE', 'SL', 'SR', 'BL', 'BR']);
  assert.equal(bd.frames, 2);
  assert.equal(bd.planar[7][1], (8 * 256 + 1) / 8388608);
  // a frame split over two packets is completed by the second
  const pl = createAudioPlugin({ codec: 'pcm', containerCodec: 'sowt', channels: 2, sampleRate: 48000 }).configure({ codec: 'pcm', containerCodec: 'sowt', channels: 2, sampleRate: 48000 });
  assert.equal(pl.decode(Uint8Array.of(1, 0, 2)).frames, 0);
  assert.equal(pl.decode(Uint8Array.of(0)).frames, 1);
});

test('canonical channel layout table matches the engine', async () => {
  await initEngine();
  const t = new AetherWasm(moduleSync()).audioLayoutTable();
  assert.equal(t.wave.slice(0, 8).join(' '), 'FL FR FC LFE BL BR FLC FRC');
  assert.equal(t.smpte.slice(0, 8).join(' '), 'FL FR FC LFE SL SR BL BR');
  assert.equal(t.waveBits.SL, 9);
  assert.deepEqual(t.named['7.1'], ['FL', 'FR', 'FC', 'LFE', 'BL', 'BR', 'SL', 'SR']);
  assert.deepEqual(t.defaults['8'], t.named['7.1']);
});

test('eac3 jocs: atmos bed reports joc, plain dd+ does not', async t => {
  if (!have(labFiles.eac3Joc) || !have(labFiles.ddp71)) return t.skip('missing fixtures');
  for (const [file, expected] of [[labFiles.eac3Joc, true], [labFiles.ddp71, false]]) {
    const d = await openDemuxer({ path: file });
    const track = d.tracks().find(x => x.kind === 'audio');
    const plugin = createAudioPlugin(track).configure(track);
    const r = decodeWindowInfo(d, plugin, track.id);
    assert.equal(Boolean(r.joc), expected, file);
    if (expected) assert.deepEqual([r.joc.rendered, r.channels], [true, 12], 'objects rendered to 7.1.4 by default');
    plugin.close();
  }
});

function decodeWindowInfo(d, plugin, trackId) {
  for (let p; (p = d.nextPacket());) {
    if (p.trackId !== trackId) continue;
    const r = plugin.decode(p);
    if (r.frames) return r;
  }
  throw new Error('no audio');
}
