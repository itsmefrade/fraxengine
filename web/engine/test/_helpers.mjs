// SPDX-License-Identifier: LGPL-3.0-only
// Shared helpers for the engine tests: fixture lookup, ffmpeg reference decode, PCM comparison.
import fs from 'node:fs';
import path from 'node:path';
import crypto from 'node:crypto';
import { execFileSync, spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';

const here = path.dirname(fileURLToPath(import.meta.url));
export const ROOT = path.resolve(here, '../../..');
export const REF_DIR = path.join(here, 'fixtures');
const LAB = (process.env.AETHER_LAB || decodeURIComponent(new URL('../../../test-media', import.meta.url).pathname));

export const fixtureFiles = {
  dtsTruehd: path.join(ROOT, 'test-media/fel-dts-truehd-indexed-local.mkv'),
  dtsx: path.join(ROOT, 'test-media/fel-dtsx-seek-local.mkv'),
  atmos: path.join(ROOT, 'test-media/fel-atmos-indexed-local.mkv'),
};
export const labFiles = {
  dtsxGravity: path.join(LAB, 'DTSX Gravity.mkv'),
  dtsHraOrchestra: path.join(LAB, 'DTS-HRA 7.1 Orchestra.mkv'),
  dtsMa51: path.join(LAB, 'dts_hd_master_audio_sound_check_5-1_lossless.mkv'),
  awakenGirl: path.join(LAB, 'awaken-girl.4K.HDR.DV.mkv'),
  felAllLayers: path.join(LAB, 'DV FEL All Layers Test (Woman at 80s).mkv'),
  felPowerRangers: path.join(LAB, 'DV FEL Power Rangers Credits Test.mkv'),
  eac3Joc: path.join(LAB, 'EAC3-JOC ATMOS.mkv'),
  pocawe: path.join(LAB, 'POCAWE_Sample.mkv'),
  amaze: path.join(LAB, 'Dolby Amaze.ATMOS.m2ts'),
  truehdCheck: path.join(LAB, 'Dolby TrueHD 7.1 Channel Check.m2ts'),
  dtsxDemo: path.join(LAB, 'DTSX_Demo_2016.m2ts'),
  dredd: path.join(LAB, "DTS-HD MA 7.1 'Dredd' Sound Check.m2ts"),
  dtsHra51: path.join(LAB, 'DTS-HRA5.1_VC1-23.976.mkv'),
  ddp71: path.join(LAB, 'Dolby Digital Plus 7.1 Channel Check.mkv'),
  vc1Lpcm: path.join(LAB, 'VC1-1080p23.976-LPCM7.1.mkv'),
};

export const have = f => fs.existsSync(f);
export const hasFfmpeg = (() => { try { return spawnSync('ffmpeg', ['-version']).status === 0; } catch { return false; } })();

/** ffmpeg channel order is the bit order of the layout mask. */
const FF_ORDER = ['FL', 'FR', 'FC', 'LFE', 'BL', 'BR', 'FLC', 'FRC', 'BC', 'SL', 'SR', 'TC', 'TFL', 'TFC', 'TFR', 'TBL', 'TBC', 'TBR'];
const LABEL_TO_FF = { L: 'FL', R: 'FR', C: 'FC', Ls: 'SL', Rs: 'SR', Lb: 'BL', Rb: 'BR', Cb: 'BC' };

/** For each engine channel (by label) the index of the same speaker in ffmpeg's interleave. */
export function ffmpegChannelMap(layout) {
  const names = layout.map(l => LABEL_TO_FF[l] ?? l);
  const sorted = [...names].sort((a, b) => FF_ORDER.indexOf(a) - FF_ORDER.indexOf(b));
  return names.map(n => sorted.indexOf(n));
}

/**
 * f32le reference decode of audio stream `streamIndex` (first `seconds` s), cached under fixtures/.
 * Returns Float32Array interleaved, or null when ffmpeg is unavailable.
 */
export function ffmpegReference(file, streamIndex, seconds, inputArgs = []) {
  if (!hasFfmpeg) return null;
  const key = crypto.createHash('sha1').update(`${file}|${streamIndex}|${seconds}|${inputArgs.join(' ')}`).digest('hex').slice(0, 12);
  const out = path.join(REF_DIR, `${path.basename(file).replace(/\.\w+$/, '').replace(/\W+/g, '_')}-a${streamIndex}-${seconds}s-${key}.f32le`);
  if (!fs.existsSync(out)) {
    fs.mkdirSync(REF_DIR, { recursive: true });   // git-ignored cache, absent in a fresh clone
    execFileSync('ffmpeg', ['-v', 'error', '-y', ...inputArgs, '-i', file, '-map', `0:${streamIndex}`, '-t', String(seconds), '-f', 'f32le', '-acodec', 'pcm_f32le', out]);
  }
  const b = fs.readFileSync(out);
  return new Float32Array(b.buffer, b.byteOffset, b.byteLength >> 2);
}

/** ffprobe streams `{index, id (PID/track number as a number or null), type}` in file order. */
export function ffprobeStreams(file) {
  const j = JSON.parse(execFileSync('ffprobe', ['-v', 'error', '-show_entries', 'stream=index,id,codec_type', '-of', 'json', file]).toString());
  return j.streams.map(s => ({ index: s.index, id: s.id == null ? null : Number(s.id), type: s.codec_type }));
}

/** ffprobe stream index of the n-th audio stream. */
export function ffmpegAudioIndex(file, n) {
  const j = JSON.parse(execFileSync('ffprobe', ['-v', 'error', '-select_streams', 'a', '-show_entries', 'stream=index', '-of', 'json', file]).toString());
  return j.streams[n].index;
}

/** Max abs difference over the common prefix; interleaved ref vs planar decode. */
export function comparePlanar(planar, layout, ref) {
  const ch = planar.length;
  const map = ffmpegChannelMap(layout);
  const frames = Math.min(planar[0].length, Math.floor(ref.length / ch));
  let max = 0;
  for (let c = 0; c < ch; c++) {
    const p = planar[c], m = map[c];
    for (let i = 0; i < frames; i++) {
      const d = Math.abs(p[i] - ref[i * ch + m]);
      if (d > max) max = d;
    }
  }
  return { frames, max, refFrames: Math.floor(ref.length / ch) };
}

export function sha256Planar(planar) {
  const h = crypto.createHash('sha256');
  const frames = planar[0].length;
  const il = new Float32Array(frames * planar.length);
  planar.forEach((p, c) => { for (let i = 0; i < frames; i++) il[i * planar.length + c] = p[i]; });
  return h.update(Buffer.from(il.buffer)).digest('hex');
}

/** Decode audio packets up to `seconds` and return concatenated planar PCM. */
export function decodeWindow(demuxer, plugin, trackId, seconds) {
  const parts = [];
  let layout = null, lossless = true, sampleRate = 0;
  const startUs = demuxer.startTimeUs ?? 0;
  for (let p; (p = demuxer.nextPacket());) {
    if (p.trackId !== trackId) continue;
    if (p.ptsUs - startUs > seconds * 1e6 + 1e5) break;
    const r = plugin.decode(p);
    if (!r.frames) continue;
    layout ??= r.layout; lossless &&= r.lossless; sampleRate = r.sampleRate;
    parts.push(r.planar);
  }
  if (!parts.length) return null;
  const ch = parts[0].length;
  const total = parts.reduce((n, pl) => n + pl[0].length, 0);
  const planar = Array.from({ length: ch }, () => new Float32Array(total));
  let o = 0;
  for (const pl of parts) { for (let c = 0; c < ch; c++) planar[c].set(pl[c], o); o += pl[0].length; }
  return { planar, layout, lossless, sampleRate };
}

/**
 * Compares in blocks of `block` frames, allowing each block to sit at a different sample lag
 * (engine[i + lag] against ref[i]) taken from `lags`. Returns `{max, lags: Map<lag, blocks>}` where `max` is
 * the worst block error at its best lag. Used where a decoder switches from the DTS core to the lossless
 * extension: ffmpeg adds a fixed delay at the switch that the engine does not.
 */
export function compareBlocks(planar, layout, ref, lags = [0], block = 4800) {
  const ch = planar.length, map = ffmpegChannelMap(layout);
  const frames = Math.min(planar[0].length, Math.floor(ref.length / ch));
  const used = new Map();
  let worst = 0;
  for (let a = 0; a < frames; a += block) {
    const b = Math.min(frames, a + block);
    let best = Infinity, bestLag = 0;
    for (const lag of lags) {
      let m = 0;
      for (let c = 0; c < ch && m < best; c++) {
        const p = planar[c], r = map[c];
        for (let i = a; i < b; i++) {
          const j = i + lag;
          const d = j >= 0 && j < p.length ? Math.abs(p[j] - ref[i * ch + r]) : 1;
          if (d > m) m = d;
        }
      }
      if (m < best) { best = m; bestLag = lag; }
      if (m === 0) break;
    }
    worst = Math.max(worst, best);
    used.set(bestLag, (used.get(bestLag) ?? 0) + 1);
  }
  return { max: worst, lags: used, frames };
}
