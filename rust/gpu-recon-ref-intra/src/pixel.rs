//! The part of rust_h265 src/pixel.rs that `intra_pred.rs` uses, for `u16` samples only.

pub trait Pixel: Copy + Clone + Default + Send + Sync + Sized + 'static {
    fn from_i32_clamped(val: i32, bit_depth: u8) -> Self;
    fn to_i32(self) -> i32;
    fn zero() -> Self;
    fn as_u16(s: &[Self]) -> Option<&[u16]>;
    fn as_u16_mut(s: &mut [Self]) -> Option<&mut [u16]>;
}

impl Pixel for u16 {
    #[inline(always)]
    fn from_i32_clamped(val: i32, bit_depth: u8) -> Self {
        let max = (1i32 << bit_depth) - 1;
        val.clamp(0, max) as u16
    }
    #[inline(always)]
    fn to_i32(self) -> i32 {
        self as i32
    }
    #[inline(always)]
    fn zero() -> Self {
        0
    }
    fn as_u16(s: &[Self]) -> Option<&[u16]> {
        Some(s)
    }
    fn as_u16_mut(s: &mut [Self]) -> Option<&mut [u16]> {
        Some(s)
    }
}

/// Verbatim from rust_h265 src/pixel.rs:231.
pub fn default_ref_sample(bit_depth: u8) -> i32 {
    1i32 << (bit_depth - 1)
}
