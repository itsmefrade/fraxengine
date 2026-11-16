//! ACE (Audio Compression Engine) mono/stereo stream decoder, ETSI TS 103 491
//! clauses 8 to 10. One `AceStream` decodes one mono or stereo stream across frames.

use crate::bitreader::*;
use crate::config::*;
use crate::fixed::*;
use crate::tables_gen::BETA_DEQUANT_ROWS;
use crate::transform;
use crate::vq;

pub const CODING_FULL: u32 = 0;
pub const CODING_OFF: u32 = 1;
pub const CODING_LEFT: u32 = 2;
pub const CODING_RIGHT: u32 = 3;

const STEREO_MIDSIDE_MONO: u8 = 1;
const STEREO_LEFTRIGHT: u8 = 2;
const STEREO_MIDSIDE: u8 = 3;

const LOGNORM_MINUS_INFINITY: i32 = -32 << 10;
const ALLOC_MODEL_PARAMETER_DEFAULT: u32 = 10;

const LTS_FILTER_CODEBOOK: [[f32; 3]; 29] = [
    [0.0, 0.0, 0.0],
    [0.064995, 0.040695, 0.020557],
    [0.079613, 0.047869, 0.006069],
    [0.115744, 0.029569, 0.004913],
    [0.15375, 0.01125, 0.005625],
    [0.097493, 0.061043, 0.030836],
    [0.119419, 0.071803, 0.009104],
    [0.173616, 0.044353, 0.007369],
    [0.230625, 0.016875, 0.008437],
    [0.12999, 0.08139, 0.041115],
    [0.159225, 0.095737, 0.012139],
    [0.231487, 0.059138, 0.009825],
    [0.3075, 0.0225, 0.01125],
    [0.162487, 0.101737, 0.051393],
    [0.199031, 0.119672, 0.015173],
    [0.289359, 0.073922, 0.012281],
    [0.384375, 0.028125, 0.014062],
    [0.194985, 0.122085, 0.061672],
    [0.238838, 0.143606, 0.018208],
    [0.347231, 0.088706, 0.014737],
    [0.46125, 0.03375, 0.016875],
    [0.227482, 0.142432, 0.071951],
    [0.278644, 0.167541, 0.021243],
    [0.405103, 0.103491, 0.017194],
    [0.538125, 0.039375, 0.019687],
    [0.25998, 0.16278, 0.082229],
    [0.31845, 0.191475, 0.024278],
    [0.462975, 0.118275, 0.01965],
    [0.615, 0.045, 0.0225],
];

#[derive(Debug, Clone)]
pub enum AceError {
    Unsupported(&'static str),
    Invalid(&'static str),
}

/// Interpretation switches for places where the specification text is ambiguous.
/// The defaults are the readings that make real streams parse to the last bit.
#[derive(Clone, Debug)]
pub struct Quirks {
    pub lts_flag_bit: bool,
    pub refine_subtract_nch: bool,
    pub adjust_variant: u8,
    pub beta_shift: u32,
    pub ms_diff_includes_beta: bool,
    pub pred_residual_chain: bool,
}

impl Default for Quirks {
    fn default() -> Self {
        Quirks {
            lts_flag_bit: true,
            refine_subtract_nch: true,
            adjust_variant: 0,
            beta_shift: 11,
            ms_diff_includes_beta: false,
            pred_residual_chain: true,
        }
    }
}

/// Parse state of a stream that the next frame refers to (the "previous" stream).
#[derive(Clone)]
struct PrevParse {
    valid: bool,
    lognorm_wo: [[i32; NUM_ACE_BANDS]; 2],
    num_coded_bands: usize,
    alloc: [i32; NUM_ACE_BANDS],
    min_guaranteed_alloc: i32,
    alloc_model_parameter: u32,
    num_ms_mono_bands: usize,
    lts_lag: u32,
    lts_coeffs: [f32; 3],
}

impl PrevParse {
    fn empty() -> Self {
        PrevParse {
            valid: false,
            lognorm_wo: [[LOGNORM_MINUS_INFINITY; NUM_ACE_BANDS]; 2],
            num_coded_bands: 0,
            alloc: [0; NUM_ACE_BANDS],
            min_guaranteed_alloc: 0,
            alloc_model_parameter: ALLOC_MODEL_PARAMETER_DEFAULT,
            num_ms_mono_bands: 0,
            lts_lag: 0,
            lts_coeffs: [0.0; 3],
        }
    }
}

/// Statistics of one decoded stream frame (for validation and tracing).
#[derive(Clone, Debug, Default)]
pub struct StreamTrace {
    pub coding_mode: u32,
    pub is_short: bool,
    pub is_highres: bool,
    pub lts_enabled: bool,
    pub lts_lag: u32,
    pub alloc_model: u32,
    pub num_coded_bands: usize,
    pub num_ms_mono_bands: usize,
    pub bits_total: usize,
    pub bits_left_at_end: i64,
    pub overrun: bool,
    pub shf: bool,
}

pub struct FrameCtx {
    pub sampling_rate: u32,
    pub is_predictive: bool,
    pub enable_deemphasis: bool,
    pub bandwidth_mode: usize,
}

struct LtsParams {
    lag: u32,
    gains: [f32; 3],
}

/// One ACE mono or stereo stream.
pub struct AceStream {
    pub is_stereo: bool,
    nch: usize,
    prev: PrevParse,
    // time domain state
    td: Vec<Vec<f32>>, // DCT-IV output of the frame awaiting completion
    td_valid: bool,
    td_short: bool,
    td_lts: LtsParams,   // LTS parameters of the frame awaiting completion
    td_deemph: bool,     // deemphasis flag of the frame awaiting completion
    prev_lts: LtsParams, // parameters of the frame before that
    hist: Vec<Vec<f32>>, // completed (post LTS, pre deemphasis) samples of the frame before td
    deemph_state: [f32; 2],
    rng: u32,
    pub out: Vec<Vec<f32>>, // completed output samples (one frame behind)
    pub out_valid: bool,
    pub quirks: Quirks,
    pub last_trace: StreamTrace,
}

struct Frame<'a> {
    br: BitReader<'a>,
    cfg: &'static BandsConfig,
    prev: &'a PrevParse,
    q: &'a Quirks,
    is_stereo: bool,
    nch: usize,
    is_predictive: bool,
    total_bits: i32,
    neb: usize,
    num_coded_bands: usize,
    coding_mode: u32,
    first_ch: usize,
    last_ch: usize,
    neff: i32,
    is_highres: bool,
    is_short: bool,
    num_mdct_blocks: usize,
    shf: bool,
    thf: bool,
    recond: u32,
    lognorm_wo: [[i32; NUM_ACE_BANDS]; 2],
    lognorm: [[i32; NUM_ACE_BANDS]; 2],
    num_band_blocks: [usize; NUM_ACE_BANDS],
    alloc: [i32; NUM_ACE_BANDS],
    alloc_model: u32,
    alloc_model_parameter: u32,
    alloc_max: [i32; NUM_ACE_BANDS],
    alloc_min: [i32; NUM_ACE_BANDS],
    alloc_delta: [i32; NUM_ACE_BANDS],
    min_guaranteed: i32,
    num_ms_mono_bands: usize,
    stereo_flag: [u8; NUM_ACE_BANDS],
    lr_region_mode: [u8; 3],
    first_refine: [i32; NUM_ACE_BANDS],
    saved_late: [i32; NUM_ACE_BANDS],
    late_refine: [i32; NUM_ACE_BANDS],
    unallocated_bits: i32,
    spec: [[f32; FRAME_LEN]; 2],
    rng: &'a mut u32,
    lts_enabled: bool,
    lts_lag: u32,
    lts_coeffs: [f32; 3],
}

fn rand_sign(rng: &mut u32) -> f32 {
    *rng ^= *rng << 13;
    *rng ^= *rng >> 17;
    *rng ^= *rng << 5;
    if *rng & 0x8000 != 0 {
        1.0
    } else {
        -1.0
    }
}

impl<'a> Frame<'a> {
    // ------------------------------------------------------------------ parsing

