//! SIMD inverse DCT (8x8, 16x16, 32x32) on 16-bit coefficients. The 1-D transform is split into even and
//! odd input rows (the matrix is symmetric / antisymmetric in the output index), and each half is summed
//! with the pairwise multiply-add `dot`, two taps per lane operation. Both passes are "vertical" passes
//! over 8 columns at a time; the block is transposed between them and after the second one (8x8 tile
//! transposes), so the horizontal pass reuses the vertical code. Bit-identical to the scalar matrix
//! version in `inverse_transform` (same rounding points and 16-bit clipping): the tests compare them.

use crate::inverse_transform::TRANSFORM;
use crate::simd::*;

/// `PAIRS[parity][i][t]`: coefficient pair for output `i` and input rows `parity + 4t`, `parity + 4t + 2`
/// of the `n`-point transform, as `lo | hi << 16`.
const fn pairs(n: usize) -> [[[i32; 8]; 16]; 2] {
    let mut t = [[[0i32; 8]; 16]; 2];
    let mut q = 0;
    while q < 2 {
        let mut i = 0;
        while i < n / 2 {
            let mut u = 0;
            while u < n / 4 {
                let k0 = q + 4 * u;
                let lo = TRANSFORM[k0 * (32 / n)][i] as i16 as u16 as u32;
                let hi = TRANSFORM[(k0 + 2) * (32 / n)][i] as i16 as u16 as u32;
                t[q][i][u] = (hi << 16 | lo) as i32;
                u += 1;
            }
            i += 1;
        }
        q += 1;
    }
    t
}
static PAIRS_8: [[[i32; 8]; 16]; 2] = pairs(8);
static PAIRS_16: [[[i32; 8]; 16]; 2] = pairs(16);
static PAIRS_32: [[[i32; 8]; 16]; 2] = pairs(32);

#[inline(always)]
fn transpose8(a: [V16; 8]) -> [V16; 8] {
    let s0 = zip_lo16(a[0], a[1]);
    let s1 = zip_hi16(a[0], a[1]);
    let s2 = zip_lo16(a[2], a[3]);
    let s3 = zip_hi16(a[2], a[3]);
    let s4 = zip_lo16(a[4], a[5]);
    let s5 = zip_hi16(a[4], a[5]);
    let s6 = zip_lo16(a[6], a[7]);
    let s7 = zip_hi16(a[6], a[7]);
    let t0 = zip_lo32(s0, s2);
    let t1 = zip_hi32(s0, s2);
    let t2 = zip_lo32(s1, s3);
    let t3 = zip_hi32(s1, s3);
    let t4 = zip_lo32(s4, s6);
    let t5 = zip_hi32(s4, s6);
    let t6 = zip_lo32(s5, s7);
    let t7 = zip_hi32(s5, s7);
    [
        zip_lo64(t0, t4),
        zip_hi64(t0, t4),
        zip_lo64(t1, t5),
        zip_hi64(t1, t5),
        zip_lo64(t2, t6),
        zip_hi64(t2, t6),
        zip_lo64(t3, t7),
        zip_hi64(t3, t7),
    ]
}

/// One vertical `N`-point inverse transform pass over 8 columns. `rows.add(k)` is input row `k`; only rows
/// `k < nz + 2` are read and rows `nz..nz + 2` must be zero. Returns the `N` output rows, each
/// `(sum + add) >> shift` clipped to 16 bits.
///
/// SAFETY: `rows` points to at least `min(nz + 2, 32)` initialised vectors.
#[inline(always)]
unsafe fn vertical<const N: usize>(rows: *const V16, nz: usize, add: i32, shift: u32) -> [V16; 32] {
    let table = match N {
        8 => &PAIRS_8,
        16 => &PAIRS_16,
        _ => &PAIRS_32,
    };
    // Row pairs (k, k + 2) of each parity, interleaved for the pairwise multiply-add.
    // Only entries with `q + 4u < nz` are written and read (the accumulation stops at the first other one),
    // so the rest stay uninitialised: zero-filling these arrays is a `memory.fill` call on WebAssembly.
    let mut zl = std::mem::MaybeUninit::<[[V16; 8]; 2]>::uninit();
    let mut zh = std::mem::MaybeUninit::<[[V16; 8]; 2]>::uninit();
    let (zlp, zhp) = (zl.as_mut_ptr() as *mut V16, zh.as_mut_ptr() as *mut V16);
    for q in 0..2 {
        for u in 0..N / 4 {
            let k0 = q + 4 * u;
            if k0 < nz {
                // SAFETY: k0 + 2 < nz + 2 (caller contract); q * 8 + u < 16.
                let (a, b) = unsafe { (*rows.add(k0), *rows.add(k0 + 2)) };
                unsafe {
                    zlp.add(q * 8 + u).write(zip_lo16(a, b));
                    zhp.add(q * 8 + u).write(zip_hi16(a, b));
                }
            }
        }
    }
    let vadd = splat32(add);
    let mut out = [zero(); 32];
    for i in 0..N / 2 {
        let mut acc = [[splat32(0); 2]; 2]; // [parity][lo/hi]
        for q in 0..2 {
            for u in 0..N / 4 {
                if q + 4 * u >= nz {
                    break;
                }
                let cv = splat_pair(table[q][i][u]);
                // SAFETY: q + 4u < nz, so the entry was written above.
                let (l, h) = unsafe { (*zlp.add(q * 8 + u), *zhp.add(q * 8 + u)) };
                acc[q][0] = add32(acc[q][0], dot(l, cv));
                acc[q][1] = add32(acc[q][1], dot(h, cv));
            }
        }
        let (e_lo, e_hi) = (add32(acc[0][0], vadd), add32(acc[0][1], vadd));
        let (o_lo, o_hi) = (acc[1][0], acc[1][1]);
        out[i] = narrow32(shr32v(add32(e_lo, o_lo), shift), shr32v(add32(e_hi, o_hi), shift));
        out[N - 1 - i] = narrow32(shr32v(sub32(e_lo, o_lo), shift), shr32v(sub32(e_hi, o_hi), shift));
    }
    out
}

