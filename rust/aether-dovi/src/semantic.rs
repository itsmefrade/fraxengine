//! Semantic RPU inspection after CRC and conservative known-panic-path preflight.
//! The only writer is `convert_rpu_to_81`, and it runs on an RPU that passed the same checks.
//! Guard coverage is not a full safety proof.
use dolby_vision::rpu::{ConversionMode, dovi_rpu::DoviRpu, rpu_data_nlq::DoviELType};
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SemanticError {
    TooLarge,
    CrcOrEnvelope,
    UnsupportedOrHostileSyntax,
    ParseRejected,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnhancementType {
    Unknown,
    Fel,
    Mel,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RpuSummary {
    pub profile: u8,
    pub enhancement: EnhancementType,
    pub disable_residual: bool,
    pub use_previous_mapping: bool,
    pub has_mapping: bool,
    pub has_display_metadata: bool,
    pub base_bit_depth: u32,
    pub enhancement_bit_depth: u32,
    pub vdr_bit_depth: u32,
    pub coefficient_log2_denom: u32,
}
/// Retain the parsed mapping after the same bounded CRC and known-path preflight.
/// This guard is conservative and is not an exhaustive parser safety proof.
pub fn parse_rpu_checked(nal: &[u8]) -> Result<DoviRpu, SemanticError> {
    if nal.len() > 64 * 1024 {
        return Err(SemanticError::TooLarge);
    }
    let unescaped = crate::rpu::verified_unescaped(nal).ok_or(SemanticError::CrcOrEnvelope)?;
    if !crate::rpu_guard::accepts(&unescaped) {
        return Err(SemanticError::UnsupportedOrHostileSyntax);
    }
    DoviRpu::parse_unspec62_nalu(nal).map_err(|_| SemanticError::ParseRejected)
}
/// One type-62 RPU NAL rewritten to Profile 8.1 (libdovi conversion mode 2), for a Profile 7 base layer shown without its
/// enhancement layer. AetherEngine's native route does the same, so both players reach Apple's Dolby Vision pipeline with one RPU.
pub fn convert_rpu_to_81(nal: &[u8]) -> Result<Vec<u8>, SemanticError> {
    let mut rpu = parse_rpu_checked(nal)?;
    rpu.convert_with_mode(ConversionMode::To81).map_err(|_| SemanticError::ParseRejected)?;
    rpu.write_hevc_unspec62_nalu().map_err(|_| SemanticError::ParseRejected)
}

pub fn inspect_rpu(nal: &[u8]) -> Result<RpuSummary, SemanticError> {
    let rpu = parse_rpu_checked(nal)?;
    Ok(RpuSummary {
        profile: rpu.dovi_profile,
        enhancement: match rpu.el_type {
            Some(DoviELType::FEL) => EnhancementType::Fel,
            Some(DoviELType::MEL) => EnhancementType::Mel,
            None => EnhancementType::Unknown,
        },
        disable_residual: rpu.header.disable_residual_flag,
        use_previous_mapping: rpu.header.use_prev_vdr_rpu_flag,
        has_mapping: rpu.rpu_data_mapping.is_some(),
        has_display_metadata: rpu.vdr_dm_data.is_some(),
        base_bit_depth: rpu.header.bl_bit_depth_minus8 as u32 + 8,
        enhancement_bit_depth: rpu.header.el_bit_depth_minus8 as u32 + 8,
        vdr_bit_depth: rpu.header.vdr_bit_depth_minus8 as u32 + 8,
        coefficient_log2_denom: rpu.header.coefficient_log2_denom as u32,
    })
}