    fn read_long_term_synthesis(&mut self) {
        self.lts_lag = 0;
        self.lts_coeffs = [0.0; 3];
        self.lts_enabled = if self.q.lts_flag_bit {
            self.br.read_bool()
        } else {
            true
        };
        if !self.lts_enabled {
            return;
        }
        let predictive = self.prev.valid && self.prev.lts_lag > 0 && self.is_predictive;
        if !predictive {
            self.lts_lag = self.br.read_uniform(1020) + 3;
            self.lts_coeffs = LTS_FILTER_CODEBOOK[self.br.read_uniform(29) as usize];
        } else {
            let d = self.br.read_golomb_limited(1020, 4, 127);
            let diff = negative_rice_map_decode(d);
            self.lts_lag = modular(self.prev.lts_lag as i32 + diff, 3, 1022) as u32;
            if self.br.read_bool() {
                self.lts_coeffs = LTS_FILTER_CODEBOOK[self.br.read_uniform(29) as usize];
            } else {
                self.lts_coeffs = self.prev.lts_coeffs;
            }
        }
    }

    fn read_lognorm_primary(&mut self) {
        const MAX_K_BOOST: u32 = 6;
        let sync_index = if self.is_predictive { 0 } else { 1 };
        let kboost = self.br.read_unary(MAX_K_BOOST);
        for band in 0..self.neb {
            let (k, limit) = lognorm_golomb(sync_index, self.is_short, band);
            for ch in self.first_ch..=self.last_ch {
                let v =
                    positive_rice_map_decode(self.br.read_golomb_limited(64, k + kboost, limit));
                self.lognorm_wo[ch][band] = v << 10;
            }
        }
        let f_pred: [i32; NUM_ACE_BANDS] = if self.is_predictive {
            F_PRED_PREDICTIVE
        } else {
            [0; NUM_ACE_BANDS]
        };
        let b_pred = if self.is_predictive {
            &B_PRED_PREDICTIVE
        } else {
            &B_PRED_NONPRED
        };
        let mut pred_residual = [0i32; 2];
        for band in 0..self.neb {
            for ch in self.first_ch..=self.last_ch {
                let b_term = ((b_pred[band] as i64 * pred_residual[ch] as i64) >> 10) as i32;
                let residual = self.lognorm_wo[ch][band];
                pred_residual[ch] = residual;
                let mut prev_frame = self.prev.lognorm_wo[ch][band];
                if prev_frame < -16 << 10 {
                    prev_frame = -16 << 10;
                }
                let f_term = ((f_pred[band] as i64 * prev_frame as i64) >> 10) as i32;
                self.lognorm_wo[ch][band] = f_term
                    + if self.q.pred_residual_chain {
                        b_term
                    } else {
                        0
                    }
                    + residual;
            }
        }
        self.transfer_lognorms();
    }

    fn transfer_lognorms(&mut self) {
        for ch in self.first_ch..=self.last_ch {
            for band in 0..self.neb {
                self.lognorm[ch][band] = self.lognorm_wo[ch][band] + self.cfg.lognorm_offset[band];
            }
        }
    }

    fn read_num_band_blocks(&mut self) {
        let mut changes = [0i32; NUM_ACE_BANDS];
        if self.br.read_bool() {
            const INDEX_TO_DK: [u32; 4] = [2, 3, 0, 1];
            let dk = INDEX_TO_DK[self.br.read_unary(4) as usize];
            let diff = dk >> 1;
            let k = dk & 1;
            let head = 1 + self.br.read_uniform(self.neb as u32) as usize;
            let narrow = self.cfg.num_narrow_bands;
            for band in 0..head.min(NUM_ACE_BANDS) {
                let is_narrow = band < narrow;
                let alpha: u32 = if is_narrow { 4 } else { 5 };
                let msymb = if k == 0 {
                    self.br.read_unary(alpha)
                } else {
                    self.br.read_uniform(alpha)
                };
                changes[band] = self.map_tf_change(msymb, is_narrow, diff);
            }
            if diff != 0 {
                self.enforce(&mut changes[0], narrow > 0);
                for band in 1..self.neb {
                    changes[band] += changes[band - 1];
                    let mut v = changes[band];
                    self.enforce(&mut v, band < narrow);
                    changes[band] = v;
                }
            }
        }
        let base_log = if self.is_short { 3 } else { 0 };
        for band in 0..NUM_ACE_BANDS {
            let l = (base_log + changes[band]).clamp(0, 5);
            self.num_band_blocks[band] = 1usize << l;
        }
    }

    fn map_tf_change(&self, msymb: u32, is_narrow: bool, diff: u32) -> i32 {
        const SHORT_NARROW: [i32; 4] = [0, -1, -2, -3];
        const SHORT_WIDE: [i32; 5] = [0, 1, -1, -2, -3];
        if diff == 0 {
            if self.is_short {
                if is_narrow {
                    SHORT_NARROW[(msymb as usize).min(3)]
                } else {
                    SHORT_WIDE[(msymb as usize).min(4)]
                }
            } else {
                msymb as i32
            }
        } else {
            positive_rice_map_decode(msymb)
        }
    }

    fn enforce(&self, v: &mut i32, is_narrow: bool) {
        let alpha = if is_narrow { 4 } else { 5 };
        let minsymb = if self.is_short { -3 } else { 0 };
        *v = modular(*v, minsymb, minsymb + alpha - 1);
    }

    fn num_bits_step(&self, band: usize) -> i32 {
        let num_bins = self.neff * self.cfg.bandsize[band] as i32;
        if num_bins < 6 {
            num_bins
        } else if num_bins < 48 {
            6
        } else {
            num_bins / 8
        }
    }

    fn read_num_bands(&mut self, alpha: u32, kparam: u32, limit: u32, prev_num: i32) -> u32 {
        if !self.is_predictive {
            self.br.read_uniform(alpha)
        } else {
            let m = self.br.read_golomb_limited(alpha, kparam, limit);
            let r = negative_rice_map_decode(m) + prev_num;
            modular(r, 0, alpha as i32 - 1) as u32
        }
    }

    fn read_num_coded_bands(&mut self) {
        let prev_noncoded = self.neb as i32
            - if self.prev.valid {
                self.prev.num_coded_bands as i32
            } else {
                self.neb as i32
            };
        let noncoded = self.read_num_bands(self.neb as u32 + 1, 0, 7, prev_noncoded) as usize;
        self.num_coded_bands = self.neb.saturating_sub(noncoded);
    }

    fn read_num_ms_mono_bands(&mut self) {
        let prev = if self.prev.valid {
            self.prev.num_ms_mono_bands as i32
        } else {
            0
        };
        self.num_ms_mono_bands = self.read_num_bands(self.neb as u32, 0, 7, prev) as usize;
    }

    fn compute_region(band: usize) -> usize {
        if band < 4 {
            0
        } else if band < 12 {
            1
        } else {
            2
        }
    }

