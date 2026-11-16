//! HEVC residual coding decode (spec 7.3.8.11 → `residual_coding`).
//!
//! Phase 2c-3 scope: decode the CABAC `residual_coding` syntax for an
//! intra-only TU and produce a buffer of dequantized coefficients (no
//! inverse transform yet - that's Phase 2c-4).
//!
//! Mirrors FFmpeg `libavcodec/hevc/cabac.c` `ff_hevc_hls_residual_coding`
//! line by line for the supported subset:
//!   - 8-bit luma/chroma 4:2:0
//!   - `transform_skip_flag` is supported for Main Profile (4×4 TUs only)
//!   - `cu_transquant_bypass_flag` supported (skips dequant + transform)
//!   - `scaling_list_enabled_flag` supported (default or explicit lists)
//!   - No `persistent_rice_adaptation_enabled` (range extension)
//!   - `sign_data_hiding_enabled_flag` supported (Phase 3a-6)
//!   - No `explicit_rdpcm` (range extension)
//!
//! Anything outside that subset returns `Unsupported`.

use crate::cabac::{CabacContexts, CabacReader};
use crate::cabac_tables::ctx;
use crate::error::DecodeError;
use crate::pps::Pps;
use crate::scaling_list::ScalingList;
use crate::sps::Sps;

/// Component index passed into `decode_residual_coding`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResidualPlane {
    Luma,
    Cb,
    Cr,
}

impl ResidualPlane {
    fn c_idx(self) -> usize {
        match self {
            ResidualPlane::Luma => 0,
            ResidualPlane::Cb => 1,
            ResidualPlane::Cr => 2,
        }
    }
}

/// Coefficient scan order (HEVC spec 7.3.8.11). For non-`SCAN_DIAG` cases,
/// only certain block sizes are allowed (intra 4×4 / 8×8 luma).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanOrder {
    Diag,
    Horiz,
    Vert,
}

// ---- Scan tables (HEVC spec 6.5, FFmpeg `data.c`) ----------------------------

/// 4×4 diagonal scan order: x coordinates.
#[rustfmt::skip]
const DIAG_SCAN_4X4_X: [u8; 16] = [
    0, 0, 1, 0,
    1, 2, 0, 1,
    2, 3, 1, 2,
    3, 2, 3, 3,
];

/// 4×4 diagonal scan order: y coordinates.
#[rustfmt::skip]
const DIAG_SCAN_4X4_Y: [u8; 16] = [
    0, 1, 0, 2,
    1, 0, 3, 2,
    1, 0, 3, 2,
    1, 3, 2, 3,
];

/// 4×4 diagonal scan inverse: `(y, x) → scan_pos`.
#[rustfmt::skip]
const DIAG_SCAN_4X4_INV: [[u8; 4]; 4] = [
    [ 0,  2,  5,  9],
    [ 1,  4,  8, 12],
    [ 3,  7, 11, 14],
    [ 6, 10, 13, 15],
];

/// 4×4 horizontal scan order: x coordinates (row by row).
#[rustfmt::skip]
const HORIZ_SCAN_4X4_X: [u8; 16] = [
    0, 1, 2, 3,
    0, 1, 2, 3,
    0, 1, 2, 3,
    0, 1, 2, 3,
];
/// 4×4 horizontal scan order: y coordinates.
#[rustfmt::skip]
const HORIZ_SCAN_4X4_Y: [u8; 16] = [
    0, 0, 0, 0,
    1, 1, 1, 1,
    2, 2, 2, 2,
    3, 3, 3, 3,
];
/// 4×4 horizontal scan inverse: `(y, x) → scan_pos`.
#[rustfmt::skip]
const HORIZ_SCAN_4X4_INV: [[u8; 4]; 4] = [
    [ 0,  1,  2,  3],
    [ 4,  5,  6,  7],
    [ 8,  9, 10, 11],
    [12, 13, 14, 15],
];

/// 4×4 vertical scan order: x coordinates (column by column).
#[rustfmt::skip]
const VERT_SCAN_4X4_X: [u8; 16] = [
    0, 0, 0, 0,
    1, 1, 1, 1,
    2, 2, 2, 2,
    3, 3, 3, 3,
];
/// 4×4 vertical scan order: y coordinates.
#[rustfmt::skip]
const VERT_SCAN_4X4_Y: [u8; 16] = [
    0, 1, 2, 3,
    0, 1, 2, 3,
    0, 1, 2, 3,
    0, 1, 2, 3,
];
/// 4×4 vertical scan inverse: `(y, x) → scan_pos`.
#[rustfmt::skip]
const VERT_SCAN_4X4_INV: [[u8; 4]; 4] = [
    [ 0,  4,  8, 12],
    [ 1,  5,  9, 13],
    [ 2,  6, 10, 14],
    [ 3,  7, 11, 15],
];

/// 2×2 diagonal scan order: x coordinates.
const DIAG_SCAN_2X2_X: [u8; 4] = [0, 0, 1, 1];
/// 2×2 diagonal scan order: y coordinates.
const DIAG_SCAN_2X2_Y: [u8; 4] = [0, 1, 0, 1];
/// 2×2 diagonal scan inverse: `(y, x) → scan_pos`.
const DIAG_SCAN_2X2_INV: [[u8; 2]; 2] = [[0, 2], [1, 3]];
/// 2×2 horizontal scan order: x coordinates.
const HORIZ_SCAN_2X2_X: [u8; 4] = [0, 1, 0, 1];
/// 2×2 horizontal scan order: y coordinates.
const HORIZ_SCAN_2X2_Y: [u8; 4] = [0, 0, 1, 1];
/// 2×2 horizontal scan inverse: `(y, x) → scan_pos`.
const HORIZ_SCAN_2X2_INV: [[u8; 2]; 2] = [[0, 1], [2, 3]];
/// 2×2 vertical scan order: x coordinates.
const VERT_SCAN_2X2_X: [u8; 4] = [0, 0, 1, 1];
/// 2×2 vertical scan order: y coordinates.
const VERT_SCAN_2X2_Y: [u8; 4] = [0, 1, 0, 1];
/// 2×2 vertical scan inverse: `(y, x) → scan_pos`.
const VERT_SCAN_2X2_INV: [[u8; 2]; 2] = [[0, 2], [1, 3]];

/// 8×8 diagonal scan inverse: `(y, x) → scan_pos`.
#[rustfmt::skip]
const DIAG_SCAN_8X8_INV: [[u8; 8]; 8] = [
    [ 0,  2,  5,  9, 14, 20, 27, 35],
    [ 1,  4,  8, 13, 19, 26, 34, 42],
    [ 3,  7, 12, 18, 25, 33, 41, 48],
    [ 6, 11, 17, 24, 32, 40, 47, 53],
    [10, 16, 23, 31, 39, 46, 52, 57],
    [15, 22, 30, 38, 45, 51, 56, 60],
    [21, 29, 37, 44, 50, 55, 59, 62],
    [28, 36, 43, 49, 54, 58, 61, 63],
];

/// 8×8 diagonal scan: x coordinates (used as the sub-block scan for 32×32).
#[rustfmt::skip]
const DIAG_SCAN_8X8_X: [u8; 64] = [
    0, 0, 1, 0,
    1, 2, 0, 1,
    2, 3, 0, 1,
    2, 3, 4, 0,
    1, 2, 3, 4,
    5, 0, 1, 2,
    3, 4, 5, 6,
    0, 1, 2, 3,
    4, 5, 6, 7,
    1, 2, 3, 4,
    5, 6, 7, 2,
    3, 4, 5, 6,
    7, 3, 4, 5,
    6, 7, 4, 5,
    6, 7, 5, 6,
    7, 6, 7, 7,
];

/// 8×8 diagonal scan: y coordinates.
#[rustfmt::skip]
const DIAG_SCAN_8X8_Y: [u8; 64] = [
    0, 1, 0, 2,
    1, 0, 3, 2,
    1, 0, 4, 3,
    2, 1, 0, 5,
    4, 3, 2, 1,
    0, 6, 5, 4,
    3, 2, 1, 0,
    7, 6, 5, 4,
    3, 2, 1, 0,
    7, 6, 5, 4,
    3, 2, 1, 7,
    6, 5, 4, 3,
    2, 7, 6, 5,
    4, 3, 7, 6,
    5, 4, 7, 6,
    5, 7, 6, 7,
];

/// `level_scale[i]` from HEVC spec 8.6.3 (the dequant per-rem6 multiplier).
const LEVEL_SCALE: [u32; 6] = [40, 45, 51, 57, 64, 72];

/// Maximum bin count for `coeff_abs_level_remaining` Exp-Golomb prefix.
const CABAC_MAX_BIN: u32 = 31;

// ---- Syntax-element decoders (mirror FFmpeg) ---------------------------------

