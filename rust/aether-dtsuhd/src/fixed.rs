//! Fixed point arithmetic of ETSI TS 103 491 clause 8.4.1.
//!
//! The bit allocation arithmetic has to be reproduced exactly because it decides how
//! many bits each band consumes, so these types follow the reference definitions
//! (arithmetic right shifts, truncating division) rather than using floats.

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct Fx<const P: u32>(pub i32);

/// 2^(i/128) with 9 fractional bits, 0 <= i < 128 (EXPONENTIAL_TABLE).
pub const EXPONENTIAL_TABLE: [u32; 128] = [
    512, 515, 518, 520, 523, 526, 529, 532, 535, 538, 540, 543, 546, 549, 552, 555, 558, 561, 564,
    567, 571, 574, 577, 580, 583, 586, 589, 593, 596, 599, 602, 606, 609, 612, 616, 619, 622, 626,
    629, 632, 636, 639, 643, 646, 650, 653, 657, 660, 664, 668, 671, 675, 679, 682, 686, 690, 693,
    697, 701, 705, 709, 712, 716, 720, 724, 728, 732, 736, 740, 744, 748, 752, 756, 760, 764, 769,
    773, 777, 781, 785, 790, 794, 798, 803, 807, 811, 816, 820, 825, 829, 834, 838, 843, 847, 852,
    856, 861, 866, 870, 875, 880, 885, 890, 894, 899, 904, 909, 914, 919, 924, 929, 934, 939, 944,
    949, 954, 960, 965, 970, 975, 981, 986, 991, 997, 1002, 1007, 1013, 1018,
];

impl<const P: u32> Fx<P> {
    pub const ZERO: Self = Fx(0);

    pub fn from_int(i: i32) -> Self {
        Fx(i.wrapping_shl(P))
    }
    pub fn from_f64(f: f64) -> Self {
        Fx((f * (1u64 << P) as f64 + 0.5).floor() as i32)
    }
    pub fn raw(self) -> i32 {
        self.0
    }
    pub fn to_f64(self) -> f64 {
        self.0 as f64 / (1u64 << P) as f64
    }
    pub fn int_part(self) -> i32 {
        self.0 >> P
    }
    pub fn frac_part(self) -> i32 {
        self.0 & ((1 << P) - 1)
    }
    /// round(): add just under one half, then truncate.
    pub fn round(self) -> i32 {
        (self.0.wrapping_add(((1 << P) >> 1) - 1)) >> P
    }
    pub fn add(self, o: Self) -> Self {
        Fx(self.0.wrapping_add(o.0))
    }
    pub fn sub(self, o: Self) -> Self {
        Fx(self.0.wrapping_sub(o.0))
    }
    pub fn mul(self, o: Self) -> Self {
        Fx((((self.0 as i64) * (o.0 as i64)) >> P) as i32)
    }
    pub fn mul_int(self, y: i32) -> Self {
        Fx(self.0.wrapping_mul(y))
    }
    pub fn div_int(self, y: i32) -> Self {
        if y == 0 {
            Fx(0)
        } else {
            Fx(self.0 / y)
        }
    }
    pub fn neg(self) -> Self {
        Fx(self.0.wrapping_neg())
    }
    pub fn max(self, o: Self) -> Self {
        if self >= o {
            self
        } else {
            o
        }
    }
    pub fn min(self, o: Self) -> Self {
        if self <= o {
            self
        } else {
            o
        }
    }
    /// FixedPoint approximation of 2^x (clause 8.4.1 `exp2`).
    pub fn exp2(self) -> Self {
        let int_part = self.int_part();
        let mut frac = self.frac_part();
        if P >= 7 {
            frac >>= P - 7;
        } else {
            frac <<= 7 - P;
        }
        let mut fe = EXPONENTIAL_TABLE[(frac as usize) & 127] as i64;
        let ip = int_part as i64;
        let sh = ip + P as i64 - 9;
        if sh >= 0 {
            fe <<= sh.min(40);
        } else {
            fe >>= (-sh).min(40);
        }
        Fx(fe.clamp(i32::MIN as i64, i32::MAX as i64) as i32)
    }
}

/// Lognorm, 10 fractional bits.
pub type Lnfp = Fx<10>;
/// Bit allocation, 10 fractional bits.
pub type Bafp = Fx<10>;
/// Band size logarithm, 4 fractional bits.
pub type Lbfp = Fx<4>;
/// Running balance, 5 fractional bits.
pub type Rbfp = Fx<5>;
/// Band quantizer, 11 fractional bits.
pub type Bqfp = Fx<11>;
