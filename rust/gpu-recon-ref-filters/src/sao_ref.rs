//! Verbatim copy of the filter half of rust_h265 src/sao.rs at commit 9515fd59 (lines 28-54 and 226-791): SaoParams, the band and
//! edge kernels, slice/tile availability, rolling and snapshot picture paths, restore_no_filter. Changes:
//! crate paths point at this crate's stand-ins and the multi-band path of apply_sao_picture runs as one band.

/// Per-CTB SAO type per plane.
use crate::state::{PictureState, Pixel, SharedTable, Sps};

/// rust_h265 `parallel::Band`: each plane's rows from a start row on, with that row.
pub type Band<'a, P> = [(&'a mut [P], usize); 3];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SaoType {
    #[default]
    NotApplied,
    Band,
    Edge,
}

/// Per-CTB SAO parameters for one CTB (covers all 3 planes for 4:2:0).
#[derive(Debug, Clone, Default)]
pub struct SaoParams {
    /// Per-plane filter type (Y, Cb, Cr).
    pub type_idx: [SaoType; 3],
    /// Per-plane signed offset values, indexed by category (cat 0 → 0).
    pub offset_val: [[i16; 5]; 3],
    /// Per-plane edge offset class (only used when `type_idx == Edge`).
    pub eo_class: [u8; 3],
    /// Per-plane band offset starting band (only used when `type_idx == Band`).
    pub band_position: [u8; 3],
}

/// EO class - direction of neighbor comparison.
const SAO_EO_HORIZ: u8 = 0;
const SAO_EO_VERT: u8 = 1;
const SAO_EO_135D: u8 = 2;
const SAO_EO_45D: u8 = 3;
// ---- Filter kernels ----

/// Band offset filter: each sample is mapped to one of 32 bands; if its
/// band is in the 4 active bands starting at `sao_left_class`, the
/// corresponding offset is added.
#[allow(clippy::too_many_arguments)]
fn sao_band_filter<P: Pixel>(
    dst: &mut [P],
    dst_row0: usize,
    src: &[P],
    src_row0: usize,
    stride_dst: usize,
    stride_src: usize,
    offset_val: &[i16; 5],
    sao_left_class: u8,
    width: usize,
    height: usize,
    x0: usize,
    y0: usize,
    bit_depth: u8,
) {
    // Build a 32-entry offset table - only the 4 active bands have offsets.
    let mut offset_table = [0i16; 32];
    for k in 0..4 {
        offset_table[(k + sao_left_class as usize) & 31] = offset_val[k + 1];
    }
    let shift = bit_depth - 5;
    for y in 0..height {
        for x in 0..width {
            let sample = src[(y0 + y - src_row0) * stride_src + (x0 + x)].to_i32();
            let band = (sample >> shift) & 31;
            let new_val = sample + offset_table[band as usize] as i32;
            dst[(y0 + y - dst_row0) * stride_dst + (x0 + x)] =
                P::from_i32_clamped(new_val, bit_depth);
        }
    }
}

/// Compare-with-neighbor sign function used by edge offset.
#[inline]
fn cmp(a: i32, b: i32) -> i32 {
    (a > b) as i32 - (a < b) as i32
}

