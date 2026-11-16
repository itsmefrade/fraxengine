//! Band configuration tables (ETSI TS 103 491 clause 10.2.3) and other small constants.

pub const NUM_ACE_BANDS: usize = 22;
pub const FRAME_LEN: usize = 1024;
pub const MAX_BAND_SIZE: usize = 208;

pub struct BandsConfig {
    pub index_largest_band: usize,
    pub num_narrow_bands: usize,
    pub boundaries: [usize; NUM_ACE_BANDS + 1],
    pub bandsize: [usize; NUM_ACE_BANDS],
    /// log2 of the band size with 4 fractional bits (raw LBFP data).
    pub log_band_size: [i32; NUM_ACE_BANDS],
    /// Lognorm offsets in LNFP raw units (10 fractional bits).
    pub lognorm_offset: [i32; NUM_ACE_BANDS],
    pub alloc_max: [i32; NUM_ACE_BANDS],
    pub alloc_min: [i32; NUM_ACE_BANDS],
    /// Refinement delta in LNFP raw units.
    pub refinement_delta: [i32; NUM_ACE_BANDS],
}

const fn lbfp(v: f64) -> i32 {
    (v * 16.0 + 0.5) as i32
}
const fn lnfp(v: f64) -> i32 {
    // const-friendly rounding for both signs
    let x = v * 1024.0;
    if x >= 0.0 {
        (x + 0.5) as i32
    } else {
        -((-x + 0.5) as i32)
    }
}

pub static BANDS_48K: BandsConfig = BandsConfig {
    index_largest_band: 21,
    num_narrow_bands: 8,
    boundaries: [
        0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 144, 176, 208, 240, 288, 336, 416, 512, 672,
        848, 1024,
    ],
    bandsize: [
        8, 8, 8, 8, 8, 8, 8, 8, 16, 16, 16, 32, 32, 32, 32, 48, 48, 80, 96, 160, 176, 176,
    ],
    log_band_size: [
        lbfp(3.0),
        lbfp(3.0),
        lbfp(3.0),
        lbfp(3.0),
        lbfp(3.0),
        lbfp(3.0),
        lbfp(3.0),
        lbfp(3.0),
        lbfp(4.0),
        lbfp(4.0),
        lbfp(4.0),
        lbfp(5.0),
        lbfp(5.0),
        lbfp(5.0),
        lbfp(5.0),
        lbfp(5.5625),
        lbfp(5.5625),
        lbfp(6.3125),
        lbfp(6.5625),
        lbfp(7.3125),
        lbfp(7.4375),
        lbfp(7.4375),
    ],
    lognorm_offset: [
        lnfp(8.8554687500),
        lnfp(8.7666015625),
        lnfp(8.3984375000),
        lnfp(8.0214843750),
        lnfp(7.8046875000),
        lnfp(7.5625000000),
        lnfp(7.4238281250),
        lnfp(7.3964843750),
        lnfp(7.8808593750),
        lnfp(7.7509765625),
        lnfp(7.6406250000),
        lnfp(8.1269531250),
        lnfp(7.9189453125),
        lnfp(7.7216796875),
        lnfp(7.5195312500),
        lnfp(7.7041015625),
        lnfp(7.5351562500),
        lnfp(7.7236328125),
        lnfp(7.5419921875),
        lnfp(7.3388671875),
        lnfp(6.4667968750),
        lnfp(5.6230468750),
    ],
    alloc_max: [
        68, 68, 68, 68, 68, 68, 68, 68, 136, 136, 136, 272, 272, 272, 272, 384, 384, 592, 608, 608,
        640, 640,
    ],
    alloc_min: [
        2, 2, 2, 2, 2, 2, 2, 2, 3, 3, 3, 6, 6, 6, 6, 8, 8, 14, 17, 28, 30, 30,
    ],
    refinement_delta: [
        lnfp(-1.125),
        lnfp(-1.125),
        lnfp(-1.125),
        lnfp(-1.125),
        lnfp(-1.125),
        lnfp(-1.125),
        lnfp(-1.125),
        lnfp(-1.125),
        lnfp(-0.625),
        lnfp(-0.625),
        lnfp(-0.625),
        lnfp(-0.125),
        lnfp(-0.125),
        lnfp(-0.125),
        lnfp(-0.125),
        lnfp(0.125),
        lnfp(0.125),
        lnfp(0.5),
        lnfp(0.625),
        lnfp(1.0),
        lnfp(1.0625),
        lnfp(1.0625),
    ],
};

