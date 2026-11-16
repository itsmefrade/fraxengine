//! Verbatim copies of `compute_luma_avail_inner` and `cip_masks` from rust_h265 src/cu_tree.rs (identical at commit 9515fd59;
//! both private there), over a `PictureState` that carries only the fields they read.
use crate::pixel::Pixel;
pub use crate::intra_pred::ReferenceAvailability;
use std::marker::PhantomData;

/// The subset of rust_h265's `PictureState` the two functions below read. Field names and types match
/// (`SharedTable<T>` there indexes like a `Vec<T>` here).
pub struct PictureState<P: Pixel> {
    pub width: u32,
    pub height: u32,
    pub log2_ctb_size: u8,
    pub log2_min_pu_size: u8,
    pub min_pu_width: usize,
    pub tab_slice_addr_rs: Vec<i32>,
    pub tab_tile_id: Vec<u32>,
    pub tab_mvf: Vec<MvField>,
    pub _p: PhantomData<P>,
}

#[derive(Clone, Copy, Default)]
pub struct MvField {
    /// 0 = intra (rust_h265 `MvField::pred_flag`).
    pub pred_flag: u8,
}

/// Core availability computation. When `constrained_intra_pred` is true,
/// additionally requires that all neighbor min-PUs in each direction were
/// intra-coded (pred_flag == 0 in tab_mvf).
pub fn compute_luma_avail_inner<P: Pixel>(
    state: &PictureState<P>,
    x0: u32,
    y0: u32,
    size: u32,
    constrained_intra_pred: bool,
) -> ReferenceAvailability {
    let pic_w = state.width;
    let pic_h = state.height;
    let log2_ctb = state.log2_ctb_size;
    let ctb_size = 1u32 << log2_ctb;
    let pic_w_in_ctbs = pic_w.div_ceil(ctb_size);

    // Current block's CTB raster address, slice addr, tile id.
    let ctb_x = x0 >> log2_ctb;
    let ctb_y = y0 >> log2_ctb;
    let cur_ctb_rs = ctb_y * pic_w_in_ctbs + ctb_x;
    let cur_slice_addr = state
        .tab_slice_addr_rs
        .get(cur_ctb_rs as usize)
        .copied()
        .unwrap_or(-1);
    let cur_tile_id = state
        .tab_tile_id
        .get(cur_ctb_rs as usize)
        .copied()
        .unwrap_or(0);

    // --- Step 1: CTB-level flags (FFmpeg hls_decode_neighbour) ---
    // Check whether a neighboring CTB is available (decoded, same slice & tile).
    let ctb_avail = |cx: u32, cy: u32| -> bool {
        let rs = cy * pic_w_in_ctbs + cx;
        let sa = state
            .tab_slice_addr_rs
            .get(rs as usize)
            .copied()
            .unwrap_or(-1);
        sa >= 0
            && sa == cur_slice_addr
            && state.tab_tile_id.get(rs as usize).copied().unwrap_or(0) == cur_tile_id
    };
    let ctb_left_flag = ctb_x > 0 && ctb_avail(ctb_x - 1, ctb_y);
    let ctb_up_flag = ctb_y > 0 && ctb_avail(ctb_x, ctb_y - 1);
    let ctb_up_left_flag = ctb_x > 0 && ctb_y > 0 && ctb_avail(ctb_x - 1, ctb_y - 1);
    let ctb_up_right_flag =
        ctb_y > 0 && ctb_x + 1 < pic_w_in_ctbs && ctb_avail(ctb_x + 1, ctb_y - 1);

    // --- Step 2: CU/TU-level flags (FFmpeg ff_hevc_set_neighbour_available) ---
    let x0b = x0 & (ctb_size - 1);
    let y0b = y0 & (ctb_size - 1);

    let cand_up = y0b > 0 || ctb_up_flag;
    let cand_left = x0b > 0 || ctb_left_flag;
    let cand_up_left = if x0b > 0 || y0b > 0 {
        cand_left && cand_up
    } else {
        ctb_up_left_flag
    };

    // cand_up_right: if the TU's right edge reaches exactly the CTB
    // boundary, the up-right samples are in the CTB above-right.
    let cand_up_right = if x0b + size == ctb_size {
        // Cross-CTB: need above-right CTB + must be at CTB top edge.
        ctb_up_right_flag && y0b == 0
    } else {
        cand_up
    };
    // Additional clamp: up-right must be within picture width.
    let cand_up_right = cand_up_right && (x0 + size) < pic_w;

    // cand_bottom_left: if the TU's bottom edge reaches the bottom of the
    // current CTB (or tile/picture bottom), bottom-left is unavailable.
    // FFmpeg uses `end_of_tiles_y = min(y_ctb + ctb_size, height)`.
    let end_of_tiles_y = ((ctb_y + 1) * ctb_size).min(pic_h);
    let cand_bottom_left = if (y0 + size) >= end_of_tiles_y {
        false
    } else {
        cand_left
    };

    // --- Step 3: Z-scan refinement (FFmpeg pred_template.c) ---
    // Within-CTB Z-scan order determines if up-right/bottom-left were
    // decoded before the current block. Use masked coordinates and a
    // Z-interleave function. Sentinel: when a coordinate is -1 (off the
    // CTB edge), return -1 so that `cur > -1` is always true.
    let min_tb_log2 = 2u32; // 4×4 min TB for Main profile
    let log2_diff = (log2_ctb as u32) - min_tb_log2;
    let tb_mask = (1i32 << log2_diff) - 1;

    let x_tb = (x0 >> min_tb_log2) as i32 & tb_mask;
    let y_tb = (y0 >> min_tb_log2) as i32 & tb_mask;
    let size_in_tbs = (size >> min_tb_log2) as i32;

    let zscan = |x: i32, y: i32| -> i32 {
        if x < 0 || y < 0 {
            return -1;
        }
        let mut val = 0i32;
        for i in 0..log2_diff {
            let m = 1i32 << i;
            if x & m != 0 {
                val += m * m;
            }
            if y & m != 0 {
                val += 2 * m * m;
            }
        }
        val
    };

    let cur_z = zscan(x_tb, y_tb);

    let up_right = cand_up_right && cur_z > zscan((x_tb + size_in_tbs) & tb_mask, y_tb - 1);

    let bottom_left = cand_bottom_left && cur_z > zscan(x_tb - 1, (y_tb + size_in_tbs) & tb_mask);

    // --- up, left, up_left use simple raster-scan checks ---
    let up = cand_up && y0 > 0;
    let left = cand_left && x0 > 0;
    let up_left = cand_up_left && x0 > 0 && y0 > 0;

    let avail = ReferenceAvailability {
        up_left,
        up,
        up_right,
        left,
        bottom_left,
    };

    // constrained_intra_pred is applied per sample by the caller through
    // `cip_masks` (spec 8.4.4.2.2 marks individual samples of non-intra
    // blocks unavailable), not per direction here.
    let _ = constrained_intra_pred;
    avail
}