    fn read_stereo_flags(&mut self) {
        let non_ms_mono = self.neb - self.num_ms_mono_bands.min(self.neb);
        for b in 0..NUM_ACE_BANDS {
            self.stereo_flag[b] = STEREO_LEFTRIGHT;
        }
        for b in non_ms_mono..self.neb {
            self.stereo_flag[b] = STEREO_MIDSIDE_MONO;
        }
        if self.coding_mode != CODING_FULL {
            return;
        }
        let mut has_lr = false;
        let full_frame = self.br.read_bool();
        if full_frame {
            let ms = self.br.read_bool();
            for b in 0..non_ms_mono {
                self.stereo_flag[b] = if ms { STEREO_MIDSIDE } else { STEREO_LEFTRIGHT };
            }
            if !ms {
                has_lr = true;
            }
        } else {
            for b in 0..non_ms_mono {
                let ms = self.br.read_bool();
                self.stereo_flag[b] = if ms { STEREO_MIDSIDE } else { STEREO_LEFTRIGHT };
                if !ms {
                    has_lr = true;
                }
            }
        }
        let num_regions = if non_ms_mono == 0 {
            0
        } else {
            Self::compute_region(non_ms_mono - 1) + 1
        };
        if has_lr {
            let per_region = [4usize, 8, self.neb.saturating_sub(12)];
            let all_5050 = self.br.read_bool();
            if all_5050 {
                for r in 0..num_regions {
                    self.lr_region_mode[r] = 0;
                }
            } else {
                let mut band = 0usize;
                for r in 0..num_regions {
                    let mut includes = false;
                    for _ in 0..per_region[r] {
                        if band < NUM_ACE_BANDS && self.stereo_flag[band] == STEREO_LEFTRIGHT {
                            includes = true;
                        }
                        band += 1;
                    }
                    if includes {
                        let mut mode = self.br.read_uint(1);
                        if mode != 0 {
                            mode = self.br.read_uint(2) + 1;
                        }
                        self.lr_region_mode[r] = mode as u8;
                    }
                }
            }
        }
    }

    fn read_alloc_model_parameter(&mut self) {
        if !self.is_predictive {
            self.alloc_model_parameter = self.br.read_unary(11);
        } else {
            let prev = self.prev.alloc_model_parameter;
            let d = self.br.read_uniform(11);
            self.alloc_model_parameter = decode_center_map(11, prev.min(10), d);
        }
    }

    fn read_alloc_model_delta(&mut self) {
        self.alloc_delta = [0; NUM_ACE_BANDS];
        let head = match self.alloc_model {
            1 | 2 => self.br.read_uniform(self.neb as u32 + 1) as usize,
            0 => {
                let all_zero = self.br.read_bool();
                if !all_zero {
                    self.br.read_uniform(self.neb as u32) as usize + 1
                } else {
                    0
                }
            }
            _ => 0,
        };
        for band in 0..head.min(NUM_ACE_BANDS) {
            let steps = self.br.read_unary(self.num_steps_band(band) as u32 + 1) as i32;
            self.alloc_delta[band] = steps * self.num_bits_step(band);
        }
    }

    /// Number of unary steps available for a band (ceil(alloc_max / step), the
    /// largest delta the band can take).
    fn num_steps_band(&self, band: usize) -> i32 {
        let step = self.num_bits_step(band).max(1);
        (self.alloc_max[band] + step - 1) / step
    }

    fn do_allocation_model_step(
        &self,
        first_bin_bits: Bafp,
        adjustments: &[i32; NUM_ACE_BANDS],
        out: &mut [i32; NUM_ACE_BANDS],
    ) -> i32 {
        const HIGH_BANDS_THRESHOLD: usize = 19;
        *out = [0; NUM_ACE_BANDS];
        let factor = Fx::<10>(BAFACTOR[(self.alloc_model_parameter as usize).min(20)]);
        for band in 0..self.num_coded_bands {
            let num_band_bins = self.cfg.bandsize[band] as i32;
            let mut bpb = first_bin_bits;
            bpb = bpb.sub(factor.mul_int(band as i32));
            if band > HIGH_BANDS_THRESHOLD {
                bpb = bpb.sub(Fx::from_int(1));
            }
            bpb = bpb.max(Fx::ZERO);
            let mut bits_per_band = bpb.mul_int(num_band_bins * self.nch as i32);
            if first_bin_bits.0 > 0 {
                bits_per_band = bits_per_band.add(Fx::from_int(adjustments[band]));
            }
            bits_per_band = bits_per_band.max(Fx::from_int(self.min_guaranteed));
            bits_per_band = bits_per_band.min(Fx::from_int(self.alloc_max[band]));
            out[band] = bits_per_band.int_part();
        }
        let mut last_coded: i32 = -1;
        for band in (0..self.num_coded_bands).rev() {
            if out[band] >= self.alloc_min[band] {
                last_coded = band as i32;
                break;
            }
        }
        let mut band = last_coded;
        while band >= 0 {
            let b = band as usize;
            out[b] = out[b].max(self.alloc_min[b]);
            band -= 1;
        }
        for band in (last_coded + 1) as usize..self.num_coded_bands {
            out[band] = self.min_guaranteed;
        }
        out[..self.neb].iter().sum()
    }

    fn do_allocation_model(
        &self,
        available: i32,
        adjustments: &[i32; NUM_ACE_BANDS],
        out: &mut [i32; NUM_ACE_BANDS],
    ) -> i32 {
        let mut low: Bafp = Fx::ZERO;
        let mut high: Bafp = Fx::from_int(8);
        for _ in 0..13 {
            let mid = low.add(high).div_int(2);
            let n = self.do_allocation_model_step(mid, adjustments, out);
            if n > available {
                high = mid;
            } else {
                low = mid;
            }
        }
        self.do_allocation_model_step(low, adjustments, out)
    }

    /// compute_unmasked_levels() is referenced by clause 9.9.13.11 but not defined anywhere in
    /// ETSI TS 103 491 V1.1.1 or V1.2.1. Returning `None` makes every stream frame that uses an
    /// allocation derived from the primary lognorms (models 0, 2 and 3) report `Unsupported`.
    /// Everything after this call (adjustment scaling, allocation search, deltas) is implemented.
    fn compute_unmasked_levels(&self) -> Option<[i32; NUM_ACE_BANDS]> {
        None
    }

    fn compute_adjustments_from_lognorms(&self) -> Option<[i32; NUM_ACE_BANDS]> {
        let lev = self.compute_unmasked_levels()?;
        let mut adj = [0i32; NUM_ACE_BANDS];
        let mut total = 0i32;
        for b in 0..self.neb {
            let bins = self.nch as i32 * self.cfg.bandsize[b] as i32;
            let mut a = ((lev[b] as i64 * bins as i64) >> 10) as i32;
            a = a.min(self.alloc_max[b]);
            adj[b] = a;
            total += a;
        }
        let cap = ((self.total_bits - 60) / 2).max(0);
        if total > cap && total > 0 {
            for b in 0..self.neb {
                if adj[b] > 0 {
                    adj[b] = (adj[b] as i64 * cap as i64 / total as i64) as i32;
                }
            }
        }
        Some(adj)
    }

    fn estimate_cost_from_adjustments(&self, n_bands: usize, adj: &[i32; NUM_ACE_BANDS]) -> i32 {
        let mut cost = 0;
        for b in 0..n_bands {
            let step = self.num_bits_step(b).max(1);
            cost += 1;
            if adj[b] > 0 {
                cost += (adj[b] + step / 2) / step;
            }
        }
        cost
    }

