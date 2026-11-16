// SPDX-License-Identifier: LGPL-3.0-only
// Synchronous byte sources for the Rust demuxer (`aether_host_read` must answer synchronously).
//
//   Blob/File in a Worker  -> FileReaderSync (no copy of the whole file)
//   {url} in a Worker      -> one continuous download into a shared ring (HttpStreamSource) where SharedArrayBuffer
//                             exists; otherwise Range requests into a block cache (HttpRangeSource: a helper Worker
//                             fetches, this thread blocks in Atomics.wait; synchronous XHR as the last fallback)
//   Uint8Array / ArrayBuffer, Blob off a Worker, Node Blob -> whole file in memory
//   {path, maxBytes?} in Node -> fs.readSync (maxBytes exposes only a leading window of a big file)
//
// Every source exposes `length` and `read(offset, dest: Uint8Array) -> bytes written`.

const IS_WORKER = typeof WorkerGlobalScope !== 'undefined' && typeof self !== 'undefined' && self instanceof WorkerGlobalScope;
export const sourceTrace = { mark: null };   // set by the demux worker: named points of the opening timeline
const HEAD = 512;   // bytes of a request's shared buffer before the body: status words, then the Content-Range text at byte 64
const IS_NODE = typeof process !== 'undefined' && !!process.versions?.node && !IS_WORKER;

export class MemorySource {
  constructor(bytes) {
    this.bytes = bytes instanceof ArrayBuffer ? new Uint8Array(bytes) : bytes;
    this.length = this.bytes.byteLength;
  }
  read(offset, dest) {
    const n = Math.max(0, Math.min(dest.length, this.length - offset));
    if (n > 0) dest.set(this.bytes.subarray(offset, offset + n));
    return n;
  }
  close() {}
}

export class BlobSyncSource {
  constructor(blob) {
    this.blob = blob;
    this.length = blob.size;
    this.reader = new FileReaderSync();
  }
  read(offset, dest) {
    const n = Math.max(0, Math.min(dest.length, this.length - offset));
    if (n === 0) return 0;
    const buf = this.reader.readAsArrayBuffer(this.blob.slice(offset, offset + n));
    dest.set(new Uint8Array(buf));
    return buf.byteLength;
  }
  close() {}
}

export class NodeFileSource {
  constructor(path, fs, maxBytes = Infinity) {
    this.fs = fs;
    this.fd = fs.openSync(path, 'r');
    this.length = Math.min(fs.fstatSync(this.fd).size, maxBytes);
  }
  read(offset, dest) {
    let total = 0;
    const want = Math.max(0, Math.min(dest.length, this.length - offset));
    while (total < want) {
      const n = this.fs.readSync(this.fd, dest, total, want - total, offset + total);
      if (n === 0) break;
      total += n;
    }
    return total;
  }
  close() { try { this.fs.closeSync(this.fd); } catch { /* already closed */ } }
}

// Runs in the helper Worker: one range fetch per message, aborted by a timer that covers the body as well. Requests overlap (a
// prefetch is in flight while the next read is served), each with its own shared buffer. The body is written into the shared buffer
// chunk by chunk, with no intermediate ArrayBuffer.
// State word 0 of the shared buffer: 0 pending, 1 done, 2 failed, 3 timed out.
const BLOCK_FETCH = `
async function blockFetch(m) {
  const i32 = new Int32Array(m.sab), u8 = new Uint8Array(m.sab);
  const ac = new AbortController();
  const timer = setTimeout(() => ac.abort(), m.timeoutMs);
  let state = 2;
  try {
    const r = await fetch(m.url, { headers: { ...m.headers, Range: m.range }, signal: ac.signal });
    i32[1] = r.status;
    const cr = r.headers.get('Content-Range') || '';
    const crb = new TextEncoder().encode(cr).slice(0, 400);
    u8.set(crb, 64); i32[3] = crb.length;
    if (r.status === 206) {
      const rd = r.body.getReader();
      let off = 0, fits = true;
      for (;;) {
        const { done, value } = await rd.read();
        if (done) break;
        if (off + value.length > m.cap) { fits = false; try { await rd.cancel(); } catch {} break; }
        u8.set(value, m.head + off); off += value.length;
      }
      if (fits) { i32[2] = off; state = 1; }
    } else { try { await r.body?.cancel(); } catch {} i32[2] = 0; state = 1; }
  } catch (err) { state = ac.signal.aborted ? 3 : 2; }
  clearTimeout(timer);
  Atomics.store(i32, 0, state); Atomics.notify(i32, 0);
  self.postMessage('done');
}
`;
const HELPER_SOURCE = BLOCK_FETCH + `
self.postMessage('ready');
self.onmessage = e => {
  const m = e.data;
  if (m.quit) { self.close(); return; }
  blockFetch(m);
};
`;

