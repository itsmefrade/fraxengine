//! SIMD row kernels for 16-bit-sample motion compensation (Main 10): the N-tap interpolation filters
//! of spec 8.5.3.3.3 and the final rounding / bi-prediction average, on `crate::simd` vectors. Every
//! kernel produces exactly the values of the scalar code in `inter_pred` (the tests compare them on
//! random data); `inter_pred` calls them only when `crate::simd::HAVE_SIMD` and the samples are u16.
//!
//! Filters are specialised per fractional phase through const generics, so zero taps disappear and the
//! half-sample luma filter (symmetric) pre-adds its tap pairs. Rows are processed in 8-wide chunks; the
//! last chunk of a row overlaps the one before it (it recomputes the same values) instead of using a
//! scalar tail, and rows narrower than 8 use 4-wide chunks.
#![allow(clippy::too_many_arguments)]

use crate::simd::*;

const QPEL: [[i16; 8]; 4] = [
    [0, 0, 0, 64, 0, 0, 0, 0],
    [-1, 4, -10, 58, 17, -5, 1, 0],
    [-1, 4, -11, 40, 40, -11, 4, -1],
    [0, 1, -5, 17, 58, -10, 4, -1],
];
const EPEL: [[i16; 4]; 8] = [
    [0, 64, 0, 0],
    [-2, 58, 10, -2],
    [-4, 54, 16, -2],
    [-6, 46, 28, -4],
    [-4, 36, 36, -4],
    [-4, 28, 46, -6],
    [-2, 16, 54, -4],
    [-2, 10, 58, -2],
];

const fn coefs<const N: usize, const F: usize>() -> [i16; N] {
    let mut o = [0i16; N];
    let mut k = 0;
    while k < N {
        o[k] = if N == 8 { QPEL[F & 3][k & 7] } else { EPEL[F & 7][k & 3] };
        k += 1;
    }
    o
}

/// Eight output lanes (as two i32 vectors) of `sum(f[k] * tap(k))`. `ld(k)` loads the eight inputs of tap
/// `k`. `PRE_ADD` allows adding the two taps of a symmetric pair first; only valid for pixel inputs
/// (two samples sum below 2^15).
#[inline(always)]
fn mac8<const N: usize, const F: usize, const PRE_ADD: bool>(ld: impl Fn(usize) -> V16) -> (V32, V32) {
    let f = coefs::<N, F>();
    let (mut lo, mut hi) = (splat32(0), splat32(0));
    if PRE_ADD && N == 8 && F == 2 {
        for k in 0..4 {
            let s = add16(ld(k), ld(7 - k));
            let c = splat16(f[k]);
            lo = mac_lo(lo, s, c);
            hi = mac_hi(hi, s, c);
        }
        return (lo, hi);
    }
    for k in 0..N {
        if f[k] != 0 {
            let c = splat16(f[k]);
            let v = ld(k);
            lo = mac_lo(lo, v, c);
            hi = mac_hi(hi, v, c);
        }
    }
    (lo, hi)
}

/// Four output lanes (low half only); the inputs come from `ld4`.
#[inline(always)]
fn mac4<const N: usize, const F: usize, const PRE_ADD: bool>(ld: impl Fn(usize) -> V16) -> V32 {
    let f = coefs::<N, F>();
    let mut lo = splat32(0);
    if PRE_ADD && N == 8 && F == 2 {
        for k in 0..4 {
            lo = mac_lo(lo, add16(ld(k), ld(7 - k)), splat16(f[k]));
        }
        return lo;
    }
    for k in 0..N {
        if f[k] != 0 {
            lo = mac_lo(lo, ld(k), splat16(f[k]));
        }
    }
    lo
}

/// Shared row driver: `tap(k, x)` returns the pointer of tap `k` at output column `x`.
#[inline(always)]
fn row<const N: usize, const F: usize, const PRE_ADD: bool>(
    out: &mut [i16],
    shift: u32,
    tap: impl Fn(usize, usize) -> *const i16,
) {
    let w = out.len();
    let op = out.as_mut_ptr();
    // SAFETY: callers assert that every tap pointer for columns 0..w stays inside their input slice
    // (8 lanes from column x <= w - 8, or 4 lanes from x <= w - 4), and `op` covers `w` outputs.
    unsafe {
        if w >= 8 {
            let mut x = 0;
            loop {
                let (lo, hi) = mac8::<N, F, PRE_ADD>(|k| ld(tap(k, x)));
                st(op.add(x), narrow32(shr32v(lo, shift), shr32v(hi, shift)));
                if x + 8 >= w {
                    break;
                }
                x = (x + 8).min(w - 8);
            }
        } else if w >= 4 {
            let mut x = 0;
            loop {
                let lo = shr32v(mac4::<N, F, PRE_ADD>(|k| ld4(tap(k, x))), shift);
                st4(op.add(x), narrow32(lo, lo));
                if x + 4 >= w {
                    break;
                }
                x = w - 4;
            }
        } else {
            for x in 0..w {
                let mut a = 0i32;
                let f = coefs::<N, F>();
                for k in 0..N {
                    a += f[k] as i32 * *tap(k, x) as i32;
                }
                *op.add(x) = (a >> shift) as i16;
            }
        }
    }
}