    fn read_alloc_diff_steps(
        &mut self,
        n_bands_ref: usize,
        alloc_ref: &[i32; NUM_ACE_BANDS],
        ref_guaranteed: i32,
        n_bands: usize,
    ) {
        let mut alloc_ref1 = [0i32; NUM_ACE_BANDS];
        let mut amax_arr = [0i32; NUM_ACE_BANDS];
        for band in 0..self.neb {
            alloc_ref1[band] = if alloc_ref[band] >= ref_guaranteed {
                alloc_ref[band] - ref_guaranteed
            } else {
                0
            };
            amax_arr[band] = self.alloc_max[band] - self.min_guaranteed;
        }
        let mut dmin = [0i32; NUM_ACE_BANDS];
        let mut dmax = [0i32; NUM_ACE_BANDS];
        let mut max_alpha = 0;
        for band in 0..self.num_coded_bands {
            let mut r = if band < n_bands_ref {
                alloc_ref1[band]
            } else {
                0
            };
            let amax = amax_arr[band];
            if r > amax {
                r = amax;
                alloc_ref1[band] = r;
            }
            let step = self.num_bits_step(band).max(1);
            dmax[band] = (amax - r + step - 1) / step;
            dmin[band] = -((r + step - 1) / step);
            let alpha = dmax[band] - dmin[band] + 1;
            max_alpha = max_alpha.max(alpha);
        }
        let k_alpha = num_bits((max_alpha - 1).max(0) as u32);
        let kparam = self.br.read_unary(k_alpha);
        for band in 0..self.num_coded_bands {
            let alpha = (dmax[band] - dmin[band] + 1).max(1);
            let d_min = dmin[band];
            let u = self.br.read_golomb(alpha as u32, kparam);
            let u = decode_center_map(alpha as u32, (-d_min) as u32, u);
            let delta = u as i32 + d_min;
            let r = if band < n_bands_ref {
                alloc_ref1[band]
            } else {
                0
            };
            let step = self.num_bits_step(band);
            let amax = amax_arr[band];
            let a = (r + step * delta).clamp(0, amax.max(0));
            self.alloc[band] = a;
        }
        for band in 0..n_bands {
            self.alloc[band] += self.min_guaranteed;
        }
    }

    fn compute_alloc_from_foundation(
        &mut self,
        choose_basic: bool,
        foundation: &[i32; NUM_ACE_BANDS],
    ) {
        let quant_level = self.br.read_unary(7);
        self.alloc = [0; NUM_ACE_BANDS];
        let mut alpha_quant = [0u32; NUM_ACE_BANDS];
        let mut alpha = 0u32;
        for band in 0..self.num_coded_bands {
            alpha_quant[band] = ((self.alloc_max[band] >> quant_level) + 1) as u32;
            alpha = alpha.max(alpha_quant[band]);
        }
        let kparam = self.br.read_uniform(alpha);
        for band in 0..self.num_coded_bands {
            if choose_basic {
                let d = self.br.read_golomb(alpha_quant[band], kparam);
                let delta = negative_rice_map_decode(d);
                let prev_alloc = foundation[band] / (1 << quant_level);
                let v = modular(prev_alloc + delta, 0, alpha_quant[band] as i32 - 1);
                self.alloc[band] = v << quant_level;
            } else {
                let v = self.br.read_golomb(alpha_quant[band], kparam) as i32;
                self.alloc[band] = v << quant_level;
            }
        }
    }

    fn adjust_refinement(&self, tentative: Bafp, num_bins: i32, band: usize) -> Bafp {
        let mut adjusted = tentative;
        let boost = Fx::<10>(REFINEMENT_BOOST[band]);
        let t = tentative.0 as i64;
        let nb = num_bins as i64 * 1024;
        match self.q.adjust_variant {
            0 => {
                let mut mult = 1;
                if t < 2 * nb {
                    mult = 2;
                }
                if t < 3 * nb {
                    adjusted = adjusted.add(boost.mul_int(num_bins * mult));
                }
            }
            1 => {
                let mut mult = 1;
                if t < 3 * nb {
                    adjusted = adjusted.add(boost.mul_int(mult * num_bins));
                }
                if t < 2 * nb {
                    mult = 2;
                    let _ = mult;
                }
            }
            2 => {
                let mut mult = 1;
                if t < 2 * nb {
                    mult = 2;
                }
                if t < 3 * nb {
                    adjusted = adjusted.add(boost.mul_int(mult));
                }
            }
            _ => {}
        }
        adjusted
    }

    fn compute_first_lognorm_refinement_alloc(&mut self) {
        const MAX_REF: i32 = 8;
        let mut carryover = 0i32;
        let nch = self.nch as i32;
        for band in 0..self.num_coded_bands {
            let l = self.cfg.bandsize[band] as i32;
            let mut band_allocation = self.alloc[band];
            let band_max = self.alloc_max[band];
            let total_bits = band_allocation + carryover;
            band_allocation = total_bits.min(band_max);
            let mut remainder = total_bits - band_allocation;
            let num_bins = l * nch;
            let delta = Fx::<10>(self.cfg.refinement_delta[band]);
            let mut tentative: Bafp = Fx::from_int(band_allocation).add(delta.mul_int(num_bins));
            tentative = self.adjust_refinement(tentative, num_bins, band);
            // (tentative + num_bins / 2) / num_bins with an arithmetic shift down to an integer
            let num = tentative.0 as i64 + (num_bins as i64 * 1024) / 2;
            let q = (num << 10).div_euclid(num_bins as i64 * 1024);
            let mut refinement = (q >> 10) as i32;
            if refinement < 0 {
                refinement = 0;
            }
            if refinement * nch > band_allocation {
                refinement = band_allocation / nch;
            }
            if refinement > MAX_REF {
                refinement = MAX_REF;
            }
            self.first_refine[band] = refinement;
            self.alloc[band] -= if self.q.refine_subtract_nch {
                refinement * nch
            } else {
                refinement
            };
            if remainder > 0 {
                let wish = MAX_REF - refinement;
                let increase = wish.min(remainder / nch);
                self.first_refine[band] += increase;
                remainder -= increase * nch;
            }
            carryover = remainder;
        }
        for band in self.num_coded_bands..self.neb {
            self.first_refine[band] = self.alloc[band] / nch;
            self.alloc[band] = 0;
        }
    }