/**
 * HTTP Range source for Workers. The Rust demuxer reads in small windows (256 KiB) at a mostly
 * sequential pace, so a block cache of `blockSize` aligned blocks with LRU eviction turns that into
 * few requests. `transport` replaces the network. Blocks come from a helper Worker's fetch while this thread blocks in
 * Atomics.wait (synchronous XHR without SharedArrayBuffer), which is why this only works in a Worker.
 */
export class HttpRangeSource {
  constructor(url, { blockSize = 1024 * 1024, maxBlocks = 32, headers = {}, length = null, transport = null, timeoutMs = 20000, helper = true, prefetch = 2, sharedHelper = null } = {}) {
    if (!transport) {
      if (typeof XMLHttpRequest === 'undefined') throw new Error('HTTP sources need XMLHttpRequest (browser Worker)');
      if (!IS_WORKER) throw new Error('openDemuxer({url}) must run inside a dedicated Worker (synchronous XHR is not allowed on the main thread)');
    }
    this.transport = transport;
    this.timeoutMs = timeoutMs;
    this.useHelper = !transport && helper && typeof SharedArrayBuffer !== 'undefined' && typeof Worker !== 'undefined' && typeof Blob !== 'undefined' && typeof URL?.createObjectURL === 'function';
    this.helperWorker = sharedHelper;   // already warm (its owner runs the first fetch): no boot and no warm-up request here
    this.ownsHelper = !sharedHelper;
    this.url = String(url);
    this.blockSize = blockSize;
    this.maxBlocks = Math.max(maxBlocks, prefetch + 3);   // the block in use and the prefetched ones must never evict each other
    this.headers = headers;
    this.blocks = new Map(); // block index -> {data: Uint8Array | null (still in flight), sab, i32, deadline}; insertion order = LRU order
    this.prefetchDepth = prefetch;
    this.lastBlock = -2;     // the block the previous read ended in: a read that continues from it is sequential
    this.requests = 0;
    this.bytesFetched = 0;
    this.length = length;
    // With the helper, the length probe waits for start() (the helper cannot boot while this thread blocks); otherwise probe now.
    if (this.length == null && !this.useHelper) this.length = this.probeLength();
  }

  /** Boots the helper Worker (it cannot start while this thread is blocked in Atomics.wait) and probes the length. Without SharedArrayBuffer or Worker it only probes. */
  async start() {
    if (this.useHelper) {
      try {
        const w = new Worker(URL.createObjectURL(new Blob([HELPER_SOURCE], { type: 'text/javascript' })));
        await new Promise((resolve, reject) => {
          const t = setTimeout(() => reject(new Error('helper boot timeout')), 5000);
          w.onmessage = () => { clearTimeout(t); resolve(); };
          w.onerror = e => { clearTimeout(t); reject(new Error(e.message || 'helper error')); };
        });
        // The first fetch of a nested Worker needs this thread's event loop (measured: a blocking wait on it hangs), so one request
        // is awaited asynchronously here before any blocking read happens.
        const sab = new SharedArrayBuffer(512 + 64);
        const done = new Promise((resolve, reject) => {
          const t = setTimeout(() => reject(new Error('helper warm-up timeout')), this.timeoutMs + 2000);
          w.onmessage = () => { clearTimeout(t); resolve(); };
        });
        w.postMessage({ sab, url: this.url, range: 'bytes=0-0', headers: this.headers, timeoutMs: this.timeoutMs, cap: 64, head: 512 });
        await done;
        w.onmessage = w.onerror = null;
        this.helperWorker = w;
      } catch (e) { this.useHelper = false; this.helperError = String(e && e.message || e); }   // sync XHR (with xhr.timeout where honoured) is the fallback
    }
    if (this.length == null) this.length = this.probeLength();
    return this;
  }

  xhr(range) {
    if (this.transport) {
      // Injected synchronous transport (tests, other runtimes): (url, range, headers) -> {status, contentRange, body}
      this.requests++;
      const r = this.transport(this.url, range, this.headers);
      return { status: r.status, response: r.body, getResponseHeader: () => r.contentRange ?? null };
    }
    if (this.useHelper && this.helperWorker) {
      // Only the length probe comes through here with the helper on (blocks go through fetchBlock): a few bytes, a small buffer.
      const e = this.helperPost(range, 64, new SharedArrayBuffer(HEAD + 64));
      const r = this.helperWait(e);
      this.requests++;
      const u8 = new Uint8Array(e.sab);
      const contentRange = e.i32[3] ? new TextDecoder().decode(u8.slice(64, 64 + e.i32[3])) : null;
      return { status: e.i32[1], response: u8.slice(HEAD, HEAD + e.i32[2]).buffer, getResponseHeader: () => contentRange };
    }
    const x = new XMLHttpRequest();
    x.open('GET', this.url, false);
    try { x.timeout = this.timeoutMs; } catch { /* not settable for sync XHR in some engines; the helper path is the reliable bound */ }
    x.responseType = 'arraybuffer';
    for (const [k, v] of Object.entries(this.headers)) x.setRequestHeader(k, v);
    x.setRequestHeader('Range', range);
    try { x.send(); } catch (e) { if (this.helperError) throw new Error(`${e.message} (helper off: ${this.helperError})`); throw e; }
    this.requests++;
    return x;
  }

