//! Signatures of rust_h265 src/intra_simd.rs; never called because `simd::HAVE_SIMD` is false.
#![allow(unused_variables)]
pub(crate) fn add_residual(dst: &mut [u16], stride: usize, residual: &[i16], log2: u8, max: u16) {
    unreachable!()
}
pub(crate) fn fill_block(dst: &mut [u16], stride: usize, log2: u8, value: u16) {
    unreachable!()
}
pub(crate) fn planar(dst: &mut [u16], stride: usize, top: &[u16], left: &[u16], log2: u8) {
    unreachable!()
}
pub(crate) fn angular_row(out: &mut [u16], r: &[u16], fact: i32) {
    unreachable!()
}
