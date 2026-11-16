// SPDX-License-Identifier: LGPL-3.0-only
// Dev tool (Node 20+): demux a Dolby Vision P7 file with web/engine (Rust MKV/MP4 demuxer + splitDolbyVision) and write
//   media/<name>-bl.hevc / -el.hevc              Annex B, original SPS (input of the WASM exact decoder, web/exact)
//   media/<name>-retag-bl.hevc / -retag-el.hevc  Annex B, SPS relabelled primaries 1 / transfer 13 (input of WebCodecs hardware)
//   media/<name>-index.json, -retag-index.json   {aus:[{bl:[off,len], el:[off,len], key, rpu:[off,len]}]} (rpu offsets into -rpu.bin)
//   media/<name>-rpu.bin                         concatenated type-62 RPU NALs (no start code)
// usage: node prep-media.mjs <file> <name> [maxFrames] [startFrame]
import fs from 'node:fs';
import { openDemuxer, splitDolbyVision, relabelPacketSps } from '../../engine/demuxer.mjs';

const [file, name, maxArg, startArg] = process.argv.slice(2);
const max = +(maxArg ?? 1440), start = +(startArg ?? 0);
const out = new URL('../media/', import.meta.url).pathname;
const d = await openDemuxer({ path: file });
const track = d.tracks().find(t => t.kind === 'video');
const size = (track.description?.[21] & 3) + 1 || 4;
console.log('track', track.id, track.codec, track.width, track.height, 'lengthSize', size, 'duration s', d.durationUs / 1e6);
const SC = Uint8Array.of(0, 0, 0, 1);
function annexB(lp) {
  const parts = []; let pos = 0, total = 0;
  while (pos + size <= lp.length) {
    let len = 0; for (let i = 0; i < size; i++) len = len * 256 + lp[pos + i];
    parts.push(SC, lp.subarray(pos + size, pos + size + len)); total += 4 + len; pos += size + len;
  }
  const o = new Uint8Array(total); let p = 0; for (const x of parts) { o.set(x, p); p += x.length; } return o;
}
const nalTypes = lp => { const t = []; let pos = 0; while (pos + size <= lp.length) { let len = 0; for (let i = 0; i < size; i++) len = len * 256 + lp[pos + i]; t.push((lp[pos + size] >> 1) & 63); pos += size + len; } return t; };
const bufs = { bl: [], el: [], rbl: [], rel: [], rpu: [] }, off = { bl: 0, el: 0, rbl: 0, rel: 0, rpu: 0 };
const plain = [], retag = [];
let n = 0, idx = 0, started = false, bytes = 0, noRpu = 0, multiRpu = 0, blTypes = {}, droppedRasl = 0, firstGop = true;
const pts = [];
for (;;) {
  const p = d.nextPacket();
  if (!p) break;
  if (p.trackId !== track.id) continue;
  const s = splitDolbyVision(p, size);
  const types = nalTypes(s.bl);
  const irap = types.some(t => t >= 16 && t <= 21);
  if (!started) { if (idx++ < start || !irap) continue; started = true; }   // begin at an IRAP so every GOP is self contained
  if (n >= max && irap) break;                                                // end on a GOP boundary
  const rasl = types.some(t => t === 8 || t === 9);
  if (irap && n > 0) firstGop = false;
  if (rasl && firstGop) { droppedRasl++; continue; }                          // RASL of the very first CRA cannot be decoded
  for (const t of types) blTypes[t] = (blTypes[t] ?? 0) + 1;
  const push = (key, a, store) => { const b = annexB(a); bufs[key].push(b); const o = off[key]; off[key] += b.length; return [o, b.length]; };
  const patch = process.env.RETAG_MATRIX ? { primaries: 1, transfer: 13, matrix: +process.env.RETAG_MATRIX } : undefined;   // P5: matrix 9 so Chrome's YCbCr to RGB matches the RECOVER inverse
  const rb = annexB(relabelPacketSps(s.bl, size, patch).data), re = annexB(relabelPacketSps(s.el, size, patch).data);
  const elTypes = nalTypes(s.el);
  const a = { bl: push('bl', s.bl), el: push('el', s.el), key: irap };
  if (rasl) a.rasl = true;                                                   // leading pictures that reference the previous GOP
  if (elTypes.some(t => t === 8 || t === 9)) a.elRasl = true;
  a.elKey = elTypes.some(t => t >= 16 && t <= 21);                           // the EL has its own IRAP positions
  if (types.some(t => t === 21)) a.cra = true;
  const r = { bl: [off.rbl, rb.length], el: [off.rel, re.length], key: irap, rasl: a.rasl, elRasl: a.elRasl, cra: a.cra, elKey: a.elKey };
  pts.push(p.ptsUs);
  bufs.rbl.push(rb); off.rbl += rb.length; bufs.rel.push(re); off.rel += re.length;
  if (s.rpu) { a.rpu = r.rpu = [off.rpu, s.rpu.length]; bufs.rpu.push(s.rpu); off.rpu += s.rpu.length; } else noRpu++;
  if (s.rpus.length > 1) multiRpu++;
  plain.push(a); retag.push(r); n++; bytes += p.data.length;
}
const w = (f, parts) => fs.writeFileSync(out + f, Buffer.concat(parts));
w(`${name}-bl.hevc`, bufs.bl); w(`${name}-el.hevc`, bufs.el); w(`${name}-retag-bl.hevc`, bufs.rbl); w(`${name}-retag-el.hevc`, bufs.rel); w(`${name}-rpu.bin`, bufs.rpu);
// display[k] = decode order index of the k-th displayed picture (container pts rank), used to pick the RPU of display frame k
const display = pts.map((p, i) => [p, i]).sort((a, b) => a[0] - b[0]).map(x => x[1]);
fs.writeFileSync(out + `${name}-index.json`, JSON.stringify({ aus: plain, display }));
fs.writeFileSync(out + `${name}-retag-index.json`, JSON.stringify({ aus: retag, display }));
const first = d; 
console.log(JSON.stringify({ name, frames: n, keyframes: plain.filter(a => a.key).length, noRpu, multiRpu, droppedRasl, rasl: plain.filter(a => a.rasl).length, blNalTypes: blTypes, blBytes: off.bl, elBytes: off.el, packetBytes: bytes,
  mbps: +(bytes * 8 / (n * 1001 / 24000) / 1e6).toFixed(1), seconds: +(n * 1001 / 24000).toFixed(1) }));
