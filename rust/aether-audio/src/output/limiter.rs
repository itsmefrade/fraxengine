//! Linked peak limiter, a port of the upstream `PeakLimiter` (Sources/AetherEngine/Audio/Spatial).
//!
//! One gain for all channels so the limiter never moves a source. Attack is instant (every sample
//! ends up under the threshold without a lookahead delay, which would shift the audio against the
//! video), release is exponential. A signal that stays under the threshold passes bit-exact.

/// -0.18 dBFS.
pub const THRESHOLD: f32 = 0.98;
pub const RELEASE_SECONDS: f64 = 0.25;

pub struct Limiter {
    release: f32,
    envelope: f32,
    limited_frames: u64,
    deepest_gain: f32,
}

impl Limiter {
    pub fn new(sample_rate: u32) -> Self {
        Self {
            release: (-1.0 / (RELEASE_SECONDS * f64::from(sample_rate))).exp() as f32,
            envelope: 0.0,
            limited_frames: 0,
            deepest_gain: 1.0,
        }
    }

    /// Limits `frames` frames of `planes` planar channels (plane `c` starts at `c * stride`).
    pub fn process(&mut self, data: &mut [f32], stride: usize, planes: usize, frames: usize) {
        for i in 0..frames {
            let mut peak = 0.0f32;
            for c in 0..planes {
                peak = peak.max(data[c * stride + i].abs());
            }
            self.envelope = peak.max(self.envelope * self.release);
            if self.envelope > THRESHOLD {
                let gain = THRESHOLD / self.envelope;
                for c in 0..planes {
                    data[c * stride + i] *= gain;
                }
                self.limited_frames += 1;
                self.deepest_gain = self.deepest_gain.min(gain);
            }
        }
    }

    pub fn reset(&mut self) {
        self.envelope = 0.0;
    }

    /// Frames reduced and the deepest reduction in dB since the previous call.
    pub fn take_stats(&mut self) -> (u64, f32) {
        let r = (self.limited_frames, 20.0 * self.deepest_gain.log10());
        self.limited_frames = 0;
        self.deepest_gain = 1.0;
        r
    }
}
