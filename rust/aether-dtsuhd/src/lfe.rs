//! ACE LFE channel decoder (ETSI TS 103 491 clauses 9.7 and 9.11).

use crate::bitreader::BitReader;
use crate::tables_gen::INTERPOLATION_FILTER_HALF;

pub const LFE_DECIMATION: usize = 64;
const LFE_ABSOLUTE_THRESHOLD: u32 = 45;
const LFE_REDUCED_THRESHOLD: u32 = 60;
const LFE_RESOLUTION_DECREMENT: u32 = 4;
const LFE_NORMALIZED_STEPSIZE: f64 = 4.48;
const FILTER_HISTORY: usize = 16;
/// The tabulated interpolation filter sums to 1.0 (DC gain 1/64 per phase). A 64x interpolator
/// needs a DC gain of 64 to preserve the amplitude of the decimated samples, so the output is
/// scaled by the interpolation factor. This is inferred, not stated by the specification.
pub const LFE_INTERP_GAIN: f64 = 64.0;

/// Reflection coefficients (Q16) of ETSI TS 102 114 clause 8.5.4 TABLE{}. The DTS-UHD
/// text refers to PREDICTOR_QUANTIZATION_COEFF without defining it; the identical
/// 128 entry LAR indexed reflection coefficient table of the DTS Coherent Acoustics
/// specification is used (same index layout: even indices positive, odd negative).
pub const RC_TABLE_Q16: [u32; 128] = [
    0, 3070, 5110, 7140, 9156, 11154, 13132, 15085, 17010, 18904, 20764, 22588, 24373, 26117,
    27818, 29474, 31085, 32648, 34164, 35631, 37049, 38418, 39738, 41008, 42230, 43404, 44530,
    45609, 46642, 47630, 48575, 49477, 50337, 51157, 51937, 52681, 53387, 54059, 54697, 55302,
    55876, 56421, 56937, 57426, 57888, 58326, 58741, 59132, 59502, 59852, 60182, 60494, 60789,
    61066, 61328, 61576, 61809, 62029, 62236, 62431, 62615, 62788, 62951, 63105, 63250, 63386,
    63514, 63635, 63749, 63855, 63956, 64051, 64140, 64224, 64302, 64376, 64446, 64512, 64573,
    64631, 64686, 64737, 64785, 64830, 64873, 64913, 64950, 64986, 65019, 65050, 65079, 65107,
    65133, 65157, 65180, 65202, 65222, 65241, 65259, 65275, 65291, 65306, 65320, 65333, 65345,
    65357, 65368, 65378, 65387, 65396, 65405, 65413, 65420, 65427, 65434, 65440, 65446, 65451,
    65456, 65461, 65466, 65470, 65474, 65478, 65481, 65485, 65488, 65491,
];

