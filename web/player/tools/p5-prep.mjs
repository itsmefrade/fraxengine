// Prepares the inputs of the Profile 5 numeric check (p5-verify.html): for display frames K of "Test Jellyfin 4K DV P5.mp4"
//   fixtures/p5ref/bl-<k>.yuv    FFmpeg's decode of the base layer (yuv420p10le, the raw IPTPQc2 codes), written with ffmpeg select
//   fixtures/p5ref/rpu-<k>.json  RPU display metadata (web/engine/dovi.mjs) + GPU tables (base64 Int32) of the RPU of that picture,
//                                parsed in decode order from the start of the stream (mapping history intact)
// usage: node tools/p5-prep.mjs [frames=0,100,200,300,400] [out=p5ref]     (p5-route-compare.html wants 0,449,898,1347,1790 in p5spread)
import fs from 'node:fs';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { openDemuxer, splitDolbyVision } from '../../engine/demuxer.mjs';
import { createDoviSession } from '../../engine/dovi.mjs';

const here = path.dirname(fileURLToPath(import.meta.url));
const SRC = (process.env.AETHER_LAB || decodeURIComponent(new URL('../../../test-media', import.meta.url).pathname)) + '/Test Jellyfin 4K DV P5.mp4';
const OUT = path.join(here, '../fixtures', process.argv[3] || 'p5ref');
fs.mkdirSync(OUT, { recursive: true });
const K = (process.argv[2] || '0,100,200,300,400').split(',').map(Number);
const kmax = Math.max(...K);

const d = await openDemuxer({ path: SRC });
const t = d.tracks().find(x => x.kind === 'video');
const ls = t.description && t.description.length > 21 ? (t.description[21] & 3) + 1 : 4;
const s = await createDoviSession();
s.reset();
const pk = [];
for (let p, n = 0; n < kmax + 40 && (p = d.nextPacket());) {
  if (p.trackId !== t.id) continue;
  n++;
  const sp = splitDolbyVision(p, ls);
  let r = null;
  if (sp.rpu) r = s.parseRpu(sp.rpu);
  pk.push({ pts: p.ptsUs, key: p.key, display: r && r.display, tables: r && r.tables, tablesError: r && r.tablesError && String(r.tablesError.message) });
}
const order = pk.map((x, i) => i).sort((a, b) => pk[a].pts - pk[b].pts);
for (const k of K) {
  const x = pk[order[k]];
  if (!x.display || !x.tables) throw new Error(`frame ${k}: no RPU data (${x.tablesError})`);
  const tb = Buffer.from(x.tables.buffer, x.tables.byteOffset, x.tables.byteLength);
  fs.writeFileSync(path.join(OUT, `rpu-${k}.json`), JSON.stringify({ frame: k, ptsUs: x.pts, decodeIndex: order[k], display: x.display, tablesB64: tb.toString('base64') }));
  console.log(`frame ${k}: pts ${x.pts} decode index ${order[k]} compressed=${x.display.compressed} inherited=${x.display.inherited} l5=${JSON.stringify(x.display.l5)} l1=${JSON.stringify(x.display.l1)} ycc offsets=${x.display.yccToRgbOffset} fullRange=${x.display.signal.fullRange}`);
}
d.close();
const sel = K.map(k => `eq(n\\,${k})`).join('+');
for (const k of K) {
  const f = path.join(OUT, `bl-${k}.yuv`);
  const r = spawnSync('ffmpeg', ['-v', 'error', '-y', '-i', SRC, '-vf', `select=eq(n\\,${k})`, '-frames:v', '1', '-fps_mode', 'passthrough', '-pix_fmt', 'yuv420p10le', '-f', 'rawvideo', f], { encoding: 'utf8' });
  if (r.status !== 0) throw new Error(r.stderr);
  console.log('wrote', f, fs.statSync(f).size);
}