/// Decode `last_significant_coeff_x_prefix` and `last_significant_coeff_y_prefix`
/// (HEVC spec 9.3.4.2.4 / FFmpeg `last_significant_coeff_xy_prefix_decode`).
fn decode_last_significant_coeff_xy_prefix(
    cabac: &mut CabacReader,
    contexts: &mut CabacContexts,
    c_idx: usize,
    log2_size: u8,
) -> (u32, u32) {
    let max = (log2_size as u32 * 2) - 1;
    let (ctx_offset, ctx_shift) = if c_idx == 0 {
        let off = 3 * (log2_size as u32 - 2) + ((log2_size as u32 - 1) >> 2);
        let shift = (log2_size as u32 + 1) >> 2;
        (off, shift)
    } else {
        (15, log2_size as u32 - 2)
    };

    let mut x_prefix = 0u32;
    while x_prefix < max {
        let inc = (x_prefix >> ctx_shift) + ctx_offset;
        let bit = cabac
            .decode_bin(&mut contexts.state[ctx::LAST_SIGNIFICANT_COEFF_X_PREFIX + inc as usize]);
        if bit == 0 {
            break;
        }
        x_prefix += 1;
    }

    let mut y_prefix = 0u32;
    while y_prefix < max {
        let inc = (y_prefix >> ctx_shift) + ctx_offset;
        let bit = cabac
            .decode_bin(&mut contexts.state[ctx::LAST_SIGNIFICANT_COEFF_Y_PREFIX + inc as usize]);
        if bit == 0 {
            break;
        }
        y_prefix += 1;
    }

    (x_prefix, y_prefix)
}

/// Decode `last_significant_coeff_*_suffix` (FLC bypass - `(prefix>>1)-1` bits).
fn decode_last_significant_coeff_suffix(cabac: &mut CabacReader, prefix: u32) -> u32 {
    let length = (prefix >> 1) - 1;
    let mut value = cabac.decode_bypass();
    for _ in 1..length {
        value = (value << 1) | cabac.decode_bypass();
    }
    value
}

/// Compute `last_significant_coeff_x` (or y) from prefix + suffix
/// (HEVC spec 7.4.9.11 eq. 7-66 / 7-67).
fn last_significant_coeff_value(prefix: u32, cabac: &mut CabacReader) -> u32 {
    if prefix > 3 {
        let suffix = decode_last_significant_coeff_suffix(cabac, prefix);
        (1 << ((prefix >> 1) - 1)) * (2 + (prefix & 1)) + suffix
    } else {
        prefix
    }
}

/// Decode `coded_sub_block_flag` (HEVC spec 9.3.4.2.6).
fn decode_coded_sub_block_flag(
    cabac: &mut CabacReader,
    contexts: &mut CabacContexts,
    c_idx: usize,
    ctx_cg: u32,
) -> u32 {
    // FFmpeg: inc = min(ctx_cg, 1) + (c_idx > 0 ? 2 : 0)
    let inc = ctx_cg.min(1) as usize + if c_idx > 0 { 2 } else { 0 };
    cabac.decode_bin(&mut contexts.state[ctx::SIGNIFICANT_COEFF_GROUP_FLAG + inc])
}

/// Decode `sig_coeff_flag` for `(x_c, y_c)` within the sub-block, using the
/// pre-computed `ctx_idx_map` and `scf_offset` (HEVC spec 9.3.4.2.5).
fn decode_sig_coeff_flag(
    cabac: &mut CabacReader,
    contexts: &mut CabacContexts,
    x_c: u8,
    y_c: u8,
    scf_offset: usize,
    ctx_idx_map: &[u8; 16],
) -> u32 {
    let inc = ctx_idx_map[((y_c as usize) << 2) + x_c as usize] as usize + scf_offset;
    cabac.decode_bin(&mut contexts.state[ctx::SIGNIFICANT_COEFF_FLAG + inc])
}

/// Decode `sig_coeff_flag` at the (0, 0) position of a sub-block - uses just
/// the per-component DC context, no map.
fn decode_sig_coeff_flag_dc(
    cabac: &mut CabacReader,
    contexts: &mut CabacContexts,
    scf_offset: usize,
) -> u32 {
    cabac.decode_bin(&mut contexts.state[ctx::SIGNIFICANT_COEFF_FLAG + scf_offset])
}

/// Decode `coeff_abs_level_greater1_flag`.
fn decode_coeff_abs_level_greater1_flag(
    cabac: &mut CabacReader,
    contexts: &mut CabacContexts,
    c_idx: usize,
    inc: u32,
) -> u32 {
    let inc = inc as usize + if c_idx > 0 { 16 } else { 0 };
    cabac.decode_bin(&mut contexts.state[ctx::COEFF_ABS_LEVEL_GREATER1_FLAG + inc])
}

/// Decode `coeff_abs_level_greater2_flag`.
fn decode_coeff_abs_level_greater2_flag(
    cabac: &mut CabacReader,
    contexts: &mut CabacContexts,
    c_idx: usize,
    inc: u32,
) -> u32 {
    let inc = inc as usize + if c_idx > 0 { 4 } else { 0 };
    cabac.decode_bin(&mut contexts.state[ctx::COEFF_ABS_LEVEL_GREATER2_FLAG + inc])
}

/// Decode `coeff_abs_level_remaining` (Rice + Exp-Golomb), HEVC spec 9.3.4.2.8.
///
/// `limited_range`: with extended_precision_processing_flag, the prefix is
/// limited to `32 - log2TransformRange` ones beyond the first three, and a
/// maximal prefix is followed by `log2TransformRange - cRiceParam` suffix
/// bits (spec 9.3.3.11; HM `xReadCoefRemainExGolomb`).
fn decode_coeff_abs_level_remaining(
    cabac: &mut CabacReader,
    c_rice_param: u32,
    limited_range: Option<u32>,
) -> u32 {
    if let Some(log2_range) = limited_range {
        let max_prefix_ext = 32 - (3 + log2_range);
        let mut prefix = 0u32;
        while prefix < max_prefix_ext + 3 && cabac.decode_bypass() != 0 {
            prefix += 1;
        }
        if prefix < 3 {
            let mut suffix = 0u32;
            for _ in 0..c_rice_param {
                suffix = (suffix << 1) | cabac.decode_bypass();
            }
            return (prefix << c_rice_param) + suffix;
        }
        let prefix_len = prefix - 3;
        let suffix_len = if prefix_len == max_prefix_ext {
            log2_range.saturating_sub(c_rice_param)
        } else {
            prefix_len
        };
        let mut code = 0u64;
        for _ in 0..(suffix_len + c_rice_param).min(32) {
            code = (code << 1) | cabac.decode_bypass() as u64;
        }
        let val = code + ((((1u64 << prefix_len) - 1) + 3) << c_rice_param);
        return val.min(u32::MAX as u64) as u32;
    }
    let mut prefix = 0u32;
    while prefix < CABAC_MAX_BIN && cabac.decode_bypass() != 0 {
        prefix += 1;
    }
    if prefix < 3 {
        let mut suffix = 0u32;
        for _ in 0..c_rice_param {
            suffix = (suffix << 1) | cabac.decode_bypass();
        }
        (prefix << c_rice_param) + suffix
    } else {
        let prefix_minus3 = prefix - 3;
        let mut suffix = 0u64;
        let num_suffix_bits = (prefix_minus3 + c_rice_param).min(32);
        for _ in 0..num_suffix_bits {
            suffix = (suffix << 1) | cabac.decode_bypass() as u64;
        }
        // Use u64 to avoid overflow when prefix_minus3 + c_rice_param is large.
        let val = (((1u64 << prefix_minus3) + 2) << c_rice_param) + suffix;
        val.min(u32::MAX as u64) as u32
    }
}

/// Decode `nb` bypass-coded `coeff_sign_flag` bits.
fn decode_coeff_sign_flag(cabac: &mut CabacReader, nb: u8) -> u32 {
    let mut ret = 0u32;
    for _ in 0..nb {
        ret = (ret << 1) | cabac.decode_bypass();
    }
    ret
}

// ---- Significance flag context map (HEVC spec table 9-19) -------------------

/// `ctx_idx_map` lookup table from FFmpeg's residual_coding implementation.
/// Indexed by `(prev_sig_group_pattern, y_c, x_c)`.
#[rustfmt::skip]
const SIG_CTX_IDX_MAP: [u8; 5 * 16] = [
    // log2_trafo_size == 2 (4×4 TU)
    0, 1, 4, 5, 2, 3, 4, 5, 6, 6, 8, 8, 7, 7, 8, 8,
    // prev_sig_group_pattern == 0 (no neighbors)
    1, 1, 1, 0, 1, 1, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0,
    // prev_sig_group_pattern == 1 (right neighbor)
    2, 2, 2, 2, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0,
    // prev_sig_group_pattern == 2 (bottom neighbor)
    2, 1, 0, 0, 2, 1, 0, 0, 2, 1, 0, 0, 2, 1, 0, 0,
    // default (prev_sig_group_pattern == 3)
    2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2,
];

// ---- Main residual_coding entry point ---------------------------------------

/// Result of decoding a single TU's residual_coding: dequantized 16-bit
/// coefficients (in raster order), the size of the block, and the position
/// of the last significant coefficient (used by Phase 2c-4 to limit IDCT
/// scan).
#[derive(Clone)]
pub struct ResidualBlock {
    pub coeffs: Vec<i16>,
    pub log2_size: u8,
    pub last_sig_x: u32,
    pub last_sig_y: u32,
    /// Coefficients lie in the top-left `nz_cols` x `nz_rows` (multiples of
    /// 4, from the coded coefficient groups); everything outside is zero.
    /// `last_sig_x` / `last_sig_y` alone do not bound this: with horizontal
    /// or vertical scans earlier coefficients can lie further out.
    pub nz_cols: u8,
    pub nz_rows: u8,
    /// When true, the inverse transform is bypassed - the dequantized
    /// coefficients ARE the spatial-domain residual (HEVC spec 8.6.4).
    pub transform_skip: bool,
    /// When true, both dequantization and inverse transform are bypassed -
    /// the raw decoded coefficient levels are the spatial-domain residual.
    /// Also disables deblocking on this CU's boundaries (HEVC spec 8.7.2).
    pub cu_transquant_bypass: bool,
    /// With extended precision and a coefficient range above 16 bits, the
    /// dequantized coefficients are here (and `coeffs` is unused); the
    /// inverse transform then takes the `i32` path.
    pub coeffs_wide: Option<Vec<i32>>,
    /// log2TransformRange: 15, or Max(15, BitDepth + 6) with extended
    /// precision.
    pub log2_range: u32,
}