/// Edge offset filter: per-pixel category (1..4) drives the offset.
///
/// `avail[1 + dy][1 + dx]` says whether samples of the CTB at offset
/// `(dx, dy)` from this one may be used as neighbours: false outside the
/// picture and across a slice or tile boundary that must not be filtered
/// across (spec 8.7.3.2). A sample whose neighbour along the EO direction is
/// unavailable is left unchanged. The check is per sample, so a CTB corner
/// sample uses the diagonal CTB's availability, not that of the CTB above,
/// below or beside it.
///
/// `dst` holds the destination plane from row `dst_row0` on (a band of it
/// when SAO runs on several threads); `src` holds the snapshot from row `src_row0` on.
#[allow(clippy::too_many_arguments)]
fn sao_edge_filter<P: Pixel>(
    dst: &mut [P],
    dst_row0: usize,
    src: &[P],
    src_row0: usize,
    stride_dst: usize,
    stride_src: usize,
    offset_val: &[i16; 5],
    eo: u8,
    width: usize,
    height: usize,
    x0: usize,
    y0: usize,
    avail: &[[bool; 3]; 3],
    bit_depth: u8,
) {
    // edge_idx maps (1 + cmp(a) + cmp(b)) → category.
    // FFmpeg uses { 1, 2, 0, 3, 4 } indexed by [2 + diff0 + diff1].
    static EDGE_IDX: [usize; 5] = [1, 2, 0, 3, 4];
    // Direction offsets for the two neighbors (a and b).
    let (a_dx, a_dy, b_dx, b_dy): (isize, isize, isize, isize) = match eo {
        SAO_EO_HORIZ => (-1, 0, 1, 0),
        SAO_EO_VERT => (0, -1, 0, 1),
        SAO_EO_135D => (-1, -1, 1, 1),
        SAO_EO_45D => (1, -1, -1, 1),
        _ => return,
    };
    let (w, h) = (width as isize, height as isize);
    // Which neighbouring CTB a CTB-relative coordinate falls in.
    let region = |v: isize, n: isize| -> usize {
        if v < 0 {
            0
        } else if v >= n {
            2
        } else {
            1
        }
    };
    let neighbour_ok = |x: isize, y: isize, dx: isize, dy: isize| -> bool {
        avail[region(y + dy, h)][region(x + dx, w)]
    };
    let a_off = a_dy * stride_src as isize + a_dx;
    let b_off = b_dy * stride_src as isize + b_dx;
    // Offset by the sum of the two neighbour comparisons (-2..=2).
    let off_by_sum: [i32; 5] = std::array::from_fn(|k| offset_val[EDGE_IDX[k]] as i32);
    let edge = |c: i32, a: i32, b: i32| c + off_by_sum[(2 + cmp(c, a) + cmp(c, b)) as usize];

    let edge_ok =
        |x: isize, y: isize| neighbour_ok(x, y, a_dx, a_dy) && neighbour_ok(x, y, b_dx, b_dy);
    for y in 0..height {
        let yi = y as isize;
        // Interior samples (x in 1..width-1) have both neighbours in this
        // CTB column, so only the CTB rows above / below matter. The first
        // and last sample may reach into the CTB to the left / right or
        // diagonally; when their neighbours are available too (the usual
        // case) they join the same run, so a 64-wide row is one vectorised
        // call with no scalar tail.
        let interior_ok =
            width > 2 && avail[region(yi + a_dy, h)][1] && avail[region(yi + b_dy, h)][1];
        let first_ok = edge_ok(0, yi);
        let last_ok = width > 1 && edge_ok(width as isize - 1, yi);
        let (start, end) = if interior_ok {
            (
                if first_ok { 0 } else { 1 },
                if last_ok { width } else { width - 1 },
            )
        } else {
            (0, 0)
        };
        if start < end {
            let n = end - start;
            let row = (y0 + y - src_row0) * stride_src + x0 + start;
            let cur = &src[row..row + n];
            let a = &src[(row as isize + a_off) as usize..][..n];
            let b = &src[(row as isize + b_off) as usize..][..n];
            let drow = (y0 + y - dst_row0) * stride_dst + x0 + start;
            let out = &mut dst[drow..drow + n];
            edge_offset_row(out, cur, a, b, &off_by_sum, bit_depth);
        }
        // Edge samples not covered by the run (no interior run, or a run
        // that had to stop short of them) take the per-sample path.
        let edges: &[usize] = if width == 1 { &[0] } else { &[0, width - 1] };
        for &x in edges {
            if x >= start && x < end {
                continue;
            }
            let xi = x as isize;
            if edge_ok(xi, yi) {
                let si = (y0 + y - src_row0) * stride_src + x0 + x;
                let cur = src[si].to_i32();
                let a = src[(si as isize + a_off) as usize].to_i32();
                let b = src[(si as isize + b_off) as usize].to_i32();
                dst[(y0 + y - dst_row0) * stride_dst + x0 + x] =
                    P::from_i32_clamped(edge(cur, a, b), bit_depth);
            }
        }
    }
}