/// Per-sample "from an intra block" flags for constrained intra prediction
/// (spec 8.4.4.2.2): with constrained_intra_pred_flag, a reference sample in
/// a non-intra CU is unavailable. Returns `(top, left)` in the layout of
/// `build_reference_samples` (index 0 is the corner, then the top row
/// rightwards / the left column downwards, 2 * size samples each). `x0`,
/// `y0` are plane sample coordinates; `(sw, sh)` map them to luma.
pub fn cip_masks<P: Pixel>(
    state: &PictureState<P>,
    x0: usize,
    y0: usize,
    size: usize,
    (sw, sh): (u32, u32),
) -> (Vec<bool>, Vec<bool>) {
    let intra_at = |x: isize, y: isize| -> bool {
        if x < 0 || y < 0 {
            return false;
        }
        let (lx, ly) = ((x as usize) << sw, (y as usize) << sh);
        let idx =
            (ly >> state.log2_min_pu_size) * state.min_pu_width + (lx >> state.log2_min_pu_size);
        state.tab_mvf.get(idx).is_some_and(|m| m.pred_flag == 0)
    };
    let (x0, y0) = (x0 as isize, y0 as isize);
    let corner = intra_at(x0 - 1, y0 - 1);
    let mut top = vec![corner; 2 * size + 1];
    let mut left = vec![corner; 2 * size + 1];
    for i in 0..2 * size {
        top[1 + i] = intra_at(x0 + i as isize, y0 - 1);
        left[1 + i] = intra_at(x0 - 1, y0 + i as isize);
    }
    (top, left)
}