    fn read_allocation(&mut self) -> Result<(), AceError> {
        self.alloc_delta = [0; NUM_ACE_BANDS];
        self.alloc = [0; NUM_ACE_BANDS];
        self.read_num_coded_bands();
        let remaining = self.br.bits_left() as i32;
        if self.num_coded_bands == 0 {
            if remaining >= self.neb as i32 * self.min_guaranteed {
                for b in 0..self.neb {
                    self.alloc[b] = self.min_guaranteed;
                }
            }
        } else {
            if self.is_stereo {
                self.read_num_ms_mono_bands();
                self.read_stereo_flags();
            }
            self.alloc_model = self.br.read_uniform(6);
            let mut choose_basic = false;
            match self.alloc_model {
                1 | 0 | 2 => {
                    self.read_alloc_model_parameter();
                    self.read_alloc_model_delta();
                }
                3 => self.read_alloc_model_parameter(),
                4 => self.alloc_model_parameter = ALLOC_MODEL_PARAMETER_DEFAULT,
                5 => {
                    choose_basic = self.br.read_bool();
                    if choose_basic {
                        self.read_alloc_model_parameter();
                    }
                }
                _ => return Err(AceError::Invalid("allocation model")),
            }
            let avail = self.br.bits_left() as i32;
            match self.alloc_model {
                1 => {
                    let adj = self.alloc_delta;
                    let mut a = [0; NUM_ACE_BANDS];
                    self.do_allocation_model(avail, &adj, &mut a);
                    self.alloc = a;
                }
                0 | 2 | 3 => {
                    let adj0 = match self.compute_adjustments_from_lognorms() {
                        Some(a) => a,
                        None => {
                            return Err(AceError::Unsupported(
                                "allocation models derived from primary lognorms need compute_unmasked_levels, which TS 103 491 does not define",
                            ))
                        }
                    };
                    let mut adj = adj0;
                    match self.alloc_model {
                        0 => {
                            for b in 0..self.neb {
                                adj[b] = (adj[b] + self.alloc_delta[b]).max(0);
                            }
                            let mut a = [0; NUM_ACE_BANDS];
                            self.do_allocation_model(avail, &adj, &mut a);
                            self.alloc = a;
                        }
                        2 => {
                            for b in 0..self.neb {
                                if adj[b] > self.num_bits_step(b) {
                                    adj[b] -= self.num_bits_step(b);
                                }
                                adj[b] += self.alloc_delta[b];
                            }
                            let mut a = [0; NUM_ACE_BANDS];
                            self.do_allocation_model(avail, &adj, &mut a);
                            self.alloc = a;
                        }
                        _ => {
                            let est =
                                self.estimate_cost_from_adjustments(self.num_coded_bands, &adj);
                            let mut alloc_ref = [0; NUM_ACE_BANDS];
                            self.do_allocation_model(avail - est, &adj, &mut alloc_ref);
                            let (n, g) = (self.num_coded_bands, self.min_guaranteed);
                            self.read_alloc_diff_steps(n, &alloc_ref, g, n);
                        }
                    }
                }
                4 => {
                    let (alloc_ref, n_ref, ref_g) = if !self.is_predictive || !self.prev.valid {
                        ([0; NUM_ACE_BANDS], 0usize, 0)
                    } else {
                        (
                            self.prev.alloc,
                            self.prev.num_coded_bands,
                            self.prev.min_guaranteed_alloc,
                        )
                    };
                    self.read_alloc_diff_steps(n_ref, &alloc_ref, ref_g, self.num_coded_bands);
                }
                5 => {
                    let est = [60, 72][(self.neff as usize - 1).min(1)];
                    let mut foundation = [0; NUM_ACE_BANDS];
                    if choose_basic {
                        let zeros = [0; NUM_ACE_BANDS];
                        self.do_allocation_model(avail - est, &zeros, &mut foundation);
                    }
                    self.compute_alloc_from_foundation(choose_basic, &foundation);
                }
                _ => {}
            }
        }
        self.compute_first_lognorm_refinement_alloc();
        let mut total_allocs = 0i32;
        for band in 0..self.neb {
            total_allocs += self.alloc[band];
            total_allocs += self.neff * self.first_refine[band];
        }
        self.unallocated_bits = self.br.bits_left() as i32 - total_allocs;
        Ok(())
    }

    fn refinement_correction(nbits: i32, bits: i32, level: i32) -> i32 {
        let half_level = if level + 1 >= 11 {
            0
        } else {
            1024 >> (level + 1)
        };
        let refinement = ((2 * bits as i64 + 1) << 10) / (1i64 << (nbits + level + 1)) as i64;
        refinement as i32 - half_level
    }

    fn do_first_lognorm_refinement(&mut self) {
        let mut bits = [[0i32; NUM_ACE_BANDS]; 2];
        for band in 0..self.neb {
            let n = self.first_refine[band] as u32;
            for ch in self.first_ch..=self.last_ch {
                bits[ch][band] = self.br.read_uint(n) as i32;
            }
        }
        for band in 0..self.neb {
            let n = self.first_refine[band];
            if n > 0 {
                for ch in self.first_ch..=self.last_ch {
                    self.lognorm_wo[ch][band] += Self::refinement_correction(n, bits[ch][band], 0);
                }
            }
        }
        self.transfer_lognorms();
    }

    fn compute_late_lognorm_refinement_alloc(&mut self) {
        const MAX_REF: i32 = 8;
        let neff = self.neff;
        let mut remaining = self.br.bits_left() as i32;
        remaining = neff * (remaining / neff);
        self.late_refine = [0; NUM_ACE_BANDS];
        for band in 0..self.neb {
            if self.first_refine[band] < MAX_REF {
                let bits = self.saved_late[band];
                self.late_refine[band] = bits;
                remaining -= bits * neff;
            }
        }
        let mut prev_remaining = 0;
        let mut rounds = 0;
        while remaining > 0 && (rounds < 2 || prev_remaining != remaining) {
            prev_remaining = remaining;
            for band in 0..self.neb {
                if remaining <= 0 {
                    break;
                }
                let nbits = self.first_refine[band] + self.late_refine[band];
                if nbits < MAX_REF && (rounds > 0 || self.saved_late[band] == 0) {
                    self.late_refine[band] += 1;
                    remaining -= neff;
                }
            }
            rounds += 1;
        }
    }

    fn do_late_lognorm_refinement(&mut self) {
        self.compute_late_lognorm_refinement_alloc();
        let mut bits = [[0i32; NUM_ACE_BANDS]; 2];
        for band in 0..self.neb {
            let n = self.late_refine[band] as u32;
            for ch in self.first_ch..=self.last_ch {
                bits[ch][band] = self.br.read_uint(n) as i32;
            }
        }
        for band in 0..self.neb {
            let n = self.late_refine[band];
            if n > 0 {
                for ch in self.first_ch..=self.last_ch {
                    self.lognorm_wo[ch][band] +=
                        Self::refinement_correction(n, bits[ch][band], self.first_refine[band]);
                }
            }
        }
        self.transfer_lognorms();
    }

    // ------------------------------------------------------------- band decoding

    fn distribute_running_balance(running_balance: i32, dist_to_end: i32) -> i32 {
        if dist_to_end > 4 {
            let (sign, rb) = if running_balance < 0 {
                (-1, -running_balance)
            } else {
                (1, running_balance)
            };
            // RBFP fraction 7/32 of the balance, truncated
            let v = (rb as i64 * 7) / 32;
            sign * v as i32
        } else {
            running_balance / dist_to_end.max(1)
        }
    }

    fn read_all_bands(&mut self) {
        let before = self.br.bits_used() as i32;
        let mut running_balance = self.unallocated_bits;
        let mut total_late_saved = 0i32;
        self.saved_late = [0; NUM_ACE_BANDS];
        for band in 0..self.neb {
            let used0 = self.br.bits_used() as i32;
            let remaining = self.total_bits - used0 - total_late_saved;
            let mut allocated = 0;
            if band < self.num_coded_bands {
                let cur = Self::distribute_running_balance(
                    running_balance,
                    (self.num_coded_bands - band) as i32,
                );
                allocated = remaining.min(self.alloc[band] + cur).max(0);
            }
            let coding_type = if !self.is_stereo {
                0
            } else {
                self.stereo_flag[band]
            };
            match coding_type {
                STEREO_LEFTRIGHT => {
                    self.read_one_band_leftright(band, allocated, remaining);
                }
                STEREO_MIDSIDE | STEREO_MIDSIDE_MONO => {
                    self.read_one_band_midside(band, allocated, remaining);
                }
                _ => {
                    self.read_one_band_mono(band, allocated, remaining, 0);
                }
            }
            let spent = self.br.bits_used() as i32 - used0;
            let surplus = self.alloc[band] - spent;
            running_balance += surplus;
            let unused = allocated - spent;
            let mut ref_bits = 0;
            if self.is_highres && unused > 0 && running_balance >= self.neff {
                ref_bits = 1;
            }
            self.saved_late[band] = ref_bits;
            running_balance -= ref_bits * self.neff;
            total_late_saved += ref_bits * self.neff;
        }
        let _ = before;
    }

    fn compute_leftright_allocations(&self, band: usize, allocated: i32) -> (i32, i32) {
        if self.coding_mode == CODING_LEFT {
            return (allocated, 0);
        }
        if self.coding_mode == CODING_RIGHT {
            return (0, allocated);
        }
        let mode = self.lr_region_mode[Self::compute_region(band)];
        const PARAMS: [(i32, i32, i32); 5] =
            [(1, 2, 1), (6, 10, 5), (6, 10, 5), (3, 4, 2), (3, 4, 2)];
        let (factor, den, round) = PARAMS[(mode as usize).min(4)];
        let larger_for_l = if mode == 0 {
            self.lognorm[0][band] >= self.lognorm[1][band]
        } else {
            mode == 1 || mode == 3
        };
        if larger_for_l {
            let l = (factor * allocated + round) / den;
            (l, allocated - l)
        } else {
            let r = (factor * allocated + round) / den;
            (allocated - r, r)
        }
    }

