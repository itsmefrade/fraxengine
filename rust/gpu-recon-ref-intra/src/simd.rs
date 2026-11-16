//! The SIMD kernels of rust_h265 are bit exact with its scalar code (intra_simd.rs tests); the reference runs
//! the scalar code only.
pub const HAVE_SIMD: bool = false;