/// Edge offset over one run of interior samples: `out[i] = clip(cur[i] +
/// offset)`, the offset chosen by `sign(cur - a) + sign(cur - b)`. Written
/// as comparisons and selects rather than a table lookup so LLVM can
/// vectorise it (SAO edge offset was 9% of decode time as a scalar loop).
#[inline(always)]
fn edge_offset_row<P: Pixel>(
    out: &mut [P],
    cur: &[P],
    a: &[P],
    b: &[P],
    off_by_sum: &[i32; 5],
    bit_depth: u8,
) {
    // Everything fits i16 (samples at most 12-bit, offsets at most 31 << 2),
    // so the loop runs in 16-bit lanes: twice the samples per vector
    // instruction of i32.
    let [o_m2, o_m1, _, o_p1, o_p2] = off_by_sum.map(|o| o as i16);
    let max = ((1i32 << bit_depth) - 1) as i16;
    for (((o, &c), &a), &b) in out.iter_mut().zip(cur).zip(a).zip(b) {
        let v = edge_offset_px(
            c.to_i32() as i16,
            a.to_i32() as i16,
            b.to_i32() as i16,
            [o_m2, o_m1, o_p1, o_p2],
            max,
        );
        *o = P::from_i32_clamped(v as i32, bit_depth);
    }
}

/// One edge-offset sample: `clip(c + offset)`, the offset picked by
/// `sign(c - a) + sign(c - b)` through selects (vectorisable), with
/// `offs = [offset(-2), offset(-1), offset(+1), offset(+2)]`.
#[inline(always)]
fn edge_offset_px(c: i16, a: i16, b: i16, offs: [i16; 4], max: i16) -> i16 {
    let s = (c > a) as i16 - (c < a) as i16 + (c > b) as i16 - (c < b) as i16;
    let off = if s == -2 {
        offs[0]
    } else if s == -1 {
        offs[1]
    } else if s == 1 {
        offs[2]
    } else if s == 2 {
        offs[3]
    } else {
        0
    };
    (c + off).clamp(0, max)
}

/// Check if two adjacent CTBs (by raster address) are separated by a tile
/// boundary that must not be filtered across, or are in different slices
/// with loop filtering across that slice boundary disabled.
/// Returns true if the boundary should be treated as "no filtering across".
#[inline]
pub fn sao_skip_slice_boundary<P: Pixel>(state: &PictureState<P>, rs_a: usize, rs_b: usize) -> bool {
    if !state.filter_across_tiles && state.tab_tile_id[rs_a] != state.tab_tile_id[rs_b] {
        return true; // tile boundary, filtering across tiles disabled
    }
    if state.tab_slice_addr_rs[rs_a] == state.tab_slice_addr_rs[rs_b] {
        return false;
    }
    // Across a slice boundary the flag of the slice that is later in
    // decoding order applies (spec 8.7.3.2).
    let later = if state.tab_ctb_ts[rs_a] > state.tab_ctb_ts[rs_b] {
        rs_a
    } else {
        rs_b
    };
    !state.filter_slice_edges[later]
}