/// Compute the dequantization scale parameters for a TU.
///
/// Returns `(shift, add, scale)` for the formula
/// `dequant = (level * scale * scale_m + add) >> shift`. `scale_m` is either
/// 16 (no scaling lists) or looked up from the active scaling matrix (when
/// `scaling_list_enabled_flag = 1`).
pub(crate) fn compute_dequant_scale(qp: i32, log2_trafo_size: u8, bit_depth: u8) -> (u32, u32, u32) {
    let shift = (bit_depth as u32 + log2_trafo_size as u32).saturating_sub(5);
    let add = if shift > 0 { 1u32 << (shift - 1) } else { 0 };
    let qp = qp.max(0) as usize;
    let scale = LEVEL_SCALE[qp % 6] << (qp / 6).min(31);
    (shift, add, scale)
}

/// Map a luma QP to the chroma QP via spec table 8-9 (chroma_format_idc=1).
#[allow(dead_code)]
fn chroma_qp(qp_y: i32, offset: i32) -> i32 {
    let qp_i = (qp_y + offset).clamp(0, 57);
    if qp_i < 30 {
        qp_i
    } else if qp_i > 43 {
        qp_i - 6
    } else {
        const QP_C: [i32; 14] = [29, 30, 31, 32, 33, 33, 34, 34, 35, 35, 36, 36, 37, 37];
        QP_C[(qp_i - 30) as usize]
    }
}

/// Decode `residual_coding` for a single TU plane (HEVC spec 7.3.8.11).
///
/// Returns the 1D coefficient buffer in raster order along with the last
/// significant coefficient position. Coefficients are 16-bit signed
/// dequantized values; the inverse transform happens in Phase 2c-4.
#[allow(clippy::too_many_arguments)]
pub fn decode_residual_coding(
    cabac: &mut CabacReader,
    contexts: &mut CabacContexts,
    sps: &Sps,
    pps: &Pps,
    log2_trafo_size: u8,
    plane: ResidualPlane,
    qp: i32,
    scan_idx: ScanOrder,
    is_intra: bool,
    intra_mode: u8,
    cu_transquant_bypass: bool,
    buffer: Vec<i16>,
) -> Result<ResidualBlock, DecodeError> {
    let r = &sps.range;
    // The streamlined decoder covers everything outside the range extensions.
    let simple = !cfg!(feature = "legacy_residual") && *r == crate::sps::SpsRangeExtension::default();
    if simple && (2..=5).contains(&log2_trafo_size) {
        return Ok(decode_residual_simple(
            cabac,
            contexts,
            sps,
            pps,
            log2_trafo_size,
            plane,
            qp,
            scan_idx,
            is_intra,
            cu_transquant_bypass,
            buffer,
        ));
    }
    decode_residual_coding_generic(
        cabac,
        contexts,
        sps,
        pps,
        log2_trafo_size,
        plane,
        qp,
        scan_idx,
        is_intra,
        intra_mode,
        cu_transquant_bypass,
        buffer,
    )
}

