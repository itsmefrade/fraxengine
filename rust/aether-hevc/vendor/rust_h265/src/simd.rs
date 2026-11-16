//! Minimal 128-bit SIMD layer for the 16-bit sample kernels: WebAssembly simd128 and AArch64 NEON,
//! behind one set of `#[inline(always)]` primitives. The kernels in `mc_simd` / `tx_simd` are written
//! once against these. Targets with neither fall back to the portable scalar code in the callers
//! (`HAVE_SIMD` is false there). All loads and stores are unaligned.
//!
//! Lane conventions: `V16` is eight i16 lanes, `V32` four i32 lanes. Pixel data are u16 samples that
//! fit in i16 (bit depth <= 12), so they are loaded as i16 lanes.
#![allow(dead_code, unused_unsafe)]

#[cfg(all(target_arch = "wasm32", target_feature = "simd128"))]
mod imp {
    use core::arch::wasm32::*;
    pub const HAVE_SIMD: bool = !cfg!(feature = "scalar");
    pub type V16 = v128;
    pub type V32 = v128;
    #[inline(always)]
    pub unsafe fn ld(p: *const i16) -> V16 {
        unsafe { v128_load(p as *const v128) }
    }
    #[inline(always)]
    pub unsafe fn st(p: *mut i16, v: V16) {
        unsafe { v128_store(p as *mut v128, v) }
    }
    /// Load four i16 lanes (the upper four are zero).
    #[inline(always)]
    pub unsafe fn ld4(p: *const i16) -> V16 {
        unsafe { v128_load64_zero(p as *const u64) }
    }
    /// Load four i32 lanes.
    #[inline(always)]
    pub unsafe fn ld_i32(p: *const i32) -> V32 {
        unsafe { v128_load(p as *const v128) }
    }
    /// Store the low four i16 lanes.
    #[inline(always)]
    pub unsafe fn st4(p: *mut i16, v: V16) {
        unsafe { v128_store64_lane::<0>(v, p as *mut u64) }
    }
    #[inline(always)]
    pub fn splat16(x: i16) -> V16 {
        i16x8_splat(x)
    }
    #[inline(always)]
    pub fn splat32(x: i32) -> V32 {
        i32x4_splat(x)
    }
    #[inline(always)]
    pub fn zero() -> V16 {
        i16x8_splat(0)
    }
    #[inline(always)]
    pub fn add16(a: V16, b: V16) -> V16 {
        i16x8_add(a, b)
    }
    #[inline(always)]
    pub fn sub16(a: V16, b: V16) -> V16 {
        i16x8_sub(a, b)
    }
    #[inline(always)]
    pub fn mul16(a: V16, b: V16) -> V16 {
        i16x8_mul(a, b)
    }
    #[inline(always)]
    pub fn adds16(a: V16, b: V16) -> V16 {
        i16x8_add_sat(a, b)
    }
    #[inline(always)]
    pub fn max16(a: V16, b: V16) -> V16 {
        i16x8_max(a, b)
    }
    #[inline(always)]
    pub fn min16(a: V16, b: V16) -> V16 {
        i16x8_min(a, b)
    }
    #[inline(always)]
    pub fn shr16<const N: u32>(a: V16) -> V16 {
        i16x8_shr(a, N)
    }
    #[inline(always)]
    pub fn shl16<const N: u32>(a: V16) -> V16 {
        i16x8_shl(a, N)
    }
    #[inline(always)]
    pub fn shr16v(a: V16, n: u32) -> V16 {
        i16x8_shr(a, n)
    }
    #[inline(always)]
    pub fn shl16v(a: V16, n: u32) -> V16 {
        i16x8_shl(a, n)
    }
    #[inline(always)]
    pub fn add32(a: V32, b: V32) -> V32 {
        i32x4_add(a, b)
    }
    #[inline(always)]
    pub fn sub32(a: V32, b: V32) -> V32 {
        i32x4_sub(a, b)
    }
    #[inline(always)]
    pub fn mul32(a: V32, b: V32) -> V32 {
        i32x4_mul(a, b)
    }
    #[inline(always)]
    pub fn shr32<const N: u32>(a: V32) -> V32 {
        i32x4_shr(a, N)
    }
    #[inline(always)]
    pub fn shr32v(a: V32, n: u32) -> V32 {
        i32x4_shr(a, n)
    }
    #[inline(always)]
    pub fn shl32v(a: V32, n: u32) -> V32 {
        i32x4_shl(a, n)
    }
    #[inline(always)]
    pub fn max32(a: V32, b: V32) -> V32 {
        i32x4_max(a, b)
    }
    #[inline(always)]
    pub fn min32(a: V32, b: V32) -> V32 {
        i32x4_min(a, b)
    }
    /// Signed i16 products of the low four lanes, widened to i32.
    #[inline(always)]
    pub fn mul_lo(a: V16, b: V16) -> V32 {
        i32x4_extmul_low_i16x8(a, b)
    }
    #[inline(always)]
    pub fn mul_hi(a: V16, b: V16) -> V32 {
        i32x4_extmul_high_i16x8(a, b)
    }
    #[inline(always)]
    pub fn mac_lo(acc: V32, a: V16, b: V16) -> V32 {
        i32x4_add(acc, i32x4_extmul_low_i16x8(a, b))
    }
    #[inline(always)]
    pub fn mac_hi(acc: V32, a: V16, b: V16) -> V32 {
        i32x4_add(acc, i32x4_extmul_high_i16x8(a, b))
    }
    /// Pairwise products summed: lane i = a[2i]*b[2i] + a[2i+1]*b[2i+1].
    #[inline(always)]
    pub fn dot(a: V16, b: V16) -> V32 {
        i32x4_dot_i16x8(a, b)
    }
    #[inline(always)]
    pub fn widen_lo(a: V16) -> V32 {
        i32x4_extend_low_i16x8(a)
    }
    #[inline(always)]
    pub fn widen_hi(a: V16) -> V32 {
        i32x4_extend_high_i16x8(a)
    }
    /// Saturating narrow of two i32 vectors to one i16 vector (`a` is the low half).
    #[inline(always)]
    pub fn narrow32(a: V32, b: V32) -> V16 {
        i16x8_narrow_i32x4(a, b)
    }
    #[inline(always)]
    pub fn zip_lo16(a: V16, b: V16) -> V16 {
        i8x16_shuffle::<0, 1, 16, 17, 2, 3, 18, 19, 4, 5, 20, 21, 6, 7, 22, 23>(a, b)
    }
    #[inline(always)]
    pub fn zip_hi16(a: V16, b: V16) -> V16 {
        i8x16_shuffle::<8, 9, 24, 25, 10, 11, 26, 27, 12, 13, 28, 29, 14, 15, 30, 31>(a, b)
    }
    #[inline(always)]
    pub fn zip_lo32(a: V16, b: V16) -> V16 {
        i8x16_shuffle::<0, 1, 2, 3, 16, 17, 18, 19, 4, 5, 6, 7, 20, 21, 22, 23>(a, b)
    }
    #[inline(always)]
    pub fn zip_hi32(a: V16, b: V16) -> V16 {
        i8x16_shuffle::<8, 9, 10, 11, 24, 25, 26, 27, 12, 13, 14, 15, 28, 29, 30, 31>(a, b)
    }
    #[inline(always)]
    pub fn zip_lo64(a: V16, b: V16) -> V16 {
        i8x16_shuffle::<0, 1, 2, 3, 4, 5, 6, 7, 16, 17, 18, 19, 20, 21, 22, 23>(a, b)
    }
    #[inline(always)]
    pub fn zip_hi64(a: V16, b: V16) -> V16 {
        i8x16_shuffle::<8, 9, 10, 11, 12, 13, 14, 15, 24, 25, 26, 27, 28, 29, 30, 31>(a, b)
    }
    /// Eight i16 lanes holding the 32-bit pattern `lo | hi << 16` four times (a coefficient pair for `dot`).
    #[inline(always)]
    pub fn pairs16(lo: i16, hi: i16) -> V16 {
        i32x4_splat(((hi as u16 as u32) << 16 | lo as u16 as u32) as i32)
    }
    /// Eight i16 lanes holding the packed 32-bit pattern `c` four times.
    #[inline(always)]
    pub fn splat_pair(c: i32) -> V16 {
        i32x4_splat(c)
    }
    #[inline(always)]
    pub fn and(a: V16, b: V16) -> V16 {
        v128_and(a, b)
    }
    #[inline(always)]
    pub fn or(a: V16, b: V16) -> V16 {
        v128_or(a, b)
    }
    #[inline(always)]
    pub fn xor(a: V16, b: V16) -> V16 {
        v128_xor(a, b)
    }
    #[inline(always)]
    pub fn andnot(a: V16, b: V16) -> V16 {
        v128_andnot(a, b)
    }
    /// Bitwise select: bits of `a` where `m` is set, else bits of `b`.
    #[inline(always)]
    pub fn select(m: V16, a: V16, b: V16) -> V16 {
        v128_bitselect(a, b, m)
    }
    #[inline(always)]
    pub fn cmpgt16(a: V16, b: V16) -> V16 {
        i16x8_gt(a, b)
    }
    #[inline(always)]
    pub fn cmplt16(a: V16, b: V16) -> V16 {
        i16x8_lt(a, b)
    }
    #[inline(always)]
    pub fn cmpeq16(a: V16, b: V16) -> V16 {
        i16x8_eq(a, b)
    }
    #[inline(always)]
    pub fn abs16(a: V16) -> V16 {
        i16x8_abs(a)
    }
    /// Non-zero when any bit of the vector is set.
    #[inline(always)]
    pub fn any(a: V16) -> bool {
        v128_any_true(a)
    }
    /// Unsigned rounding average of the u16 lanes: `(a + b + 1) >> 1`.
    #[inline(always)]
    pub fn avgr16(a: V16, b: V16) -> V16 {
        u16x8_avgr(a, b)
    }
}