/// Apply SAO to the entire reconstructed picture, after deblocking.
/// Per-CTB SAO parameters must already be in `state.sao_params`.
///
/// Uses up to `threads` threads, over bands of CTB rows; the result is
/// identical for any thread count.
pub fn apply_sao_picture<P: Pixel>(state: &mut PictureState<P>, sps: &Sps, threads: usize) {
    // Each CTB's parameters say whether its slice uses SAO (they stay "not
    // applied" when the slice's flags are off), so no slice header is
    // needed.
    if !sps.sample_adaptive_offset_enabled_flag || !any_sao(state) {
        return;
    }
    let pic_h = state.height as usize;
    let ctb_size = 1usize << sps.ctb_log2_size_y;
    let pic_h_in_ctbs = pic_h.div_ceil(ctb_size);

    let ctb_c_h = ctb_size >> state.chroma_shift_h;
    if threads <= 1 && !state.any_no_filter {
        sao_picture_rolling(state, sps, pic_h_in_ctbs, [ctb_size, ctb_c_h, ctb_c_h]);
        return;
    }
    // One band covering every CTB row: the thread-count-1 case of the original multi-band path, taken here
    // when `any_no_filter` (the original takes it then too).
    let _ = threads;
    let mut planes = [
        std::mem::take(&mut state.y_plane),
        std::mem::take(&mut state.u_plane),
        std::mem::take(&mut state.v_plane),
    ];
    let snapshot: [SharedTable<P>; 3] = [planes[0].clone(), planes[1].clone(), planes[2].clone()];
    {
        let src = [&snapshot[0][..], &snapshot[1][..], &snapshot[2][..]];
        let state: &PictureState<P> = state;
        let [y, u, v] = &mut planes;
        let band: Band<'_, P> = [(&mut y[..], 0), (&mut u[..], 0), (&mut v[..], 0)];
        sao_ctb_rows(state, sps, src, band, 0..pic_h_in_ctbs);
    }
    let [y, u, v] = planes;
    state.y_plane = y;
    state.u_plane = u;
    state.v_plane = v;
    let src = [&snapshot[0][..], &snapshot[1][..], &snapshot[2][..]];
    let block_rows = state.tab_no_filter.len() / state.min_pu_width.max(1);
    restore_no_filter(state, src, 0..block_rows);
    state.sao_snapshot = snapshot;
}

/// Single-threaded `apply_sao_picture`, CTB row by CTB row, with the deblocked samples each row reads (its
/// own rows and one more above and below) copied into a buffer of `ctb rows + 2` rows instead of a snapshot
/// of the whole picture (25 MB for 4K 10-bit), so the copy stays in cache until the filter reads it: about
/// 1% less decode time on the 4K FEL base layers (WASM), and no picture-sized buffer. The last two rows of
/// one CTB row's buffer (still unfiltered when copied) start the next one's.
fn sao_picture_rolling<P: Pixel>(
    state: &mut PictureState<P>,
    sps: &Sps,
    ctb_rows: usize,
    ctb_h: [usize; 3],
) {
    let mut planes = [
        std::mem::take(&mut state.y_plane),
        std::mem::take(&mut state.u_plane),
        std::mem::take(&mut state.v_plane),
    ];
    let strides = [state.y_stride, state.uv_stride, state.uv_stride];
    let plane_rows: [usize; 3] = std::array::from_fn(|c| planes[c].len() / strides[c].max(1));
    let mut buf = std::mem::take(&mut state.sao_snapshot);
    for c in 0..3 {
        buf[c].resize((ctb_h[c] + 2) * strides[c], P::zero());
    }
    // Rows held in `buf[c]`: plane rows `held[c]`.
    let mut held: [std::ops::Range<usize>; 3] = std::array::from_fn(|_| 0..0);
    for ry in 0..ctb_rows {
        let mut band_rows: [std::ops::Range<usize>; 3] = std::array::from_fn(|_| 0..0);
        for c in 0..3 {
            let (stride, n) = (strides[c], plane_rows[c]);
            let r0 = (ry * ctb_h[c]).min(n);
            let r1 = if ry + 1 == ctb_rows {
                n
            } else {
                ((ry + 1) * ctb_h[c]).min(n)
            };
            band_rows[c] = r0..r1;
            let want = r0.saturating_sub(1)..(r1 + 1).min(n);
            // Rows already held (unfiltered when they were copied) move to the top.
            let keep = want.start.max(held[c].start)..want.end.min(held[c].end);
            let mut have = want.start;
            if keep.start == want.start && keep.start < keep.end {
                let from = (keep.start - held[c].start) * stride;
                buf[c].copy_within(from..from + keep.len() * stride, 0);
                have = keep.end;
            }
            let rows = want.end.saturating_sub(have);
            if buf[c].len() < (want.len()) * stride {
                buf[c].resize(want.len() * stride, P::zero());
            }
            let at = (have - want.start) * stride;
            buf[c][at..at + rows * stride]
                .copy_from_slice(&planes[c][have * stride..(have + rows) * stride]);
            held[c] = want;
        }
        let [y, u, v] = &mut planes;
        let band = [
            (
                &mut y[band_rows[0].start * strides[0]..band_rows[0].end * strides[0]],
                band_rows[0].start,
            ),
            (
                &mut u[band_rows[1].start * strides[1]..band_rows[1].end * strides[1]],
                band_rows[1].start,
            ),
            (
                &mut v[band_rows[2].start * strides[2]..band_rows[2].end * strides[2]],
                band_rows[2].start,
            ),
        ];
        let src = [&buf[0][..], &buf[1][..], &buf[2][..]];
        let src_row0 = [held[0].start, held[1].start, held[2].start];
        sao_ctb_rows_at(state, sps, src, src_row0, band, ry..ry + 1);
    }
    let [y, u, v] = planes;
    state.y_plane = y;
    state.u_plane = u;
    state.v_plane = v;
    state.sao_snapshot = buf;
}