/// The general `residual_coding` decoder, including every range-extension tool. Bin-for-bin the
/// reference for `decode_residual_simple`.
#[allow(clippy::too_many_arguments)]
pub fn decode_residual_coding_generic(
    cabac: &mut CabacReader,
    contexts: &mut CabacContexts,
    sps: &Sps,
    pps: &Pps,
    log2_trafo_size: u8,
    plane: ResidualPlane,
    qp: i32,
    scan_idx: ScanOrder,
    is_intra: bool,
    intra_mode: u8,
    cu_transquant_bypass: bool,
    buffer: Vec<i16>,
) -> Result<ResidualBlock, DecodeError> {
    let rext = &sps.range;
    // Decode transform_skip_flag (HEVC spec 7.3.8.11 / 9.3.4.2.5), allowed
    // up to Log2MaxTransformSkipSize (4x4 unless the PPS range extension
    // raises it). Not coded for transquant-bypass CUs.
    let transform_skip = !cu_transquant_bypass
        && pps.transform_skip_enabled_flag
        && log2_trafo_size <= pps.log2_max_transform_skip_block_size
        && {
            let inc = if plane == ResidualPlane::Luma { 0 } else { 1 };
            cabac.decode_bin(&mut contexts.state[ctx::TRANSFORM_SKIP_FLAG + inc]) != 0
        };
    let ts_or_bypass = transform_skip || cu_transquant_bypass;

    // Range extensions: residual DPCM for blocks without a transform.
    // Explicit (inter): signalled flag and direction (spec 7.3.8.11).
    // Implicit (intra): horizontal for intra mode 10, vertical for 26.
    let chroma_inc = usize::from(plane != ResidualPlane::Luma);
    let mut rdpcm_vertical = false;
    let explicit_rdpcm = !is_intra
        && rext.explicit_rdpcm_enabled_flag
        && ts_or_bypass
        && cabac.decode_bin(&mut contexts.state[ctx::EXPLICIT_RDPCM_FLAG + chroma_inc]) != 0;
    if explicit_rdpcm {
        rdpcm_vertical =
            cabac.decode_bin(&mut contexts.state[ctx::EXPLICIT_RDPCM_DIR_FLAG + chroma_inc]) != 0;
    }
    let implicit_rdpcm = is_intra
        && rext.implicit_rdpcm_enabled_flag
        && ts_or_bypass
        && (intra_mode == 10 || intra_mode == 26);
    if implicit_rdpcm {
        rdpcm_vertical = intra_mode == 26;
    }
    let rdpcm = explicit_rdpcm || implicit_rdpcm;
    // transform_skip_context_enabled_flag: one sig_coeff_flag context per
    // component for blocks without a transform (spec 9.3.4.2.5).
    let single_sig_ctx = rext.transform_skip_context_enabled_flag && ts_or_bypass;
    // persistent_rice_adaptation_enabled_flag: sbType selects a StatCoeff.
    let persistent_rice = rext.persistent_rice_adaptation_enabled_flag;
    let sb_type = 2 * usize::from(plane == ResidualPlane::Luma) + usize::from(ts_or_bypass);
    // extended_precision_processing_flag: coefficient range
    // log2TransformRange = Max(15, BitDepth + 6) (spec 7.4.3.2.2) and a
    // limited coeff_abs_level_remaining prefix.
    let extended = rext.extended_precision_processing_flag;

    // Horizontal and vertical scan orders are used for angular intra modes
    // 6..14 (vert) and 22..30 (horiz) at log2_trafo_size <= 3.

    let trafo_size = 1usize << log2_trafo_size;
    let c_idx = plane.c_idx();
    // `buffer` is a recycled allocation (any contents): reuse it instead of
    // allocating a coefficient block per transform block.
    let mut coeffs = buffer;
    coeffs.clear();
    coeffs.resize(trafo_size * trafo_size, 0);

    // ---- Last significant coefficient position ----
    let (last_x_prefix, last_y_prefix) =
        decode_last_significant_coeff_xy_prefix(cabac, contexts, c_idx, log2_trafo_size);
    let mut last_sig_x = last_significant_coeff_value(last_x_prefix, cabac);
    let mut last_sig_y = last_significant_coeff_value(last_y_prefix, cabac);
    if scan_idx == ScanOrder::Vert {
        std::mem::swap(&mut last_sig_x, &mut last_sig_y);
    }

    // ---- Sub-block (CG) selection ----
    let x_cg_last = (last_sig_x >> 2) as usize;
    let y_cg_last = (last_sig_y >> 2) as usize;

    // Pick scan tables and compute the linear scan position of the last sig.
    let (scan_x_off, scan_y_off, scan_x_cg, scan_y_cg);
    let mut num_coeff: u32;

    {
        let last_x_c = (last_sig_x & 3) as usize;
        let last_y_c = (last_sig_y & 3) as usize;
        // Select 4x4 within-block scan based on scan_idx.
        match scan_idx {
            ScanOrder::Diag => {
                scan_x_off = &DIAG_SCAN_4X4_X[..];
                scan_y_off = &DIAG_SCAN_4X4_Y[..];
                num_coeff = DIAG_SCAN_4X4_INV[last_y_c][last_x_c] as u32;
            }
            ScanOrder::Horiz => {
                scan_x_off = &HORIZ_SCAN_4X4_X[..];
                scan_y_off = &HORIZ_SCAN_4X4_Y[..];
                num_coeff = HORIZ_SCAN_4X4_INV[last_y_c][last_x_c] as u32;
            }
            ScanOrder::Vert => {
                scan_x_off = &VERT_SCAN_4X4_X[..];
                scan_y_off = &VERT_SCAN_4X4_Y[..];
                num_coeff = VERT_SCAN_4X4_INV[last_y_c][last_x_c] as u32;
            }
        }
        // Select CG-level scan based on scan_idx and TU size.
        match trafo_size {
            4 => {
                // 1×1 sub-block scan - no CG scan needed.
                scan_x_cg = &SCAN_1X1[..];
                scan_y_cg = &SCAN_1X1[..];
            }
            8 => match scan_idx {
                ScanOrder::Diag => {
                    num_coeff += (DIAG_SCAN_2X2_INV[y_cg_last][x_cg_last] as u32) << 4;
                    scan_x_cg = &DIAG_SCAN_2X2_X[..];
                    scan_y_cg = &DIAG_SCAN_2X2_Y[..];
                }
                ScanOrder::Horiz => {
                    num_coeff += (HORIZ_SCAN_2X2_INV[y_cg_last][x_cg_last] as u32) << 4;
                    scan_x_cg = &HORIZ_SCAN_2X2_X[..];
                    scan_y_cg = &HORIZ_SCAN_2X2_Y[..];
                }
                ScanOrder::Vert => {
                    num_coeff += (VERT_SCAN_2X2_INV[y_cg_last][x_cg_last] as u32) << 4;
                    scan_x_cg = &VERT_SCAN_2X2_X[..];
                    scan_y_cg = &VERT_SCAN_2X2_Y[..];
                }
            },
            16 => {
                num_coeff += (DIAG_SCAN_4X4_INV[y_cg_last][x_cg_last] as u32) << 4;
                scan_x_cg = &DIAG_SCAN_4X4_X[..];
                scan_y_cg = &DIAG_SCAN_4X4_Y[..];
            }
            32 => {
                num_coeff += (DIAG_SCAN_8X8_INV[y_cg_last][x_cg_last] as u32) << 4;
                scan_x_cg = &DIAG_SCAN_8X8_X[..];
                scan_y_cg = &DIAG_SCAN_8X8_Y[..];
            }
            _ => {
                return Err(DecodeError::InvalidSyntax("invalid trafo_size"));
            }
        }
    }
    num_coeff += 1;
    let num_last_subset = ((num_coeff - 1) >> 4) as usize;

    // Spec 8.6.3: the QP used for dequantization includes the bit-depth
    // offset: qP = qPY + QpBdOffsetY (luma) or qP = qPC + QpBdOffsetC (chroma).
    // QpBdOffset = 6 * (BitDepth - 8). For 8-bit this is 0; for 10-bit it's 12.
    let bd = if plane == ResidualPlane::Luma {
        sps.bit_depth_luma
    } else {
        sps.bit_depth_chroma
    };
    let qp_bd_offset = 6 * (bd as i32 - 8);
    let (_, _, scale) = compute_dequant_scale(qp + qp_bd_offset, log2_trafo_size, bd);
    let log2_range: u32 = if extended {
        (bd as u32 + 6).max(15)
    } else {
        15
    };
    let limited_range = extended.then_some(log2_range);
    // Forward transform shift log2TransformRange - BitDepth - log2(nTbS);
    // clipped at 0 for transform skip with extended precision (HM).
    let mut transform_shift = log2_range as i32 - bd as i32 - log2_trafo_size as i32;
    if transform_skip && extended {
        transform_shift = transform_shift.max(0);
    }
    // Dequantization bdShift (spec 8.6.3): BitDepth + log2(nTbS) + 10 -
    // log2TransformRange, i.e. 10 - transform_shift (scale carries the
    // QP / 6 shift).
    let shift = (10 - transform_shift) as u32;
    let add = 1u32 << (shift - 1);
    let (coeff_min, coeff_max) = (-(1i64 << log2_range), (1i64 << log2_range) - 1);
    let wide = log2_range > 15 && !cu_transquant_bypass;
    let mut coeffs_wide: Vec<i32> = if wide {
        vec![0; trafo_size * trafo_size]
    } else {
        Vec::new()
    };

    // Resolve the active scaling matrix and DC scale value. Transform-skip
    // blocks larger than 4x4 use the flat factor 16 (spec 8.6.4.2).
    let (scale_matrix, dc_scale) = if transform_skip && log2_trafo_size > 2 {
        (None, 16)
    } else {
        resolve_scaling_matrix(sps, pps, log2_trafo_size, c_idx, is_intra)
    };

    // 8×8 grid of CG flags. The largest TU is 32×32 → 8×8 sub-blocks.
    let mut significant_coeff_group_flag = [[false; 8]; 8];
    let mut greater1_ctx: u32 = 1;
    // Bounding box of the coefficient groups that hold coefficients, in
    // groups: lets the inverse transform skip all-zero rows and columns.
    let (mut nz_cg_cols, mut nz_cg_rows) = (0u8, 0u8);

    // ---- Reverse scan from last sub-block to (0, 0) ----
    for i in (0..=num_last_subset).rev() {
        let offset = i << 4;
        let x_cg = scan_x_cg[i] as usize;
        let y_cg = scan_y_cg[i] as usize;
        let mut implicit_non_zero_coeff = false;

        if i < num_last_subset && i > 0 {
            // Decode coded_sub_block_flag with right/below neighbor context.
            let mut ctx_cg = 0u32;
            if x_cg < (1 << (log2_trafo_size - 2)) - 1
                && significant_coeff_group_flag[y_cg][x_cg + 1]
            {
                ctx_cg += 1;
            }
            if y_cg < (1 << (log2_trafo_size - 2)) - 1
                && significant_coeff_group_flag[y_cg + 1][x_cg]
            {
                ctx_cg += 1;
            }
            significant_coeff_group_flag[y_cg][x_cg] =
                decode_coded_sub_block_flag(cabac, contexts, c_idx, ctx_cg) != 0;
            implicit_non_zero_coeff = true;
        } else {
            // First and last subsets are implicitly non-zero (last because it
            // contains the last significant coeff, first because the DC
            // sub-block always exists if there are any non-zero coefficients
            // in the TU).
            significant_coeff_group_flag[y_cg][x_cg] =
                (x_cg == x_cg_last && y_cg == y_cg_last) || (x_cg == 0 && y_cg == 0);
        }

        // Compute prev_sig pattern (right + below neighbor sub-blocks).
        let mut prev_sig = 0u32;
        if x_cg < (((1 << log2_trafo_size) - 1) >> 2)
            && significant_coeff_group_flag[y_cg][x_cg + 1]
        {
            prev_sig |= 1;
        }
        if y_cg < (((1 << log2_trafo_size) - 1) >> 2)
            && significant_coeff_group_flag[y_cg + 1][x_cg]
        {
            prev_sig |= 2;
        }

        let last_scan_pos = (num_coeff as i32) - (offset as i32) - 1;
        let mut significant_coeff_flag_idx: [u8; 16] = [0; 16];
        let mut nb_significant_coeff_flag = 0u8;
        let n_end_initial: i32 = if i == num_last_subset {
            // The last significant coefficient is implicitly significant.
            significant_coeff_flag_idx[0] = last_scan_pos as u8;
            nb_significant_coeff_flag = 1;
            last_scan_pos - 1
        } else {
            15
        };

        if significant_coeff_group_flag[y_cg][x_cg] && n_end_initial >= 0 {
            // ---- Compute scf_offset and ctx_idx_map_p ----
            let ctx_idx_map_p: &[u8; 16];
            let mut scf_offset: usize = 0;
            if c_idx != 0 {
                scf_offset = 27;
            }
            if single_sig_ctx {
                // Every position maps to 2: contexts 42 (luma) / 43 (chroma).
                ctx_idx_map_p = (&SIG_CTX_IDX_MAP[64..80]).try_into().unwrap();
                scf_offset = if c_idx == 0 { 40 } else { 41 };
            } else if log2_trafo_size == 2 {
                ctx_idx_map_p = (&SIG_CTX_IDX_MAP[0..16]).try_into().unwrap();
            } else {
                let map_idx = ((prev_sig + 1) << 4) as usize;
                ctx_idx_map_p = (&SIG_CTX_IDX_MAP[map_idx..map_idx + 16])
                    .try_into()
                    .unwrap();
                if c_idx == 0 {
                    if x_cg > 0 || y_cg > 0 {
                        scf_offset += 3;
                    }
                    if log2_trafo_size == 3 {
                        scf_offset += if scan_idx == ScanOrder::Diag { 9 } else { 15 };
                    } else {
                        scf_offset += 21;
                    }
                } else if log2_trafo_size == 3 {
                    scf_offset += 9;
                } else {
                    scf_offset += 12;
                }
            }

            // Iterate scan positions n_end_initial..=1, decoding sig_coeff_flag.
            let mut n = n_end_initial;
            while n > 0 {
                let x_c = scan_x_off[n as usize];
                let y_c = scan_y_off[n as usize];
                if decode_sig_coeff_flag(cabac, contexts, x_c, y_c, scf_offset, ctx_idx_map_p) != 0
                {
                    significant_coeff_flag_idx[nb_significant_coeff_flag as usize] = n as u8;
                    nb_significant_coeff_flag += 1;
                    implicit_non_zero_coeff = false;
                }
                n -= 1;
            }

            // Position 0 (DC of the sub-block).
            if !implicit_non_zero_coeff {
                // Re-derive scf_offset for position 0.
                let scf_offset_0 = if single_sig_ctx {
                    if c_idx == 0 { 42 } else { 43 }
                } else if i == 0 {
                    if c_idx == 0 { 0 } else { 27 }
                } else {
                    2 + scf_offset
                };
                if decode_sig_coeff_flag_dc(cabac, contexts, scf_offset_0) != 0 {
                    significant_coeff_flag_idx[nb_significant_coeff_flag as usize] = 0;
                    nb_significant_coeff_flag += 1;
                }
            } else {
                significant_coeff_flag_idx[nb_significant_coeff_flag as usize] = 0;
                nb_significant_coeff_flag += 1;
            }
        }

        let n_end = nb_significant_coeff_flag;
        if n_end == 0 {
            continue;
        }
        nz_cg_cols = nz_cg_cols.max(x_cg as u8 + 1);
        nz_cg_rows = nz_cg_rows.max(y_cg as u8 + 1);

        // ---- Coefficient levels ----
        let mut ctx_set = if i > 0 && c_idx == 0 { 2u32 } else { 0 };
        if i != num_last_subset && greater1_ctx == 0 {
            ctx_set += 1;
        }
        greater1_ctx = 1;

        let mut coeff_abs_level_greater1_flag = [0u8; 8];
        let mut first_greater1_idx: i32 = -1;
        // escapeDataPresent (spec 7.3.8.11): some level in the sub-block
        // needs coeff_abs_level_remaining.
        let mut escape_data_present = n_end > 8;
        let g1_count = (n_end as usize).min(8);
        for (m, slot) in coeff_abs_level_greater1_flag
            .iter_mut()
            .enumerate()
            .take(g1_count)
        {
            let inc = (ctx_set << 2) + greater1_ctx;
            let bit = decode_coeff_abs_level_greater1_flag(cabac, contexts, c_idx, inc);
            *slot = bit as u8;
            if bit != 0 {
                greater1_ctx = 0;
                if first_greater1_idx == -1 {
                    first_greater1_idx = m as i32;
                } else {
                    escape_data_present = true;
                }
            } else if greater1_ctx > 0 && greater1_ctx < 3 {
                greater1_ctx += 1;
            }
        }

        // Record sub-block scan extremes for sign data hiding. The first
        // entry in `significant_coeff_flag_idx` is the highest scan position
        // decoded in this sub-block (the "last" non-zero in forward scan
        // order); the entry at `n_end - 1` is the lowest scan position (the
        // "first" non-zero in forward scan order, i.e. the DC-ward one).
        let last_nz_pos_in_cg = significant_coeff_flag_idx[0] as i32;
        let first_nz_pos_in_cg = significant_coeff_flag_idx[(n_end as usize) - 1] as i32;

        // greater2 only applies to the first level > 1.
        if first_greater1_idx != -1 {
            let greater2 =
                decode_coeff_abs_level_greater2_flag(cabac, contexts, c_idx, ctx_set) as u8;
            coeff_abs_level_greater1_flag[first_greater1_idx as usize] += greater2;
            escape_data_present |= greater2 != 0;
        }

        // Sign data hiding gating (HEVC spec 7.4.9.11 / FFmpeg cabac.c:1348-1355).
        // In Main Profile (no Range Extensions), SDH depends only on the
        // scan-distance test. transform_skip_flag only disables SDH when
        // implicit_rdpcm_enabled is active (a Range Extension feature we
        // don't support). cu_transquant_bypass_flag disables SDH per spec.
        // See FFmpeg cabac.c:1348-1355 for the full gate.
        let sign_hidden = pps.sign_data_hiding_enabled_flag
            && !cu_transquant_bypass
            && !rdpcm
            && (last_nz_pos_in_cg - first_nz_pos_in_cg >= 4);

        // Sign flags (bypass). When SDH is active on this sub-block, the
        // encoder omitted the sign bit of the first non-zero coefficient in
        // forward scan order - we'll recover it from the parity of the sum
        // of absolute levels below. `sign_hidden` implies `n_end >= 2`
        // (because `last_nz_pos_in_cg - first_nz_pos_in_cg >= 4` requires
        // at least two non-zero coefficients in the sub-block), so
        // `sign_nb >= 1` here.
        //
        // `sign_bits` must be kept in a 16-bit window to match FFmpeg's
        // `uint16_t coeff_sign_flag` (see spec 9.3.4.2.9). Using a raw
        // `u32` would leak the previously-consumed sign bits into bit 16
        // and higher on each `<<= 1`, and `(sign_bits >> 15) != 0` would
        // then spuriously flip later coefficients. Earlier fixtures hid
        // this pre-existing bug because they had few non-zero coefficients
        // per sub-block; the new signhide fixture forces the issue.
        if rext.cabac_bypass_alignment_enabled_flag && escape_data_present {
            cabac.align_bypass();
        }
        let sign_nb = if sign_hidden { n_end - 1 } else { n_end };
        let coeff_sign_flag = decode_coeff_sign_flag(cabac, sign_nb);
        let mut sign_bits: u32 = (coeff_sign_flag << (16 - sign_nb)) & 0xffff;

        // Levels in reverse scan order, dequantize, and store. With
        // persistent Rice adaptation the Rice parameter starts from
        // StatCoeff, which the first coeff_abs_level_remaining of the
        // sub-block updates, and is not capped at 4 (spec 9.3.3.11).
        // Valid streams keep the Rice parameter far below 31 (it would need
        // levels near 3 << 30); the cap only stops malformed input from
        // shifting past the integer width.
        let mut c_rice_param = if persistent_rice {
            (u32::from(contexts.stat_coeff[sb_type]) / 4).min(31)
        } else {
            0
        };
        let mut rice_updated = false;
        let mut update_rice = |c_rice_param: &mut u32, level: i64, remaining: i64| {
            if level > (3i64 << *c_rice_param) {
                *c_rice_param = if persistent_rice {
                    (*c_rice_param + 1).min(31)
                } else {
                    (*c_rice_param + 1).min(4)
                };
            }
            if persistent_rice && !rice_updated {
                let stat = &mut contexts.stat_coeff[sb_type];
                let init = u32::from(*stat) / 4;
                if remaining >= (3i64 << init) {
                    *stat = stat.saturating_add(1);
                } else if 2 * remaining < (1i64 << init) && *stat > 0 {
                    *stat -= 1;
                }
                rice_updated = true;
            }
        };
        let mut sum_abs: i64 = 0;
        for m in 0..n_end as usize {
            let n = significant_coeff_flag_idx[m] as usize;
            let x_c = (x_cg << 2) + scan_x_off[n] as usize;
            let y_c = (y_cg << 2) + scan_y_off[n] as usize;
            let mut trans_coeff_level: i64;

            if m < 8 {
                trans_coeff_level = 1 + coeff_abs_level_greater1_flag[m] as i64;
                let needed_for_remaining = if m as i32 == first_greater1_idx { 3 } else { 2 };
                if trans_coeff_level == needed_for_remaining {
                    let last =
                        decode_coeff_abs_level_remaining(cabac, c_rice_param, limited_range) as i64;
                    trans_coeff_level += last;
                    update_rice(&mut c_rice_param, trans_coeff_level, last);
                }
            } else {
                let last =
                    decode_coeff_abs_level_remaining(cabac, c_rice_param, limited_range) as i64;
                trans_coeff_level = 1 + last;
                update_rice(&mut c_rice_param, trans_coeff_level, last);
            }

            // Sign data hiding: accumulate the absolute level, and at the
            // hidden coefficient (the one at `first_nz_pos_in_cg` in scan
            // order, which is the LAST one visited by this reverse loop)
            // derive its sign from the parity.
            if sign_hidden {
                sum_abs += trans_coeff_level;
                if (n as i32) == first_nz_pos_in_cg && (sum_abs & 1) != 0 {
                    trans_coeff_level = -trans_coeff_level;
                }
            }

            if (sign_bits >> 15) != 0 {
                trans_coeff_level = -trans_coeff_level;
            }
            sign_bits = (sign_bits << 1) & 0xffff;

            if cu_transquant_bypass {
                // No dequantization: raw coefficient levels are the spatial residual.
                coeffs[y_c * trafo_size + x_c] = trans_coeff_level as i16;
            } else {
                // Dequantize with scaling matrix lookup (HEVC spec 8.6.3).
                let scale_m: u32 = match &scale_matrix {
                    Some(sm) => {
                        // For 16×16 and 32×32 TUs, the DC position uses dc_scale.
                        if x_c != 0 || y_c != 0 || log2_trafo_size < 4 {
                            let pos = match log2_trafo_size {
                                3 => (y_c << 3) + x_c,
                                4 => ((y_c >> 1) << 3) + (x_c >> 1),
                                5 => ((y_c >> 2) << 3) + (x_c >> 2),
                                _ => (y_c << 2) + x_c, // log2 == 2 (4×4)
                            };
                            sm[pos] as u32
                        } else {
                            dc_scale as u32
                        }
                    }
                    None => 16,
                };
                let dq = (trans_coeff_level * scale as i64 * scale_m as i64 + add as i64) >> shift;
                let dq = dq.clamp(coeff_min, coeff_max);
                if wide {
                    coeffs_wide[y_c * trafo_size + x_c] = dq as i32;
                } else {
                    coeffs[y_c * trafo_size + x_c] = dq as i16;
                }
            }
        }
    }

    // For transform_skip, apply the additional dequant shift that compensates
    // for the IDCT scaling we're skipping. FFmpeg's hevcdsp.dequant() does:
    //   shift = 15 - bit_depth - log2_trafo_size
    //   coeff = (coeff + (1 << (shift-1))) >> shift
    // This is separate from the standard per-coefficient dequant already applied
    // above. Without this, the coefficients are ~32× too large for 8-bit 4×4
    // (shift=5), causing massive clipping artifacts.
    // transform_skip_rotation_enabled_flag: 4x4 intra blocks without a
    // transform are rotated by 180 degrees (spec 8.6.2; HM applies it to
    // transquant-bypass blocks as well as transform-skip ones).
    let rotate = rext.transform_skip_rotation_enabled_flag
        && log2_trafo_size == 2
        && is_intra
        && ts_or_bypass;
    if transform_skip {
        // tsShift = 5 + Log2(nTbS) then bdShift = 20 - BitDepth (spec
        // 8.6.4.2), folded into one shift (the forward transform shift);
        // it is negative for large blocks at high bit depths.
        let mut wide_or_narrow: Vec<i32> = if wide {
            std::mem::take(&mut coeffs_wide)
        } else {
            coeffs.iter().map(|&c| c as i32).collect()
        };
        if rotate {
            wide_or_narrow.reverse();
        }
        let ts_shift = transform_shift;
        for (dst, &c) in coeffs.iter_mut().zip(&wide_or_narrow) {
            let v = if ts_shift > 0 {
                (c + (1 << (ts_shift - 1))) >> ts_shift
            } else {
                c << -ts_shift
            };
            *dst = v as i16;
        }
    } else if rotate {
        coeffs.reverse();
    }
    if rdpcm {
        // Directional residual modification (spec 8.6.8): accumulate along
        // the RDPCM direction.
        if rdpcm_vertical {
            for i in trafo_size..coeffs.len() {
                coeffs[i] = coeffs[i].wrapping_add(coeffs[i - trafo_size]);
            }
        } else {
            for row in coeffs.chunks_exact_mut(trafo_size) {
                for x in 1..trafo_size {
                    row[x] = row[x].wrapping_add(row[x - 1]);
                }
            }
        }
    }

    Ok(ResidualBlock {
        coeffs,
        log2_size: log2_trafo_size,
        last_sig_x,
        last_sig_y,
        nz_cols: nz_cg_cols * 4,
        nz_rows: nz_cg_rows * 4,
        transform_skip,
        cu_transquant_bypass,
        coeffs_wide: (wide && !transform_skip).then_some(coeffs_wide),
        log2_range,
    })
}

