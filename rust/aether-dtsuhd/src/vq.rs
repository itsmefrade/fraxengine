//! Pyramid vector quantizer helpers (ETSI TS 103 491 clause 9.10.4).

use crate::config::MAX_BAND_SIZE;
use crate::fixed::Fx;
use crate::tables_gen::{K_ESTIMATION_COEFFICIENTS, MAX_BITS_FOR_N, MAX_K_FOR_N};
use std::sync::OnceLock;

const MAX_K_TABLE: usize = 260;
const SAT: u64 = 1u64 << 62;

/// pyramid(n, k): number of length-n non-negative integer vectors with sum k,
/// saturated at 2^62. Row n=0 is unused.
fn pyramid_table() -> &'static Vec<Vec<u64>> {
    static T: OnceLock<Vec<Vec<u64>>> = OnceLock::new();
    T.get_or_init(|| {
        let mut t = vec![vec![0u64; MAX_K_TABLE]; MAX_BAND_SIZE + 2];
        // n = 1: exactly one vector for every k
        for k in 0..MAX_K_TABLE {
            t[1][k] = 1;
        }
        for n in 2..MAX_BAND_SIZE + 2 {
            t[n][0] = 1;
            for k in 1..MAX_K_TABLE {
                t[n][k] = (t[n - 1][k] + t[n][k - 1]).min(SAT);
            }
        }
        t
    })
}

/// Number of vectors of the unsigned pyramid code (N, K).
pub fn pyramid(n: usize, k: usize) -> u64 {
    if n == 0 {
        return if k == 0 { 1 } else { 0 };
    }
    if n > MAX_BAND_SIZE + 1 || k >= MAX_K_TABLE {
        return SAT;
    }
    pyramid_table()[n][k]
}

pub fn max_k_for_n(n: usize) -> i32 {
    if n < 1 || n > MAX_BAND_SIZE {
        return -1;
    }
    MAX_K_FOR_N[n - 1]
}

type Nkfp = Fx<9>;

/// get_best_k_for_n (clause 9.10.4.9.4).
pub fn get_best_k_for_n(
    allocated_bits: i32,
    remaining_bits: i32,
    n: usize,
    is_highres: bool,
) -> usize {
    const MAXIMUM_K_LOWRES: i32 = 128;
    const NUM_INDEX_BITS: i32 = 32;
    const NUM_DIMENSIONS_WITH_MAX_K: usize = 4;
    const NUM_DIMENSIONS_WITH_MAX_K_LOWRES: usize = 5;
    const MAXIMUM_WORST_CASE_EXCESS: i32 = 8;
    const MINIMUM_BITS_FOR_LINEAR_EXCESS_BOUNDING: i32 = 6;
    const MAXIMUM_N_FOR_LINEAR_EXCESS_BOUNDING: usize = 7;
    const A_COEFF: i64 = 78;
    const B_COEFF: i64 = 993;
    const FP_RES: u32 = 9;
    const FP_ROUNDING: i64 = 1 << (FP_RES - 1);
    const MAX_BITS_FOR_N_LOWRES: [i32; 5] = [10, 17, 23, 29, 35];

    let hard_limit = allocated_bits.min(remaining_bits);
    if hard_limit < 2 || n < 2 || n > MAX_BAND_SIZE {
        return 0;
    }
    let mut max_k = max_k_for_n(n);
    if !is_highres && max_k > MAXIMUM_K_LOWRES {
        max_k = MAXIMUM_K_LOWRES;
    }
    let max_bits = if !is_highres && n < NUM_DIMENSIONS_WITH_MAX_K_LOWRES + 2 {
        MAX_BITS_FOR_N_LOWRES[n - 2]
    } else if n < NUM_DIMENSIONS_WITH_MAX_K + 2 {
        MAX_BITS_FOR_N[n - 2]
    } else {
        NUM_INDEX_BITS + (n as i32).min(max_k)
    };
    let bits = hard_limit.min(max_bits);
    let (c0, c1) = K_ESTIMATION_COEFFICIENTS[n - 2];
    let mut fp_exponent: Nkfp = Fx::from_int(bits);
    fp_exponent = fp_exponent.div_int(n as i32 - 1);
    let int_exponent = fp_exponent.int_part();
    let frac_only: Nkfp = Fx(fp_exponent.frac_part());
    let fp_exp_factor = frac_only.exp2();
    let mut term = fp_exp_factor.mul_int(c1);
    let diff = int_exponent - FP_RES as i32;
    if diff >= 0 {
        term = Fx(((term.0 as i64) << diff.min(30)).clamp(i32::MIN as i64, i32::MAX as i64) as i32);
    } else {
        term = Fx(term.0 >> (-diff).min(31));
    }
    let fp_k = term.sub(Fx(c0));
    let mut k = fp_k.round();
    if k < 1 {
        k = 1;
    } else if k > max_k {
        k = max_k;
    }
    let gap = hard_limit - bits;
    if gap < MAXIMUM_WORST_CASE_EXCESS {
        let mut may_exceed_gap = true;
        if bits >= MINIMUM_BITS_FOR_LINEAR_EXCESS_BOUNDING {
            let ub = ((A_COEFF * bits as i64 + B_COEFF + FP_ROUNDING) >> FP_RES) as i32;
            if gap > ub {
                may_exceed_gap = false;
            }
        } else if n <= MAXIMUM_N_FOR_LINEAR_EXCESS_BOUNDING {
            let ub = (n as i32 - 1) / 2;
            if gap > ub {
                may_exceed_gap = false;
            }
        }
        if may_exceed_gap {
            let mut bits_for_signs = (n as i32).min(k);
            if bits_for_signs >= remaining_bits {
                k = hard_limit - 1;
                bits_for_signs = k;
            }
            let bits_for_index = remaining_bits - bits_for_signs;
            if bits_for_index < NUM_INDEX_BITS {
                let bfi = bits_for_index.max(0) as u32;
                let mut bit_mask: u32 = u32::MAX;
                if bfi < 32 {
                    bit_mask >>= 32 - bfi.max(0);
                    if bfi == 0 {
                        bit_mask = 0;
                    }
                }
                let mut complement: u32 = !bit_mask;
                while k > n as i32
                    && ((pyramid(n, k as usize).wrapping_sub(1)) as u32 & complement != 0
                        || pyramid(n, k as usize).wrapping_sub(1) >> 32 != 0)
                {
                    k -= 1;
                }
                while k > 0
                    && (((pyramid(n, k as usize).wrapping_sub(1)) as u32 & complement) != 0
                        || pyramid(n, k as usize).wrapping_sub(1) >> 32 != 0)
                {
                    k -= 1;
                    complement <<= 1;
                }
            }
        }
    }
    k.max(0) as usize
}