/// Whether any CTB of the picture uses SAO.
pub fn any_sao<P: Pixel>(state: &PictureState<P>) -> bool {
    (0..state.sao_params.len()).any(|i| {
        state.sao_params[i]
            .type_idx
            .iter()
            .any(|&t| t != SaoType::NotApplied)
    })
}

/// SAO of CTB rows `rows`, writing `band` (the three planes from rows
/// `band[c].1` on) and reading the deblocked samples `src` (whole planes,
/// at least the rows these CTBs and their neighbours cover).
pub fn sao_ctb_rows<P: Pixel>(
    state: &PictureState<P>,
    sps: &Sps,
    src: [&[P]; 3],
    band: Band<'_, P>,
    rows: std::ops::Range<usize>,
) {
    sao_ctb_rows_at(state, sps, src, [0; 3], band, rows)
}

/// `sao_ctb_rows` with `src[c]` holding the deblocked samples from row `src_row0[c]` on.
fn sao_ctb_rows_at<P: Pixel>(
    state: &PictureState<P>,
    sps: &Sps,
    [y_src, u_src, v_src]: [&[P]; 3],
    src_row0: [usize; 3],
    band: Band<'_, P>,
    rows: std::ops::Range<usize>,
) {
    let [(y_band, y_row0), (u_band, c_row0), (v_band, _)] = band;
    let pic_w = state.width as usize;
    let pic_h = state.height as usize;
    let pic_w_c = (state.width >> state.chroma_shift_w) as usize;
    let pic_h_c = (state.height >> state.chroma_shift_h) as usize;
    let ctb_size = 1usize << sps.ctb_log2_size_y;
    let pic_w_in_ctbs = pic_w.div_ceil(ctb_size);
    let pic_h_in_ctbs = pic_h.div_ceil(ctb_size);
    for ry in rows.clone() {
        for rx in 0..pic_w_in_ctbs {
            let ctb_rs = ry * pic_w_in_ctbs + rx;
            let sao = state.sao_params[ctb_rs].clone();

            // Which of the 8 neighbouring CTBs may supply EO neighbours.
            let mut avail = [[false; 3]; 3];
            for (dy, row) in avail.iter_mut().enumerate() {
                for (dx, ok) in row.iter_mut().enumerate() {
                    let (nx, ny) = ((rx + dx).wrapping_sub(1), (ry + dy).wrapping_sub(1));
                    *ok = nx < pic_w_in_ctbs
                        && ny < pic_h_in_ctbs
                        && (nx == rx && ny == ry
                            || !sao_skip_slice_boundary(state, ctb_rs, ny * pic_w_in_ctbs + nx));
                }
            }

            // Luma
            if sao.type_idx[0] != SaoType::NotApplied {
                let x0 = rx * ctb_size;
                let y0 = ry * ctb_size;
                let w = (x0 + ctb_size).min(pic_w) - x0;
                let h = (y0 + ctb_size).min(pic_h) - y0;
                let bit_depth_y = sps.bit_depth_luma;
                match sao.type_idx[0] {
                    SaoType::Band => sao_band_filter::<P>(
                        y_band,
                        y_row0,
                        y_src,
                        src_row0[0],
                        state.y_stride,
                        state.y_stride,
                        &sao.offset_val[0],
                        sao.band_position[0],
                        w,
                        h,
                        x0,
                        y0,
                        bit_depth_y,
                    ),
                    SaoType::Edge => sao_edge_filter::<P>(
                        y_band,
                        y_row0,
                        y_src,
                        src_row0[0],
                        state.y_stride,
                        state.y_stride,
                        &sao.offset_val[0],
                        sao.eo_class[0],
                        w,
                        h,
                        x0,
                        y0,
                        &avail,
                        bit_depth_y,
                    ),
                    SaoType::NotApplied => {}
                }
            }

            // Chroma: operates on the chroma CTU grid, sized by the chroma
            // subsampling (equal to the luma grid for 4:4:4).
            if state.chroma_array_type != 0 {
                let ctb_c_w = ctb_size >> state.chroma_shift_w;
                let ctb_c_h = ctb_size >> state.chroma_shift_h;
                let x0_c = rx * ctb_c_w;
                let y0_c = ry * ctb_c_h;
                let w_c = (x0_c + ctb_c_w).min(pic_w_c) - x0_c;
                let h_c = (y0_c + ctb_c_h).min(pic_h_c) - y0_c;
                for c_idx in 1..=2 {
                    if sao.type_idx[c_idx] == SaoType::NotApplied {
                        continue;
                    }
                    let (dst_plane, src_plane) = if c_idx == 1 {
                        (&mut *u_band, u_src)
                    } else {
                        (&mut *v_band, v_src)
                    };
                    let src_row0 = src_row0[c_idx];
                    let bit_depth_c = sps.bit_depth_chroma;
                    match sao.type_idx[c_idx] {
                        SaoType::Band => sao_band_filter::<P>(
                            dst_plane,
                            c_row0,
                            src_plane,
                            src_row0,
                            state.uv_stride,
                            state.uv_stride,
                            &sao.offset_val[c_idx],
                            sao.band_position[c_idx],
                            w_c,
                            h_c,
                            x0_c,
                            y0_c,
                            bit_depth_c,
                        ),
                        SaoType::Edge => sao_edge_filter::<P>(
                            dst_plane,
                            c_row0,
                            src_plane,
                            src_row0,
                            state.uv_stride,
                            state.uv_stride,
                            &sao.offset_val[c_idx],
                            sao.eo_class[c_idx],
                            w_c,
                            h_c,
                            x0_c,
                            y0_c,
                            &avail,
                            bit_depth_c,
                        ),
                        SaoType::NotApplied => {}
                    }
                }
            }
        }
    }
}