// ---- Streamlined decoder (no range extensions) ------------------------------

/// 4x4 within-sub-block scan positions per scan order (diagonal, horizontal, vertical).
const SCAN4_X: [[u8; 16]; 3] = [DIAG_SCAN_4X4_X, HORIZ_SCAN_4X4_X, VERT_SCAN_4X4_X];
const SCAN4_Y: [[u8; 16]; 3] = [DIAG_SCAN_4X4_Y, HORIZ_SCAN_4X4_Y, VERT_SCAN_4X4_Y];

/// `sig_coeff_flag` context index (before the per-CG offset) per scan position: the
/// `SIG_CTX_IDX_MAP` entry of map `m` (0 = 4x4 TU, 1 + prev_sig_group_pattern otherwise) at the
/// position scan order `s` visits as its `n`-th.
const fn build_sig_ctx() -> [[[u8; 16]; 3]; 5] {
    let mut t = [[[0u8; 16]; 3]; 5];
    let mut m = 0;
    while m < 5 {
        let mut s = 0;
        while s < 3 {
            let mut n = 0;
            while n < 16 {
                let (x, y) = (SCAN4_X[s][n] as usize, SCAN4_Y[s][n] as usize);
                t[m][s][n] = SIG_CTX_IDX_MAP[m * 16 + (y << 2) + x];
                n += 1;
            }
            s += 1;
        }
        m += 1;
    }
    t
}
static SIG_CTX_BY_SCAN: [[[u8; 16]; 3]; 5] = build_sig_ctx();