  /**
   * Bounded blocking requests. A synchronous XHR in a Worker can ignore xhr.timeout (a server that stalls after the first byte then
   * blocks the thread for the browser's own limit, 60 s in Chrome), so the fetch runs in a helper Worker with an AbortController and a
   * timer, and this thread blocks on Atomics.wait over a SharedArrayBuffer with a bound of its own. Each block owns one buffer for as
   * long as it is cached, so a block is a view on it (no per-request allocation, no copy out of it) and the buffer is recycled when
   * the block is evicted.
   */
  helperPost(range, cap, sab) {
    const i32 = new Int32Array(sab);
    Atomics.store(i32, 0, 0);
    this.helperWorker.postMessage({ sab, url: this.url, range, headers: this.headers, timeoutMs: this.timeoutMs, cap, head: HEAD });
    return { sab, i32, range, deadline: performance.now() + this.timeoutMs + 2000, data: null };
  }

  // Waits for a posted request; throws on a timeout or a failed request.
  helperWait(e) {
    const left = Math.max(1, e.deadline - performance.now());
    const r = Atomics.wait(e.i32, 0, 0, left);
    const state = Atomics.load(e.i32, 0);
    if (r === 'timed-out' || state === 0) {
      this.dropHelper();   // the helper itself is stuck: it cannot be trusted with another request
      throw new Error(`HTTP source: no response within ${Math.round(this.timeoutMs / 1000)} s (${e.range})`);
    }
    // 3: the helper's own timer aborted the fetch; 2: network error. The helper stays usable for the next request.
    if (state === 3) throw new Error(`HTTP source: no response within ${Math.round(this.timeoutMs / 1000)} s (${e.range})`);
    if (state === 2) throw new Error(`HTTP source: request failed (${e.range})`);
    return state;
  }

  // Starts the fetch of block i into a recycled (or new) buffer without waiting for it.
  fetchBlock(i, prefetched = false) {
    let sab = null;
    if (this.blocks.size >= this.maxBlocks) {
      // Evict the least recently used block that is done; its buffer is the one reused.
      // (a prefetch that finished unawaited counts as done: nobody else will ever release its buffer)
      // never the block being served: read() has not copied it yet (prefetches left in flight by a seek can make it the oldest done one)
      for (const [k, v] of this.blocks) if (k !== this.serving && (v.data || Atomics.load(v.i32, 0) !== 0)) { this.blocks.delete(k); sab = v.sab; break; }
    }
    sab ??= new SharedArrayBuffer(HEAD + this.blockSize + 16);
    const start = i * this.blockSize;
    const end = Math.min(this.length, start + this.blockSize) - 1;
    const e = this.helperPost(`bytes=${start}-${end}`, this.blockSize + 16, sab);
    e.prefetched = prefetched;
    this.blocks.set(i, e);
    return e;
  }

  // The block's bytes once its request has finished.
  finishBlock(e) {
    if (e.data) return e.data;
    this.helperWait(e);
    if (e.i32[1] !== 206) { for (const [k, v] of this.blocks) if (v === e) this.blocks.delete(k); throw new Error(`HTTP source: range ${e.range} gave status ${e.i32[1]}`); }
    e.data = new Uint8Array(e.sab, HEAD, e.i32[2]);
    this.bytesFetched += e.data.length;
    this.requests++;
    return e.data;
  }

  dropHelper() {
    for (const [k, v] of this.blocks) if (!v.data && v.sab) this.blocks.delete(k);   // in flight: never completes
    const w = this.helperWorker; this.helperWorker = null;
    if (w && this.ownsHelper) { try { w.postMessage({ quit: true }); } catch { /* gone */ } w.terminate(); }
  }

  probeLength() {
    const x = this.xhr('bytes=0-0');
    if (x.status === 206) {
      const m = /\/(\d+)\s*$/.exec(x.getResponseHeader('Content-Range') || '');
      if (m) return Number(m[1]);
      throw new Error('HTTP source: 206 without a usable Content-Range');
    }
    if (x.status === 200) throw new Error('HTTP source: server ignored the Range header');
    throw new Error(`HTTP source: status ${x.status}`);
  }