/// Samples of lossless or PCM-protected blocks keep their deblocked values
/// (spec 8.7.3): copy them back from the SAO input `src`, for the 4x4
/// block rows `block_rows`.
pub fn restore_no_filter<P: Pixel>(
    state: &mut PictureState<P>,
    [y_src, u_src, v_src]: [&[P]; 3],
    block_rows: std::ops::Range<usize>,
) {
    if state.any_no_filter {
        let (csw, csh) = (state.chroma_shift_w as usize, state.chroma_shift_h as usize);
        let (ys, uvs) = (state.y_stride, state.uv_stride);
        for by in block_rows {
            for bx in 0..state.min_pu_width {
                if state.tab_no_filter[by * state.min_pu_width + bx] == 0 {
                    continue;
                }
                let (x, y) = (bx * 4, by * 4);
                for r in y..y + 4 {
                    let o = r * ys + x;
                    if o + 4 <= state.y_plane.len() {
                        state.y_plane[o..o + 4].copy_from_slice(&y_src[o..o + 4]);
                    }
                }
                let (cx, cy, cw, ch) = (x >> csw, y >> csh, 4 >> csw, 4 >> csh);
                for r in cy..cy + ch {
                    let o = r * uvs + cx;
                    if o + cw <= state.u_plane.len() {
                        state.u_plane[o..o + cw].copy_from_slice(&u_src[o..o + cw]);
                        state.v_plane[o..o + cw].copy_from_slice(&v_src[o..o + cw]);
                    }
                }
            }
        }
    }
}
