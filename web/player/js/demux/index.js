// SPDX-License-Identifier: LGPL-3.0-only
// Demux contract. The Rust/WASM demuxer implements the same shape:
//
//   interface Demuxer { tracks(): Track[]; seek(timeUs): void; nextPacket(): Packet | null }   (the Rust one is opened asynchronously)
//   Track  { id, kind: 'video'|'audio'|'subtitle', codec (WebCodecs codec string or
//            'truehd'|'dts'|'eac3'|'ac3'|'flac'|'pcm'), description?: Uint8Array, width?, height?,
//            sampleRate?, channels?, timescale }       (this interim demuxer also fills color?)
//   Packet { trackId, ptsUs, dtsUs, durationUs, key: boolean, data: Uint8Array }
//
// seek(timeUs) positions every track so that nextPacket() resumes at or before timeUs (video: the last
// sync sample at or before it). nextPacket() returns null at end of stream. The Uint8Array in a packet is
// owned by the caller (the demux worker transfers its buffer).
// Optional: durationUs (number), close().
//
// The Rust/WASM demuxer (web/engine/demuxer.mjs: MKV, MP4/MOV/fMP4, MPEG-TS / M2TS, raw audio) is the default and is
// opened by demux-worker.js. createJsDemuxer below is the interim JS MP4 demuxer, kept only as the fallback selected
// with ?demux=js.
import { Mp4Demuxer } from './mp4.js';

export function createJsDemuxer(source) {
  const head = source.read(0, 12);
  const tag = String.fromCharCode(head[4], head[5], head[6], head[7]);
  if (['ftyp', 'moov', 'styp', 'mdat', 'free', 'moof', 'skip', 'wide'].includes(tag)) return new Mp4Demuxer(source);
  throw new Error('the interim JS demuxer only reads MP4/MOV (use the default Rust demuxer for other containers)');
}
