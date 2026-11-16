// SPDX-License-Identifier: LGPL-3.0-only
// Dumps the video packets of a file for the browser decode benches: <out>.json (track facts, base64 hvcC, packet table) and <out>.bin (concatenated packets).
// usage: node fel/tools/dump-packets.mjs <file> <out-base> [maxPackets]
import fs from 'node:fs';
import { openDemuxer } from '../../engine/demuxer.mjs';
const [, , file, out, max = '100000'] = process.argv;
const d = await openDemuxer({ path: file });
const t = d.tracks().find(x => x.kind === 'video');
const parts = [], pk = []; let off = 0;
for (let p; (p = d.nextPacket()) && pk.length < Number(max);) {
  if (p.trackId !== t.id) continue;
  const b = Buffer.from(p.data.buffer, p.data.byteOffset, p.data.byteLength);
  parts.push(b); pk.push({ off, len: b.length, pts: p.ptsUs, dur: p.durationUs, key: !!p.key }); off += b.length;
}
fs.writeFileSync(out + '.bin', Buffer.concat(parts));
fs.writeFileSync(out + '.json', JSON.stringify({ codec: t.codec, fallbackCodecString: t.fallbackCodecString, width: t.width, height: t.height, color: t.color, dolbyVision: t.dolbyVision, description: Buffer.from(t.description).toString('base64'), packets: pk }));
console.log(file.split('/').pop(), pk.length, 'packets', off, 'bytes', JSON.stringify(t.color), JSON.stringify(t.dolbyVision));