/// In-place `N x N` inverse DCT (`N` = 8, 16, 32) of a block whose non-zero coefficients lie in the top-left
/// `nz_cols x nz_rows`, for bit depth `bd` (second-pass shift `20 - bd`).
pub(crate) fn idct<const N: usize>(coeffs: &mut [i16], bd: u32, nz_cols: usize, nz_rows: usize) {
    assert!(coeffs.len() >= N * N);
    let (nz_cols, nz_rows) = (nz_cols.clamp(1, N), nz_rows.clamp(1, N));
    let cp = coeffs.as_mut_ptr();
    let mut tmp = std::mem::MaybeUninit::<[i16; 1024]>::uninit();
    let tp = tmp.as_mut_ptr() as *mut i16;
    // Input rows of one pass: rows `0..nz` plus two zero rows are written before `vertical` reads them.
    let mut rows = std::mem::MaybeUninit::<[V16; 32]>::uninit();
    // SAFETY: all vector accesses are 8 lanes inside the N x N block (columns are multiples of 8 below N)
    // or inside `tmp` (N * N <= 1024). `tmp` rows `0..cols8` are written by pass 1 before pass 2 reads
    // rows `0..nz_cols` of it (nz_cols <= cols8).
    unsafe {
        // Pass 1: vertical transform of 8 columns at a time; the result is stored transposed (row c of
        // `tmp` holds column c of the pass-1 output).
        let cols8 = nz_cols.div_ceil(8) * 8;
        let mut c0 = 0;
        while c0 < cols8 {
            let rp = rows.as_mut_ptr() as *mut V16;
            for k in 0..nz_rows {
                rp.add(k).write(ld(cp.add(k * N + c0)));
            }
            for k in nz_rows..(nz_rows + 2).min(32) {
                rp.add(k).write(zero());
            }
            let out = vertical::<N>(rp, nz_rows, 64, 7);
            for t in 0..N / 8 {
                let tile = transpose8([
                    out[8 * t],
                    out[8 * t + 1],
                    out[8 * t + 2],
                    out[8 * t + 3],
                    out[8 * t + 4],
                    out[8 * t + 5],
                    out[8 * t + 6],
                    out[8 * t + 7],
                ]);
                for l in 0..8 {
                    st(tp.add((c0 + l) * N + 8 * t), tile[l]);
                }
            }
            c0 += 8;
        }
        // Pass 2: the same vertical transform over the transposed data, 8 original rows at a time; the
        // result is transposed back while it is stored.
        let shift = 20 - bd;
        let add = 1i32 << (shift - 1);
        let mut r0 = 0;
        while r0 < N {
            let rp = rows.as_mut_ptr() as *mut V16;
            for k in 0..nz_cols {
                rp.add(k).write(ld(tp.add(k * N + r0)));
            }
            for k in nz_cols..(nz_cols + 2).min(32) {
                rp.add(k).write(zero());
            }
            let out = vertical::<N>(rp, nz_cols, add, shift);
            for t in 0..N / 8 {
                let tile = transpose8([
                    out[8 * t],
                    out[8 * t + 1],
                    out[8 * t + 2],
                    out[8 * t + 3],
                    out[8 * t + 4],
                    out[8 * t + 5],
                    out[8 * t + 6],
                    out[8 * t + 7],
                ]);
                for l in 0..8 {
                    st(cp.add((r0 + l) * N + 8 * t), tile[l]);
                }
            }
            r0 += 8;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Compare against the scalar butterfly transforms (the reference of `inverse_transform`'s own test).
    #[test]
    fn simd_idct_matches_butterflies() {
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut rnd = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for (n, f) in [(8usize, idct::<8> as fn(&mut [i16], u32, usize, usize)), (16, idct::<16>), (32, idct::<32>)] {
            for iter in 0..600 {
                let cols = 4 * (1 + rnd() as usize % (n / 4));
                let rows = 4 * (1 + rnd() as usize % (n / 4));
                let mut block = vec![0i16; n * n];
                for y in 0..rows {
                    for x in 0..cols {
                        if rnd() % 3 == 0 {
                            block[y * n + x] =
                                if iter % 4 == 0 { rnd() as i16 } else { (rnd() % 512) as i16 - 256 };
                        }
                    }
                }
                for bd in [8u32, 10, 12] {
                    let mut a = block.clone();
                    let mut b = block.clone();
                    f(&mut a, bd, cols, rows);
                    crate::inverse_transform::reference_idct(&mut b, n, bd);
                    assert_eq!(a, b, "n={n} bd={bd} cols={cols} rows={rows} iter={iter}");
                }
            }
        }
    }
}