#[cfg(all(target_arch = "aarch64", not(all(target_arch = "wasm32"))))]
mod imp {
    use core::arch::aarch64::*;
    pub const HAVE_SIMD: bool = !cfg!(feature = "scalar");
    pub type V16 = int16x8_t;
    pub type V32 = int32x4_t;
    #[inline(always)]
    pub unsafe fn ld(p: *const i16) -> V16 {
        unsafe { vld1q_s16(p) }
    }
    #[inline(always)]
    pub unsafe fn st(p: *mut i16, v: V16) {
        unsafe { vst1q_s16(p, v) }
    }
    #[inline(always)]
    pub unsafe fn ld4(p: *const i16) -> V16 {
        unsafe { vcombine_s16(vld1_s16(p), vdup_n_s16(0)) }
    }
    #[inline(always)]
    pub unsafe fn ld_i32(p: *const i32) -> V32 {
        unsafe { vld1q_s32(p) }
    }
    #[inline(always)]
    pub unsafe fn st4(p: *mut i16, v: V16) {
        unsafe { vst1_s16(p, vget_low_s16(v)) }
    }
    #[inline(always)]
    pub fn splat16(x: i16) -> V16 {
        unsafe { vdupq_n_s16(x) }
    }
    #[inline(always)]
    pub fn splat32(x: i32) -> V32 {
        unsafe { vdupq_n_s32(x) }
    }
    #[inline(always)]
    pub fn zero() -> V16 {
        unsafe { vdupq_n_s16(0) }
    }
    #[inline(always)]
    pub fn add16(a: V16, b: V16) -> V16 {
        unsafe { vaddq_s16(a, b) }
    }
    #[inline(always)]
    pub fn sub16(a: V16, b: V16) -> V16 {
        unsafe { vsubq_s16(a, b) }
    }
    #[inline(always)]
    pub fn mul16(a: V16, b: V16) -> V16 {
        unsafe { vmulq_s16(a, b) }
    }
    #[inline(always)]
    pub fn adds16(a: V16, b: V16) -> V16 {
        unsafe { vqaddq_s16(a, b) }
    }
    #[inline(always)]
    pub fn max16(a: V16, b: V16) -> V16 {
        unsafe { vmaxq_s16(a, b) }
    }
    #[inline(always)]
    pub fn min16(a: V16, b: V16) -> V16 {
        unsafe { vminq_s16(a, b) }
    }
    #[inline(always)]
    pub fn shr16<const N: u32>(a: V16) -> V16 {
        unsafe { vshlq_s16(a, vdupq_n_s16(-(N as i16))) }
    }
    #[inline(always)]
    pub fn shl16<const N: u32>(a: V16) -> V16 {
        unsafe { vshlq_s16(a, vdupq_n_s16(N as i16)) }
    }
    #[inline(always)]
    pub fn shr16v(a: V16, n: u32) -> V16 {
        unsafe { vshlq_s16(a, vdupq_n_s16(-(n as i16))) }
    }
    #[inline(always)]
    pub fn shl16v(a: V16, n: u32) -> V16 {
        unsafe { vshlq_s16(a, vdupq_n_s16(n as i16)) }
    }
    #[inline(always)]
    pub fn add32(a: V32, b: V32) -> V32 {
        unsafe { vaddq_s32(a, b) }
    }
    #[inline(always)]
    pub fn sub32(a: V32, b: V32) -> V32 {
        unsafe { vsubq_s32(a, b) }
    }
    #[inline(always)]
    pub fn mul32(a: V32, b: V32) -> V32 {
        unsafe { vmulq_s32(a, b) }
    }
    #[inline(always)]
    pub fn shr32<const N: u32>(a: V32) -> V32 {
        unsafe { vshlq_s32(a, vdupq_n_s32(-(N as i32))) }
    }
    #[inline(always)]
    pub fn shr32v(a: V32, n: u32) -> V32 {
        unsafe { vshlq_s32(a, vdupq_n_s32(-(n as i32))) }
    }
    #[inline(always)]
    pub fn shl32v(a: V32, n: u32) -> V32 {
        unsafe { vshlq_s32(a, vdupq_n_s32(n as i32)) }
    }
    #[inline(always)]
    pub fn max32(a: V32, b: V32) -> V32 {
        unsafe { vmaxq_s32(a, b) }
    }
    #[inline(always)]
    pub fn min32(a: V32, b: V32) -> V32 {
        unsafe { vminq_s32(a, b) }
    }
    #[inline(always)]
    pub fn mul_lo(a: V16, b: V16) -> V32 {
        unsafe { vmull_s16(vget_low_s16(a), vget_low_s16(b)) }
    }
    #[inline(always)]
    pub fn mul_hi(a: V16, b: V16) -> V32 {
        unsafe { vmull_high_s16(a, b) }
    }
    #[inline(always)]
    pub fn mac_lo(acc: V32, a: V16, b: V16) -> V32 {
        unsafe { vmlal_s16(acc, vget_low_s16(a), vget_low_s16(b)) }
    }
    #[inline(always)]
    pub fn mac_hi(acc: V32, a: V16, b: V16) -> V32 {
        unsafe { vmlal_high_s16(acc, a, b) }
    }
    #[inline(always)]
    pub fn dot(a: V16, b: V16) -> V32 {
        unsafe { vpaddq_s32(vmull_s16(vget_low_s16(a), vget_low_s16(b)), vmull_high_s16(a, b)) }
    }
    #[inline(always)]
    pub fn widen_lo(a: V16) -> V32 {
        unsafe { vmovl_s16(vget_low_s16(a)) }
    }
    #[inline(always)]
    pub fn widen_hi(a: V16) -> V32 {
        unsafe { vmovl_high_s16(a) }
    }
    #[inline(always)]
    pub fn narrow32(a: V32, b: V32) -> V16 {
        unsafe { vcombine_s16(vqmovn_s32(a), vqmovn_s32(b)) }
    }
    #[inline(always)]
    pub fn zip_lo16(a: V16, b: V16) -> V16 {
        unsafe { vzip1q_s16(a, b) }
    }
    #[inline(always)]
    pub fn zip_hi16(a: V16, b: V16) -> V16 {
        unsafe { vzip2q_s16(a, b) }
    }
    #[inline(always)]
    pub fn zip_lo32(a: V16, b: V16) -> V16 {
        unsafe { vreinterpretq_s16_s32(vzip1q_s32(vreinterpretq_s32_s16(a), vreinterpretq_s32_s16(b))) }
    }
    #[inline(always)]
    pub fn zip_hi32(a: V16, b: V16) -> V16 {
        unsafe { vreinterpretq_s16_s32(vzip2q_s32(vreinterpretq_s32_s16(a), vreinterpretq_s32_s16(b))) }
    }
    #[inline(always)]
    pub fn zip_lo64(a: V16, b: V16) -> V16 {
        unsafe { vreinterpretq_s16_s64(vzip1q_s64(vreinterpretq_s64_s16(a), vreinterpretq_s64_s16(b))) }
    }
    #[inline(always)]
    pub fn zip_hi64(a: V16, b: V16) -> V16 {
        unsafe { vreinterpretq_s16_s64(vzip2q_s64(vreinterpretq_s64_s16(a), vreinterpretq_s64_s16(b))) }
    }
    #[inline(always)]
    pub fn pairs16(lo: i16, hi: i16) -> V16 {
        unsafe { vreinterpretq_s16_s32(vdupq_n_s32(((hi as u16 as u32) << 16 | lo as u16 as u32) as i32)) }
    }
    #[inline(always)]
    pub fn splat_pair(c: i32) -> V16 {
        unsafe { vreinterpretq_s16_s32(vdupq_n_s32(c)) }
    }
    #[inline(always)]
    pub fn and(a: V16, b: V16) -> V16 {
        unsafe { vandq_s16(a, b) }
    }
    #[inline(always)]
    pub fn or(a: V16, b: V16) -> V16 {
        unsafe { vorrq_s16(a, b) }
    }
    #[inline(always)]
    pub fn xor(a: V16, b: V16) -> V16 {
        unsafe { veorq_s16(a, b) }
    }
    #[inline(always)]
    pub fn andnot(a: V16, b: V16) -> V16 {
        unsafe { vbicq_s16(a, b) }
    }
    #[inline(always)]
    pub fn select(m: V16, a: V16, b: V16) -> V16 {
        unsafe { vbslq_s16(vreinterpretq_u16_s16(m), a, b) }
    }
    #[inline(always)]
    pub fn cmpgt16(a: V16, b: V16) -> V16 {
        unsafe { vreinterpretq_s16_u16(vcgtq_s16(a, b)) }
    }
    #[inline(always)]
    pub fn cmplt16(a: V16, b: V16) -> V16 {
        unsafe { vreinterpretq_s16_u16(vcltq_s16(a, b)) }
    }
    #[inline(always)]
    pub fn cmpeq16(a: V16, b: V16) -> V16 {
        unsafe { vreinterpretq_s16_u16(vceqq_s16(a, b)) }
    }
    #[inline(always)]
    pub fn abs16(a: V16) -> V16 {
        unsafe { vabsq_s16(a) }
    }
    #[inline(always)]
    pub fn any(a: V16) -> bool {
        unsafe { vmaxvq_u16(vreinterpretq_u16_s16(a)) != 0 }
    }
    #[inline(always)]
    pub fn avgr16(a: V16, b: V16) -> V16 {
        unsafe { vreinterpretq_s16_u16(vrhaddq_u16(vreinterpretq_u16_s16(a), vreinterpretq_u16_s16(b))) }
    }
}

