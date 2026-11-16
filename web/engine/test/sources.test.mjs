// SPDX-License-Identifier: LGPL-3.0-only
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { createSource, HttpRangeSource, HttpStreamSource, ReadInterrupted, STREAM_TAIL_PRELOAD } from '../sources.mjs';

const DATA = Uint8Array.from({ length: 5000 }, (_, i) => i & 255);
const transport = (url, range) => {
  const m = /bytes=(\d+)-(\d+)/.exec(range);
  const a = Number(m[1]), b = Math.min(Number(m[2]), DATA.length - 1);
  return { status: 206, contentRange: `bytes ${a}-${b}/${DATA.length}`, body: DATA.slice(a, b + 1).buffer };
};

test('HTTP source with an injected transport probes its length and reads blocks without a helper worker', async () => {
  const s = await createSource({ url: 'http://x/y', transport, blockSize: 1024 });
  assert.ok(s instanceof HttpRangeSource);
  assert.equal(s.length, DATA.length);
  assert.equal(s.useHelper, false);
  const out = new Uint8Array(100);
  assert.equal(s.read(1000, out), 100);
  assert.deepEqual(out, DATA.subarray(1000, 1100));
  s.close();
});

// The stream reader against a stand-in for its download: each start fills the ring from its offset at once.
const MB = 1 << 20, STREAM_LEN = 64 * MB, RING = 4 * MB;
const streamByte = i => (i ^ (i >>> 8) ^ (i >>> 16)) & 255;
function streamSource() {
  const s = new HttpStreamSource('http://x/y', { length: STREAM_LEN, ringBytes: RING });
  const starts = [], sideReads = [];
  const worker = {
    postMessage(m) {
      starts.push(m.off);
      const n = Math.min(RING, STREAM_LEN - m.off), ring = new Uint8Array(m.ring);
      for (let k = 0; k < n; k++) ring[(m.off + k) % RING] = streamByte(m.off + k);
      new BigInt64Array(m.ctrl, 16)[0] = BigInt(m.off + n);
      if (m.off + n >= STREAM_LEN) new Int32Array(m.ctrl, 0, 4)[2] = 1;
    },
  };
  const side = { read(off, dest) { sideReads.push(off); for (let k = 0; k < dest.length; k++) dest[k] = streamByte(off + k); return dest.length; }, close() {} };
  s.attach(worker, side).begin(0);
  return { s, starts, sideReads };
}
const readAt = (s, off, n) => { const d = new Uint8Array(n); assert.equal(s.read(off, d), n); for (let k = 0; k < n; k += 997) assert.equal(d[k], streamByte(off + k)); };

test('stream source serves sequential reads and small steps back from the ring without a new request', () => {
  const { s, starts } = streamSource();
  readAt(s, 0, 1000); readAt(s, 1000, 256 << 10); readAt(s, 500, 2000);
  assert.deepEqual(starts, [0]);
  assert.equal(s.stats().restarts, 0);
});

test('stream source restarts the download at a far seek, a little before the target', () => {
  const { s, starts } = streamSource();
  readAt(s, 20 * MB, 4096);
  assert.deepEqual(starts, [0, 20 * MB - 128 * 1024]);
  readAt(s, 20 * MB - 100 * 1024, 100);   // the demuxer stepping back after a seek
  assert.equal(starts.length, 2);
});

test('stream source keeps the first bytes of the file before the ring wraps over them', () => {
  // A download that keeps going: after each read it fills the ring up to one ring past the position the reader published.
  const s = new HttpStreamSource('http://x/y', { length: STREAM_LEN, ringBytes: RING });
  const starts = []; let m = null, end = 0;
  const fill = () => {
    const big = new BigInt64Array(m.ctrl, 16), ring = new Uint8Array(m.ring), to = Math.min(STREAM_LEN, Number(big[1]) + RING);
    for (; end < to; end++) ring[end % RING] = streamByte(end);
    big[0] = BigInt(end);
  };
  s.attach({ postMessage(x) { if (x.off === undefined) return; m = x; end = x.off; starts.push(x.off); fill(); } }, { read() { return 0; }, close() {} }).begin(0);
  for (let off = 0; off < 6 * MB; off += 256 << 10) { readAt(s, off, 256 << 10); fill(); }   // the ring has wrapped over byte 0
  readAt(s, 30 * MB, 4096); fill();                                                            // a far seek
  readAt(s, 0, 4096); readAt(s, MB, 4096);                                                     // the demuxer goes back to the header
  assert.deepEqual(starts, [0, 30 * MB - 128 * 1024]);
  assert.equal(s.stats().headReads, 2);
});

test('stream source reads file tail metadata on the side and keeps its download', () => {
  const { s, starts, sideReads } = streamSource();
  readAt(s, STREAM_LEN - 2 * MB, 4096);
  assert.deepEqual(starts, [0]);
  assert.deepEqual(sideReads, [STREAM_LEN - 2 * MB]);
  readAt(s, 100, 100);
  assert.deepEqual(starts, [0]);
});