    fn read_one_band_leftright(&mut self, band: usize, allocated: i32, remaining: i32) -> i32 {
        let (a0, a1) = self.compute_leftright_allocations(band, allocated);
        let mut spent = self.read_one_band_mono(band, a0, remaining, 0);
        let rem2 = remaining - spent;
        spent = self.read_one_band_mono(band, a1, rem2, 1);
        spent
    }

    fn read_one_band_mono(
        &mut self,
        band: usize,
        allocated: i32,
        remaining: i32,
        ch: usize,
    ) -> i32 {
        let start = self.cfg.boundaries[band];
        let end = self.cfg.boundaries[band + 1];
        let size = end - start;
        let logn = self.cfg.log_band_size[band];
        let used0 = self.br.bits_used() as i32;
        let nblocks = self.num_band_blocks[band];
        let mut vec = vec![0f32; size];
        self.read_split_vector(allocated, remaining, size, nblocks, 0, logn, 1.0, &mut vec);
        transform::reshape_band(
            FRAME_LEN,
            start,
            end,
            nblocks,
            self.num_mdct_blocks,
            &mut vec,
        );
        self.spec[ch][start..end].copy_from_slice(&vec);
        self.br.bits_used() as i32 - used0
    }

    fn read_one_band_midside(&mut self, band: usize, allocated: i32, remaining: i32) -> i32 {
        let start = self.cfg.boundaries[band];
        let end = self.cfg.boundaries[band + 1];
        let n = end - start;
        let logn = self.cfg.log_band_size[band];
        let allocated = allocated.min(remaining);
        let used0 = self.br.bits_used() as i32;
        let sp = self.compute_split_parameters(allocated, remaining, 2 * n, logn + 16);
        let beta_bits = sp.spent;
        let mut local = beta_bits;
        let (mut bits_m, mut bits_s) = (sp.bits1, sp.bits2);
        const MIN_DIFF: i32 = 3;
        if bits_m >= bits_s {
            let s = self.read_one_band_mono(band, bits_m, remaining, 0);
            local += s;
            let diff = bits_m
                - if self.q.ms_diff_includes_beta {
                    local
                } else {
                    s
                };
            if diff > MIN_DIFF && !sp.is_final {
                bits_s += diff - MIN_DIFF;
            }
            local += self.read_one_band_mono(band, bits_s, remaining, 1);
        } else {
            let s = self.read_one_band_mono(band, bits_s, remaining, 1);
            local += s;
            let diff = bits_s
                - if self.q.ms_diff_includes_beta {
                    local
                } else {
                    s
                };
            if diff > MIN_DIFF && !sp.is_final {
                bits_m += diff - MIN_DIFF;
            }
            local += self.read_one_band_mono(band, bits_m, remaining, 0);
        }
        // midside_to_leftright
        for i in start..end {
            let m = sp.norm1 * self.spec[0][i];
            let s = sp.norm2 * self.spec[1][i];
            self.spec[0][i] = m + s;
            self.spec[1][i] = m - s;
        }
        vq::normalize(&mut self.spec[0][start..end], 1.0);
        vq::normalize(&mut self.spec[1][start..end], 1.0);
        let _ = used0;
        local
    }

    fn compute_quant_level(&self, bits: i32, n: usize, logn: i32) -> u32 {
        // all in LBFP (4 fractional bits)
        let fp_bits: Lbfp = Fx::from_int(bits);
        let half_log = Fx::<4>(logn >> 1);
        let e0 = fp_bits
            .add(half_log.sub(Fx::from_int(1)).mul_int(n as i32 - 1))
            .div_int(n as i32 - 1);
        let e1 = fp_bits.sub(Fx::from_int(2));
        let exponent = e0.min(e1);
        if exponent.0 < 8 {
            1
        } else if exponent.int_part() > 8 || (exponent.int_part() == 8 && exponent.frac_part() > 0)
        {
            256
        } else {
            let res = exponent.exp2().int_part() as u32;
            (res + 1) & !1
        }
    }

    fn compute_split_parameters(
        &mut self,
        allocated: i32,
        _remaining: i32,
        n: usize,
        logn: i32,
    ) -> SplitParams {
        let q = self.compute_quant_level(allocated, n, logn);
        let used0 = self.br.bits_used() as i32;
        let index = self.br.read_uniform(q + 1);
        let spent = self.br.bits_used() as i32 - used0;
        let bits = allocated - spent;
        if index == 0 {
            return SplitParams {
                spent,
                bits1: 0,
                bits2: bits,
                norm1: 0.0,
                norm2: 1.0,
                is_final: true,
            };
        }
        if index >= q {
            return SplitParams {
                spent,
                bits1: bits,
                bits2: 0,
                norm1: 1.0,
                norm2: 0.0,
                is_final: true,
            };
        }
        let half = (q / 2) as usize;
        let row = BETA_DEQUANT_ROWS[half.saturating_sub(1).min(127)];
        let int_beta: i32 = if (index as usize) <= half {
            row[(index as usize).min(row.len() - 1)]
        } else {
            -row[((q - index) as usize).min(row.len() - 1)]
        };
        let beta = int_beta as f64 / (1u64 << self.q.beta_shift) as f64;
        let fp_beta = Fx::<11>((int_beta as i64 * 2048 / (1i64 << self.q.beta_shift)) as i32);
        let fp_delta = fp_beta.mul_int(-(n as i32 - 1));
        let delta = fp_delta.round();
        let beta_term = (2.0f64).powf(2.0 * beta);
        let norm1 = (beta_term / (1.0 + beta_term)).sqrt() as f32;
        let norm2 = (1.0 / (1.0 + beta_term)).sqrt() as f32;
        let mut bits1 = bits.min((bits - delta + 1) / 2);
        if bits1 < 0 {
            bits1 = 0;
        }
        SplitParams {
            spent,
            bits1,
            bits2: bits - bits1,
            norm1,
            norm2,
            is_final: false,
        }
    }

    fn is_split_required(&self, allocated: i32, n: usize, depth: u32) -> bool {
        let max_depth = 3 + if self.is_highres { 1 } else { 0 };
        if depth > max_depth || n < 4 {
            return false;
        }
        allocated > crate::tables_gen::MAX_BITS_FOR_N[n - 2]
    }

    #[allow(clippy::too_many_arguments)]
    fn read_split_vector(
        &mut self,
        allocated: i32,
        remaining: i32,
        n: usize,
        num_blocks: usize,
        depth: u32,
        logn_band: i32,
        gain: f32,
        out: &mut [f32],
    ) -> i32 {
        let allocated = allocated.min(remaining);
        if self.is_split_required(allocated, n, depth) {
            let n1 = (n + 1) >> 1;
            let n2 = n - n1;
            let pnb = (num_blocks + 1) >> 1;
            let logn = logn_band - 16 * depth as i32;
            let sp = self.compute_split_parameters(allocated, remaining, n, logn);
            let mut remaining = remaining - sp.spent;
            let (mut bits1, mut bits2) = (sp.bits1, sp.bits2);
            let (v1, v2) = out.split_at_mut(n1);
            let mut local;
            const MIN_DIFF: i32 = 3;
            if bits1 >= bits2 {
                local = self.read_split_vector(
                    bits1,
                    remaining,
                    n1,
                    pnb,
                    depth + 1,
                    logn_band,
                    gain * sp.norm1,
                    v1,
                );
                remaining -= local;
                let diff = bits1 - local;
                if diff > MIN_DIFF && !sp.is_final {
                    bits2 += diff - MIN_DIFF;
                }
                local += self.read_split_vector(
                    bits2,
                    remaining,
                    n2,
                    pnb,
                    depth + 1,
                    logn_band,
                    gain * sp.norm2,
                    v2,
                );
            } else {
                local = self.read_split_vector(
                    bits2,
                    remaining,
                    n2,
                    pnb,
                    depth + 1,
                    logn_band,
                    gain * sp.norm2,
                    v2,
                );
                remaining -= local;
                let diff = bits2 - local;
                if diff > MIN_DIFF && !sp.is_final {
                    bits1 += diff - MIN_DIFF;
                }
                local += self.read_split_vector(
                    bits1,
                    remaining,
                    n1,
                    pnb,
                    depth + 1,
                    logn_band,
                    gain * sp.norm1,
                    v1,
                );
            }
            local + sp.spent
        } else {
            self.read_nonsplit_vector(allocated, remaining, n, num_blocks, gain, out)
        }
    }

