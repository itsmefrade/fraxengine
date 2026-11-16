// SPDX-License-Identifier: LGPL-3.0-only
// WebKit shows EDR from a WebGPU canvas only when the page's own thread presents it. A canvas transferred to a worker, a bitmap copied
// from a worker and every other route read exactly 1.000 on the screen (Safari 27.2, XDR panel, a test page: main thread
// display-p3-linear v=3 reads 2.998, the same canvas in a worker 1.000). Decode, pairing and pacing of the compositor path do not have
// to leave the worker, but GPU reconstruction shares its device with the compositor, so the video worker's module runs here as it is, with this
// object standing in for its worker scope. Every session imports its own instance of the module (the query makes the URL unique).
let nextId = 1;
const hosts = globalThis.__fraxVideoHosts ??= new Map();

export class MainVideoHost {
  onmessage = null; onerror = null;
  #closed = false; #closeSent = false; #inbox = []; #loaded = false; #id = String(nextId++);
  constructor() {
    const scope = this.scope = { onmessage: null, postMessage: m => { if (!this.#closed) queueMicrotask(() => { if (!this.#closed) this.onmessage?.({ data: m }); }); } };
    hosts.set(this.#id, scope);
    import(`./video-worker.js?mainhost=${this.#id}`).then(() => {
      this.#loaded = true;
      const q = this.#inbox; this.#inbox = [];
      for (const m of q) this.#deliver(m);
    }, e => { this.onerror?.({ message: String(e?.message ?? e) }); });
  }
  #deliver(m) {
    if (m.type === 'close') this.#closeSent = true;
    queueMicrotask(() => this.scope.onmessage?.({ data: m }));
  }
  postMessage(m) { if (this.#loaded) this.#deliver(m); else this.#inbox.push(m); }
  terminate() {
    if (!this.#closeSent && this.#loaded) this.#deliver({ type: 'close' });   // the module's timers and decoders outlive a terminate otherwise
    this.#closed = true;
    hosts.delete(this.#id);
  }
}