/// Coded sub-block (CG) scan of a transform block: x and y of the i-th CG in scan order.
fn cg_scan(log2: u8, scan: ScanOrder) -> (&'static [u8], &'static [u8]) {
    match (log2, scan) {
        (2, _) => (&SCAN_1X1, &SCAN_1X1),
        (3, ScanOrder::Diag) => (&DIAG_SCAN_2X2_X, &DIAG_SCAN_2X2_Y),
        (3, ScanOrder::Horiz) => (&HORIZ_SCAN_2X2_X, &HORIZ_SCAN_2X2_Y),
        (3, ScanOrder::Vert) => (&VERT_SCAN_2X2_X, &VERT_SCAN_2X2_Y),
        (4, _) => (&DIAG_SCAN_4X4_X, &DIAG_SCAN_4X4_Y),
        _ => (&DIAG_SCAN_8X8_X, &DIAG_SCAN_8X8_Y),
    }
}

/// Context-coded bins whose context depends on the previous bin's position but not on its value
/// (`last_sig_coeff_*_prefix`): the state of the next bin's context is read before this bin stores its
/// own, and taken from this bin's result when both are the same context, so a run of bins on one
/// context does not wait for a store-to-load round trip per bin.
#[inline(always)]
fn last_prefix(e: &mut crate::cabac::Engine, data: &[u8], st: &mut [u8], base: usize, max: u32, shift: u32) -> u32 {
    let mut p = 0u32;
    let mut c = base;
    let mut cur = st[c];
    while p < max {
        let cn = base + ((p + 1) >> shift) as usize;
        let other = st[cn];
        let mut s = cur;
        let bit = e.decode_bin(data, &mut s);
        st[c] = s;
        if bit == 0 {
            break;
        }
        p += 1;
        cur = if cn == c { s } else { other };
        c = cn;
    }
    p
}

/// The `sig_coeff_flag`s of scan positions `n..=1` of one sub-block: appends the significant positions to
/// `idx` from `nb` on and returns the new count. The next position's context state is read before this
/// bin stores its own and, when it is the same context, replaced by this bin's new state, so a run on one
/// context does not wait for a store-to-load round trip per bin. Out of line on WebAssembly, where V8
/// allocates registers better for the loop alone than inside the whole residual decoder.
#[cfg_attr(target_arch = "wasm32", inline(never))]
#[cfg_attr(not(target_arch = "wasm32"), inline(always))]
#[allow(clippy::too_many_arguments)]
fn sig_flags(
    e: &mut crate::cabac::Engine,
    data: &[u8],
    st: &mut [u8],
    table: &[u8; 16],
    sig_base: usize,
    mut n: usize,
    idx: &mut [u8; 16],
    mut nb: usize,
) -> usize {
    if n == 0 {
        return nb;
    }
    let mut c = sig_base + table[n & 15] as usize;
    // SAFETY (every `get_unchecked` on `st` below): `table[_] <= 8` and `sig_base <=
    // SIGNIFICANT_COEFF_FLAG + 39` with the 4x4 map's entries only at offset <= 27, so the index is
    // below SIGNIFICANT_COEFF_FLAG + 44 <= 179 = st.len() (asserted by the caller in debug builds).
    let mut cur = unsafe { *st.get_unchecked(c) };
    let mut fused = e.fused(cur);
    while n > 0 {
        let cn = sig_base + table[(n - 1) & 15] as usize;
        let other = unsafe { *st.get_unchecked(cn) };
        let pick = |_, ns| if cn == c { ns } else { other };
        let (bit, s, next, next_fused) = e.decode_bin_chain(data, fused, cur, pick);
        unsafe { *st.get_unchecked_mut(c) = s };
        idx[nb & 15] = n as u8;
        nb += bit as usize;
        cur = next;
        fused = next_fused;
        c = cn;
        n -= 1;
    }
    nb
}

