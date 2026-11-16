// SPDX-License-Identifier: LGPL-3.0-only
// AudioDecoderPlugin backed by WebCodecs AudioDecoder (AAC, Opus, FLAC).
export class WebCodecsAudioPlugin {
  constructor() { this.dec = null; this.out = []; this.error = null; this.track = null; }

  async configure(track) {
    this.track = track;
    const cfg = { codec: track.codec, sampleRate: track.sampleRate, numberOfChannels: track.channels };
    if (track.description) cfg.description = track.description;
    const sup = await AudioDecoder.isConfigSupported(cfg);
    if (!sup.supported) throw new Error(`AudioDecoder does not support ${JSON.stringify({ ...cfg, description: undefined })}`);
    this.cfg = cfg;
    this._make();
    return { sampleRate: track.sampleRate, channels: track.channels };
  }

  _make() {
    this.dec?.state !== 'closed' && this.dec?.close();
    const dec = new AudioDecoder({
      output: data => {
        if (dec !== this.dec) { data.close(); return; }
        const planar = [];
        for (let c = 0; c < data.numberOfChannels; c++) {
          const a = new Float32Array(data.numberOfFrames);
          data.copyTo(a, { planeIndex: c, format: 'f32-planar' });
          planar.push(a);
        }
        this.out.push({ ptsUs: data.timestamp, frames: data.numberOfFrames, channels: data.numberOfChannels, sampleRate: data.sampleRate, planar });
        data.close();
      },
      error: e => { this.error = e; },
    });
    dec.configure(this.cfg);
    this.dec = dec;
  }

  _take() {
    if (this.error) { const e = this.error; this.error = null; throw e; }
    const o = this.out; this.out = []; return o;
  }

  decode(packet) {
    this.dec.decode(new EncodedAudioChunk({ type: 'key', timestamp: packet.ptsUs, duration: packet.durationUs, data: packet.data }));
    return this._take();
  }

  poll() { return this._take(); }

  async flush() {
    await this.dec.flush();
    return this._take();
  }

  reset() { this.out = []; this.error = null; this._make(); }

  close() { if (this.dec && this.dec.state !== 'closed') this.dec.close(); this.dec = null; this.out = []; }
}