pub static BANDS_44K1: BandsConfig = BandsConfig {
    index_largest_band: 20,
    num_narrow_bands: 6,
    boundaries: [
        0, 8, 16, 24, 32, 40, 48, 64, 80, 96, 112, 128, 160, 192, 224, 272, 320, 368, 448, 560,
        720, 928, 1024,
    ],
    bandsize: [
        8, 8, 8, 8, 8, 8, 16, 16, 16, 16, 16, 32, 32, 32, 48, 48, 48, 80, 112, 160, 208, 96,
    ],
    log_band_size: [
        lbfp(3.0),
        lbfp(3.0),
        lbfp(3.0),
        lbfp(3.0),
        lbfp(3.0),
        lbfp(3.0),
        lbfp(4.0),
        lbfp(4.0),
        lbfp(4.0),
        lbfp(4.0),
        lbfp(4.0),
        lbfp(5.0),
        lbfp(5.0),
        lbfp(5.0),
        lbfp(5.5625),
        lbfp(5.5625),
        lbfp(5.5625),
        lbfp(6.3125),
        lbfp(6.8125),
        lbfp(7.3125),
        lbfp(7.6875),
        lbfp(6.5625),
    ],
    lognorm_offset: [
        lnfp(8.8554687500),
        lnfp(8.7666015625),
        lnfp(8.3984375000),
        lnfp(8.0214843750),
        lnfp(7.8046875000),
        lnfp(7.5625000000),
        lnfp(7.4238281250),
        lnfp(7.3964843750),
        lnfp(7.8808593750),
        lnfp(7.7509765625),
        lnfp(7.6406250000),
        lnfp(8.1269531250),
        lnfp(7.9189453125),
        lnfp(7.7216796875),
        lnfp(7.5195312500),
        lnfp(7.7041015625),
        lnfp(7.5351562500),
        lnfp(7.7236328125),
        lnfp(7.5419921875),
        lnfp(7.3388671875),
        lnfp(6.4667968750),
        lnfp(5.6230468750),
    ],
    alloc_max: [
        68, 68, 68, 68, 68, 68, 136, 136, 136, 136, 136, 272, 272, 272, 408, 384, 384, 592, 708,
        608, 756, 348,
    ],
    alloc_min: [
        2, 2, 2, 2, 2, 2, 3, 3, 3, 3, 3, 6, 6, 6, 8, 8, 8, 14, 19, 28, 36, 17,
    ],
    refinement_delta: [
        lnfp(-1.125),
        lnfp(-1.125),
        lnfp(-1.125),
        lnfp(-1.125),
        lnfp(-1.125),
        lnfp(-1.125),
        lnfp(-0.625),
        lnfp(-0.625),
        lnfp(-0.625),
        lnfp(-0.625),
        lnfp(-0.625),
        lnfp(-0.125),
        lnfp(-0.125),
        lnfp(-0.125),
        lnfp(0.125),
        lnfp(0.125),
        lnfp(0.125),
        lnfp(0.5),
        lnfp(0.75),
        lnfp(1.0),
        lnfp(1.1875),
        lnfp(0.625),
    ],
};

/// Number of coded bands for each bandwidth mode.
pub const BANDWIDTH_MODE_TABLE: [usize; 4] = [17, 19, 21, 22];