    fn read_nonsplit_vector(
        &mut self,
        allocated: i32,
        remaining: i32,
        n: usize,
        num_blocks: usize,
        gain: f32,
        out: &mut [f32],
    ) -> i32 {
        let used0 = self.br.bits_used() as i32;
        let k = vq::get_best_k_for_n(allocated, remaining, n, self.is_highres);
        let mut pulses = vec![0i32; n];
        if k > 0 {
            let alpha = vq::pyramid(n, k).min(u32::MAX as u64 + 1);
            let index = if alpha > u32::MAX as u64 {
                self.br.read_uint(32) as u64
            } else {
                self.br.read_uniform(alpha as u32) as u64
            };
            vq::make_unsigned_pyramid_vector(index, n, k, &mut pulses);
            let nz = pulses.iter().filter(|&&v| v != 0).count().min(32);
            let mask = self.br.read_uint(nz as u32);
            // first read bit belongs to the lowest index non-zero entry
            let mut bit = nz as i32 - 1;
            for v in pulses.iter_mut() {
                if *v != 0 {
                    if (mask >> bit) & 1 == 1 {
                        *v = -*v;
                    }
                    bit -= 1;
                }
            }
            for (o, &p) in out.iter_mut().zip(pulses.iter()) {
                *o = p as f32;
            }
            vq::recondition(n, k, num_blocks, self.recond, out);
        } else {
            for o in out.iter_mut() {
                *o = 0.0;
            }
            if self.shf {
                for o in out.iter_mut() {
                    *o = rand_sign(self.rng);
                }
            }
        }
        vq::normalize(out, gain);
        self.br.bits_used() as i32 - used0
    }

    fn apply_lognorms(&mut self) {
        for ch in 0..self.nch {
            for band in 0..self.neb {
                let ln = self.lognorm[ch][band];
                let norm = if ln == LOGNORM_MINUS_INFINITY {
                    0.0
                } else {
                    (2.0f64).powf((ln as f64 / 1024.0).min(100.0)) as f32
                };
                for i in self.cfg.boundaries[band]..self.cfg.boundaries[band + 1] {
                    self.spec[ch][i] *= norm;
                }
            }
        }
    }
}

struct SplitParams {
    spent: i32,
    bits1: i32,
    bits2: i32,
    norm1: f32,
    norm2: f32,
    is_final: bool,
}

impl AceStream {
    pub fn new(is_stereo: bool) -> Self {
        let nch = if is_stereo { 2 } else { 1 };
        AceStream {
            is_stereo,
            nch,
            prev: PrevParse::empty(),
            td: vec![vec![0.0; FRAME_LEN]; nch],
            td_valid: false,
            td_short: false,
            td_lts: LtsParams {
                lag: 0,
                gains: [0.0; 3],
            },
            td_deemph: false,
            prev_lts: LtsParams {
                lag: 0,
                gains: [0.0; 3],
            },
            hist: vec![vec![0.0; FRAME_LEN]; nch],
            deemph_state: [0.0; 2],
            rng: 0x9e37_79b9,
            out: vec![vec![0.0; FRAME_LEN]; nch],
            out_valid: false,
            quirks: Quirks::default(),
            last_trace: StreamTrace::default(),
        }
    }

    /// Resets parse prediction state (sync frame) but keeps the time domain overlap so that
    /// playback stays continuous across sync frames.
    pub fn reset_parse_state(&mut self) {
        self.prev = PrevParse::empty();
    }

    /// Resets everything (random access).
    pub fn reset_all(&mut self) {
        let s = AceStream::new(self.is_stereo);
        let q = self.quirks.clone();
        *self = s;
        self.quirks = q;
    }