/// `residual_coding()` (spec 7.3.8.11) for streams without range extensions, followed by
/// dequantisation, producing exactly what `decode_residual_coding_generic` produces. The arithmetic
/// decoder registers live in locals for the whole call, every decision loop is branch-light (the
/// significance and greater-than-1 flags are decoded without data-dependent branches, and the
/// context state of the next bin comes from a register when it is the context just updated), the
/// bypass bins of signs and escape levels are decoded several per division, and the dequantisation
/// uses 32-bit arithmetic unless a level is large enough to need 64 bits.
#[allow(clippy::too_many_arguments)]
fn decode_residual_simple(
    cabac: &mut CabacReader,
    contexts: &mut CabacContexts,
    sps: &Sps,
    pps: &Pps,
    log2: u8,
    plane: ResidualPlane,
    qp: i32,
    scan_idx: ScanOrder,
    is_intra: bool,
    cu_transquant_bypass: bool,
    buffer: Vec<i16>,
) -> ResidualBlock {
    let data = cabac.data();
    let mut e = cabac.engine();
    let st = &mut contexts.state;
    let c_idx = plane.c_idx();
    let luma = c_idx == 0;
    let chroma_inc = usize::from(!luma);
    let scan = scan_idx as usize;
    let size = 1usize << log2;

    let transform_skip = !cu_transquant_bypass
        && pps.transform_skip_enabled_flag
        && log2 <= pps.log2_max_transform_skip_block_size
        && e.decode_bin(data, &mut st[ctx::TRANSFORM_SKIP_FLAG + chroma_inc]) != 0;

    // Raw coefficient levels for the reconstruction command stream (FORMAT-idct.md). Entropy-only recording
    // (levels-only mode of this thread's level sink, `crate::recon::levels_for`): nothing reads the dense
    // coefficient block, so only the raw levels are recorded and the block, the dequantisation and the
    // transform-skip scaling are skipped (`ResidualBlock::coeffs` stays empty).
    #[cfg(feature = "recon-stream")]
    let (mut recon_levels, levels_only) = crate::recon::levels_begin();
    #[cfg(feature = "recon-stream")]
    let dense = !levels_only;
    #[cfg(not(feature = "recon-stream"))]
    let dense = true;

    let mut coeffs = buffer;
    coeffs.clear();
    if dense {
        coeffs.resize(size * size, 0);
    }

    // ---- Last significant coefficient position ----
    let log2u = log2 as u32;
    let (ctx_offset, ctx_shift) = if luma {
        (3 * (log2u - 2) + ((log2u - 1) >> 2), (log2u + 1) >> 2)
    } else {
        (15, log2u - 2)
    };
    let max_prefix = log2u * 2 - 1;
    let x_prefix = last_prefix(
        &mut e,
        data,
        st,
        ctx::LAST_SIGNIFICANT_COEFF_X_PREFIX + ctx_offset as usize,
        max_prefix,
        ctx_shift,
    );
    let y_prefix = last_prefix(
        &mut e,
        data,
        st,
        ctx::LAST_SIGNIFICANT_COEFF_Y_PREFIX + ctx_offset as usize,
        max_prefix,
        ctx_shift,
    );
    let last_value = |e: &mut crate::cabac::Engine, prefix: u32| -> u32 {
        if prefix > 3 {
            let nbits = (prefix >> 1) - 1;
            let suffix = e.bypass_bits(data, nbits);
            (1 << nbits) * (2 + (prefix & 1)) + suffix
        } else {
            prefix
        }
    };
    let mut last_sig_x = last_value(&mut e, x_prefix);
    let mut last_sig_y = last_value(&mut e, y_prefix);
    if scan_idx == ScanOrder::Vert {
        std::mem::swap(&mut last_sig_x, &mut last_sig_y);
    }

    // ---- Position of the last coefficient in scan order ----
    let x_cg_last = (last_sig_x >> 2) as usize;
    let y_cg_last = (last_sig_y >> 2) as usize;
    let (last_x_c, last_y_c) = ((last_sig_x & 3) as usize, (last_sig_y & 3) as usize);
    let mut num_coeff: u32 = match scan_idx {
        ScanOrder::Diag => DIAG_SCAN_4X4_INV[last_y_c][last_x_c],
        ScanOrder::Horiz => HORIZ_SCAN_4X4_INV[last_y_c][last_x_c],
        ScanOrder::Vert => VERT_SCAN_4X4_INV[last_y_c][last_x_c],
    } as u32;
    let (scan_x_cg, scan_y_cg) = cg_scan(log2, scan_idx);
    num_coeff += match log2 {
        2 => 0,
        3 => (match scan_idx {
            ScanOrder::Diag => DIAG_SCAN_2X2_INV[y_cg_last & 1][x_cg_last & 1],
            ScanOrder::Horiz => HORIZ_SCAN_2X2_INV[y_cg_last & 1][x_cg_last & 1],
            ScanOrder::Vert => VERT_SCAN_2X2_INV[y_cg_last & 1][x_cg_last & 1],
        } as u32)
            << 4,
        4 => (DIAG_SCAN_4X4_INV[y_cg_last & 3][x_cg_last & 3] as u32) << 4,
        _ => (DIAG_SCAN_8X8_INV[y_cg_last & 7][x_cg_last & 7] as u32) << 4,
    };
    num_coeff += 1;
    let num_last_subset = ((num_coeff - 1) >> 4) as usize;

    // ---- Dequantisation parameters (spec 8.6.3), only for the dense block ----
    let bd = if luma { sps.bit_depth_luma } else { sps.bit_depth_chroma };
    let qp_bd_offset = 6 * (bd as i32 - 8);
    let transform_shift = 15 - bd as i32 - log2 as i32;
    let (mut scale, mut dq_shift, mut dq_add, mut flat_limit) = (0i64, 0u32, 0i64, 0i32);
    let (mut scale_matrix, mut dc_scale) = (None, 16u8);
    if dense {
        let (_, _, s) = compute_dequant_scale(qp + qp_bd_offset, log2, bd);
        scale = s as i64;
        dq_shift = (10 - transform_shift) as u32;
        dq_add = 1i64 << (dq_shift - 1);
        if !(transform_skip && log2 > 2) {
            (scale_matrix, dc_scale) = resolve_scaling_matrix(sps, pps, log2, c_idx, is_intra);
        }
        // Largest |level| whose flat-matrix product still fits an i32 (the common case).
        flat_limit = ((i32::MAX as i64 - dq_add) / (scale * 16)) as i32;
    }
    let scale16 = scale * 16;
    let sign_hiding = pps.sign_data_hiding_enabled_flag && !cu_transquant_bypass;
    let ncg_minus1 = (size >> 2) - 1;

    let mut cg_flags = [0u16; 9]; // coded sub-block flags, bit x of row y
    let mut greater1_ctx: usize = 1;
    let (mut nz_cg_cols, mut nz_cg_rows) = (0u8, 0u8);

    for i in (0..=num_last_subset).rev() {
        let x_cg = scan_x_cg[i] as usize;
        let y_cg = scan_y_cg[i] as usize;
        let right = x_cg < ncg_minus1 && (cg_flags[y_cg] >> (x_cg + 1)) & 1 != 0;
        let below = y_cg < ncg_minus1 && (cg_flags[y_cg + 1] >> x_cg) & 1 != 0;

        let mut implicit_nz = false;
        let coded = if i < num_last_subset && i > 0 {
            let inc = usize::from(right || below) + chroma_inc * 2;
            implicit_nz = true;
            e.decode_bin(data, &mut st[ctx::SIGNIFICANT_COEFF_GROUP_FLAG + inc]) != 0
        } else {
            (x_cg == x_cg_last && y_cg == y_cg_last) || (x_cg == 0 && y_cg == 0)
        };
        cg_flags[y_cg] |= u16::from(coded) << x_cg;

        let last_scan_pos = num_coeff as i32 - ((i as i32) << 4) - 1;
        // Scan positions of the significant coefficients, highest first. At most 16 entries (15
        // positions above the sub-block DC, then the DC), so `nb & 15` is `nb` wherever it is written.
        let mut idx = [0u8; 16];
        let mut nb = 0usize;
        let n_end_initial: i32 = if i == num_last_subset {
            idx[0] = last_scan_pos as u8;
            nb = 1;
            last_scan_pos - 1
        } else {
            15
        };

        if coded && n_end_initial >= 0 {
            let prev_sig = usize::from(right) | (usize::from(below) << 1);
            let mut scf_offset: usize = if luma { 0 } else { 27 };
            let map_id;
            if log2 == 2 {
                map_id = 0;
            } else {
                map_id = prev_sig + 1;
                if luma {
                    if x_cg > 0 || y_cg > 0 {
                        scf_offset += 3;
                    }
                    scf_offset += if log2 == 3 {
                        if scan_idx == ScanOrder::Diag { 9 } else { 15 }
                    } else {
                        21
                    };
                } else {
                    scf_offset += if log2 == 3 { 9 } else { 12 };
                }
            }
            let table = &SIG_CTX_BY_SCAN[map_id][scan];
            let nb0 = nb;
            let sig_base = ctx::SIGNIFICANT_COEFF_FLAG + scf_offset;
            // Largest context index: chroma offset 27 + 12 + map entry 2 (or the 4x4 map's 8 at offset at
            // most 27): below SIGNIFICANT_COEFF_FLAG_LEN, so `sig_base + table[n]` is inside `st`.
            debug_assert!(sig_base + 8 < st.len() && ctx::SIGNIFICANT_COEFF_FLAG_LEN >= 44);
            nb = sig_flags(&mut e, data, st, table, sig_base, n_end_initial as usize, &mut idx, nb);
            if implicit_nz && nb == nb0 {
                idx[nb & 15] = 0;
                nb += 1;
            } else {
                let off0 = if i == 0 {
                    if luma { 0 } else { 27 }
                } else {
                    2 + scf_offset
                };
                let bit = e.decode_bin(data, &mut st[ctx::SIGNIFICANT_COEFF_FLAG + off0]);
                idx[nb & 15] = 0;
                nb += bit as usize;
            }
        }

        if nb == 0 {
            continue;
        }
        nz_cg_cols = nz_cg_cols.max(x_cg as u8 + 1);
        nz_cg_rows = nz_cg_rows.max(y_cg as u8 + 1);

        // ---- Greater-than-1 / greater-than-2 flags ----
        let mut ctx_set = if i > 0 && luma { 2usize } else { 0 };
        if i != num_last_subset && greater1_ctx == 0 {
            ctx_set += 1;
        }
        let g1_count = nb.min(8);
        let g1_base = ctx::COEFF_ABS_LEVEL_GREATER1_FLAG + chroma_inc * 16 + (ctx_set << 2);
        let g1_st = &mut st[g1_base..g1_base + 4];
        // greater1Ctx runs 1, 2, 3, 3, ... until the first flag set, then stays 0. Context 0 is untouched
        // until then, and context g + 1 until g has been used, so the next state comes from the stored
        // states read before this bin's update or, for the context just updated, from this bin.
        let s_zero = g1_st[0];
        let mut g: usize = 1;
        let mut cur = g1_st[1];
        let mut fused = e.fused(cur);
        let mut g1_flags = 0u32; // bit m: greater1 flag of the m-th coefficient
        for m in 0..g1_count {
            let gi = if g == 0 { 0 } else { (g + 1).min(3) };
            let other = g1_st[gi & 3];
            let (bit, s, next, next_fused) = e.decode_bin_chain(data, fused, cur, |b, ns| {
                if b != 0 {
                    if g == 0 { ns } else { s_zero }
                } else if gi == g {
                    ns
                } else {
                    other
                }
            });
            g1_st[g & 3] = s;
            g1_flags |= bit << m;
            cur = next;
            fused = next_fused;
            g = if bit != 0 { 0 } else { gi };
        }
        greater1_ctx = g;
        // Index of the first coefficient with the flag set (8 = none).
        let first_g1 = crate::cabac::lowest_bit32(g1_flags | 0x100);
        let first_nz_pos = idx[(nb - 1) & 15] as i32;
        let last_nz_pos = idx[0] as i32;
        let mut g2 = 0u32;
        if first_g1 != 8 {
            g2 = e.decode_bin(
                data,
                &mut st[ctx::COEFF_ABS_LEVEL_GREATER2_FLAG + chroma_inc * 4 + ctx_set],
            );
        }

        // ---- Signs, then escape levels, then the coefficients in reverse scan order ----
        let sign_hidden = sign_hiding && last_nz_pos - first_nz_pos >= 4;
        let sign_nb = if sign_hidden { nb - 1 } else { nb } as u32;
        let signs = e.bypass_bits(data, sign_nb) << (32 - sign_nb.max(1)); // first sign in bit 31
        let signs = if sign_nb == 0 { 0 } else { signs };

        // Base level of the m-th coefficient (bit 0 of `ones`: m-th level is at least 2, bit `first_g1`
        // of `twos`: at least 3) and which levels carry coeff_abs_level_remaining: those whose flags
        // all came out set, and every coefficient past the eighth.
        let first_bit = (1u32 << first_g1) & 0xff;
        let twos = if g2 != 0 { first_bit } else { 0 };
        let escape = (g1_flags & !first_bit) | twos | (((1u32 << nb) - 1) & !0xff);
        let mut rem = [0u32; 16];
        if escape != 0 {
            let mut c_rice = 0u32;
            let mut todo = escape;
            while todo != 0 {
                let m = crate::cabac::lowest_bit32(todo);
                todo &= todo - 1;
                let r = abs_level_remaining(&mut e, data, c_rice);
                rem[(m & 15) as usize] = r;
                let level = 1 + ((g1_flags >> m) & 1) as i64 + ((twos >> m) & 1) as i64 + r as i64;
                if level > (3i64 << c_rice) {
                    c_rice = (c_rice + 1).min(4);
                }
            }
        }

        let (cx, cy) = (x_cg << 2, y_cg << 2);
        #[cfg(feature = "recon-stream")]
        let mut rec = [0u32; 16];
        let mut parity = 0u32;
        for m in 0..nb {
            let n = idx[m & 15] as usize;
            let abs = 1 + ((g1_flags >> m) & 1) as i64 + ((twos >> m) & 1) as i64 + rem[m & 15] as i64;
            // Sign data hiding: the last coefficient visited (the first in scan order) has no sign bin;
            // its sign is the parity of the sum of the sub-block's absolute levels.
            parity ^= abs as u32;
            let hidden_neg = sign_hidden && m + 1 == nb && parity & 1 != 0;
            let neg = (signs >> (31 - (m as u32 & 31))) & 1 != 0 || hidden_neg;
            let level = if neg { -abs } else { abs };

            let x_c = cx + SCAN4_X[scan][n & 15] as usize;
            let y_c = cy + SCAN4_Y[scan][n & 15] as usize;
            let pos = y_c * size + x_c;
            #[cfg(feature = "recon-stream")]
            {
                rec[m & 15] = pos as u32 | (level as i16 as u16 as u32) << 16;
            }
            if !dense {
                continue;
            }
            if cu_transquant_bypass {
                coeffs[pos] = level as i16;
            } else if let Some(sm) = &scale_matrix {
                let scale_m: i64 = if x_c != 0 || y_c != 0 || log2 < 4 {
                    let mpos = match log2 {
                        3 => (y_c << 3) + x_c,
                        4 => ((y_c >> 1) << 3) + (x_c >> 1),
                        5 => ((y_c >> 2) << 3) + (x_c >> 2),
                        _ => (y_c << 2) + x_c,
                    };
                    sm[mpos] as i64
                } else {
                    dc_scale as i64
                };
                let dq = (level * scale * scale_m + dq_add) >> dq_shift;
                coeffs[pos] = dq.clamp(-32768, 32767) as i16;
            } else {
                let dq = if level.unsigned_abs() <= flat_limit as u64 {
                    ((level as i32 * scale16 as i32 + dq_add as i32) >> dq_shift) as i64
                } else {
                    (level * scale16 + dq_add) >> dq_shift
                };
                coeffs[pos] = dq.clamp(-32768, 32767) as i16;
            }
        }
        #[cfg(feature = "recon-stream")]
        if let Some(l) = recon_levels.as_mut() {
            // A fixed 16-word copy into the spare capacity instead of a `memcpy` call per sub-block (a
            // call into the module's own `memcpy` on WebAssembly, where bulk memory is off).
            debug_assert!(nb <= 16);
            l.reserve(16);
            let len = l.len();
            // SAFETY: `reserve(16)` leaves room for 16 words past `len`; the first `nb` <= 16 of them are
            // initialised by the copy before `set_len`.
            unsafe {
                std::ptr::copy_nonoverlapping(rec.as_ptr(), l.as_mut_ptr().add(len), 16);
                l.set_len(len + nb);
            }
        }
    }

    // Transform skip: the residual is the dequantised block scaled back by the forward shift.
    if transform_skip && dense {
        for c in coeffs.iter_mut() {
            let v = *c as i32;
            *c = if transform_shift > 0 {
                (v + (1 << (transform_shift - 1))) >> transform_shift
            } else {
                v << -transform_shift
            } as i16;
        }
    }

    cabac.set_engine(e);
    #[cfg(feature = "recon-stream")]
    crate::recon::levels_end(recon_levels, qp + qp_bd_offset);
    ResidualBlock {
        coeffs,
        log2_size: log2,
        last_sig_x,
        last_sig_y,
        nz_cols: nz_cg_cols * 4,
        nz_rows: nz_cg_rows * 4,
        transform_skip,
        cu_transquant_bypass,
        coeffs_wide: None,
        log2_range: 15,
    }
}