/// `out[i] = (sum_k f[k] * src[i + k]) >> shift` for the filter of phase `F`.
#[inline(always)]
pub(crate) fn h_row<const N: usize, const F: usize>(out: &mut [i16], src: &[u16], shift: u32) {
    assert!(src.len() + 1 > out.len() + N - 1 && !out.is_empty());
    let sp = src.as_ptr() as *const i16;
    // SAFETY: column x + tap k <= w - 1 + N - 1 < src.len() (asserted above); chunks never exceed w.
    row::<N, F, true>(out, shift, |k, x| unsafe { sp.add(x + k) });
}

/// `out[i] = (sum_k f[k] * src[base + k * stride + i]) >> shift` over pixel rows.
#[inline(always)]
pub(crate) fn v_px_row<const N: usize, const F: usize>(
    out: &mut [i16],
    src: &[u16],
    base: usize,
    stride: usize,
    shift: u32,
) {
    assert!(!out.is_empty() && base + (N - 1) * stride + out.len() <= src.len());
    let sp = src.as_ptr() as *const i16;
    // SAFETY: row k <= N - 1 and column <= w - 1 stay below src.len() (asserted above).
    row::<N, F, true>(out, shift, |k, x| unsafe { sp.add(base + k * stride + x) });
}

/// Second pass of a 2-D filter over 16-bit intermediates.
#[inline(always)]
pub(crate) fn v_i16_row<const N: usize, const F: usize>(
    out: &mut [i16],
    src: &[i16],
    base: usize,
    stride: usize,
    shift: u32,
) {
    assert!(!out.is_empty() && base + (N - 1) * stride + out.len() <= src.len());
    let sp = src.as_ptr();
    // SAFETY: as for `v_px_row`.
    row::<N, F, false>(out, shift, |k, x| unsafe { sp.add(base + k * stride + x) });
}

/// Block-level drivers: the phase is dispatched once per block instead of once per row. `out(j)` yields output
/// row `j` (a buffer of `w` lanes) and `done(j)` is called after it is written.
#[inline(always)]
pub(crate) fn h_block<const N: usize, const F: usize>(
    h: usize,
    shift: u32,
    src: &[u16],
    org: usize,
    src_stride: usize,
    w: usize,
    mut out: impl FnMut(usize) -> *mut [i16],
    mut done: impl FnMut(usize),
) {
    for j in 0..h {
        // SAFETY: `out` returns a live exclusive row buffer of `w` lanes that nothing else touches until
        // `done(j)`; the pointer is not retained.
        let o = unsafe { &mut *out(j) };
        h_row::<N, F>(o, &src[org + j * src_stride..][..w + N - 1], shift);
        done(j);
    }
}

#[inline(always)]
pub(crate) fn v_px_block<const N: usize, const F: usize>(
    h: usize,
    shift: u32,
    src: &[u16],
    org: usize,
    src_stride: usize,
    mut out: impl FnMut(usize) -> *mut [i16],
    mut done: impl FnMut(usize),
) {
    for j in 0..h {
        // SAFETY: as in `h_block`.
        let o = unsafe { &mut *out(j) };
        v_px_row::<N, F>(o, src, org + j * src_stride, src_stride, shift);
        done(j);
    }
}

#[inline(always)]
pub(crate) fn v_i16_block<const N: usize, const F: usize>(
    h: usize,
    src: &[i16],
    stride: usize,
    mut out: impl FnMut(usize) -> *mut [i16],
    mut done: impl FnMut(usize),
) {
    for j in 0..h {
        // SAFETY: as in `h_block`.
        let o = unsafe { &mut *out(j) };
        v_i16_row::<N, F>(o, src, j * stride, stride, 6);
        done(j);
    }
}

/// Dispatch a block driver on the phase (`frac` in 1..=3 for N = 8, 1..=7 for N = 4).
macro_rules! block_by_phase {
    ($n:ident, $frac:expr, $f:ident, ($($a:expr),*)) => {
        if $n == 8 {
            match $frac {
                1 => $f::<$n, 1>($($a),*),
                2 => $f::<$n, 2>($($a),*),
                _ => $f::<$n, 3>($($a),*),
            }
        } else {
            match $frac {
                1 => $f::<$n, 1>($($a),*),
                2 => $f::<$n, 2>($($a),*),
                3 => $f::<$n, 3>($($a),*),
                4 => $f::<$n, 4>($($a),*),
                5 => $f::<$n, 5>($($a),*),
                6 => $f::<$n, 6>($($a),*),
                _ => $f::<$n, 7>($($a),*),
            }
        }
    };
}
pub(crate) use block_by_phase;