  block(i) {
    this.serving = i;
    const hit = this.blocks.get(i);
    if (hit) {
      this.blocks.delete(i); this.blocks.set(i, hit);
      try {
        const data = hit.data ?? this.finishBlock(hit);
        this.afterBlock(i);
        return data;
      } catch (err) {
        // A prefetch that failed (a network blip nobody was waiting for) gets one fresh request; a failed demand read is final.
        this.blocks.delete(i);
        if (!hit.prefetched || !this.helperWorker) throw err;
      }
    }
    if (this.useHelper && this.helperWorker) {
      const data = this.finishBlock(this.fetchBlock(i));
      this.afterBlock(i);
      return data;
    }
    const start = i * this.blockSize;
    const end = Math.min(this.length, start + this.blockSize) - 1;
    const x = this.xhr(`bytes=${start}-${end}`);
    if (x.status !== 206) throw new Error(`HTTP source: range ${start}-${end} gave status ${x.status}`);
    const b = new Uint8Array(x.response);
    this.bytesFetched += b.length;
    this.blocks.set(i, { data: b });
    while (this.blocks.size > this.maxBlocks) this.blocks.delete(this.blocks.keys().next().value);
    this.lastBlock = i;
    return b;
  }

  // Sequential reads (the demuxer's usual pace) get the next blocks requested while the current one is being parsed, so the
  // blocking wait on a block shrinks to the part the network had not finished.
  afterBlock(i) {
    const sequential = i === this.lastBlock || i === this.lastBlock + 1;
    this.lastBlock = i;
    if (!sequential || !this.helperWorker) return;
    for (let k = 1; k <= this.prefetchDepth; k++) {
      const n = i + k;
      if (n * this.blockSize >= this.length) break;
      if (!this.blocks.has(n)) this.fetchBlock(n, true);
    }
  }

  read(offset, dest) {
    let done = 0;
    while (done < dest.length && offset + done < this.length) {
      const pos = offset + done;
      const b = this.block(Math.floor(pos / this.blockSize));
      const within = pos % this.blockSize;
      const n = Math.min(dest.length - done, b.length - within);
      if (n <= 0) break;
      dest.set(b.subarray(within, within + n), done);
      done += n;
    }
    return done;
  }
  close() { this.blocks.clear(); this.dropHelper(); }
}

// Remote media as one continuous download into a ring, the way a native player reads it. A separate request per block pays the
// first-byte wait of a distant CDN (300 to 700 ms measured) every time: on a simulated CDN with 500 ms and a 90 Mbit/s link the
// 1 MiB blocks delivered 2 MiB/s (16 Mbit/s) and a 73 Mbit/s remux played a third of real time. The ring also is the network cushion:
// it fills ahead of the demuxer, so demux reads return at once and only an empty ring blocks. The ring is 64 MiB, a quarter of it
// kept behind the reader: the whole ring ends up resident, and at 256 MiB the page renderer measured 800 to 808 MB on a 4K HEVC
// remux against 283 MB for the plain element on the same file. 48 MiB ahead is 5 s of an 80 Mbit/s remux and 20 s of a 20 Mbit/s
// file; the element route buffers a further 20 s of its own in the SourceBuffer. A read outside what the ring holds restarts the download there (a seek); metadata in the last megabytes of the file
// (Matroska cues, an MP4 moov) goes through a small block cache so that reading it does not throw the download away.
const STREAM_TAIL_BYTES = 32 << 20;
export const STREAM_RING_BYTES = 64 << 20;
const STREAM_KEEP_BEHIND = 0.25;        // share of the ring kept behind the reader, so a short step back needs no new request (a restart on the CDN measured 1 to 5.6 s)
const STREAM_FORWARD_GAP = 8 << 20;     // a jump this small is waited for instead of restarting the download
export const STREAM_TAIL_PRELOAD = 1 << 20;    // one request for the end of the file: Matroska Cues and an MP4 moov usually sit in the last megabytes
const STREAM_HEAD_KEEP = 2 << 20;       // of the file's first bytes, copied once before the download can overwrite them or moves away from them
const STREAM_BACK_MARGIN = 128 << 10;  // a restart begins this far before the read: demuxers step back a little after a seek. 1 MiB here cost 0.4 to 1.2 s (the CDN starts slowly: 8 to 25 Mbit/s for the first megabyte)
const STREAM_WRITE_END = 0, STREAM_READ_POS = 1, STREAM_START = 2;   // BigInt64 slots: end of the data, the reader's position, where this download began
const STREAM_TICK = 0, STREAM_GEN = 1, STREAM_STATE = 2;             // Int32 slots: bumped on every write, the download's generation, its state
const STREAM_RUNNING = 0, STREAM_ENDED = 1, STREAM_FAILED = 2;

