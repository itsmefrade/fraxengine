//! SIMD kernels for 16-bit-sample intra prediction and residual addition (`crate::simd`). Each equals the
//! scalar function it replaces in `intra_pred` exactly; the tests compare them on random data.
//! Samples are at most 10 bits wide for the kernels that use 16-bit arithmetic (`max <= 1023`).

use crate::simd::*;

/// `dst[y * stride + x] = clip(dst[y * stride + x] + residual[y * size + x])` for a `size x size` block
/// (`size` = 4, 8, 16 or 32). The saturating 16-bit add is exact after the clip.
pub(crate) fn add_residual(dst: &mut [u16], stride: usize, residual: &[i16], log2: u8, max: u16) {
    let size = 1usize << log2;
    assert!(residual.len() >= size * size && dst.len() >= (size - 1) * stride + size);
    let (dp, rp) = (dst.as_mut_ptr() as *mut i16, residual.as_ptr());
    let (vmax, vzero) = (splat16(max as i16), zero());
    // SAFETY: row y < size and column < size on both slices (asserted above).
    unsafe {
        for y in 0..size {
            let (d, r) = (dp.add(y * stride), rp.add(y * size));
            if size >= 8 {
                let mut x = 0;
                while x < size {
                    let v = adds16(ld(d.add(x)), ld(r.add(x)));
                    st(d.add(x), min16(max16(v, vzero), vmax));
                    x += 8;
                }
            } else {
                let v = adds16(ld4(d), ld4(r));
                st4(d, min16(max16(v, vzero), vmax));
            }
        }
    }
}

/// Fill a `size x size` block with `value`.
pub(crate) fn fill_block(dst: &mut [u16], stride: usize, log2: u8, value: u16) {
    let size = 1usize << log2;
    assert!(dst.len() >= (size - 1) * stride + size);
    let dp = dst.as_mut_ptr() as *mut i16;
    let v = splat16(value as i16);
    // SAFETY: row y < size, column < size (asserted above).
    unsafe {
        for y in 0..size {
            let d = dp.add(y * stride);
            if size >= 8 {
                let mut x = 0;
                while x < size {
                    st(d.add(x), v);
                    x += 8;
                }
            } else {
                st4(d, v);
            }
        }
    }
}

/// Planar prediction (spec 8.4.4.2.5) of a `size x size` block from `top` / `left` (corner at index 0).
pub(crate) fn planar(dst: &mut [u16], stride: usize, top: &[u16], left: &[u16], log2: u8) {
    let size = 1usize << log2;
    assert!(dst.len() >= (size - 1) * stride + size && top.len() > size + size && left.len() > size + size);
    let shift = log2 as u32 + 1;
    let top_right = top[1 + size] as i32;
    let bottom_left = left[1 + size] as i32;
    let dp = dst.as_mut_ptr() as *mut i16;
    let lanes = if size >= 8 { 8 } else { 4 };
    // Per column x: a(x) = size-1-x (multiplies left[y]), b(x) = (x+1) * top_right (fixed), c(x) = (size-1-y)
    // * top[x] (stepped down by top[x] per row).
    let mut x0 = 0;
    while x0 < size {
        let mut coef = [0i16; 8];
        let mut tops = [0i16; 8];
        let mut b = [0i32; 8];
        for l in 0..lanes {
            let x = x0 + l;
            coef[l] = (size - 1 - x) as i16;
            tops[l] = top[1 + x] as i16;
            b[l] = (x as i32 + 1) * top_right;
        }
        // SAFETY: arrays are 8 long; row y < size and columns x0..x0 + lanes < size + (lanes - size.min(lanes)).
        unsafe {
            let a_v = ld(coef.as_ptr());
            let t_v = ld(tops.as_ptr());
            let b_lo = ld_i32(b.as_ptr());
            let b_hi = ld_i32(b.as_ptr().add(4));
            // c starts at (size - 1) * top[x] for y = 0.
            let k = splat16((size - 1) as i16);
            let (mut c_lo, mut c_hi) = (mul_lo(t_v, k), mul_hi(t_v, k));
            for y in 0..size {
                let ly = splat16(left[1 + y] as i16);
                let d = splat32((y as i32 + 1) * bottom_left + size as i32);
                let lo = add32(add32(add32(mul_lo(a_v, ly), b_lo), c_lo), d);
                let hi = add32(add32(add32(mul_hi(a_v, ly), b_hi), c_hi), d);
                let out = narrow32(shr32v(lo, shift), shr32v(hi, shift));
                let dy = dp.add(y * stride + x0);
                if lanes == 8 {
                    st(dy, out);
                } else {
                    st4(dy, out);
                }
                c_lo = sub32(c_lo, widen_lo(t_v));
                c_hi = sub32(c_hi, widen_hi(t_v));
            }
        }
        x0 += lanes;
    }
}

