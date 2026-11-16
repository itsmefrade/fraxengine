// SPDX-License-Identifier: LGPL-3.0-only
// Synchronous random-access byte sources for the demux worker's JS demuxer (?demux=js). The demux contract is synchronous
// (nextPacket() returns a packet), as for the engine's Rust/WASM demuxer, so reads are sync: FileReaderSync for Blob/File,
// the engine's HttpRangeSource for HTTP. openUrl also describes a URL for the engine demuxer, which opens its own source.

import { HttpRangeSource, HttpStreamSource } from '../../engine/sources.mjs';

class WindowedSource {
  constructor(windowBytes) { this.windowBytes = windowBytes; this.cacheOff = 0; this.cache = new Uint8Array(0); this.size = 0; }
  _fetch(off, len) { throw new Error('abstract'); }
  // Returns a view that may alias the cache; callers must copy before keeping it.
  read(off, len) {
    if (off < 0 || off > this.size) throw new Error(`read out of range ${off}`);
    len = Math.min(len, this.size - off);
    if (len <= 0) return new Uint8Array(0);
    const c = this.cache;
    if (off >= this.cacheOff && off + len <= this.cacheOff + c.length) return c.subarray(off - this.cacheOff, off - this.cacheOff + len);
    const want = Math.min(Math.max(len, this.windowBytes), this.size - off);
    this.cache = this._fetch(off, want);
    this.cacheOff = off;
    return this.cache.subarray(0, len);
  }
  close() { this.cache = new Uint8Array(0); }
}

export class BlobSource extends WindowedSource {
  constructor(blob) { super(1 << 20); this.blob = blob; this.size = blob.size; this.reader = new FileReaderSync(); this.name = blob.name || 'blob'; this.mode = 'blob'; }
  _fetch(off, len) { return new Uint8Array(this.reader.readAsArrayBuffer(this.blob.slice(off, off + len))); }
}

// Delegates to the engine's HttpRangeSource: a helper Worker does the fetch under an AbortController and this thread blocks on
// Atomics.wait, so a stalled server errors after ~20 s (sync XHR timeout is ignored by Chrome in Workers, which means ~60 s).
export class RangeSource extends WindowedSource {
  constructor(url, size, http, headers = {}) { super(4 << 20); this.url = url; this.size = size; this.http = http; this.headers = headers; this.name = url; this.mode = 'http-range'; }
  static async open(url, size, headers = {}) {
    // Only where no SharedArrayBuffer exists (no continuous download possible). 1 MiB requests spend most of their time waiting for the first
    // byte (measured 300 to 700 ms each, 16 to 42 Mbit/s against 80 continuous); 4 MiB blocks with 32 MiB in flight hide that wait.
    return new RangeSource(url, size, await new HttpRangeSource(url, { length: size, headers, blockSize: 4 << 20, maxBlocks: 20, prefetch: 8 }).start(), headers);
  }
  _fetch(off, len) {
    const dest = new Uint8Array(len);
    const n = this.http.read(off, dest);
    if (n < len) throw new Error(`range request failed at byte ${off}: short read`);
    return dest;
  }
  close() { super.close(); this.http.close(); }
}

const originOf = url => { try { return new URL(url).origin; } catch { return 'the media server'; } };

// HTTP media is read as one continuous download (HttpStreamSource) wherever SharedArrayBuffer exists: one open-ended request that
// also tells the size, the file's last megabyte shortly after (preloadTail), and a new request only for a seek, whatever the origin: a site that
// passes its media server through under its own origin read in 1 MiB ranges two at a time reached 43 Mbit/s of a link that carries
// 83 to 90 in one request (178 to 188 ms per megabyte).
const canStream = () => typeof SharedArrayBuffer !== 'undefined' && typeof Worker !== 'undefined';

// Servers without Range support (e.g. python http.server) answer 200 with the whole body; in that case download once into a Blob
// and read it through FileReaderSync, unless `requireRange` is set (a remote movie would be a multi-GB download that looks like a
// hang): then that is an error. `headers` go on every request. A redirect to another origin (a media host behind the server) is
// followed once here and the source then reads from where it landed, without the caller's headers (credentials such as a token stay
// with the origin they were meant for; browsers drop Authorization on a cross-origin redirect too). `size` is the length when the
// caller knows it: a cross-origin 206 hides Content-Range unless the server lists it in Access-Control-Expose-Headers.
export async function openUrl(url, headers = {}, { size = null, requireRange = false, describeOnly = false, fillLimit = 0 } = {}) {
  try { url = new URL(url, self.location.href).href; } catch {}   // the download helpers are blob Workers: a relative address has no base there
  if (describeOnly && canStream()) {
    const s = await HttpStreamSource.open(url, { headers, size, fillLimit });
    if (s) return { mode: 'http-range', url: s.url, size: s.length, headers: s.headers, name: s.url, far: true, prebuilt: s };
  }
  let r;
  const ac = new AbortController(), timer = setTimeout(() => ac.abort(), 20000);
  try { r = await fetch(url, { headers: { ...headers, Range: 'bytes=0-0' }, signal: ac.signal }); } catch (e) {
    if (ac.signal.aborted) throw new Error(`media server ${originOf(url)} did not answer within 20 s`);
    throw new Error(`media request to ${originOf(url)} failed: it (or the host it redirects to) sends no CORS headers for this page, or the network failed`);
  } finally { clearTimeout(timer); }
  let src = url, srcHeaders = headers;
  if (r.redirected && r.url) { src = r.url; if (originOf(r.url) !== originOf(url)) srcHeaders = {}; }
  if (r.status === 206) {
    const m = /\/(\d+)$/.exec(r.headers.get('Content-Range') || '');
    await r.arrayBuffer();
    let total = m ? Number(m[1]) : size;
    // Content-Length is readable across origins even when Content-Range is hidden: an open-ended range reports the whole file through
    // it (some CDNs send CORS but do not expose Content-Range). Only the headers are read.
    if (!total) {
      const ac2 = new AbortController(), t2 = setTimeout(() => ac2.abort(), 20000);
      try {
        const r2 = await fetch(src, { headers: { ...srcHeaders, Range: 'bytes=0-' }, signal: ac2.signal });
        if (r2.status === 206) total = Number(r2.headers.get('Content-Length')) || null;
      } catch {} finally { clearTimeout(t2); ac2.abort(); }
    }
    // The engine demuxer opens its own source from this description: a RangeSource here only added a helper Worker and a request nobody reads.
    if (total) return describeOnly ? { mode: 'http-range', url: src, size: total, headers: srcHeaders, name: src, far: true } : RangeSource.open(src, total, srcHeaders);
    if (requireRange) throw new Error(`media server ${originOf(src)} answers Range requests but hides Content-Range (needs Access-Control-Expose-Headers: Content-Range) and the size is unknown`);
  }
  if (requireRange) {
    try { await r.body?.cancel(); } catch {}
    throw new Error(r.ok ? `media server ${originOf(src)} does not support HTTP Range requests (answered ${r.status} to a Range request)` : `media request failed: HTTP ${r.status} from ${originOf(src)}`);
  }
  const full = r.status === 206 ? await fetch(src, { headers: srcHeaders }) : r;
  if (!full.ok) throw new Error(`HTTP ${full.status} for ${src}`);
  const blob = await full.blob();
  const s = new BlobSource(blob);
  s.mode = 'http-full-download';
  s.name = src;
  return s;
}