// Runs in a helper Worker. A connection that breaks or stalls is resumed where it stopped (the buffered bytes stay); only attempts
// without any progress count against the limit. The end of the data is published with a compare-exchange, so a download superseded by
// a seek in the middle of a write cannot move the new one's end.
const STREAM_HELPER = BLOCK_FETCH + `
let ac = null, fillLimit = 0;
self.postMessage('ready');
self.onmessage = async e => {
  const m = e.data;
  if (m.quit) { ac?.abort(); self.close(); return; }
  if (m.setLimit !== undefined) { fillLimit = m.setLimit; return; }
  if (m.sab) { blockFetch(m); return; }
  if (m.limit !== undefined) fillLimit = m.limit;
  ac?.abort();
  const big = new BigInt64Array(m.ctrl, 16), i32 = new Int32Array(m.ctrl, 0, 4), ring = new Uint8Array(m.ring), R = ring.length;
  const live = () => Atomics.load(i32, 1) === m.gen;
  // When this download's response and its first body bytes came (absolute ms), for the seek timeline. Slot 0 is the reader's: the request.
  const times = new Float64Array(m.ctrl, 40, 3), stamp = k => { if (live() && !times[k]) times[k] = performance.timeOrigin + performance.now(); };
  const finish = s => { if (live()) { Atomics.store(i32, 2, s); Atomics.add(i32, 0, 1); Atomics.notify(i32, 0); } };
  let end = m.off, told = false, quick = 0, stalls = 0, total = m.length, url = m.url, headers = m.headers, probed = !m.probe;
  const tell = () => { if (!told) { told = true; self.postMessage('started'); } };
  let linkT = 0, linkB = 0;   // i32[3]: the link in kbit/s, bytes since the first ones of this attempt (not its wait for the first byte, not while the ring is full)
  while (live()) {
    linkT = 0;
    const before = end, attempt = ac = new AbortController();
    let stalled = false, timer = 0;
    const arm = () => { clearTimeout(timer); timer = setTimeout(() => { stalled = true; attempt.abort(); }, m.timeoutMs); };
    const drop = rd => { clearTimeout(timer); rd.cancel().catch(() => {}); };
    try {
      arm();
      const r = await fetch(url, { headers: { ...headers, Range: 'bytes=' + end + '-' }, signal: attempt.signal });
      stamp(1);
      if (!probed) {
        // The opening request doubles as the size probe: the file size comes from Content-Range, or from the Content-Length of this open-ended range when a CDN hides Content-Range.
        probed = true;
        const cr = Number((r.headers.get('Content-Range') || '').split('/').pop());
        total = r.status === 206 ? cr || Number(r.headers.get('Content-Length')) || m.sizeHint || 0 : 0;
        const landed = r.redirected && r.url ? r.url : null;
        self.postMessage({ head: { status: r.status, size: total, url: landed } });
        if (!total) { clearTimeout(timer); try { await r.body?.cancel(); } catch {} return; }
        if (landed) { url = landed; if (new URL(landed).origin !== new URL(m.url).origin) headers = {}; }
      }
      if (r.status === 416) { clearTimeout(timer); try { await r.body?.cancel(); } catch {} tell(); finish(1); return; }
      if (r.status !== 206) { try { await r.body?.cancel(); } catch {} throw 'fatal'; }
      tell();
      const rd = r.body.getReader();
      for (;;) {
        const { done, value } = await rd.read();
        arm();
        if (!live()) { drop(rd); return; }
        if (done) break;
        stamp(2);
        let v = value;
        while (v.length) {
          const room = (fillLimit ? Math.min(R, fillLimit) : R) - (end - Number(Atomics.load(big, 1)));
          if (room <= 0) {   // ring full: not reading holds the server back through the connection's own flow control
            clearTimeout(timer); linkT = 0;
            await new Promise(res => setTimeout(res, 20));
            if (!live()) { drop(rd); return; }
            arm();
            continue;
          }
          const n = Math.min(v.length, room, R - (end % R));
          ring.set(v.subarray(0, n), end % R);
          const prev = BigInt(end);
          end += n; v = v.subarray(n);
          const now = performance.now();
          if (!linkT) { linkT = now; linkB = end; } else if (now - linkT >= 400) { Atomics.store(i32, 3, Math.round((end - linkB) * 8 / (now - linkT))); if (now - linkT >= 5000) { linkT = now; linkB = end; } }
          if (Atomics.compareExchange(big, 0, prev, BigInt(end)) !== prev) { drop(rd); return; }
          Atomics.add(i32, 0, 1); Atomics.notify(i32, 0);
        }
      }
      clearTimeout(timer);
      if (end >= total) { finish(1); return; }
    } catch (err) {
      clearTimeout(timer);
      if (!probed) { self.postMessage({ head: { status: 0 } }); return; }
      if (err === 'fatal') { tell(); finish(2); return; }
    }
    if (!live()) return;
    if (end > before) { quick = 0; stalls = 0; } else if (stalled) stalls++; else quick++;
    if (stalls >= 2 || quick >= 4) { tell(); finish(2); return; }
    await new Promise(res => setTimeout(res, 200 * (quick + 1)));
  }
};
`;