/// Portable stand-in with the same API, for targets with neither simd128 nor NEON. `HAVE_SIMD` is false
/// there, so the callers never take the SIMD branches; this only has to compile (and stay correct).
#[cfg(not(any(
    all(target_arch = "wasm32", target_feature = "simd128"),
    target_arch = "aarch64"
)))]
mod imp {
    pub const HAVE_SIMD: bool = false;
    #[derive(Clone, Copy)]
    pub struct V16(pub [i16; 8]);
    #[derive(Clone, Copy)]
    pub struct V32(pub [i32; 4]);
    fn m16(a: V16, b: V16, f: impl Fn(i16, i16) -> i16) -> V16 {
        V16(std::array::from_fn(|i| f(a.0[i], b.0[i])))
    }
    fn m32(a: V32, b: V32, f: impl Fn(i32, i32) -> i32) -> V32 {
        V32(std::array::from_fn(|i| f(a.0[i], b.0[i])))
    }
    pub unsafe fn ld(p: *const i16) -> V16 {
        V16(unsafe { std::ptr::read_unaligned(p as *const [i16; 8]) })
    }
    pub unsafe fn st(p: *mut i16, v: V16) {
        unsafe { std::ptr::write_unaligned(p as *mut [i16; 8], v.0) }
    }
    pub unsafe fn ld4(p: *const i16) -> V16 {
        let lo: [i16; 4] = unsafe { std::ptr::read_unaligned(p as *const [i16; 4]) };
        V16([lo[0], lo[1], lo[2], lo[3], 0, 0, 0, 0])
    }
    pub unsafe fn st4(p: *mut i16, v: V16) {
        unsafe { std::ptr::write_unaligned(p as *mut [i16; 4], [v.0[0], v.0[1], v.0[2], v.0[3]]) }
    }
    pub unsafe fn ld_i32(p: *const i32) -> V32 {
        V32(unsafe { std::ptr::read_unaligned(p as *const [i32; 4]) })
    }
    pub fn splat16(x: i16) -> V16 {
        V16([x; 8])
    }
    pub fn splat32(x: i32) -> V32 {
        V32([x; 4])
    }
    pub fn zero() -> V16 {
        V16([0; 8])
    }
    pub fn add16(a: V16, b: V16) -> V16 {
        m16(a, b, i16::wrapping_add)
    }
    pub fn sub16(a: V16, b: V16) -> V16 {
        m16(a, b, i16::wrapping_sub)
    }
    pub fn mul16(a: V16, b: V16) -> V16 {
        m16(a, b, i16::wrapping_mul)
    }
    pub fn adds16(a: V16, b: V16) -> V16 {
        m16(a, b, i16::saturating_add)
    }
    pub fn max16(a: V16, b: V16) -> V16 {
        m16(a, b, i16::max)
    }
    pub fn min16(a: V16, b: V16) -> V16 {
        m16(a, b, i16::min)
    }
    pub fn shr16<const N: u32>(a: V16) -> V16 {
        shr16v(a, N)
    }
    pub fn shl16<const N: u32>(a: V16) -> V16 {
        shl16v(a, N)
    }
    pub fn shr16v(a: V16, n: u32) -> V16 {
        V16(a.0.map(|x| x >> n))
    }
    pub fn shl16v(a: V16, n: u32) -> V16 {
        V16(a.0.map(|x| x << n))
    }
    pub fn add32(a: V32, b: V32) -> V32 {
        m32(a, b, i32::wrapping_add)
    }
    pub fn sub32(a: V32, b: V32) -> V32 {
        m32(a, b, i32::wrapping_sub)
    }
    pub fn mul32(a: V32, b: V32) -> V32 {
        m32(a, b, i32::wrapping_mul)
    }
    pub fn shr32<const N: u32>(a: V32) -> V32 {
        shr32v(a, N)
    }
    pub fn shr32v(a: V32, n: u32) -> V32 {
        V32(a.0.map(|x| x >> n))
    }
    pub fn shl32v(a: V32, n: u32) -> V32 {
        V32(a.0.map(|x| x << n))
    }
    pub fn max32(a: V32, b: V32) -> V32 {
        m32(a, b, i32::max)
    }
    pub fn min32(a: V32, b: V32) -> V32 {
        m32(a, b, i32::min)
    }
    pub fn mul_lo(a: V16, b: V16) -> V32 {
        V32(std::array::from_fn(|i| a.0[i] as i32 * b.0[i] as i32))
    }
    pub fn mul_hi(a: V16, b: V16) -> V32 {
        V32(std::array::from_fn(|i| a.0[i + 4] as i32 * b.0[i + 4] as i32))
    }
    pub fn mac_lo(acc: V32, a: V16, b: V16) -> V32 {
        add32(acc, mul_lo(a, b))
    }
    pub fn mac_hi(acc: V32, a: V16, b: V16) -> V32 {
        add32(acc, mul_hi(a, b))
    }
    pub fn dot(a: V16, b: V16) -> V32 {
        V32(std::array::from_fn(|i| {
            a.0[2 * i] as i32 * b.0[2 * i] as i32 + a.0[2 * i + 1] as i32 * b.0[2 * i + 1] as i32
        }))
    }
    pub fn widen_lo(a: V16) -> V32 {
        V32(std::array::from_fn(|i| a.0[i] as i32))
    }
    pub fn widen_hi(a: V16) -> V32 {
        V32(std::array::from_fn(|i| a.0[i + 4] as i32))
    }
    pub fn narrow32(a: V32, b: V32) -> V16 {
        V16(std::array::from_fn(|i| {
            let v = if i < 4 { a.0[i] } else { b.0[i - 4] };
            v.clamp(i16::MIN as i32, i16::MAX as i32) as i16
        }))
    }
    fn zip(a: V16, b: V16, width: usize, high: bool) -> V16 {
        // Interleave `width`-lane groups (width in i16 lanes) of `a` and `b` from the low or high half.
        let mut out = [0i16; 8];
        let half = if high { 4 } else { 0 };
        let mut o = 0;
        let mut g = 0;
        while o < 8 {
            for k in 0..width {
                out[o + k] = a.0[half + g * width + k];
                out[o + width + k] = b.0[half + g * width + k];
            }
            o += 2 * width;
            g += 1;
        }
        V16(out)
    }
    pub fn zip_lo16(a: V16, b: V16) -> V16 {
        zip(a, b, 1, false)
    }
    pub fn zip_hi16(a: V16, b: V16) -> V16 {
        zip(a, b, 1, true)
    }
    pub fn zip_lo32(a: V16, b: V16) -> V16 {
        zip(a, b, 2, false)
    }
    pub fn zip_hi32(a: V16, b: V16) -> V16 {
        zip(a, b, 2, true)
    }
    pub fn zip_lo64(a: V16, b: V16) -> V16 {
        zip(a, b, 4, false)
    }
    pub fn zip_hi64(a: V16, b: V16) -> V16 {
        zip(a, b, 4, true)
    }
    pub fn pairs16(lo: i16, hi: i16) -> V16 {
        V16([lo, hi, lo, hi, lo, hi, lo, hi])
    }
    pub fn splat_pair(c: i32) -> V16 {
        pairs16(c as i16, (c >> 16) as i16)
    }
    pub fn and(a: V16, b: V16) -> V16 {
        m16(a, b, |x, y| x & y)
    }
    pub fn or(a: V16, b: V16) -> V16 {
        m16(a, b, |x, y| x | y)
    }
    pub fn xor(a: V16, b: V16) -> V16 {
        m16(a, b, |x, y| x ^ y)
    }
    pub fn andnot(a: V16, b: V16) -> V16 {
        m16(a, b, |x, y| x & !y)
    }
    pub fn select(m: V16, a: V16, b: V16) -> V16 {
        V16(std::array::from_fn(|i| (a.0[i] & m.0[i]) | (b.0[i] & !m.0[i])))
    }
    pub fn cmpgt16(a: V16, b: V16) -> V16 {
        m16(a, b, |x, y| if x > y { -1 } else { 0 })
    }
    pub fn cmplt16(a: V16, b: V16) -> V16 {
        m16(a, b, |x, y| if x < y { -1 } else { 0 })
    }
    pub fn cmpeq16(a: V16, b: V16) -> V16 {
        m16(a, b, |x, y| if x == y { -1 } else { 0 })
    }
    pub fn abs16(a: V16) -> V16 {
        V16(a.0.map(i16::wrapping_abs))
    }
    pub fn any(a: V16) -> bool {
        a.0.iter().any(|&x| x != 0)
    }
    pub fn avgr16(a: V16, b: V16) -> V16 {
        m16(a, b, |x, y| ((x as u16 as u32 + y as u16 as u32 + 1) >> 1) as i16)
    }
}

pub use imp::*;
