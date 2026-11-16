// SPDX-License-Identifier: LGPL-3.0-only
// AudioWorklet: pulls planar f32 from the shared ring. READ (frames consumed) is the master clock.
// Layout and slot meaning: see protocol.js (duplicated here because worklet scope has no import of workers' helpers
// beyond what this module graph provides; the numbers must match CTL).
import { CTL } from './protocol.js';

class RingProcessor extends AudioWorkletProcessor {
  constructor() {
    super();
    this.ring = null;
    this.port.onmessage = e => {
      const m = e.data;
      if (m.type === 'attach') {
        const r = m.ring;
        this.ctl = new Int32Array(r.ctl);
        this.cap = r.cap;
        this.planes = [];
        for (let c = 0; c < r.channels; c++) this.planes.push(new Float32Array(r.data, c * r.cap * 4, r.cap));
        this.ring = r;
        this.drainedSent = false;
      } else if (m.type === 'detach') {
        this.ring = null;
      }
    };
  }

  process(_inputs, outputs) {
    const out = outputs[0];
    const r = this.ring;
    if (!r || !out.length) return true;
    const n = out[0].length;
    const ctl = this.ctl;
    if (Atomics.load(ctl, CTL.PLAY) !== 1) return true;   // outputs are pre-zeroed
    const rd = Atomics.load(ctl, CTL.READ);
    const wr = Atomics.load(ctl, CTL.WRITE);
    const avail = (wr - rd) | 0;
    const take = avail < n ? (avail > 0 ? avail : 0) : n;
    if (take > 0) {
      const cap = this.cap;
      let start = rd % cap; if (start < 0) start += cap;
      const first = Math.min(take, cap - start);
      const chans = Math.min(out.length, this.planes.length);
      for (let c = 0; c < chans; c++) {
        const src = this.planes[c], dst = out[c];
        for (let i = 0; i < first; i++) dst[i] = src[start + i];
        for (let i = first; i < take; i++) dst[i] = src[i - first];
      }
      Atomics.store(ctl, CTL.READ, (rd + take) | 0);
    }
    if (take < n) {
      if (Atomics.load(ctl, CTL.EOF) === 1) {
        if (!this.drainedSent) { Atomics.store(ctl, CTL.DRAINED, 1); this.drainedSent = true; this.port.postMessage({ type: 'drained' }); }
      } else {
        Atomics.add(ctl, CTL.UNDERRUNS, 1);
      }
    }
    return true;
  }
}
registerProcessor('aether-ring', RingProcessor);