/** Thrown out of a read that was waiting for the download when the reader's owner asked for the wait to end (a seek). */
export class ReadInterrupted extends Error {
  constructor() { super('read interrupted'); this.name = 'ReadInterrupted'; }
}

export class HttpStreamSource {
  constructor(url, { length, headers = {}, ringBytes = STREAM_RING_BYTES, timeoutMs = 20000 } = {}) {
    this.url = String(url); this.length = length; this.headers = headers; this.R = ringBytes; this.keepBehind = Math.floor(ringBytes * STREAM_KEEP_BEHIND); this.timeoutMs = timeoutMs;
    this.restarts = 0; this.waitMs = 0; this.slowWaits = 0; this.pos = 0; this.sideReads = 0; this.head = null; this.headReads = 0;
    // Set by the owner to a function that says whether a read still waiting for data should give up (see #waitFor).
    this.interrupt = null;
  }

  // The shared state; separate from start() so a test can drive the reader with a stand-in for the download.
  attach(worker, side) {
    this.ctrl = new SharedArrayBuffer(16 + 3 * 8 + 3 * 8);
    this.ringBuf = new SharedArrayBuffer(this.R);
    this.i32 = new Int32Array(this.ctrl, 0, 4);
    this.big = new BigInt64Array(this.ctrl, 16, 3);
    this.times = new Float64Array(this.ctrl, 40, 3);   // request, response, first body bytes of the current download (absolute ms)
    this.ring = new Uint8Array(this.ringBuf);
    this.worker = worker; this.side = side;
    return this;
  }

  begin(off) { this.#begin(off); }

  /**
   * One request instead of a size probe, a helper warm-up and a separate start of the download: the open-ended range that starts the
   * download also says how big the file is (the file's tail, Cues or moov, is asked for later by preloadTail). Over a CDN with 500 ms
   * to the first byte that is 1.5 s less before the first header byte. Anything unusual (no 206, no size, CORS, a timeout) returns null and
   * the caller takes the probing path, which reports the reason.
   */
  static async open(url, { headers = {}, size = null, ringBytes = STREAM_RING_BYTES, timeoutMs = 20000, fillLimit = 0 } = {}) {
    let w;
    try { w = new Worker(URL.createObjectURL(new Blob([STREAM_HELPER], { type: 'text/javascript' }))); } catch { return null; }
    const s = new HttpStreamSource(url, { length: 0, headers, ringBytes, timeoutMs });
    s.attach(w, new HttpRangeSource(url, { length: 0, headers, blockSize: 1 << 20, maxBlocks: 16, prefetch: 3, timeoutMs, sharedHelper: w }));
    const head = new Promise(resolve => {
      const t = setTimeout(() => resolve(null), timeoutMs + 2000);
      w.onmessage = e => { if (e.data?.head) { clearTimeout(t); resolve(e.data.head); } };
      w.onerror = () => { clearTimeout(t); resolve(null); };
    });
    s.fillCapped = fillLimit > 0;
    s.#begin(0, { probe: true, sizeHint: size, limit: fillLimit });
    sourceTrace.mark?.('open-request');
    const h = await head;
    if (!h || h.status !== 206 || !h.size) { s.close(); return null; }
    sourceTrace.mark?.('open-response');
    w.onmessage = w.onerror = null;
    s.length = s.side.length = h.size;
    if (h.url) {
      s.url = s.side.url = h.url;
      if (new URL(h.url).origin !== new URL(url).origin) s.headers = s.side.headers = {};
    }
    return s;
  }

  async start() {
    const w = new Worker(URL.createObjectURL(new Blob([STREAM_HELPER], { type: 'text/javascript' })));
    this.attach(w, null);
    await new Promise((resolve, reject) => {
      const t = setTimeout(() => reject(new Error('stream helper boot timeout')), 5000);
      w.onmessage = () => { clearTimeout(t); resolve(); };
      w.onerror = e => { clearTimeout(t); reject(new Error(e.message || 'stream helper error')); };
    });
    sourceTrace.mark?.('stream-helper-boot');
    // The first fetch of a nested Worker needs this thread's event loop (see HttpRangeSource.start), so both first fetches are awaited.
    const started = new Promise(resolve => { w.onmessage = () => resolve(); });
    this.#begin(0);
    const side = new HttpRangeSource(this.url, { length: this.length, headers: this.headers, blockSize: 1 << 20, maxBlocks: 16, prefetch: 1, timeoutMs: this.timeoutMs }).start();
    started.then(() => sourceTrace.mark?.('stream-first-response'));
    const [s] = await Promise.all([side, started]);
    sourceTrace.mark?.('side-helper-warm');
    this.side = s;
    w.onmessage = w.onerror = null;
    return this;
  }

  #begin(off, extra = null) {
    const gen = Atomics.add(this.i32, STREAM_GEN, 1) + 1;
    Atomics.store(this.i32, STREAM_STATE, STREAM_RUNNING);
    Atomics.store(this.big, STREAM_START, BigInt(off));
    Atomics.store(this.big, STREAM_READ_POS, BigInt(off));
    Atomics.store(this.big, STREAM_WRITE_END, BigInt(off));
    this.times[1] = this.times[2] = 0; this.times[0] = performance.timeOrigin + performance.now();
    this.worker.postMessage({ gen, off, length: this.length, url: this.url, headers: this.headers, ctrl: this.ctrl, ring: this.ringBuf, timeoutMs: this.timeoutMs, ...extra });
  }

