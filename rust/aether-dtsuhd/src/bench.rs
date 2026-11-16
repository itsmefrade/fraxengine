//! Synthetic full-load workload for speed measurements (native and wasm32).
//!
//! The lab sample is mostly silent or hits the unsupported allocation models, so it does not
//! exercise the transform path. This drives the real stream decoder with random payloads that are
//! accepted (coding mode on, supported allocation model) which is the per-frame work of a
//! fully decodable 5.1.4 stream: one mono and four stereo ACE streams.

use crate::ace::{AceError, AceStream, FrameCtx};

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

/// Decodes `frames` frames of a synthetic 5.1.4 stream (9 full bandwidth channels).
/// Returns (number of stream frames decoded, checksum of the output).
pub fn synthetic_stream_load(frames: usize, seed: u64) -> (usize, f64) {
    let mut rng = Rng(seed | 1);
    let mut streams: Vec<AceStream> = vec![AceStream::new(false)];
    streams.extend((0..4).map(|_| AceStream::new(true)));
    let mut ok = 0usize;
    let mut sum = 0f64;
    for f in 0..frames {
        for (i, s) in streams.iter_mut().enumerate() {
            let len = if i == 0 { 260 } else { 330 };
            let ctx = FrameCtx {
                sampling_rate: 48000,
                is_predictive: false,
                enable_deemphasis: true,
                bandwidth_mode: 2,
            };
            // Random payloads are accepted when the allocation model is supported; retry otherwise.
            for _ in 0..16 {
                let mut p: Vec<u8> = (0..len).map(|_| rng.next() as u8).collect();
                p[0] |= 0x80; // coding mode "full"
                p[0] &= !0x40; // no long term synthesis flag
                match s.decode_frame(&p, &ctx) {
                    Ok(_) => {
                        ok += 1;
                        break;
                    }
                    Err(AceError::Unsupported(_)) | Err(AceError::Invalid(_)) => {
                        s.reset_all();
                    }
                }
            }
            for ch in &s.out {
                sum += ch[(f * 7) % 1024] as f64;
            }
        }
    }
    (ok, sum)
}