/// make_unsigned_pyramid_vector: decode `index` into a non-negative pulse vector.
pub fn make_unsigned_pyramid_vector(mut index: u64, n: usize, k: usize, out: &mut [i32]) {
    let mut n = n;
    let mut k = k;
    let mut pos = 0;
    loop {
        if n == 0 {
            return;
        }
        if n == 1 {
            out[pos] = k as i32;
            return;
        }
        if k == 0 {
            for v in out[pos..pos + n].iter_mut() {
                *v = 0;
            }
            return;
        }
        // largest j in [0, k-1] with index >= pyramid(n, j); j = -1 otherwise
        let mut jj: i64 = -1;
        let mut j = k as i64 - 1;
        while j >= 0 {
            if index >= pyramid(n, j as usize) {
                jj = j;
                break;
            }
            j -= 1;
        }
        out[pos] = (k as i64 - jj - 1) as i32;
        if jj >= 0 {
            index -= pyramid(n, jj as usize);
        }
        n -= 1;
        k = (jj + 1) as usize;
        pos += 1;
    }
}

/// Reconditioning (clause 9.10.4.7).
pub fn recondition(n: usize, k: usize, num_blocks: usize, level: u32, v: &mut [f32]) {
    const TABLE: [f32; 4] = [0.0, 0.36, 0.47, 0.75];
    if num_blocks == 0 || 2 * k >= num_blocks || level == 0 || k == 0 {
        return;
    }
    let num_samples_block = n / num_blocks;
    let sigma = (3.0f32 * ((k as f32 / n as f32).powf(0.73)) * TABLE[level as usize & 3]).exp();
    let num_segments = num_samples_block / 4;
    let left = num_samples_block % 4;
    let m = make_matrix_4(sigma);
    if num_segments == 0 {
        return;
    }
    for blk in 0..num_blocks {
        let base = blk * num_samples_block;
        if left != 0 {
            for seg in 0..num_segments {
                apply_matrix4(&m, &mut v[base + seg * 4..base + seg * 4 + 4]);
            }
            v[base..base + num_samples_block].reverse();
        }
        for seg in 0..num_segments {
            apply_matrix4(&m, &mut v[base + seg * 4..base + seg * 4 + 4]);
        }
        interleave_vector(&mut v[base..base + num_samples_block], 4);
    }
}

fn make_matrix_4(sigma: f32) -> [[f32; 4]; 4] {
    let pi_4 = std::f32::consts::FRAC_PI_4;
    let pi_2 = std::f32::consts::FRAC_PI_2;
    let a = (sigma * pi_4).cos();
    let b = (sigma * pi_4).sin();
    let c = (sigma * pi_2).sin();
    let d = c;
    let div = (a * a + 2.0 * b * b + c * c).sqrt();
    [
        [a / div, -b / div, b / div, -d / div],
        [b / div, a / div, d / div, b / div],
        [-b / div, -d / div, a / div, b / div],
        [d / div, -b / div, -b / div, a / div],
    ]
}

fn apply_matrix4(m: &[[f32; 4]; 4], v: &mut [f32]) {
    let mut out = [0f32; 4];
    for i in 0..4 {
        for j in 0..4 {
            out[i] += m[i][j] * v[j];
        }
    }
    v[..4].copy_from_slice(&out);
}

fn interleave_vector(v: &mut [f32], size: usize) {
    let length = v.len();
    let rows = size;
    let cols = length / size;
    let mut out = v.to_vec();
    for m in 0..rows {
        for n in 0..cols {
            out[m * cols + n] = v[n * rows + m];
        }
    }
    v.copy_from_slice(&out);
}

/// normalize(): scale to L2 norm `gain` (all-zero vectors stay zero).
pub fn normalize(v: &mut [f32], gain: f32) {
    let mut e = 0f64;
    for &x in v.iter() {
        e += (x as f64) * (x as f64);
    }
    let n = e.sqrt();
    if n <= 0.0 {
        for x in v.iter_mut() {
            *x = 0.0;
        }
        return;
    }
    let f = (gain as f64 / n) as f32;
    for x in v.iter_mut() {
        *x *= f;
    }
}
