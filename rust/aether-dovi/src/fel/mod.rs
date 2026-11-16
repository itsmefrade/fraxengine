//! Experimental scalar FEL reconstruction of LSB-aligned planar 10-bit codes.
//! This is a reference kernel, not a certified Dolby renderer or browser pipeline.
mod gpu;
mod mapping;
mod plane;
mod sample;
pub use gpu::{GPU_TABLE_WORDS, GpuFrameTables};
pub use mapping::{Curve, FelFrameParams, MappingHistory, Nlq, Piece};
pub use plane::{Frame420, Plane};
use sample::{chroma_site_luma, enhancement_chroma, enhancement_luma};
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Allocation,
    Dimensions,
    Buffer,
    SampleRange,
    Parameters,
    Unsupported,
    Overflow,
    Cancelled,
    MissingHistory,
    Epoch,
    Rpu,
}
/// Explicit arithmetic and output domain. Reference16 is the target reference
/// path; UpstreamVdrF64 retains the original engine's earlier quantization only
/// for comparison. Neither variant applies tone mapping or display metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReconstructionMode {
    Reference16,
    UpstreamVdrF64,
}
/// Spatial processing remains an explicit engine compatibility choice. Annex B
/// is informative and its 16-bit clipping domain is ambiguous for LSB10 inputs;
/// no universal encoder inverse filter can be recovered from the RPU flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpatialPolicy {
    EngineBilinearLumaAnnexBChromaClip10,
}
/// Half-up final quantization from the clamped unsigned 16-bit signal domain.
pub fn quantize_reference16(code: u16, depth: u8) -> Result<u16> {
    if !(10..=16).contains(&depth) {
        return Err(Error::Parameters);
    }
    if depth == 16 {
        return Ok(code);
    }
    let shift = 16 - depth;
    Ok(((u32::from(code) + (1 << (shift - 1))) >> shift).min((1 << depth) - 1) as u16)
}
pub type Result<T> = std::result::Result<T, Error>;

/// Counts are per Y/Cb/Cr plane. Clipping counts measure the reconstructed sum
/// after the prediction was clamped, not clipping inside the prediction function.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct CompositionStats {
    pub residual_nonzero: [u64; 3],
    pub reconstructed_below_zero: [u64; 3],
    pub reconstructed_above_ceiling: [u64; 3],
}
impl CompositionStats {
    fn signal(&mut self, component: usize, prediction: i32, residual: i32, max: i32) -> u16 {
        self.residual_nonzero[component] += u64::from(residual != 0);
        let sum = i64::from(prediction) + i64::from(residual);
        self.reconstructed_below_zero[component] += u64::from(sum < 0);
        self.reconstructed_above_ceiling[component] += u64::from(sum > i64::from(max));
        sum.clamp(0, i64::from(max)) as u16
    }
}
/// Caller-owned output planes. Composition performs no allocation per pixel.
pub struct Output420<'a> {
    pub y: &'a mut [u16],
    pub cb: &'a mut [u16],
    pub cr: &'a mut [u16],
}
/// Compose tightly packed planes: raw16 codes for Reference16, VDR-depth codes
/// for UpstreamVdrF64. Reference16 requires separate final quantization. A cancelled call leaves a
/// partial output which the caller must discard. Inputs never alias mutable output.
/// Limits are intentionally narrower than all possible Dolby Vision streams.
pub fn compose(
    params: &FelFrameParams,
    bl: Frame420<'_>,
    el: Frame420<'_>,
    out: Output420<'_>,
    mode: ReconstructionMode,
    _spatial_policy: SpatialPolicy,
    cancel: &AtomicBool,
) -> Result<CompositionStats> {
    let width = bl.y.width();
    let height = bl.y.height();
    let (cw, ch) = (width.div_ceil(2), height.div_ceil(2));
    if out.y.len() != width * height || out.cb.len() != cw * ch || out.cr.len() != cw * ch {
        return Err(Error::Buffer);
    }
    // Kernel supports matched or half-resolution EL grids. Reject undocumented ratios.
    if !((width == el.y.width() && height == el.y.height())
        || (width.div_ceil(2) == el.y.width() && height.div_ceil(2) == el.y.height()))
    {
        return Err(Error::Unsupported);
    }
    let max = match mode {
        ReconstructionMode::Reference16 => 65535,
        ReconstructionMode::UpstreamVdrF64 => (1_i32 << params.vdr_depth()) - 1,
    };
    let lookup = params.nlq_tables(mode)?;
    let mut stats = CompositionStats::default();
    for y in 0..height {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        for x in 0..width {
            let code = bl.y.get(x, y);
            let input = [code, bl.cb.get(x / 2, y / 2), bl.cr.get(x / 2, y / 2)];
            let pred = match mode {
                ReconstructionMode::Reference16 => params.predict_reference16(0, code, input),
                ReconstructionMode::UpstreamVdrF64 => params.predict(0, code, input),
            };
            let residual = if params.disable_residual() {
                0
            } else {
                lookup[0][enhancement_luma(el.y, x, y, width, height) as usize]
            };
            out.y[y * width + x] = stats.signal(0, pred, residual, max);
        }
    }
    for y in 0..ch {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        for x in 0..cw {
            let input = [
                chroma_site_luma(bl.y, x, y),
                bl.cb.get(x, y),
                bl.cr.get(x, y),
            ];
            for (component, plane, output) in [(1, el.cb, &mut *out.cb), (2, el.cr, &mut *out.cr)] {
                let pred = match mode {
                    ReconstructionMode::Reference16 => {
                        params.predict_reference16(component, input[component], input)
                    }
                    ReconstructionMode::UpstreamVdrF64 => {
                        params.predict(component, input[component], input)
                    }
                };
                let residual = if params.disable_residual() {
                    0
                } else {
                    lookup[component][enhancement_chroma(plane, x, y, cw, ch) as usize]
                };
                output[y * cw + x] = stats.signal(component, pred, residual, max);
            }
        }
    }
    Ok(stats)
}

/// Per-frame GPU parameter block for one type-62 RPU NAL: polynomial / MMR
/// coefficients, prediction tables and NLQ residual tables as `GPU_TABLE_WORDS`
/// little-endian i32 words. `history` carries mapping reuse across frames, so
/// feed RPUs in decode order and call `history.reset(epoch)` on every seek.
pub fn gpu_tables_for_rpu(history: &mut MappingHistory, epoch: u64, rpu_nal: &[u8]) -> Result<GpuFrameTables> {
    GpuFrameTables::from_params(&history.resolve(epoch, rpu_nal)?)
}