  #end() { return Number(Atomics.load(this.big, STREAM_WRITE_END)); }
  #low() { return Math.max(Number(Atomics.load(this.big, STREAM_START)), this.#end() - this.R); }

  read(offset, dest) {
    const want = Math.min(dest.length, this.length - offset);
    if (want <= 0) return 0;
    if (offset < this.#low() || offset > this.#end() + STREAM_FORWARD_GAP) {
      if (this.head && offset + want <= this.head.length) { dest.set(this.head.subarray(offset, offset + want), 0); this.headReads++; return want; }
      if (offset >= this.length - STREAM_TAIL_BYTES) {
        this.sideReads++;
        this.preloadTail();
        const t = this.#tailData();
        if (t && offset >= t.start && offset + want <= t.start + t.data.length) { dest.set(t.data.subarray(offset - t.start, offset - t.start + want), 0); return want; }
        return this.side.read(offset, dest);
      }
      this.#keepHead();
      this.restarts++;
      if (this.restarts <= 32) sourceTrace.mark?.(`restart ${(offset / 1e9).toFixed(3)}G low ${(this.#low() / 1e9).toFixed(3)} end ${(this.#end() / 1e9).toFixed(3)}`);
      this.#begin(Math.max(0, offset - STREAM_BACK_MARGIN));
    }
    // Under a read-ahead cap the download counts from here, so what is kept behind must not use up the cap: a reader 16 MiB past a restart would wait for ever
    const readPos = Math.max(this.#low(), offset - (this.fillCapped ? 0 : this.keepBehind));
    // The download may write up to one ring past this position. Once that passes the ring's size the file's first bytes are
    // overwritten, so their copy is taken now (with a 64 MiB ring that is 16 MiB into a file, long before the first seek).
    if (readPos > 0) this.#keepHead();
    Atomics.store(this.big, STREAM_READ_POS, BigInt(readPos));
    this.pos = offset + want;
    if (this.#end() < offset + want) this.#waitFor(offset + want);
    const n = Math.min(want, this.#end() - offset);
    if (n <= 0) return 0;
    const R = this.R, at = offset % R, first = Math.min(n, R - at);
    dest.set(this.ring.subarray(at, at + first), 0);
    if (n > first) dest.set(this.ring.subarray(0, n - first), first);
    // A read behind the last one starts below the position the download was told to keep: it may have been overwritten while
    // it was copied. Then the bytes are fetched again (the ring no longer holds them, so this restarts the download there).
    if (offset < this.#low()) return this.read(offset, dest);
    // Anything but Matroska may keep what it needs to start at the end of the file (an MP4 moov), so its tail is asked for with the
    // first bytes. Matroska starts from the head alone and only a seek reads its Cues: there the tail waits for preloadTail().
    if (offset === 0 && n >= 4 && !this.tailAsked && !(dest[0] === 0x1a && dest[1] === 0x45 && dest[2] === 0xdf && dest[3] === 0xa3)) this.preloadTail();
    return n;
  }

  // The first bytes of the file survive the first restart. A Matroska file with attachments in front of its first cluster (10 MB of
  // subtitle fonts measured) makes the header parse jump over them, which restarts the download there; the demuxer then goes back to
  // the header when playback is positioned, and without this copy that was two more restarts (0.56 to 0.66 s each on a distant server).
  #keepHead() {
    if (this.head || Number(Atomics.load(this.big, STREAM_START)) !== 0 || this.#end() > this.R) return;
    const n = Math.min(this.#end(), STREAM_HEAD_KEEP, this.R);
    if (n > 0) this.head = this.ring.slice(0, n);
  }

  // The file tail in one request of its own, at most once. An explicit range: a suffix range is not CORS-safelisted (a preflight,
  // and servers that do not list Range refuse it). The first frame must not wait for it or share the link with it unless the
  // container needs it to start, so the player calls this once playback has its first packet, and a read in the tail calls it.
  preloadTail() {
    const size = this.length;
    if (this.tailAsked || !size) return;
    this.tailAsked = true;
    if (size <= STREAM_TAIL_PRELOAD || typeof this.side?.helperPost !== 'function') return;
    this.tail = { e: this.side.helperPost(`bytes=${size - STREAM_TAIL_PRELOAD}-${size - 1}`, STREAM_TAIL_PRELOAD + 16, new SharedArrayBuffer(HEAD + STREAM_TAIL_PRELOAD + 16)), data: null, start: 0 };
  }

  // The tail requested by preloadTail(); null when it did not come as the expected 206.
  #tailData() {
    const t = this.tail;
    if (!t) return null;
    if (!t.data) {
      try {
        this.side.helperWait(t.e);
        const n = t.e.i32[2];
        if (t.e.i32[1] !== 206 || n !== STREAM_TAIL_PRELOAD) throw new Error('unexpected');
        t.data = new Uint8Array(t.e.sab, HEAD, n); t.start = this.length - n;
      } catch { this.tail = null; return null; }
    }
    return t;
  }

  #waitFor(target) {
    const t0 = performance.now(), bound = 2 * this.timeoutMs + 5000;
    let last = this.#end(), deadline = t0 + bound;
    while (this.#end() < target) {
      const tick = Atomics.load(this.i32, STREAM_TICK), state = Atomics.load(this.i32, STREAM_STATE);
      if (this.#end() >= target || state === STREAM_ENDED) break;
      // A reader that reads ahead for playback blocks here for as long as the link takes, and whoever wants it to do something
      // else (a seek) cannot reach it: its messages wait for this thread. The owner's flag is looked at on every write of the
      // download and at least every 50 ms. Nothing of the source changes: the data keeps arriving, the next read finds it.
      if (this.interrupt?.()) { this.waitMs += performance.now() - t0; throw new ReadInterrupted(); }
      if (state === STREAM_FAILED) throw new Error(`HTTP source: download failed at byte ${this.#end()}`);
      if (performance.now() > deadline) throw new Error(`HTTP source: no data within ${Math.round(bound / 1000)} s at byte ${this.#end()}`);
      Atomics.wait(this.i32, STREAM_TICK, tick, this.interrupt ? 50 : 500);
      if (this.#end() > last) { last = this.#end(); deadline = performance.now() + bound; }
    }
    const waited = performance.now() - t0;
    if (waited > 400 && this.slowWaits++ < 32) sourceTrace.mark?.(`slow-wait ${Math.round(waited)}ms at ${(target / 1e9).toFixed(3)}G`);
    this.waitMs += waited;
  }

  /** Ends the read-ahead cap of an opening done before Play. */
  releaseFill() { this.fillCapped = false; if (this.big && Atomics.load(this.big, STREAM_READ_POS) > 0n) this.#keepHead(); this.worker?.postMessage({ setLimit: 0 }); }

  /** Download state for the buffering policy: bytes held ahead of the demuxer, time the demuxer spent blocked, restarts. */
  stats() {
    return { kind: 'stream', ahead: Math.max(0, this.#end() - this.pos), end: this.#end(), linkKbps: Atomics.load(this.i32, 3), pos: this.pos, ringBytes: this.R, aheadCap: this.R - this.keepBehind, ended: Atomics.load(this.i32, STREAM_STATE) === STREAM_ENDED, waitMs: Math.round(this.waitMs), restarts: this.restarts, sideReads: this.sideReads, headReads: this.headReads, times: Array.from(this.times) };
  }

  close() {
    try { this.worker?.postMessage({ quit: true }); } catch { /* gone */ }
    this.worker?.terminate();
    this.side?.close();
    this.ring = this.ringBuf = this.head = null;
  }
}

/** Pick a source for what the caller passed. */
export async function createSource(input) {
  if (input instanceof Uint8Array || input instanceof ArrayBuffer) return new MemorySource(input);
  if (typeof Blob !== 'undefined' && input instanceof Blob) {
    if (typeof FileReaderSync !== 'undefined') return new BlobSyncSource(input);
    return new MemorySource(await input.arrayBuffer());
  }
  if (input && typeof input === 'object') {
    if (input.prebuilt) return input.prebuilt;
    if (input.url && input.stream && input.length > 0 && typeof SharedArrayBuffer !== 'undefined' && typeof Worker !== 'undefined') {
      try { return await new HttpStreamSource(input.url, input).start(); } catch { /* the block source below does not need the helper */ }
    }
    if (input.url) return new HttpRangeSource(input.url, input).start();
    if (input.path) {
      if (!IS_NODE) throw new Error('{path} sources are Node only');
      return new NodeFileSource(input.path, await import('node:fs'), input.maxBytes);
    }
    if (input.bytes) return new MemorySource(input.bytes);
  }
  throw new Error('openDemuxer: source must be a File/Blob, {url}, {path} (Node) or bytes');
}