/// Lognorm Golomb parameters (alpha is always 64): (kparam, limit) per
/// [sync_index][transform_index][band]; sync_index 0 = predictive, 1 = non-predictive.
pub fn lognorm_golomb(sync_index: usize, short: bool, band: usize) -> (u32, u32) {
    let t = short as usize;
    match (sync_index, t) {
        (0, _) => {
            if band == 0 {
                (1, 31)
            } else {
                (0, 15)
            }
        }
        (_, 0) => match band {
            0 => (2, 31),
            1 => (1, 31),
            _ => (0, 15),
        },
        _ => {
            if band == 0 {
                (3, 31)
            } else {
                (0, 15)
            }
        }
    }
}

/// F_PREDICTOR_TABLE (predictive set), raw LNFP.
pub const F_PRED_PREDICTIVE: [i32; NUM_ACE_BANDS] = [
    lnfp(0.5996093750),
    lnfp(0.5800781250),
    lnfp(0.5654296875),
    lnfp(0.5498046875),
    lnfp(0.5371093750),
    lnfp(0.5253906250),
    lnfp(0.5146484375),
    lnfp(0.5048828125),
    lnfp(0.5),
    lnfp(0.5),
    lnfp(0.5),
    lnfp(0.5),
    lnfp(0.5),
    lnfp(0.5),
    lnfp(0.5),
    lnfp(0.5),
    lnfp(0.5),
    lnfp(0.5),
    lnfp(0.5),
    lnfp(0.5),
    lnfp(0.5),
    lnfp(0.5),
];

pub const B_PRED_PREDICTIVE: [i32; NUM_ACE_BANDS] = [
    lnfp(0.0),
    lnfp(1.0),
    lnfp(0.75),
    lnfp(0.5),
    lnfp(0.5),
    lnfp(0.4501953125),
    lnfp(0.4003906250),
    lnfp(0.4003906250),
    lnfp(0.3496093750),
    lnfp(0.5),
    lnfp(0.5),
    lnfp(0.5),
    lnfp(0.5996093750),
    lnfp(0.5996093750),
    lnfp(0.5996093750),
    lnfp(0.5996093750),
    lnfp(0.5996093750),
    lnfp(0.5996093750),
    lnfp(0.7001953125),
    lnfp(0.75),
    lnfp(0.7998046875),
    lnfp(0.7998046875),
];

pub const B_PRED_NONPRED: [i32; NUM_ACE_BANDS] = [
    lnfp(0.0),
    lnfp(0.9248046875),
    lnfp(0.9501953125),
    lnfp(0.7001953125),
    lnfp(0.7001953125),
    lnfp(0.5996093750),
    lnfp(0.5),
    lnfp(0.5),
    lnfp(0.4501953125),
    lnfp(0.5996093750),
    lnfp(0.625),
    lnfp(0.625),
    lnfp(0.7753906250),
    lnfp(0.7998046875),
    lnfp(0.7998046875),
    lnfp(0.7998046875),
    lnfp(0.7998046875),
    lnfp(0.8251953125),
    lnfp(0.9248046875),
    lnfp(1.0),
    lnfp(1.0),
    lnfp(1.0),
];

/// LOGNORM_REFINEMENT_BOOST_TABLE, raw LNFP.
pub const REFINEMENT_BOOST: [i32; NUM_ACE_BANDS] = [
    lnfp(0.375),
    lnfp(0.375),
    lnfp(0.375),
    lnfp(0.375),
    lnfp(0.375),
    lnfp(0.375),
    lnfp(0.375),
    lnfp(0.375),
    lnfp(0.5),
    lnfp(0.5),
    lnfp(0.5),
    lnfp(0.625),
    lnfp(0.625),
    lnfp(0.625),
    lnfp(0.625),
    lnfp(0.679688),
    lnfp(0.734375),
    lnfp(0.789063),
    lnfp(0.843750),
    lnfp(0.898438),
    lnfp(0.937500),
    lnfp(0.937500),
];

/// BAFACTOR table (clause 9.9.13.10), raw BAFP (10 fractional bits).
pub const BAFACTOR: [i32; 21] = [
    82, 90, 98, 106, 114, 122, 130, 138, 146, 154, 162, 170, 178, 186, 194, 202, 210, 218, 226,
    234, 242,
];