/// One row of angular interpolation: `out[x] = ((32 - fact) * r[x] + fact * r[x + 1] + 16) >> 5`, in 16-bit
/// arithmetic (the weights sum to 32, so for samples below 2^10 the sum stays below 2^15).
pub(crate) fn angular_row(out: &mut [u16], r: &[u16], fact: i32) {
    let w = out.len();
    assert!(r.len() >= w + 1);
    let (op, rp) = (out.as_mut_ptr() as *mut i16, r.as_ptr() as *const i16);
    let (wa, wb, rnd) = (splat16((32 - fact) as i16), splat16(fact as i16), splat16(16));
    // SAFETY: loads read r[x..x + 8] and r[x + 1..x + 9] with x + 8 <= w, so at most index w; the 4-wide form
    // reads r[0..4] / r[1..5] with w == 4.
    unsafe {
        if w >= 8 {
            let mut x = 0;
            while x < w {
                let v = add16(add16(mul16(ld(rp.add(x)), wa), mul16(ld(rp.add(x + 1)), wb)), rnd);
                st(op.add(x), shr16::<5>(v));
                x += 8;
            }
        } else {
            let v = add16(add16(mul16(ld4(rp), wa), mul16(ld4(rp.add(1)), wb)), rnd);
            st4(op, shr16::<5>(v));
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

    #[test]
    fn add_residual_matches_scalar() {
        let mut seed = 3u64;
        for log2 in 2..=5u8 {
            let size = 1usize << log2;
            for _ in 0..50 {
                let stride = size + 7;
                let mut dst: Vec<u16> = (0..stride * size).map(|_| (lcg(&mut seed) % 1024) as u16).collect();
                let res: Vec<i16> = (0..size * size).map(|_| (lcg(&mut seed) as i32 % 3000 - 1500) as i16).collect();
                let mut want = dst.clone();
                for y in 0..size {
                    for x in 0..size {
                        let v = want[y * stride + x] as i32 + res[y * size + x] as i32;
                        want[y * stride + x] = v.clamp(0, 1023) as u16;
                    }
                }
                add_residual(&mut dst, stride, &res, log2, 1023);
                assert_eq!(dst, want);
            }
        }
    }

    #[test]
    fn planar_matches_scalar() {
        let mut seed = 5u64;
        for log2 in 2..=5u8 {
            let size = 1usize << log2;
            for trial in 0..60 {
                let mk = |seed: &mut u64| -> Vec<u16> {
                    (0..2 * size + 1)
                        .map(|_| if trial % 5 == 0 { 1023 } else { (lcg(seed) % 1024) as u16 })
                        .collect()
                };
                let (top, left) = (mk(&mut seed), mk(&mut seed));
                let stride = size + 3;
                let mut dst = vec![0u16; stride * size];
                planar(&mut dst, stride, &top, &left, log2);
                for y in 0..size {
                    for x in 0..size {
                        let pred = (size - 1 - x) as i32 * left[1 + y] as i32
                            + (x + 1) as i32 * top[1 + size] as i32
                            + (size - 1 - y) as i32 * top[1 + x] as i32
                            + (y + 1) as i32 * left[1 + size] as i32
                            + size as i32;
                        assert_eq!(dst[y * stride + x], (pred >> (log2 + 1)) as u16, "size {size} ({x},{y})");
                    }
                }
            }
        }
    }

    #[test]
    fn angular_row_matches_scalar() {
        let mut seed = 9u64;
        for &w in &[4usize, 8, 16, 32] {
            for fact in 1..32 {
                let r: Vec<u16> = (0..w + 1).map(|_| (lcg(&mut seed) % 1024) as u16).collect();
                let mut out = vec![0u16; w];
                angular_row(&mut out, &r, fact);
                for x in 0..w {
                    let v = ((32 - fact) * r[x] as i32 + fact * r[x + 1] as i32 + 16) >> 5;
                    assert_eq!(out[x] as i32, v);
                }
            }
        }
    }
}