/// Integer-position row: `out[i] = src[i] << shift`.
pub(crate) fn copy_row(out: &mut [i16], src: &[u16], shift: u32) {
    let w = out.len();
    assert!(src.len() >= w);
    let (sp, op) = (src.as_ptr() as *const i16, out.as_mut_ptr());
    // SAFETY: indices below w on both slices.
    unsafe {
        let mut x = 0;
        while x + 8 <= w {
            st(op.add(x), shl16v(ld(sp.add(x)), shift));
            x += 8;
        }
        while x < w {
            *op.add(x) = (*sp.add(x)) << shift;
            x += 1;
        }
    }
}

/// Column driver for the integer-position kernels: `op8(x)` handles the 8 lanes from column `x`, `op4` 4 lanes
/// for rows narrower than 8, `op1` one lane for rows narrower than 4. The last chunk overlaps the previous
/// one, which is harmless since the output never aliases the inputs (they are reference pictures).
#[inline(always)]
fn px_chunks(w: usize, mut op8: impl FnMut(usize), mut op4: impl FnMut(usize), mut op1: impl FnMut(usize)) {
    if w >= 8 {
        let mut x = 0;
        loop {
            op8(x);
            if x + 8 >= w {
                break;
            }
            x = (x + 8).min(w - 8);
        }
    } else if w >= 4 {
        op4(0);
        if w > 4 {
            op4(w - 4);
        }
    } else {
        for x in 0..w {
            op1(x);
        }
    }
}

/// Integer-position uni-prediction without weights: `dst[i] = src[i]` (see `inter_pred::copy_block`).
#[inline(always)]
pub(crate) fn copy_px_row(dst: &mut [u16], src: &[u16]) {
    let w = dst.len();
    assert!(src.len() >= w);
    let (sp, dp) = (src.as_ptr() as *const i16, dst.as_mut_ptr() as *mut i16);
    // SAFETY: every chunk lies within columns 0..w of both slices.
    unsafe {
        px_chunks(
            w,
            |x| st(dp.add(x), ld(sp.add(x))),
            |x| st4(dp.add(x), ld4(sp.add(x))),
            |x| *dp.add(x) = *sp.add(x),
        );
    }
}

/// Integer-position bi-prediction without weights: `dst[i] = (a[i] + b[i] + 1) >> 1` (see
/// `inter_pred::avg_block`).
#[inline(always)]
pub(crate) fn avg_px_row(dst: &mut [u16], a: &[u16], b: &[u16]) {
    let w = dst.len();
    assert!(a.len() >= w && b.len() >= w);
    let (ap, bp, dp) = (a.as_ptr() as *const i16, b.as_ptr() as *const i16, dst.as_mut_ptr() as *mut i16);
    // SAFETY: every chunk lies within columns 0..w of all three slices.
    unsafe {
        px_chunks(
            w,
            |x| st(dp.add(x), avgr16(ld(ap.add(x)), ld(bp.add(x)))),
            |x| st4(dp.add(x), avgr16(ld4(ap.add(x)), ld4(bp.add(x)))),
            |x| *dp.add(x) = ((*ap.add(x) as u16 as u32 + *bp.add(x) as u16 as u32 + 1) >> 1) as i16,
        );
    }
}

