//! Exact GPU preparation derived from validated typed mappings. MMR remains a
//! three-input mapping; no one-dimensional replacement is permitted.
use crate::fel::{Error, FelFrameParams, Piece, ReconstructionMode, Result};
pub const GPU_TABLE_WORDS: usize = 6832;
pub const GPU_DESCRIPTOR_OFFSET: usize = 64;
pub const GPU_COEFFICIENT_OFFSET: usize = 160;
pub const GPU_PREDICTION_OFFSET: usize = 688;
pub const GPU_NLQ_OFFSET: usize = 3760;
const MAGIC: i32 = 0x46474c31;
/// Version-1 little-endian signed/unsigned word packet used by the JS adapter.
/// MMR coefficients remain signed i32 Q values. Exact shaders require emulated
/// signed64 accumulation and unsigned64 Q20 term products, not approximate f32.
#[derive(Clone, Debug)]
pub struct GpuFrameTables {
    words: Vec<i32>,
}
impl GpuFrameTables {
    pub fn words(&self) -> &[i32] {
        &self.words
    }
    pub fn requires_mmr(&self) -> bool {
        self.words[6] != 0
    }
    pub fn from_params(params: &FelFrameParams) -> Result<Self> {
        let mut words = Vec::new();
        words
            .try_reserve_exact(GPU_TABLE_WORDS)
            .map_err(|_| Error::Allocation)?;
        words.resize(GPU_TABLE_WORDS, 0);
        words[0] = MAGIC;
        words[1] = 1;
        words[2] = GPU_TABLE_WORDS as i32;
        words[3] = i32::from(params.coefficient_denominator());
        words[4] = i32::from(params.vdr_depth());
        words[5] = i32::from(params.disable_residual());
        let nlq = params.nlq_tables(ReconstructionMode::Reference16)?;
        for (c, curve) in params.curves().iter().enumerate() {
            let descriptor = GPU_DESCRIPTOR_OFFSET + c * 32;
            words[descriptor] = curve.pivots().len() as i32;
            words[descriptor + 27] = i32::from(curve.pivots()[0]);
            words[descriptor + 28] = i32::from(curve.pivots()[curve.pivots().len() - 1]);
            for (i, &pivot) in curve.pivots().iter().enumerate() {
                words[descriptor + 2 + i] = i32::from(pivot);
            }
            for (p, piece) in curve.pieces().iter().enumerate() {
                let coefficients = GPU_COEFFICIENT_OFFSET + (c * 8 + p) * 22;
                let (kind, order) = match piece {
                    Piece::Polynomial { order, .. } => (0, *order),
                    Piece::Mmr {
                        order,
                        constant,
                        coefficients: values,
                    } => {
                        words[6] |= 1 << c;
                        words[descriptor + 1] = 1;
                        words[coefficients] = *constant;
                        for (o, row) in values.iter().enumerate() {
                            for (k, &value) in row.iter().enumerate() {
                                words[coefficients + 1 + o * 7 + k] = value;
                            }
                        }
                        (1, *order)
                    }
                };
                words[descriptor + 11 + p] = kind;
                words[descriptor + 19 + p] = i32::from(order);
            }
            for code in 0..1024 {
                if matches!(
                    curve.pieces()[curve.select(code as u16)],
                    Piece::Polynomial { .. }
                ) {
                    words[GPU_PREDICTION_OFFSET + c * 1024 + code] =
                        params.predict_reference16(c, code as u16, [0; 3]);
                }
                // WGSL adds an unsigned16 prediction to signed32 NLQ. Ensure
                // every possible sum fits i32 before a shader uses that addition.
                let residual = nlq[c][code];
                if residual > i32::MAX - 65535 {
                    return Err(Error::Unsupported);
                }
                words[GPU_NLQ_OFFSET + c * 1024 + code] = residual;
            }
        }
        Ok(Self { words })
    }
}