test('stream source reports what it holds ahead of the reader and returns a short read at the end', () => {
  const { s } = streamSource();
  readAt(s, 0, MB);
  assert.equal(s.stats().ahead, RING - MB);
  assert.equal(s.stats().ended, false);
  const d = new Uint8Array(4096);
  const near = STREAM_LEN - 100;
  s.begin(near - MB);   // the download has reached the end of the file
  assert.equal(s.stats().ended, true);
  assert.equal(s.read(near, d), 100);
});

test('HTTP source keeps a 20 s default bound that callers can lower', () => {
  assert.equal(new HttpRangeSource('http://x/y', { transport, length: 10 }).timeoutMs, 20000);
  assert.equal(new HttpRangeSource('http://x/y', { transport, length: 10, timeoutMs: 5000 }).timeoutMs, 5000);
});

test('HTTP source serves sequential, backward and straddling reads from its block cache', async () => {
  const s = await createSource({ url: 'http://x/y', transport, blockSize: 1024, maxBlocks: 2 });
  const out = new Uint8Array(3000);
  assert.equal(s.read(0, out), 3000);
  assert.deepEqual(out, DATA.subarray(0, 3000));
  assert.equal(s.requests, 4);   // the length probe and blocks 0 to 2
  const tail = new Uint8Array(1500);
  assert.equal(s.read(DATA.length - 1000, tail), 1000);   // clipped at the end of the file
  assert.deepEqual(tail.subarray(0, 1000), DATA.subarray(DATA.length - 1000));
  assert.equal(s.read(10, out.subarray(0, 2500)), 2500);   // back to the start after eviction
  assert.deepEqual(out.subarray(0, 2500), DATA.subarray(10, 2510));
  assert.ok(s.blocks.size <= s.maxBlocks);
  s.close();
});

test('HTTP source sends caller headers on the length probe and on every block request', async () => {
  const seen = [];
  const s = await createSource({ url: 'http://x/y', headers: { Authorization: 'MediaBrowser Token="t"' }, blockSize: 1024, transport: (url, range, headers) => { seen.push(headers); return transport(url, range); } });
  s.read(2000, new Uint8Array(100));
  assert.ok(seen.length >= 2);
  for (const h of seen) assert.equal(h.Authorization, 'MediaBrowser Token="t"');
  s.close();
});

// A finished request, as the helper leaves it: status words, then the body after the 512 byte head.
function tailRequest(status, bytes) {
  const sab = new SharedArrayBuffer(512 + bytes.length), i32 = new Int32Array(sab);
  i32[0] = 1; i32[1] = status; i32[2] = bytes.length;
  new Uint8Array(sab, 512).set(bytes);
  return { sab, i32 };
}

test('stream source answers tail reads from the tail requested with the opening request', () => {
  const { s, starts, sideReads } = streamSource();
  const tail = Uint8Array.from({ length: STREAM_TAIL_PRELOAD }, (_, k) => streamByte(STREAM_LEN - STREAM_TAIL_PRELOAD + k));
  s.tail = { e: tailRequest(206, tail), data: null, start: 0 };
  s.side.helperWait = () => 1;
  readAt(s, STREAM_LEN - 4096, 4096);
  assert.deepEqual(sideReads, []);
  assert.deepEqual(starts, [0]);
  readAt(s, STREAM_LEN - STREAM_TAIL_PRELOAD - 8192, 4096);   // before the preloaded window: block cache
  assert.deepEqual(sideReads, [STREAM_LEN - STREAM_TAIL_PRELOAD - 8192]);
});

test('a tail request that did not come as 206 falls back to the block cache', () => {
  const { s, sideReads } = streamSource();
  s.tail = { e: tailRequest(200, new Uint8Array(STREAM_TAIL_PRELOAD)), data: null, start: 0 };
  s.side.helperWait = () => 1;
  readAt(s, STREAM_LEN - 4096, 4096);
  assert.deepEqual(sideReads, [STREAM_LEN - 4096]);
  assert.equal(s.tail, null);
});

test('a read that waits for the download ends when its owner interrupts it, and the source reads on afterwards', () => {
  // a download that delivers the first megabyte and then nothing until told to
  const s = new HttpStreamSource('http://x/y', { length: STREAM_LEN, ringBytes: RING });
  let fill = null;
  const worker = { postMessage(m) {
    const ring = new Uint8Array(m.ring), end = new BigInt64Array(m.ctrl, 16);
    fill = upTo => { for (let k = Number(end[0]); k < upTo; k++) ring[k % RING] = streamByte(k); end[0] = BigInt(upTo); };
    end[0] = BigInt(m.off); fill(m.off + MB);
  } };
  s.attach(worker, { read() { throw new Error('no side read expected'); }, close() {} }).begin(0);
  readAt(s, 0, 4096);
  let asked = 0, stop = false;
  s.interrupt = () => { asked++; return stop; };
  readAt(s, 4096, 4096);                       // data that is there never asks
  assert.equal(asked, 0);
  stop = true;
  assert.throws(() => s.read(MB - 100, new Uint8Array(4096)), ReadInterrupted);   // 100 bytes are there, the rest is not
  assert.ok(asked > 0);
  assert.equal(s.stats().restarts, 0);
  // the download went on meanwhile: the same read now succeeds, from the same request
  stop = false; fill(2 * MB);
  readAt(s, MB - 100, 4096);
  assert.equal(s.stats().restarts, 0);
});