/// `coeff_abs_level_remaining` (spec 9.3.3.11) without extended precision: a unary prefix of up to 31
/// bypass ones, then a Rice or Exp-Golomb suffix.
#[inline(always)]
fn abs_level_remaining(e: &mut crate::cabac::Engine, data: &[u8], rice: u32) -> u32 {
    if let Some(v) = e.remaining_short(data, rice) {
        return v;
    }
    let prefix = e.bypass_unary(data, CABAC_MAX_BIN);
    if prefix < 3 {
        (prefix << rice) + e.bypass_bits(data, rice)
    } else {
        let pm3 = prefix - 3;
        let nbits = (pm3 + rice).min(32);
        let suffix = e.bypass_bits(data, nbits) as u64;
        let val = (((1u64 << pm3) + 2) << rice) + suffix;
        val.min(u32::MAX as u64) as u32
    }
}

/// Resolve the active scaling matrix and DC scale value for a TU.
///
/// Returns `(Some(matrix), dc_scale)` when scaling lists are enabled, or
/// `(None, 16)` when they are not. The matrix is a 64-element `[u8; 64]`
/// in raster order for the 8×8 base matrix. For 4×4 TUs, only the first
/// 16 entries are meaningful.
///
/// Matrix ID mapping matches FFmpeg: `matrix_id = 3 * is_inter + c_idx`.
fn resolve_scaling_matrix(
    sps: &Sps,
    pps: &Pps,
    log2_trafo_size: u8,
    c_idx: usize,
    is_intra: bool,
) -> (Option<[u8; 64]>, u8) {
    if !sps.scaling_list_enabled_flag {
        return (None, 16);
    }

    // PPS scaling list takes priority over SPS scaling list.
    let sl: &ScalingList = if pps.pps_scaling_list_data_present_flag {
        pps.scaling_list.as_ref().unwrap()
    } else {
        sps.scaling_list.as_ref().unwrap()
    };

    let intra_base = if is_intra { 0 } else { 3 };
    let matrix_id = intra_base + c_idx;
    // 32x32 chroma blocks exist only in 4:4:4; they use the 16x16 chroma
    // lists and DC values (spec 7.4.5, ChromaArrayType == 3).
    let size_id = if log2_trafo_size == 5 && c_idx != 0 {
        2
    } else {
        (log2_trafo_size - 2) as usize
    };

    let matrix = sl.sl[size_id][matrix_id];
    let dc_scale = if log2_trafo_size >= 4 {
        sl.sl_dc[size_id - 2][matrix_id]
    } else {
        // DC scale is not separately coded for 4×4 and 8×8; use matrix[0].
        matrix[0]
    };

    (Some(matrix), dc_scale)
}

/// 1×1 scan order (used as the sub-block scan for 4×4 TUs).
const SCAN_1X1: [u8; 1] = [0];
