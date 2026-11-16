//! Packing of rust_h265 picture state into the GPU layouts of web/gpu-recon/FORMAT-deblock.md and
//! FORMAT-sao.md. This is the code G0 ports: it reads only `PictureState` fields rust_h265 already fills
//! (boundary strengths, QP, no-filter, slice / tile tables, SAO params) and calls the same slice / tile
//! boundary rules the CPU filters use, so the GPU never needs PU / TU / MV information.

use crate::deblock_ref::{get_qp_y, skip_horizontal_slice_boundary, skip_vertical_slice_boundary};
use crate::sao_ref::{SaoType, sao_skip_slice_boundary};
use crate::state::{PictureState, Pixel, Pps, Sps};

pub fn align64(v: usize) -> usize {
    (v + 63) & !63
}

/// The 16-word uniform block shared by the deblocking and SAO kernels.
pub fn params_words<P: Pixel>(state: &PictureState<P>, sps: &Sps, pps: &Pps) -> [u32; 16] {
    let (w, h) = (state.width as usize, state.height as usize);
    let (stride_y, stride_c) = (align64(w), align64(w / 2));
    let off_cb = stride_y * h / 2;
    let off_cr = off_cb + stride_c * (h / 2) / 2;
    let ctb = 1usize << state.log2_ctb_size;
    [
        w as u32,
        h as u32,
        stride_y as u32,
        stride_c as u32,
        off_cb as u32,
        off_cr as u32,
        sps.bit_depth_luma as u32,
        sps.bit_depth_chroma as u32,
        state.log2_ctb_size as u32,
        w.div_ceil(ctb) as u32,
        pps.pps_cb_qp_offset as u32,
        pps.pps_cr_qp_offset as u32,
        state.any_no_filter as u32,
        h.div_ceil(ctb) as u32,
        (w / 8) as u32,
        0,
    ]
}

/// The picture buffer of web/gpu-recon/README.md: Y, Cb, Cr back to back, two 16-bit samples per u32 (even x in
/// the low half), plane strides `align64(width)` and `align64(width / 2)`. `planes` have rows of `strides` samples.
pub fn pack_picture<P: Pixel>(
    planes: [&[P]; 3],
    strides: [usize; 3],
    width: usize,
    height: usize,
) -> Vec<u32> {
    let dims = [(width, height), (width / 2, height / 2), (width / 2, height / 2)];
    let mut out = Vec::new();
    for c in 0..3 {
        let (pw, ph) = dims[c];
        let gs = align64(pw);
        for y in 0..ph {
            for xw in 0..gs / 2 {
                let s = |x: usize| {
                    if x < pw {
                        planes[c][y * strides[c] + x].to_i32() as u32
                    } else {
                        0
                    }
                };
                out.push(s(2 * xw) | (s(2 * xw + 1) << 16));
            }
        }
    }
    out
}


/// One u32 per 8x8 luma block (FORMAT-deblock.md "block map"): the boundary strengths the edge filters read at
/// that block (vertical edge at its left side for both 4-row halves, horizontal edge at its top for both 4-column
/// halves), the four 4x4 no-filter flags and QpY.
pub fn pack_block8_map<P: Pixel>(state: &PictureState<P>) -> Vec<u32> {
    let (w, h) = (state.width as usize, state.height as usize);
    let bs_w = w >> 2;
    let bs = |tab: &[u8], x: usize, y: usize| (tab[(y >> 2) * bs_w + (x >> 2)] as u32) & 3;
    let nf = |x: usize, y: usize| {
        (state.tab_no_filter[(y >> 2) * state.min_pu_width + (x >> 2)] != 0) as u32
    };
    let mut out = Vec::with_capacity((w / 8) * (h / 8));
    for by in 0..h / 8 {
        for bx in 0..w / 8 {
            let (x, y) = (bx * 8, by * 8);
            let qp = get_qp_y(state, x as i32, y as i32);
            out.push(
                bs(&state.bs_vertical, x, y)
                    | bs(&state.bs_vertical, x, y + 4) << 2
                    | bs(&state.bs_horizontal, x, y) << 4
                    | bs(&state.bs_horizontal, x + 4, y) << 6
                    | nf(x, y) << 8
                    | nf(x + 4, y) << 9
                    | nf(x, y + 4) << 10
                    | nf(x + 4, y + 4) << 11
                    | ((qp as i8 as u8 as u32) << 16),
            );
        }
    }
    out
}

