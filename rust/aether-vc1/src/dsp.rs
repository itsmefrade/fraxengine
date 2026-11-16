//! Integer DSP kernels (transforms, deblocking, overlap smoothing, motion compensation).
//! Ported from FFmpeg libavcodec/vc1dsp.c, hpeldsp.c and h264chroma (LGPL-2.1-or-later).

/// Count of transform values that left a conformant range (|v| >= 8192) (non conformant input). Only maintained
/// with the `diag` feature; used by the differential tests to discard streams on which SIMD and
/// scalar implementations legitimately differ.
#[cfg(feature = "diag")]
pub static OVERFLOWS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

#[inline(always)]
fn chk16(v: i32) -> i16 {
    #[cfg(feature = "diag")]
    if !(-8192..8192).contains(&v) {
        OVERFLOWS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    v as i16
}

#[inline(always)]
pub fn clip_u8(v: i32) -> u8 {
    v.clamp(0, 255) as u8
}

#[inline(always)]
fn add_clip(d: &mut u8, v: i32) {
    *d = clip_u8(*d as i32 + v);
}

// ---------------------------------------------------------------- inverse transforms

pub fn inv_trans_8x8_dc(dst: &mut [u8], off: usize, stride: usize, block: &[i16; 64]) {
    let mut dc = block[0] as i32;
    dc = (3 * dc + 1) >> 1;
    dc = (3 * dc + 16) >> 5;
    for i in 0..8 {
        let row = &mut dst[off + i * stride..off + i * stride + 8];
        for p in row.iter_mut() {
            add_clip(p, dc);
        }
    }
}

pub fn inv_trans_8x8(block: &mut [i16; 64]) {
    let mut temp = [0i16; 64];
    for i in 0..8 {
        let s = |k: usize| block[i + k] as i32;
        let mut t1 = 12 * (s(0) + s(32)) + 4;
        let mut t2 = 12 * (s(0) - s(32)) + 4;
        let mut t3 = 16 * s(16) + 6 * s(48);
        let mut t4 = 6 * s(16) - 16 * s(48);
        let t5 = t1 + t3;
        let t6 = t2 + t4;
        let t7 = t2 - t4;
        let t8 = t1 - t3;
        t1 = 16 * s(8) + 15 * s(24) + 9 * s(40) + 4 * s(56);
        t2 = 15 * s(8) - 4 * s(24) - 16 * s(40) - 9 * s(56);
        t3 = 9 * s(8) - 16 * s(24) + 4 * s(40) + 15 * s(56);
        t4 = 4 * s(8) - 9 * s(24) + 15 * s(40) - 16 * s(56);
        let d = &mut temp[i * 8..i * 8 + 8];
        d[0] = chk16((t5 + t1) >> 3);
        d[1] = chk16((t6 + t2) >> 3);
        d[2] = chk16((t7 + t3) >> 3);
        d[3] = chk16((t8 + t4) >> 3);
        d[4] = chk16((t8 - t4) >> 3);
        d[5] = chk16((t7 - t3) >> 3);
        d[6] = chk16((t6 - t2) >> 3);
        d[7] = chk16((t5 - t1) >> 3);
    }
    for i in 0..8 {
        let s = |k: usize| temp[i + k] as i32;
        let mut t1 = 12 * (s(0) + s(32)) + 64;
        let mut t2 = 12 * (s(0) - s(32)) + 64;
        let mut t3 = 16 * s(16) + 6 * s(48);
        let mut t4 = 6 * s(16) - 16 * s(48);
        let t5 = t1 + t3;
        let t6 = t2 + t4;
        let t7 = t2 - t4;
        let t8 = t1 - t3;
        t1 = 16 * s(8) + 15 * s(24) + 9 * s(40) + 4 * s(56);
        t2 = 15 * s(8) - 4 * s(24) - 16 * s(40) - 9 * s(56);
        t3 = 9 * s(8) - 16 * s(24) + 4 * s(40) + 15 * s(56);
        t4 = 4 * s(8) - 9 * s(24) + 15 * s(40) - 16 * s(56);
        block[i] = chk16((t5 + t1) >> 7);
        block[i + 8] = chk16((t6 + t2) >> 7);
        block[i + 16] = chk16((t7 + t3) >> 7);
        block[i + 24] = chk16((t8 + t4) >> 7);
        block[i + 32] = chk16((t8 - t4 + 1) >> 7);
        block[i + 40] = chk16((t7 - t3 + 1) >> 7);
        block[i + 48] = chk16((t6 - t2 + 1) >> 7);
        block[i + 56] = chk16((t5 - t1 + 1) >> 7);
    }
}

pub fn inv_trans_8x4_dc(dst: &mut [u8], off: usize, stride: usize, block: &[i16]) {
    let mut dc = block[0] as i32;
    dc = (3 * dc + 1) >> 1;
    dc = (17 * dc + 64) >> 7;
    for i in 0..4 {
        for p in dst[off + i * stride..off + i * stride + 8].iter_mut() {
            add_clip(p, dc);
        }
    }
}

pub fn inv_trans_8x4(dst: &mut [u8], off: usize, stride: usize, block: &mut [i16]) {
    for i in 0..4 {
        let b = i * 8;
        let s = |k: usize| block[b + k] as i32;
        let mut t1 = 12 * (s(0) + s(4)) + 4;
        let mut t2 = 12 * (s(0) - s(4)) + 4;
        let mut t3 = 16 * s(2) + 6 * s(6);
        let mut t4 = 6 * s(2) - 16 * s(6);
        let t5 = t1 + t3;
        let t6 = t2 + t4;
        let t7 = t2 - t4;
        let t8 = t1 - t3;
        t1 = 16 * s(1) + 15 * s(3) + 9 * s(5) + 4 * s(7);
        t2 = 15 * s(1) - 4 * s(3) - 16 * s(5) - 9 * s(7);
        t3 = 9 * s(1) - 16 * s(3) + 4 * s(5) + 15 * s(7);
        t4 = 4 * s(1) - 9 * s(3) + 15 * s(5) - 16 * s(7);
        block[b] = chk16((t5 + t1) >> 3);
        block[b + 1] = chk16((t6 + t2) >> 3);
        block[b + 2] = chk16((t7 + t3) >> 3);
        block[b + 3] = chk16((t8 + t4) >> 3);
        block[b + 4] = chk16((t8 - t4) >> 3);
        block[b + 5] = chk16((t7 - t3) >> 3);
        block[b + 6] = chk16((t6 - t2) >> 3);
        block[b + 7] = chk16((t5 - t1) >> 3);
    }
    for i in 0..8 {
        let s = |k: usize| block[i + k] as i32;
        let t1 = 17 * (s(0) + s(16)) + 64;
        let t2 = 17 * (s(0) - s(16)) + 64;
        let t3 = 22 * s(8) + 10 * s(24);
        let t4 = 22 * s(24) - 10 * s(8);
        add_clip(&mut dst[off + i], (t1 + t3) >> 7);
        add_clip(&mut dst[off + stride + i], (t2 - t4) >> 7);
        add_clip(&mut dst[off + 2 * stride + i], (t2 + t4) >> 7);
        add_clip(&mut dst[off + 3 * stride + i], (t1 - t3) >> 7);
    }
}

pub fn inv_trans_4x8_dc(dst: &mut [u8], off: usize, stride: usize, block: &[i16]) {
    let mut dc = block[0] as i32;
    dc = (17 * dc + 4) >> 3;
    dc = (12 * dc + 64) >> 7;
    for i in 0..8 {
        for p in dst[off + i * stride..off + i * stride + 4].iter_mut() {
            add_clip(p, dc);
        }
    }
}

pub fn inv_trans_4x8(dst: &mut [u8], off: usize, stride: usize, block: &mut [i16]) {
    for i in 0..8 {
        let b = i * 8;
        let s = |k: usize| block[b + k] as i32;
        let t1 = 17 * (s(0) + s(2)) + 4;
        let t2 = 17 * (s(0) - s(2)) + 4;
        let t3 = 22 * s(1) + 10 * s(3);
        let t4 = 22 * s(3) - 10 * s(1);
        block[b] = chk16((t1 + t3) >> 3);
        block[b + 1] = chk16((t2 - t4) >> 3);
        block[b + 2] = chk16((t2 + t4) >> 3);
        block[b + 3] = chk16((t1 - t3) >> 3);
    }
    for i in 0..4 {
        let s = |k: usize| block[i + k] as i32;
        let mut t1 = 12 * (s(0) + s(32)) + 64;
        let mut t2 = 12 * (s(0) - s(32)) + 64;
        let mut t3 = 16 * s(16) + 6 * s(48);
        let mut t4 = 6 * s(16) - 16 * s(48);
        let t5 = t1 + t3;
        let t6 = t2 + t4;
        let t7 = t2 - t4;
        let t8 = t1 - t3;
        t1 = 16 * s(8) + 15 * s(24) + 9 * s(40) + 4 * s(56);
        t2 = 15 * s(8) - 4 * s(24) - 16 * s(40) - 9 * s(56);
        t3 = 9 * s(8) - 16 * s(24) + 4 * s(40) + 15 * s(56);
        t4 = 4 * s(8) - 9 * s(24) + 15 * s(40) - 16 * s(56);
        add_clip(&mut dst[off + i], (t5 + t1) >> 7);
        add_clip(&mut dst[off + stride + i], (t6 + t2) >> 7);
        add_clip(&mut dst[off + 2 * stride + i], (t7 + t3) >> 7);
        add_clip(&mut dst[off + 3 * stride + i], (t8 + t4) >> 7);
        add_clip(&mut dst[off + 4 * stride + i], (t8 - t4 + 1) >> 7);
        add_clip(&mut dst[off + 5 * stride + i], (t7 - t3 + 1) >> 7);
        add_clip(&mut dst[off + 6 * stride + i], (t6 - t2 + 1) >> 7);
        add_clip(&mut dst[off + 7 * stride + i], (t5 - t1 + 1) >> 7);
    }
}

pub fn inv_trans_4x4_dc(dst: &mut [u8], off: usize, stride: usize, block: &[i16]) {
    let mut dc = block[0] as i32;
    dc = (17 * dc + 4) >> 3;
    dc = (17 * dc + 64) >> 7;
    for i in 0..4 {
        for p in dst[off + i * stride..off + i * stride + 4].iter_mut() {
            add_clip(p, dc);
        }
    }
}

pub fn inv_trans_4x4(dst: &mut [u8], off: usize, stride: usize, block: &mut [i16]) {
    for i in 0..4 {
        let b = i * 8;
        let s = |k: usize| block[b + k] as i32;
        let t1 = 17 * (s(0) + s(2)) + 4;
        let t2 = 17 * (s(0) - s(2)) + 4;
        let t3 = 22 * s(1) + 10 * s(3);
        let t4 = 22 * s(3) - 10 * s(1);
        block[b] = chk16((t1 + t3) >> 3);
        block[b + 1] = chk16((t2 - t4) >> 3);
        block[b + 2] = chk16((t2 + t4) >> 3);
        block[b + 3] = chk16((t1 - t3) >> 3);
    }
    for i in 0..4 {
        let s = |k: usize| block[i + k] as i32;
        let t1 = 17 * (s(0) + s(16)) + 64;
        let t2 = 17 * (s(0) - s(16)) + 64;
        let t3 = 22 * s(8) + 10 * s(24);
        let t4 = 22 * s(24) - 10 * s(8);
        add_clip(&mut dst[off + i], (t1 + t3) >> 7);
        add_clip(&mut dst[off + stride + i], (t2 - t4) >> 7);
        add_clip(&mut dst[off + 2 * stride + i], (t2 + t4) >> 7);
        add_clip(&mut dst[off + 3 * stride + i], (t1 - t3) >> 7);
    }
}

// ---------------------------------------------------------------- pixel put helpers

pub fn put_signed_pixels_clamped(block: &[i16], dst: &mut [u8], off: usize, stride: usize) {
    for i in 0..8 {
        for j in 0..8 {
            dst[off + i * stride + j] = clip_u8(block[i * 8 + j] as i32 + 128);
        }
    }
}

pub fn put_pixels_clamped(block: &[i16], dst: &mut [u8], off: usize, stride: usize) {
    for i in 0..8 {
        for j in 0..8 {
            dst[off + i * stride + j] = clip_u8(block[i * 8 + j] as i32);
        }
    }
}

pub fn add_pixels_clamped(block: &[i16], dst: &mut [u8], off: usize, stride: usize) {
    for i in 0..8 {
        for j in 0..8 {
            add_clip(&mut dst[off + i * stride + j], block[i * 8 + j] as i32);
        }
    }
}

// ---------------------------------------------------------------- overlap smoothing

/// `top` points at the bottom-block's rows of the upper block: indexes into `a` at `ta`, `bottom` at `ba`.
pub fn v_s_overlap(top: &mut [i16; 64], bottom: &mut [i16; 64]) {
    v_s_overlap_ab(top, bottom)
}

pub fn v_s_overlap_ab(top: &mut [i16], bottom: &mut [i16]) {
    let mut rnd1 = 4;
    let mut rnd2 = 3;
    for i in 0..8 {
        let a = top[48 + i] as i32;
        let b = top[56 + i] as i32;
        let c = bottom[i] as i32;
        let d = bottom[8 + i] as i32;
        let d1 = a - d;
        let d2 = a - d + b - c;
        top[48 + i] = (((a * 8) - d1 + rnd1) >> 3) as i16;
        top[56 + i] = (((b * 8) - d2 + rnd2) >> 3) as i16;
        bottom[i] = (((c * 8) + d2 + rnd1) >> 3) as i16;
        bottom[8 + i] = (((d * 8) + d1 + rnd2) >> 3) as i16;
        rnd2 = 7 - rnd2;
        rnd1 = 7 - rnd1;
    }
}

/// left/right are slices starting at the first sample of the row run; strides in elements.
pub fn h_s_overlap(left: &mut [i16], lstride: usize, right: &mut [i16], rstride: usize, flags: i32) {
    let mut rnd1 = if flags & 2 != 0 { 3 } else { 4 };
    let mut rnd2 = 7 - rnd1;
    let mut lo = 0usize;
    let mut ro = 0usize;
    for _ in 0..8 {
        let a = left[lo + 6] as i32;
        let b = left[lo + 7] as i32;
        let c = right[ro] as i32;
        let d = right[ro + 1] as i32;
        let d1 = a - d;
        let d2 = a - d + b - c;
        left[lo + 6] = (((a * 8) - d1 + rnd1) >> 3) as i16;
        left[lo + 7] = (((b * 8) - d2 + rnd2) >> 3) as i16;
        right[ro] = (((c * 8) + d2 + rnd1) >> 3) as i16;
        right[ro + 1] = (((d * 8) + d1 + rnd2) >> 3) as i16;
        ro += rstride;
        lo += lstride;
        if flags & 1 != 0 {
            rnd2 = 7 - rnd2;
            rnd1 = 7 - rnd1;
        }
    }
}

// ---------------------------------------------------------------- loop filter

/// Core of the VC-1 deblocking decision for one line, given the eight pixels p[-4..=3] across the
/// edge. Returns (filtered?, new p[-1], new p[0]).
#[inline(always)]
fn filter_core(m4: i32, m3: i32, m2: i32, m1: i32, p0: i32, p1: i32, p2: i32, p3: i32, pq: i32) -> (bool, i32, i32) {
    let mut a0 = (2 * (m2 - p1) - 5 * (m1 - p0) + 4) >> 3;
    let a0_sign = a0 >> 31;
    a0 = (a0 ^ a0_sign) - a0_sign;
    if a0 < pq {
        let a1 = ((2 * (m4 - m1) - 5 * (m3 - m2) + 4) >> 3).abs();
        let a2 = ((2 * (p0 - p3) - 5 * (p1 - p2) + 4) >> 3).abs();
        if a1 < a0 || a2 < a0 {
            let clip = m1 - p0;
            let clip_sign = clip >> 31;
            let clip = ((clip ^ clip_sign) - clip_sign) >> 1;
            if clip != 0 {
                let a3 = a1.min(a2);
                let d = 5 * (a3 - a0);
                let mut d_sign = d >> 31;
                let mut d = ((d ^ d_sign) - d_sign) >> 3;
                d_sign ^= a0_sign;
                if d_sign ^ clip_sign == 0 {
                    d = d.min(clip);
                    d = (d ^ d_sign) - d_sign;
                    return (true, clip_u8(m1 - d) as i32, clip_u8(p0 + d) as i32);
                }
                return (true, m1, p0);
            }
        }
    }
    (false, m1, p0)
}

/// Filter one line whose eight pixels are contiguous (vertical edge): `p` points at p[0].
#[inline(always)]
unsafe fn filter_line_contig(p: *mut u8, pq: i32) -> bool {
    let w = (p.offset(-4) as *const [u8; 8]).read_unaligned();
    let (f, n1, n0) = filter_core(w[0] as i32, w[1] as i32, w[2] as i32, w[3] as i32, w[4] as i32, w[5] as i32, w[6] as i32, w[7] as i32, pq);
    if f {
        *p.offset(-1) = n1 as u8;
        *p = n0 as u8;
    }
    f
}

/// Filter one line across a horizontal edge: pixels are `stride` apart, `p` points at p[0].
#[inline(always)]
unsafe fn filter_line_strided(p: *mut u8, stride: isize, pq: i32) -> bool {
    let px = |o: isize| -> i32 { *p.offset(o * stride) as i32 };
    let (f, n1, n0) = filter_core(px(-4), px(-3), px(-2), px(-1), px(0), px(1), px(2), px(3), pq);
    if f {
        *p.offset(-stride) = n1 as u8;
        *p = n0 as u8;
    }
    f
}

/// `src` is the first pixel after the edge, `step` moves along the edge, `stride` across it.
pub fn loop_filter(buf: &mut [u8], src: isize, step: isize, stride: isize, len: usize, pq: i32) {
    // extent of every pixel touched
    let last_line = (len as isize - 1) * step;
    let lo = src - 4 * stride.abs();
    let hi = src + last_line + 3 * stride.abs();
    if lo < 0 || hi >= buf.len() as isize || step < 0 || stride <= 0 {
        return;
    }
    let base = buf.as_mut_ptr();
    let mut s = src;
    let mut i = 0;
    // SAFETY: all offsets are within [lo, hi] which was checked against the buffer length above.
    unsafe {
        if stride == 1 {
            while i < len {
                let p = base.offset(s);
                if filter_line_contig(p.offset(2 * step), pq) {
                    filter_line_contig(p, pq);
                    filter_line_contig(p.offset(step), pq);
                    filter_line_contig(p.offset(3 * step), pq);
                }
                s += step * 4;
                i += 4;
            }
        } else {
            while i < len {
                let p = base.offset(s);
                if filter_line_strided(p.offset(2 * step), stride, pq) {
                    filter_line_strided(p, stride, pq);
                    filter_line_strided(p.offset(step), stride, pq);
                    filter_line_strided(p.offset(3 * step), stride, pq);
                }
                s += step * 4;
                i += 4;
            }
        }
    }
}

pub fn v_loop_filter(buf: &mut [u8], pos: usize, stride: usize, len: usize, pq: i32) {
    loop_filter(buf, pos as isize, 1, stride as isize, len, pq)
}
pub fn h_loop_filter(buf: &mut [u8], pos: usize, stride: usize, len: usize, pq: i32) {
    loop_filter(buf, pos as isize, stride as isize, 1, len, pq)
}

// ---------------------------------------------------------------- motion compensation

/// Copy a `bw` x `bh` block into `dst` (stride `dst_stride`, starting at `dst_off`), replicating edge
/// pixels as FFmpeg `emulated_edge_mc` does. `src_off` is the plane index of the block's first pixel
/// as given by the caller's pointer, `(x0, y0)` its coordinate in a view with row stride `src_stride`
/// and extent `w` x `h`.
#[allow(clippy::too_many_arguments)]
pub fn emu_edge(
    dst: &mut [u8],
    dst_off: usize,
    dst_stride: usize,
    plane: &[u8],
    src_off: isize,
    src_stride: isize,
    bw: i32,
    bh: i32,
    x0: i32,
    y0: i32,
    w: i32,
    h: i32,
) {
    if w <= 0 || h <= 0 {
        // nothing sensible to read: leave mid grey
        for j in 0..bh as usize {
            for i in 0..bw as usize {
                dst[dst_off + j * dst_stride + i] = 128;
            }
        }
        return;
    }
    for j in 0..bh {
        let yy = (y0 + j).clamp(0, h - 1) - y0;
        let row = src_off + yy as isize * src_stride;
        for i in 0..bw {
            let xx = (x0 + i).clamp(0, w - 1) - x0;
            let idx = row + xx as isize;
            dst[dst_off + j as usize * dst_stride + i as usize] =
                if idx >= 0 && (idx as usize) < plane.len() { plane[idx as usize] } else { 128 };
        }
    }
}

#[inline(always)]
fn ver_filter_16(src: &[u8], pos: usize, stride: usize, mode: usize) -> i32 {
    let s = |k: isize| src[(pos as isize + k * stride as isize) as usize] as i32;
    match mode {
        1 => -4 * s(-1) + 53 * s(0) + 18 * s(1) - 3 * s(2),
        2 => -s(-1) + 9 * s(0) + 9 * s(1) - s(2),
        3 => -3 * s(-1) + 18 * s(0) + 53 * s(1) - 4 * s(2),
        _ => 0,
    }
}

#[inline(always)]
fn hor_filter_16(t: &[i16], pos: usize, mode: usize) -> i32 {
    let s = |k: isize| t[(pos as isize + k) as usize] as i32;
    match mode {
        1 => -4 * s(-1) + 53 * s(0) + 18 * s(1) - 3 * s(2),
        2 => -s(-1) + 9 * s(0) + 9 * s(1) - s(2),
        3 => -3 * s(-1) + 18 * s(0) + 53 * s(1) - 4 * s(2),
        _ => 0,
    }
}

#[inline(always)]
fn mspel_filter(src: &[u8], pos: usize, stride: usize, mode: usize, r: i32) -> i32 {
    let s = |k: isize| src[(pos as isize + k * stride as isize) as usize] as i32;
    match mode {
        0 => s(0),
        1 => (-4 * s(-1) + 53 * s(0) + 18 * s(1) - 3 * s(2) + 32 - r) >> 6,
        2 => (-s(-1) + 9 * s(0) + 9 * s(1) - s(2) + 8 - r) >> 4,
        3 => (-3 * s(-1) + 18 * s(0) + 53 * s(1) - 4 * s(2) + 32 - r) >> 6,
        _ => 0,
    }
}

/// Bicubic motion compensation of an `n` x `n` block (8 or 16). `src[spos]` is the block origin and
/// must have 1 pixel of margin to the left/top and 2 to the right/bottom.
#[allow(clippy::too_many_arguments)]
pub fn mspel_mc(
    dst: &mut [u8],
    doff: usize,
    dstride: usize,
    src: &[u8],
    spos: usize,
    sstride: usize,
    hmode: usize,
    vmode: usize,
    rnd: i32,
    n: usize,
    avg: bool,
) {
    #[inline(always)]
    fn store(d: &mut u8, v: i32, avg: bool) {
        let v = clip_u8(v);
        *d = if avg { ((*d as i32 + v as i32 + 1) >> 1) as u8 } else { v };
    }
    if vmode != 0 {
        if hmode != 0 {
            const SHIFT_VALUE: [i32; 4] = [0, 5, 1, 5];
            let shift = (SHIFT_VALUE[hmode] + SHIFT_VALUE[vmode]) >> 1;
            let mut tmp = [0i16; 19 * 16];
            let tw = n + 3;
            let r = (1 << (shift - 1)) + rnd - 1;
            for j in 0..n {
                for i in 0..tw {
                    let p = spos + j * sstride + i - 1;
                    tmp[j * tw + i] = ((ver_filter_16(src, p, sstride, vmode) + r) >> shift) as i16;
                }
            }
            let r = 64 - rnd;
            for j in 0..n {
                for i in 0..n {
                    let v = (hor_filter_16(&tmp, j * tw + 1 + i, hmode) + r) >> 7;
                    store(&mut dst[doff + j * dstride + i], v, avg);
                }
            }
        } else {
            let r = 1 - rnd;
            for j in 0..n {
                for i in 0..n {
                    let v = mspel_filter(src, spos + j * sstride + i, sstride, vmode, r);
                    store(&mut dst[doff + j * dstride + i], v, avg);
                }
            }
        }
        return;
    }
    for j in 0..n {
        for i in 0..n {
            let v = mspel_filter(src, spos + j * sstride + i, 1, hmode, rnd);
            store(&mut dst[doff + j * dstride + i], v, avg);
        }
    }
}

/// Half-pel (bilinear) luma MC for 16x16 or 8x8 blocks. `dxy` bit0 = x half, bit1 = y half.
#[allow(clippy::too_many_arguments)]
pub fn hpel_mc(
    dst: &mut [u8],
    doff: usize,
    dstride: usize,
    src: &[u8],
    spos: usize,
    sstride: usize,
    dxy: usize,
    n: usize,
    no_rnd: bool,
    avg: bool,
) {
    let rb: u32 = if no_rnd { 0 } else { 1 };
    for j in 0..n {
        let d = &mut dst[doff + j * dstride..doff + j * dstride + n];
        let r0 = &src[spos + j * sstride..spos + j * sstride + n + 1];
        match dxy {
            0 => {
                if avg {
                    for i in 0..n {
                        d[i] = ((d[i] as u32 + r0[i] as u32 + 1) >> 1) as u8;
                    }
                } else {
                    d.copy_from_slice(&r0[..n]);
                }
            }
            1 => {
                for i in 0..n {
                    let v = (r0[i] as u32 + r0[i + 1] as u32 + rb) >> 1;
                    d[i] = if avg { ((d[i] as u32 + v + 1) >> 1) as u8 } else { v as u8 };
                }
            }
            2 => {
                let r1 = &src[spos + (j + 1) * sstride..spos + (j + 1) * sstride + n];
                for i in 0..n {
                    let v = (r0[i] as u32 + r1[i] as u32 + rb) >> 1;
                    d[i] = if avg { ((d[i] as u32 + v + 1) >> 1) as u8 } else { v as u8 };
                }
            }
            _ => {
                let r1 = &src[spos + (j + 1) * sstride..spos + (j + 1) * sstride + n + 1];
                for i in 0..n {
                    let v = (r0[i] as u32 + r0[i + 1] as u32 + r1[i] as u32 + r1[i + 1] as u32 + 1 + rb) >> 2;
                    d[i] = if avg { ((d[i] as u32 + v + 1) >> 1) as u8 } else { v as u8 };
                }
            }
        }
    }
}

/// Chroma bilinear MC. `no_rnd`: VC-1 "no rounding" variant (bias 32-4 instead of 32).
#[allow(clippy::too_many_arguments)]
pub fn chroma_mc(
    dst: &mut [u8],
    doff: usize,
    dstride: usize,
    src: &[u8],
    spos: usize,
    sstride: usize,
    w: usize,
    h: usize,
    x: i32,
    y: i32,
    no_rnd: bool,
    avg: bool,
) {
    let a = ((8 - x) * (8 - y)) as u32;
    let b = (x * (8 - y)) as u32;
    let c = ((8 - x) * y) as u32;
    let d = (x * y) as u32;
    let bias: u32 = if no_rnd { 32 - 4 } else { 32 };
    for j in 0..h {
        let r0 = &src[spos + j * sstride..spos + j * sstride + w + 1];
        let r1 = &src[spos + (j + 1) * sstride..spos + (j + 1) * sstride + w + 1];
        let o = &mut dst[doff + j * dstride..doff + j * dstride + w];
        if avg {
            for i in 0..w {
                let v = (a * r0[i] as u32 + b * r0[i + 1] as u32 + c * r1[i] as u32 + d * r1[i + 1] as u32 + bias) >> 6;
                o[i] = ((o[i] as u32 + v + 1) >> 1) as u8;
            }
        } else {
            for i in 0..w {
                o[i] = ((a * r0[i] as u32 + b * r0[i + 1] as u32 + c * r1[i] as u32 + d * r1[i + 1] as u32 + bias) >> 6) as u8;
            }
        }
    }
}

/// VC-1 emulation prevention removal (0x000003 followed by a byte < 4).
pub fn unescape(src: &[u8]) -> Vec<u8> {
    let mut dst = Vec::with_capacity(src.len());
    let size = src.len();
    if size < 4 {
        dst.extend_from_slice(src);
        return dst;
    }
    let mut i = 0;
    while i < size {
        if src[i] == 3 && i >= 2 && src[i - 1] == 0 && src[i - 2] == 0 && i < size - 1 && src[i + 1] < 4 {
            dst.push(src[i + 1]);
            i += 2;
        } else {
            dst.push(src[i]);
            i += 1;
        }
    }
    dst
}
