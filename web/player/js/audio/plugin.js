// SPDX-License-Identifier: LGPL-3.0-only
// Audio decoder plug-in interface. The audio worker only talks to this shape: WebCodecs for AAC/Opus/FLAC and the
// Rust/WASM engine (web/engine) for TrueHD, DTS family, AC-3, E-AC-3 and PCM.
//
//   interface AudioDecoderPlugin {
//     configure(track): Promise<{ sampleRate, channels }>   // actual output format
//     decode(packet):   Promise<DecodedAudio[]> | DecodedAudio[]   // whatever is ready now (may be empty)
//     poll?():          DecodedAudio[]                      // asynchronous decoders: output that arrived since
//     flush():          Promise<DecodedAudio[]>             // drain at end of stream
//     reset():          void                                // drop state on seek (next packet is a fresh start)
//     close():          void
//   }
//   DecodedAudio { ptsUs, frames, channels, sampleRate, planar: Float32Array[channels] (each length frames),
//                  layout?: canonical channel labels per plane, lossless?, joc? }
//
// A plug-in is registered with a predicate over the Track.
import { WebCodecsAudioPlugin } from './webcodecs-plugin.js';
import { EngineAudioPlugin, ENGINE_CODECS } from './engine-plugin.js';

const registry = [
  { name: 'webcodecs', accepts: t => /^(mp4a\.|opus$|flac$)/.test(t.codec), create: () => new WebCodecsAudioPlugin() },
  { name: 'rust-engine', accepts: t => ENGINE_CODECS.test(t.codec), create: () => new EngineAudioPlugin() },
];

export function registerAudioPlugin(entry) { registry.unshift(entry); }

export function createAudioPlugin(track) {
  const e = registry.find(r => r.accepts(track));
  if (!e) throw new Error(`no audio decoder for codec "${track.codec}"`);
  const p = e.create();
  p.pluginName = e.name;
  return p;
}