/// One u32 per CTB (raster order) for deblocking: bit 0 = do not filter the vertical edge on the CTB's left
/// side, bit 1 = do not filter the horizontal edge on its top side (slice / tile rules of
/// `skip_*_slice_boundary`), bits 8..15 beta_offset (i8), bits 16..23 tc_offset (i8).
pub fn pack_deblock_ctbs<P: Pixel>(state: &PictureState<P>) -> Vec<u32> {
    let (w, h) = (state.width as usize, state.height as usize);
    let l = state.log2_ctb_size as usize;
    let (wc, hc) = (w.div_ceil(1 << l), h.div_ceil(1 << l));
    let mut out = Vec::with_capacity(wc * hc);
    for cy in 0..hc {
        for cx in 0..wc {
            let (x, y) = (cx << l, cy << l);
            let skip_left = cx > 0 && skip_vertical_slice_boundary(state, x, y);
            let skip_top = cy > 0 && skip_horizontal_slice_boundary(state, x, y);
            let (b, t) = state.tab_deblock_offsets[cy * wc + cx];
            out.push(
                skip_left as u32
                    | (skip_top as u32) << 1
                    | (b as u8 as u32) << 8
                    | (t as u8 as u32) << 16,
            );
        }
    }
    out
}

/// Four u32 per CTB (raster order) for SAO (FORMAT-sao.md).
pub fn pack_sao_ctbs<P: Pixel>(state: &PictureState<P>) -> Vec<u32> {
    let (w, h) = (state.width as usize, state.height as usize);
    let l = state.log2_ctb_size as usize;
    let (wc, hc) = (w.div_ceil(1 << l), h.div_ceil(1 << l));
    let mut out = Vec::with_capacity(wc * hc * 4);
    for ry in 0..hc {
        for rx in 0..wc {
            let rs = ry * wc + rx;
            let sao = &state.sao_params[rs];
            // Same availability rule as sao_ctb_rows_at.
            let mut avail = 0u32;
            for dy in 0..3 {
                for dx in 0..3 {
                    let (nx, ny) = ((rx + dx).wrapping_sub(1), (ry + dy).wrapping_sub(1));
                    let ok = nx < wc
                        && ny < hc
                        && (nx == rx && ny == ry
                            || !sao_skip_slice_boundary(state, rs, ny * wc + nx));
                    avail |= (ok as u32) << (dy * 3 + dx);
                }
            }
            let mut w0 = avail;
            let mut offs = [0u32; 3];
            for c in 0..3 {
                let t = match sao.type_idx[c] {
                    SaoType::NotApplied => 0u32,
                    SaoType::Band => 1,
                    SaoType::Edge => 2,
                };
                let class = match sao.type_idx[c] {
                    SaoType::Band => sao.band_position[c] as u32 & 31,
                    SaoType::Edge => sao.eo_class[c] as u32 & 3,
                    SaoType::NotApplied => 0,
                };
                w0 |= t << (9 + 2 * c) | class << (15 + 5 * c);
                for k in 0..4 {
                    let o = sao.offset_val[c][k + 1];
                    debug_assert!((-128..=127).contains(&o));
                    offs[c] |= (o as i8 as u8 as u32) << (8 * k);
                }
            }
            out.extend_from_slice(&[w0, offs[0], offs[1], offs[2]]);
        }
    }
    out
}
