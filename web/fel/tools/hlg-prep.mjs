// SPDX-License-Identifier: LGPL-3.0-only
// Prepares the inputs of the HLG / Profile 8.4 numeric check (fel/hlg-verify.html) for display frames K of a file:
//   fel/media/hlg/<name>.json|.bin     the first packets (decode order) with the SPS relabelled the way the player does (relabelForChrome)
//   fel/media/hlg/<name>-rpu-<k>.json  RPU display metadata + GPU tables (base64 Int32) of the RPU of display frame k (parsed from the stream start)
//   fel/media/hlg/<name>-bl-<k>.yuv    FFmpeg's decode of the base layer (yuv420p10le, the raw codes)
// usage: node fel/tools/hlg-prep.mjs <file> <name> [frames=5,100,200,280]
import fs from 'node:fs';
import { spawnSync } from 'node:child_process';
import { openDemuxer, splitDolbyVision, relabelForChrome, relabelPacketSps } from '../../engine/demuxer.mjs';
import { createDoviSession } from '../../engine/dovi.mjs';

const [, , file, name, ks = '5,100,200,280'] = process.argv;
const OUT = new URL('../media/hlg/', import.meta.url).pathname;
fs.mkdirSync(OUT, { recursive: true });
const K = ks.split(',').map(Number);
const d = await openDemuxer({ path: file });
const t = d.tracks().find(x => x.kind === 'video');
const ls = t.description && t.description.length > 21 ? (t.description[21] & 3) + 1 : 4;
const s = await createDoviSession(); s.reset();
const nmax = Math.max(...K) + 80;
const pk = [], parts = []; let off = 0;
for (let p; (p = d.nextPacket()) && pk.length < nmax;) {
  if (p.trackId !== t.id) continue;
  const sp = t.dolbyVision ? splitDolbyVision(p, ls) : null;
  const r = sp && sp.rpu ? s.parseRpu(sp.rpu) : null;
  const bl = Buffer.from(relabelPacketSps(sp && sp.bl.length ? sp.bl : p.data, ls).data);   // in-band SPS carry the colour tag too (as demux-worker.js does)
  parts.push(bl);
  pk.push({ off, len: bl.length, pts: p.ptsUs, dur: p.durationUs, key: !!p.key, display: r && r.display, tables: r && r.tables });
  off += bl.length;
}
const rel = relabelForChrome(t);
const nonneg = pk.map((x, i) => i).filter(i => pk[i].pts >= 0).sort((a, b) => pk[a].pts - pk[b].pts);
fs.writeFileSync(OUT + name + '.bin', Buffer.concat(parts));
fs.writeFileSync(OUT + name + '.json', JSON.stringify({ codec: t.codec, fallbackCodecString: t.fallbackCodecString, width: t.width, height: t.height, color: t.color, dolbyVision: t.dolbyVision, description: Buffer.from(rel.track.description).toString('base64'), originalColor: rel.original,
  packets: pk.map(({ off, len, pts, dur, key }) => ({ off, len, pts, dur, key })), frames: K.map(k => ({ k, pts: pk[nonneg[k]].pts, decodeIndex: nonneg[k] })) }));
for (const k of K) {
  const x = pk[nonneg[k]];
  const tb = x.tables ? Buffer.from(x.tables.buffer, x.tables.byteOffset, x.tables.byteLength).toString('base64') : null;
  fs.writeFileSync(`${OUT}${name}-rpu-${k}.json`, JSON.stringify({ frame: k, ptsUs: x.pts, decodeIndex: nonneg[k], display: x.display, tablesB64: tb }));
  const f = `${OUT}${name}-bl-${k}.yuv`;
  const r = spawnSync('ffmpeg', ['-v', 'error', '-y', '-i', file, '-vf', `select=eq(n\\,${k})`, '-frames:v', '1', '-fps_mode', 'passthrough', '-pix_fmt', 'yuv420p10le', '-f', 'rawvideo', f], { encoding: 'utf8' });
  if (r.status !== 0) throw new Error(r.stderr);
  console.log(`frame ${k}: pts ${x.pts} decode ${nonneg[k]} yuv ${fs.statSync(f).size} l1 ${JSON.stringify(x.display && x.display.l1)}`);
}
d.close();