pub fn db_to_linear(n: i32) -> f64 {
    const CRT2: [f64; 3] = [1.0, 1.25992107, 1.58740103];
    let prem = ((n % 3) + 3) % 3;
    let exp = (n - prem) / 3;
    CRT2[prem as usize] * (2.0f64).powi(exp)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LfeMode {
    Absolute,
    Predict,
    Reduced,
    Synthetic,
}

/// Persistent state of one LFE channel.
#[derive(Clone)]
pub struct LfeChannel {
    /// Decimated samples of the previous frame (16 values) and the one before for prediction.
    dec_prev: Vec<f64>,
    have_prev: bool,
    rng: u32,
    pub last_mode: Option<LfeMode>,
    pub last_dbnorm: u32,
    pub last_savings: u32,
}

impl Default for LfeChannel {
    fn default() -> Self {
        Self::new()
    }
}

impl LfeChannel {
    pub fn new() -> Self {
        LfeChannel {
            dec_prev: vec![0.0; 16],
            have_prev: false,
            rng: 0x1234_5678,
            last_mode: None,
            last_dbnorm: 0,
            last_savings: 0,
        }
    }

    fn rand_pm1(&mut self) -> f64 {
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 17;
        self.rng ^= self.rng << 5;
        (self.rng as f64 / u32::MAX as f64) * 2.0 - 1.0
    }

    /// Reads one LFE channel for the current frame from `br` and returns the 16
    /// decimated values (already scaled). The caller upsamples the previous frame.
    pub fn read_decode(&mut self, br: &mut BitReader<'_>, frame_duration: usize) -> Vec<f64> {
        let n = frame_duration / LFE_DECIMATION;
        const RES_TABLE: [u32; 3] = [8, 10, 12];
        let ridx = br.read_uniform(3) as usize;
        let mut resolution = RES_TABLE[ridx.min(2)];
        let raw = br.read_uint(2);
        let savings = if raw == 0 { 0 } else { raw + 1 };
        let dbnorm = br.read_uint(6);
        self.last_dbnorm = dbnorm;
        self.last_savings = savings;
        let mut samples = vec![0i32; n];
        let mut predictor = [0f64; 2];
        let mode;
        if savings != 0 {
            // read_predictor
            for p in predictor.iter_mut() {
                let index = br.read_uniform(255) as usize;
                let mag = RC_TABLE_Q16[(index + 1) / 2 % 128] as f64 / 65536.0;
                *p = if index & 1 == 1 { -mag } else { mag };
            }
            // TS 103 491 clause 9.7.3.3.4 prints `-p0 / (1 + p1)`. Measured against the decoded
            // LFE signal itself (order 2 LPC fit of the output, 54 to 73 windows of the lab sample)
            // the product form is the self-consistent one (median coefficient error 0.027,
            // against 0.51 for the printed quotient), so the usual two stage lattice to direct
            // form conversion is used.
            predictor[0] = -predictor[0] * (1.0 + predictor[1]);
            predictor[1] = -predictor[1];
            resolution = resolution.saturating_sub(savings);
            self.read_buffer(br, resolution, &mut samples);
            mode = LfeMode::Predict;
        } else if dbnorm <= LFE_ABSOLUTE_THRESHOLD {
            self.read_buffer(br, resolution, &mut samples);
            mode = LfeMode::Absolute;
        } else if dbnorm <= LFE_REDUCED_THRESHOLD {
            resolution = resolution.saturating_sub(LFE_RESOLUTION_DECREMENT);
            self.read_buffer(br, resolution, &mut samples);
            mode = LfeMode::Reduced;
        } else {
            mode = LfeMode::Synthetic;
        }
        self.last_mode = Some(mode);
        let factor = db_to_linear(dbnorm as i32) * (((1u64 << resolution) >> 1) as f64);
        let stepsize = LFE_NORMALIZED_STEPSIZE / factor;
        let mut out = vec![0f64; n];
        match mode {
            LfeMode::Predict => {
                let mut m1 = if self.have_prev {
                    self.dec_prev[n - 1]
                } else {
                    0.0
                };
                let mut m2 = if self.have_prev {
                    self.dec_prev[n - 2]
                } else {
                    0.0
                };
                for i in 0..n {
                    let v = samples[i] as f64 * stepsize + predictor[0] * m1 + predictor[1] * m2;
                    out[i] = v;
                    m2 = m1;
                    m1 = v;
                }
            }
            LfeMode::Absolute | LfeMode::Reduced => {
                for i in 0..n {
                    out[i] = samples[i] as f64 * stepsize;
                }
            }
            LfeMode::Synthetic => {
                const LFE_NORM_FACTOR: f64 = 1.73;
                for i in 0..n {
                    out[i] = self.rand_pm1() * LFE_NORM_FACTOR / db_to_linear(dbnorm as i32);
                }
            }
        }
        out
    }

    fn read_buffer(&mut self, br: &mut BitReader<'_>, resolution: u32, out: &mut [i32]) {
        let kparam = br.read_uniform(resolution.max(1));
        for o in out.iter_mut() {
            let s = br.read_golomb(1u32 << resolution.min(31), kparam);
            *o = if s & 1 == 1 {
                -(((s as i64 + 1) / 2) as i32)
            } else {
                (s / 2) as i32
            };
        }
    }

    /// Stores the decimated block of the current frame and returns the 1024 output
    /// samples of the previous frame (zeros while there is no previous frame).
    pub fn push_and_upsample(&mut self, current: &[f64], frame_duration: usize) -> Vec<f32> {
        let n = current.len();
        let mut out = vec![0f32; frame_duration];
        if self.have_prev {
            // buffer = previous decimated (n) followed by current decimated (n)
            let mut buf = Vec::with_capacity(2 * n);
            buf.extend_from_slice(&self.dec_prev);
            buf.extend_from_slice(current);
            for j in 0..n {
                for i in 0..LFE_DECIMATION {
                    let mut acc = 0f64;
                    for k in 0..FILTER_HISTORY {
                        let idx = i + k * LFE_DECIMATION;
                        let c = if idx < 512 {
                            INTERPOLATION_FILTER_HALF[idx]
                        } else {
                            INTERPOLATION_FILTER_HALF[1023 - idx]
                        };
                        acc += c * buf[j + FILTER_HISTORY - 1 - k];
                    }
                    out[j * LFE_DECIMATION + i] = (acc * LFE_INTERP_GAIN) as f32;
                }
            }
        }
        self.dec_prev.clear();
        self.dec_prev.extend_from_slice(current);
        self.have_prev = true;
        out
    }

    pub fn reset(&mut self) {
        self.have_prev = false;
        self.dec_prev.iter_mut().for_each(|v| *v = 0.0);
    }
}
