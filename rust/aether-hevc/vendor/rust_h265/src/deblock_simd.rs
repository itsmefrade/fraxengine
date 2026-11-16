//! SIMD luma deblocking of one 8-line edge for 16-bit samples of at most 10 bits (all arithmetic in 16-bit
//! lanes: the filters' intermediate sums stay below 2^14). The eight lines of the edge are the eight lanes;
//! the decisions of spec 8.7.2.5.3 (per 4-line segment, from its first and last line) are made on scalars,
//! the strong and weak filters then run on all lanes at once and the results are selected per lane.
//! Bit-identical to `deblock::filter_luma_edge` (the in-source tests compare them on random data).

use crate::simd::*;

/// 8x8 transpose of eight vectors of eight i16 lanes (same as `tx_simd`'s; kept local to avoid a dependency).
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

/// Filter the luma edge whose first Q sample is `plane[base]`. `VERT`: a vertical edge (the eight lines are
/// rows `stride` apart, taps are consecutive samples of a row); otherwise a horizontal edge (taps are rows
/// `stride` apart, the lines are consecutive columns). `beta` and `tcs` (one per 4-line segment, 0 = segment
/// not filtered) are already scaled for the bit depth; `no_filter[segment][side]` protects P (0) / Q (1).
pub(crate) fn luma_edge<const VERT: bool>(
    plane: &mut [u16],
    base: usize,
    stride: usize,
    beta: i32,
    tcs: [i32; 2],
    max: i32,
    no_filter: [[bool; 2]; 2],
) {
    // Region touched: taps p3..q3.
    let (first, last) = if VERT {
        (base - 4, base + 7 * stride + 4)
    } else {
        (base - 4 * stride, base + 3 * stride + 8)
    };
    assert!(last <= plane.len() && first < last);
    let pp = plane.as_mut_ptr() as *mut i16;
    // SAFETY: every access below is an 8-lane load/store inside [first, last) (asserted above): rows
    // base + l * stride - 4 .. + 8 for l < 8 (vertical) or base + (t - 4) * stride .. + 8 for t < 8 (horizontal).
    unsafe {
        let mut taps: [V16; 8];
        if VERT {
            let mut lines = [zero(); 8];
            for (l, v) in lines.iter_mut().enumerate() {
                *v = ld(pp.add(base + l * stride - 4));
            }
            taps = transpose8(lines);
        } else {
            taps = [zero(); 8];
            for (t, v) in taps.iter_mut().enumerate() {
                *v = ld(pp.add(base + t * stride - 4 * stride));
            }
        }
        // Scalar decisions from lines 0 and 3 of each segment.
        let mut a = [[0i16; 8]; 8];
        for t in 0..8 {
            st(a[t].as_mut_ptr(), taps[t]);
        }
        let mut strong = [0i16; 8];
        let mut weak = [0i16; 8];
        let mut modp1 = [0i16; 8];
        let mut modq1 = [0i16; 8];
        let mut pmask = [0i16; 8];
        let mut qmask = [0i16; 8];
        let mut tc_l = [0i16; 8];
        let mut any = false;
        for j in 0..2 {
            let tc = tcs[j];
            if tc == 0 {
                continue;
            }
            let g = |t: usize, l: usize| a[t][4 * j + l] as i32;
            let dp = |l: usize| (g(1, l) - 2 * g(2, l) + g(3, l)).abs();
            let dq = |l: usize| (g(6, l) - 2 * g(5, l) + g(4, l)).abs();
            let (d0_p, d0_q, d3_p, d3_q) = (dp(0), dq(0), dp(3), dq(3));
            let (d0, d3) = (d0_p + d0_q, d3_p + d3_q);
            if d0 + d3 >= beta {
                continue;
            }
            let beta_3 = beta >> 3;
            let beta_2 = beta >> 2;
            let tc25 = (tc * 5 + 1) >> 1;
            let strong_line = |l: usize, d: i32| {
                (g(0, l) - g(3, l)).abs() + (g(7, l) - g(4, l)).abs() < beta_3
                    && (g(3, l) - g(4, l)).abs() < tc25
                    && (d << 1) < beta_2
            };
            let is_strong = strong_line(0, d0) && strong_line(3, d3);
            let side = (beta + (beta >> 1)) >> 3;
            let (nd_p, nd_q) = (d0_p + d3_p < side, d0_q + d3_q < side);
            for l in 4 * j..4 * j + 4 {
                tc_l[l] = tc as i16;
                if is_strong {
                    strong[l] = -1;
                } else {
                    weak[l] = -1;
                    modp1[l] = if nd_p { -1 } else { 0 };
                    modq1[l] = if nd_q { -1 } else { 0 };
                }
                pmask[l] = if no_filter[j][0] { 0 } else { -1 };
                qmask[l] = if no_filter[j][1] { 0 } else { -1 };
                any = true;
            }
        }
        if !any {
            return;
        }
        let strong = ld(strong.as_ptr());
        let weak = ld(weak.as_ptr());
        let modp1 = ld(modp1.as_ptr());
        let modq1 = ld(modq1.as_ptr());
        let pmask = ld(pmask.as_ptr());
        let qmask = ld(qmask.as_ptr());
        let tc = ld(tc_l.as_ptr());
        let [p3, p2, p1, p0, q0, q1, q2, q3] = taps;
        let ntc = sub16(zero(), tc);
        // ---- strong filter (spec 8.7.2.5.7, dE == 2): changes clipped to +-2*tc ----
        let tc2 = add16(tc, tc);
        let ntc2 = sub16(zero(), tc2);
        let clip2 = |new: V16, old: V16| add16(old, max16(min16(sub16(new, old), tc2), ntc2));
        let four = splat16(4);
        let two = splat16(2);
        let s_p2 = clip2(
            shr16::<3>(add16(add16(add16(add16(add16(add16(p3, p3), add16(p2, add16(p2, p2))), p1), p0), q0), four)),
            p2,
        );
        let s_p1 = clip2(shr16::<2>(add16(add16(add16(add16(p2, p1), p0), q0), two)), p1);
        let s_p0 = clip2(
            shr16::<3>(add16(
                add16(add16(add16(p2, add16(p1, p1)), add16(p0, p0)), add16(q0, q0)),
                add16(q1, four),
            )),
            p0,
        );
        let s_q0 = clip2(
            shr16::<3>(add16(
                add16(add16(add16(p1, add16(p0, p0)), add16(q0, q0)), add16(q1, q1)),
                add16(q2, four),
            )),
            q0,
        );
        let s_q1 = clip2(shr16::<2>(add16(add16(add16(add16(p0, q0), q1), q2), two)), q1);
        let s_q2 = clip2(
            shr16::<3>(add16(add16(add16(add16(add16(add16(q3, q3), add16(q2, add16(q2, q2))), q1), q0), p0), four)),
            q2,
        );
        // ---- weak filter (dE == 1) ----
        let raw = shr16::<4>(add16(
            sub16(mul16(sub16(q0, p0), splat16(9)), mul16(sub16(q1, p1), splat16(3))),
            splat16(8),
        ));
        let on = cmplt16(abs16(raw), mul16(tc, splat16(10)));
        let delta0 = max16(min16(raw, tc), ntc);
        let tc_half = shr16::<1>(tc);
        let ntc_half = sub16(zero(), tc_half);
        let avg_p = shr16::<1>(add16(add16(p2, p0), splat16(1)));
        let avg_q = shr16::<1>(add16(add16(q2, q0), splat16(1)));
        let dp1 = max16(min16(shr16::<1>(add16(sub16(avg_p, p1), delta0)), tc_half), ntc_half);
        let dq1 = max16(min16(shr16::<1>(sub16(sub16(avg_q, q1), delta0)), tc_half), ntc_half);
        let w_p0 = select(on, add16(p0, delta0), p0);
        let w_q0 = select(on, sub16(q0, delta0), q0);
        let w_p1 = select(and(on, modp1), add16(p1, dp1), p1);
        let w_q1 = select(and(on, modq1), add16(q1, dq1), q1);
        // ---- per-lane selection and clipping to the sample range ----
        let (vmax, vzero) = (splat16(max as i16), zero());
        let pick = |orig: V16, s: V16, w: V16, side: V16| {
            let mut v = select(strong, s, select(weak, w, orig));
            v = min16(max16(v, vzero), vmax);
            select(side, v, orig)
        };
        let n_p2 = pick(p2, s_p2, p2, pmask);
        let n_p1 = pick(p1, s_p1, w_p1, pmask);
        let n_p0 = pick(p0, s_p0, w_p0, pmask);
        let n_q0 = pick(q0, s_q0, w_q0, qmask);
        let n_q1 = pick(q1, s_q1, w_q1, qmask);
        let n_q2 = pick(q2, s_q2, q2, qmask);
        taps = [p3, n_p2, n_p1, n_p0, n_q0, n_q1, n_q2, q3];
        if VERT {
            let lines = transpose8(taps);
            for (l, v) in lines.iter().enumerate() {
                st(pp.add(base + l * stride - 4), *v);
            }
        } else {
            for t in 1..7 {
                st(pp.add(base + t * stride - 4 * stride), taps[t]);
            }
        }
    }
}