/// `dst[i] = clip((a[i] (+ b[i]) + offset) >> shift, 0, max)` with saturating 16-bit adds (exact after
/// the clip, see `inter_pred::bi_average_row`). `b = None` is the uni-prediction rounding.
#[inline(always)]
pub(crate) fn round_row(
    dst: &mut [u16],
    a: &[i16],
    b: Option<&[i16]>,
    offset: i16,
    shift: u32,
    max: i16,
) {
    let w = dst.len();
    assert!(a.len() >= w && b.is_none_or(|b| b.len() >= w));
    let (dp, ap) = (dst.as_mut_ptr() as *mut i16, a.as_ptr());
    let (voff, vmax, vzero) = (splat16(offset), splat16(max), zero());
    // SAFETY: chunks stay below w on all slices.
    unsafe {
        let mut x = 0;
        while x + 8 <= w {
            let mut v = ld(ap.add(x));
            if let Some(b) = b {
                v = adds16(v, ld(b.as_ptr().add(x)));
            }
            v = shr16v(adds16(v, voff), shift);
            st(dp.add(x), min16(max16(v, vzero), vmax));
            x += 8;
        }
        while x < w {
            let mut v = *ap.add(x);
            if let Some(b) = b {
                v = v.saturating_add(*b.as_ptr().add(x));
            }
            *dp.add(x) = (v.saturating_add(offset) >> shift).clamp(0, max) as u16 as i16;
            x += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lcg(s: &mut u64) -> u32 {
        *s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (*s >> 33) as u32
    }

    fn reference<const N: usize>(f: &[i16; N], vals: impl Fn(usize) -> i32, shift: u32) -> i16 {
        let a: i32 = (0..N).map(|k| f[k] as i32 * vals(k)).sum();
        (a >> shift) as i16
    }

    fn check_phase<const N: usize, const F: usize>() {
        let mut seed = 7u64 + F as u64;
        let f = coefs::<N, F>();
        for &w in &[2usize, 3, 4, 5, 6, 8, 12, 16, 20, 24, 32, 48, 64] {
            for trial in 0..40 {
                let max = if trial % 3 == 0 { 1023 } else { 1023u32 };
                let stride = 70usize;
                let src: Vec<u16> = (0..stride * (N + 1))
                    .map(|_| match trial % 4 {
                        0 => 0,
                        1 => 1023,
                        _ => (lcg(&mut seed) % (max + 1)) as u16,
                    })
                    .collect();
                // horizontal
                let mut out = vec![0i16; w];
                let want: Vec<i16> =
                    (0..w).map(|i| reference(&f, |k| src[i + k] as i32, 2)).collect();
                if w >= 1 && (N == 4 || F <= 3) {
                    h_row::<N, F>(&mut out, &src[..w + N - 1], 2);
                    assert_eq!(out, want, "h N={N} F={F} w={w}");
                    // vertical over pixels
                    let want: Vec<i16> =
                        (0..w).map(|i| reference(&f, |k| src[3 + k * stride + i] as i32, 2)).collect();
                    v_px_row::<N, F>(&mut out, &src, 3, stride, 2);
                    assert_eq!(out, want, "v N={N} F={F} w={w}");
                    // vertical over 16-bit intermediates (full i16 range of first-pass results)
                    let mid: Vec<i16> =
                        src.iter().map(|&p| ((p as i32 * 22 - 6138).clamp(-6138, 22506)) as i16).collect();
                    let want: Vec<i16> =
                        (0..w).map(|i| reference(&f, |k| mid[3 + k * stride + i] as i32, 6)).collect();
                    v_i16_row::<N, F>(&mut out, &mid, 3, stride, 6);
                    assert_eq!(out, want, "v16 N={N} F={F} w={w}");
                }
            }
        }
    }

    #[test]
    fn luma_phases_match_scalar() {
        check_phase::<8, 1>();
        check_phase::<8, 2>();
        check_phase::<8, 3>();
    }

    #[test]
    fn chroma_phases_match_scalar() {
        check_phase::<4, 1>();
        check_phase::<4, 2>();
        check_phase::<4, 3>();
        check_phase::<4, 4>();
        check_phase::<4, 5>();
        check_phase::<4, 6>();
        check_phase::<4, 7>();
    }

    #[test]
    fn rounding_and_copy_match_scalar() {
        let mut seed = 99u64;
        for &w in &[1usize, 2, 3, 4, 6, 7, 8, 9, 12, 16, 24, 64] {
            let a: Vec<i16> = (0..w).map(|_| (lcg(&mut seed) % 40000) as i32 as i16 - 3000).collect();
            let b: Vec<i16> = (0..w).map(|_| (lcg(&mut seed) % 40000) as i32 as i16 - 3000).collect();
            let mut d = vec![0u16; w];
            round_row(&mut d, &a, Some(&b), 16, 5, 1023);
            for i in 0..w {
                let v = a[i].saturating_add(b[i]).saturating_add(16) >> 5;
                assert_eq!(d[i], v.clamp(0, 1023) as u16);
            }
            round_row(&mut d, &a, None, 8, 4, 1023);
            for i in 0..w {
                assert_eq!(d[i], (a[i].saturating_add(8) >> 4).clamp(0, 1023) as u16);
            }
            let s: Vec<u16> = (0..w).map(|_| (lcg(&mut seed) % 1024) as u16).collect();
            let mut o = vec![0i16; w];
            copy_row(&mut o, &s, 4);
            for i in 0..w {
                assert_eq!(o[i], (s[i] << 4) as i16);
            }
            let t: Vec<u16> = (0..w).map(|_| (lcg(&mut seed) % 4096) as u16).collect();
            let mut p = vec![0u16; w];
            copy_px_row(&mut p, &t);
            assert_eq!(p, t);
            avg_px_row(&mut p, &s, &t);
            for i in 0..w {
                assert_eq!(p[i], ((s[i] as u32 + t[i] as u32 + 1) >> 1) as u16);
            }
        }
    }
}