    pub fn decode_frame(
        &mut self,
        payload: &[u8],
        ctx: &FrameCtx,
    ) -> Result<StreamTrace, AceError> {
        let cfg: &'static BandsConfig = if ctx.sampling_rate == 44100 {
            &BANDS_44K1
        } else {
            &BANDS_48K
        };
        let neb = BANDWIDTH_MODE_TABLE[ctx.bandwidth_mode & 3];
        let total_bits = (payload.len() * 8) as i32;
        let q = self.quirks.clone();
        let prev = self.prev.clone();
        let mut rng = self.rng;
        let mut f = Frame {
            br: BitReader::new(payload),
            cfg,
            prev: &prev,
            q: &q,
            is_stereo: self.is_stereo,
            nch: self.nch,
            is_predictive: ctx.is_predictive,
            total_bits,
            neb,
            num_coded_bands: neb,
            coding_mode: CODING_FULL,
            first_ch: 0,
            last_ch: self.nch - 1,
            neff: self.nch as i32,
            is_highres: false,
            is_short: false,
            num_mdct_blocks: 1,
            shf: false,
            thf: false,
            recond: 0,
            lognorm_wo: [[LOGNORM_MINUS_INFINITY; NUM_ACE_BANDS]; 2],
            lognorm: [[LOGNORM_MINUS_INFINITY; NUM_ACE_BANDS]; 2],
            num_band_blocks: [1; NUM_ACE_BANDS],
            alloc: [0; NUM_ACE_BANDS],
            alloc_model: 0,
            alloc_model_parameter: ALLOC_MODEL_PARAMETER_DEFAULT,
            alloc_max: [0; NUM_ACE_BANDS],
            alloc_min: [0; NUM_ACE_BANDS],
            alloc_delta: [0; NUM_ACE_BANDS],
            min_guaranteed: 0,
            num_ms_mono_bands: 0,
            stereo_flag: [STEREO_LEFTRIGHT; NUM_ACE_BANDS],
            lr_region_mode: [0; 3],
            first_refine: [0; NUM_ACE_BANDS],
            saved_late: [0; NUM_ACE_BANDS],
            late_refine: [0; NUM_ACE_BANDS],
            unallocated_bits: 0,
            spec: [[0.0; FRAME_LEN]; 2],
            rng: &mut rng,
            lts_enabled: false,
            lts_lag: 0,
            lts_coeffs: [0.0; 3],
        };
        let mut trace = StreamTrace {
            bits_total: total_bits as usize,
            ..Default::default()
        };
        f.coding_mode = f.br.read_unary(4);
        // compute_effective_channels
        if !f.is_stereo || f.coding_mode == CODING_FULL {
            f.first_ch = 0;
            f.last_ch = f.nch - 1;
        } else {
            f.first_ch = if f.coding_mode == CODING_LEFT { 0 } else { 1 };
            f.last_ch = f.first_ch;
        }
        f.neff = (f.last_ch - f.first_ch + 1) as i32;
        f.min_guaranteed = f.neff;
        for band in 0..NUM_ACE_BANDS {
            let mut m = cfg.alloc_max[band] * f.neff;
            if f.is_stereo {
                m += cfg.bandsize[band] as i32;
            }
            f.alloc_max[band] = m;
            f.alloc_min[band] = (f.nch as i32).max(cfg.alloc_min[band]);
        }
        f.read_long_term_synthesis();
        trace.coding_mode = f.coding_mode;
        trace.lts_enabled = f.lts_enabled;
        trace.lts_lag = f.lts_lag;
        let mut result: Result<(), AceError> = Ok(());
        if f.coding_mode != CODING_OFF {
            f.is_highres = if f.total_bits < 1360 * f.neff {
                false
            } else {
                f.br.read_bool()
            };
            f.is_short = f.br.read_bool();
            f.num_mdct_blocks = if f.is_short { 8 } else { 1 };
            f.shf = f.br.read_bool();
            if f.is_short {
                f.thf = f.br.read_bool();
            }
            f.read_lognorm_primary();
            f.read_num_band_blocks();
            f.recond = f.br.read_uint(2);
            result = f.read_allocation();
            if result.is_ok() {
                f.do_first_lognorm_refinement();
                f.read_all_bands();
                f.do_late_lognorm_refinement();
                f.apply_lognorms();
            }
            trace.is_short = f.is_short;
            trace.is_highres = f.is_highres;
            trace.alloc_model = f.alloc_model;
            trace.num_coded_bands = f.num_coded_bands;
            trace.num_ms_mono_bands = f.num_ms_mono_bands;
            trace.shf = f.shf;
        }
        trace.bits_left_at_end = f.br.bits_left();
        trace.overrun = f.br.overrun();
        // convert band-contiguous spectra to the MDCT frame layout
        let mut spectrum = vec![vec![0f32; FRAME_LEN]; self.nch];
        if f.coding_mode != CODING_OFF && result.is_ok() {
            for ch in 0..self.nch {
                if f.is_short {
                    for band in 0..neb {
                        let start = cfg.boundaries[band];
                        let end = cfg.boundaries[band + 1];
                        let per = (end - start) / 8;
                        for blk in 0..8 {
                            for n in 0..per {
                                spectrum[ch][blk * 128 + start / 8 + n] =
                                    f.spec[ch][start + blk * per + n];
                            }
                        }
                    }
                } else {
                    spectrum[ch][..].copy_from_slice(&f.spec[ch][..]);
                }
            }
        }
        let is_short = f.is_short && f.coding_mode != CODING_OFF;
        // save the parse state for the next frame
        let new_prev = PrevParse {
            valid: true,
            lognorm_wo: f.lognorm_wo,
            num_coded_bands: f.num_coded_bands,
            alloc: f.alloc,
            min_guaranteed_alloc: f.min_guaranteed,
            alloc_model_parameter: f.alloc_model_parameter,
            num_ms_mono_bands: f.num_ms_mono_bands,
            lts_lag: f.lts_lag,
            lts_coeffs: f.lts_coeffs,
        };
        let (lts_lag, lts_coeffs) = (f.lts_lag, f.lts_coeffs);
        drop(f);
        self.last_trace = trace.clone();
        self.rng = rng;
        self.prev = new_prev;
        result?;
        self.finish_time_domain(
            spectrum,
            is_short,
            lts_lag,
            lts_coeffs,
            ctx.enable_deemphasis,
        );
        Ok(trace)
    }

    fn finish_time_domain(
        &mut self,
        mut spectrum: Vec<Vec<f32>>,
        is_short: bool,
        lag: u32,
        gains: [f32; 3],
        deemph: bool,
    ) {
        let n_curr = if is_short { 128 } else { 1024 };
        let n_prev = if self.td_short { 128 } else { 1024 };
        if self.td_valid {
            for ch in 0..self.nch {
                transform::windowed_imdct(&mut self.td[ch], &mut spectrum[ch], n_prev, n_curr);
            }
            // td now holds the completed frame: apply LTS (with the parameters of that frame)
            for ch in 0..self.nch {
                let mut buf = std::mem::take(&mut self.td[ch]);
                lts_apply(&mut buf, &self.hist[ch], &self.prev_lts, &self.td_lts);
                self.hist[ch].copy_from_slice(&buf);
                if self.td_deemph {
                    let mut p = self.deemph_state[ch];
                    for s in buf.iter_mut() {
                        *s += p;
                        p = 0.85 * *s;
                    }
                    self.deemph_state[ch] = p;
                }
                for v in buf.iter_mut() {
                    if !v.is_finite() {
                        *v = 0.0;
                    }
                }
                self.out[ch].copy_from_slice(&buf);
                self.td[ch] = buf;
            }
            self.out_valid = true;
            self.prev_lts = LtsParams {
                lag: self.td_lts.lag,
                gains: self.td_lts.gains,
            };
        } else {
            for ch in 0..self.nch {
                // first frame after a reset: nothing to complete; transform the current frame
                for blk in spectrum[ch].chunks_mut(n_curr) {
                    transform::idct4_block(blk);
                }
            }
            self.out_valid = false;
        }
        for ch in 0..self.nch {
            self.td[ch].copy_from_slice(&spectrum[ch]);
        }
        self.td_valid = true;
        self.td_short = is_short;
        self.td_lts = LtsParams { lag, gains };
        self.td_deemph = deemph;
    }
}

const LTS_WINDOW_TRANSITION: usize = 102;

fn lts_apply(buf: &mut [f32], hist: &[f32], prev: &LtsParams, curr: &LtsParams) {
    let n = buf.len();
    if prev.lag == 0 && curr.lag == 0 {
        return;
    }
    let gain_diff = (curr.gains[0] - prev.gains[0]).abs();
    let mut alpha = 2.0f32;
    if prev.lag == curr.lag {
        alpha = if gain_diff < 0.25 {
            1.75
        } else if gain_diff < 0.5 {
            2.0
        } else {
            2.5
        };
    }
    let mut pw = vec![0f32; n];
    let mut cw = vec![1f32; n];
    for i in 0..LTS_WINDOW_TRANSITION.min(n) {
        let w = (std::f32::consts::FRAC_PI_2 * (i as f32 + 0.5) / LTS_WINDOW_TRANSITION as f32)
            .sin()
            .powf(alpha);
        pw[i] = 1.0 - w;
        cw[i] = w;
    }
    // working buffer: history (1024) followed by the current frame
    let mut x = Vec::with_capacity(hist.len() + n);
    x.extend_from_slice(hist);
    x.extend_from_slice(buf);
    let off = hist.len() as isize;
    let get = |x: &Vec<f32>, i: isize| -> f32 {
        if i < 0 || i as usize >= x.len() {
            0.0
        } else {
            x[i as usize]
        }
    };
    for i in 0..n {
        let idx = off + i as isize;
        let mut acc = 0f32;
        if pw[i] != 0.0 && prev.lag != 0 {
            let l = prev.lag as isize;
            let t = (get(&x, idx - l - 2) + get(&x, idx - l + 2)) * prev.gains[2]
                + (get(&x, idx - l - 1) + get(&x, idx - l + 1)) * prev.gains[1]
                + get(&x, idx - l) * prev.gains[0];
            acc += pw[i] * t;
        }
        if cw[i] != 0.0 && curr.lag != 0 {
            let l = curr.lag as isize;
            let t = (get(&x, idx - l - 2) + get(&x, idx - l + 2)) * curr.gains[2]
                + (get(&x, idx - l - 1) + get(&x, idx - l + 1)) * curr.gains[1]
                + get(&x, idx - l) * curr.gains[0];
            acc += cw[i] * t;
        }
        x[idx as usize] += acc;
    }
    buf.copy_from_slice(&x[hist.len()..]);
}
