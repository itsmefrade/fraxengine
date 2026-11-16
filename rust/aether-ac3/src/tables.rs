//! Static tables and table generators (ported from FFmpeg ac3tab.c, ac3dec_data.c).

pub const AC3_MAX_CHANNELS: usize = 7;
pub const EAC3_MAX_CHANNELS: usize = 16;
pub const AC3_BLOCK_SIZE: usize = 256;
pub const AC3_MAX_BLOCKS: usize = 6;
pub const AC3_CRITICAL_BANDS: usize = 50;
pub const AC3_MAX_CPL_BANDS: usize = 18;
pub const SPX_MAX_BANDS: usize = 17;
pub const CPL_CH: usize = 0;

pub const EXP_REUSE: u8 = 0;
pub const EXP_D45: u8 = 3;

pub const DBA_REUSE: u8 = 0;
pub const DBA_NEW: u8 = 1;
pub const DBA_NONE: u8 = 2;
pub const DBA_RESERVED: u8 = 3;

pub const CHMODE_DUALMONO: u8 = 0;
pub const CHMODE_MONO: u8 = 1;
pub const CHMODE_STEREO: u8 = 2;

pub const FRAME_TYPE_INDEPENDENT: u8 = 0;
pub const FRAME_TYPE_DEPENDENT: u8 = 1;
pub const FRAME_TYPE_AC3_CONVERT: u8 = 2;
pub const FRAME_TYPE_RESERVED: u8 = 3;

/// Possible frame sizes in 16-bit words, indexed by [frame_size_code][fscod].
pub const FRAME_SIZE_TAB: [[u16; 3]; 38] = [
    [64, 69, 96],
    [64, 70, 96],
    [80, 87, 120],
    [80, 88, 120],
    [96, 104, 144],
    [96, 105, 144],
    [112, 121, 168],
    [112, 122, 168],
    [128, 139, 192],
    [128, 140, 192],
    [160, 174, 240],
    [160, 175, 240],
    [192, 208, 288],
    [192, 209, 288],
    [224, 243, 336],
    [224, 244, 336],
    [256, 278, 384],
    [256, 279, 384],
    [320, 348, 480],
    [320, 349, 480],
    [384, 417, 576],
    [384, 418, 576],
    [448, 487, 672],
    [448, 488, 672],
    [512, 557, 768],
    [512, 558, 768],
    [640, 696, 960],
    [640, 697, 960],
    [768, 835, 1152],
    [768, 836, 1152],
    [896, 975, 1344],
    [896, 976, 1344],
    [1024, 1114, 1536],
    [1024, 1115, 1536],
    [1152, 1253, 1728],
    [1152, 1254, 1728],
    [1280, 1393, 1920],
    [1280, 1394, 1920],
];

/// Number of full-bandwidth channels per audio coding mode.
pub const CHANNELS_TAB: [u8; 8] = [2, 1, 2, 3, 3, 4, 4, 5];

/// Output order helper: `DEC_CHANNEL_MAP[acmod][lfe][i]` is the bitstream
/// channel stored at output slot `i`.
pub const DEC_CHANNEL_MAP: [[[u8; 6]; 2]; 8] = [
    [[0, 1, 0, 0, 0, 0], [0, 1, 2, 0, 0, 0]],
    [[0, 0, 0, 0, 0, 0], [0, 1, 0, 0, 0, 0]],
    [[0, 1, 0, 0, 0, 0], [0, 1, 2, 0, 0, 0]],
    [[0, 2, 1, 0, 0, 0], [0, 2, 1, 3, 0, 0]],
    [[0, 1, 2, 0, 0, 0], [0, 1, 3, 2, 0, 0]],
    [[0, 2, 1, 3, 0, 0], [0, 2, 1, 4, 3, 0]],
    [[0, 1, 2, 3, 0, 0], [0, 1, 4, 2, 3, 0]],
    [[0, 2, 1, 3, 4, 0], [0, 2, 1, 5, 3, 4]],
];

pub const SAMPLE_RATE_TAB: [u32; 3] = [48000, 44100, 32000];

pub const BITRATE_TAB: [u16; 19] = [
    32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384, 448, 512, 576, 640,
];

/// Rematrixing band boundaries.
pub const REMATRIX_BAND_TAB: [u8; 5] = [13, 25, 37, 61, 253];

/// Default coupling band structure for E-AC-3 (Table E2.16).
pub const EAC3_DEFAULT_CPL_BAND_STRUCT: [u8; 18] =
    [0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 1, 1, 0, 1, 1, 1, 1, 1];

/// Default spectral extension band structure (Table E2.15).
pub const EAC3_DEFAULT_SPX_BAND_STRUCT: [u8; 17] =
    [0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 1, 0, 1, 0, 1, 0, 1];

pub const BAP_TAB: [u8; 64] = [
    0, 1, 1, 1, 1, 1, 2, 2, 3, 3, 3, 4, 4, 5, 5, 6, 6, 6, 6, 7, 7, 7, 7, 8, 8, 8, 8, 9, 9, 9, 9,
    10, 10, 10, 10, 11, 11, 11, 11, 12, 12, 12, 12, 13, 13, 13, 13, 14, 14, 14, 14, 14, 14, 14, 14,
    15, 15, 15, 15, 15, 15, 15, 15, 15,
];

pub const HEBAP_TAB: [u8; 64] = [
    0, 1, 2, 3, 4, 5, 6, 7, 8, 8, 8, 8, 9, 9, 9, 10, 10, 10, 10, 11, 11, 11, 11, 12, 12, 12, 12,
    13, 13, 13, 13, 14, 14, 14, 14, 15, 15, 15, 15, 16, 16, 16, 16, 17, 17, 17, 17, 18, 18, 18, 18,
    18, 18, 18, 18, 19, 19, 19, 19, 19, 19, 19, 19, 19,
];

pub const SLOW_DECAY_TAB: [i32; 4] = [0x0f, 0x11, 0x13, 0x15];
pub const FAST_DECAY_TAB: [i32; 4] = [0x3f, 0x53, 0x67, 0x7b];
pub const SLOW_GAIN_TAB: [i32; 4] = [0x540, 0x4d8, 0x478, 0x410];
pub const DB_PER_BIT_TAB: [i32; 4] = [0x000, 0x700, 0x900, 0xb00];
pub const FLOOR_TAB: [i32; 8] = [0x2f0, 0x2b0, 0x270, 0x230, 0x1f0, 0x170, 0x0f0, -2048];
pub const FAST_GAIN_TAB: [i32; 8] = [0x080, 0x100, 0x180, 0x200, 0x280, 0x300, 0x380, 0x400];

/// Starting frequency coefficient bin for each critical band.
pub const BAND_START_TAB: [u8; 51] = [
    0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25,
    26, 27, 28, 31, 34, 37, 40, 43, 46, 49, 55, 61, 67, 73, 79, 85, 97, 109, 121, 133, 157, 181,
    205, 229, 253,
];

/// Maps each frequency coefficient bin to the critical band that contains it.
pub const BIN_TO_BAND_TAB: [u8; 253] = {
    let mut t = [0u8; 253];
    let mut band = 0usize;
    while band < 50 {
        let mut bin = BAND_START_TAB[band] as usize;
        while bin < BAND_START_TAB[band + 1] as usize {
            t[bin] = band as u8;
            bin += 1;
        }
        band += 1;
    }
    t
};

/// Quantization table: levels for symmetric, bits for asymmetric (bap -> value).
pub const QUANTIZATION_TAB: [u8; 16] = [0, 3, 5, 7, 11, 15, 5, 6, 7, 8, 9, 10, 11, 12, 14, 16];

const fn symmetric_dequant(code: i32, levels: i32) -> i32 {
    ((code - (levels >> 1)) * (1 << 24)) / levels
}

/// 3 values in 5 bits (bap=1 mantissas and GAQ gains).
pub const UNGROUP_3_IN_5_BITS_TAB: [[u8; 3]; 32] = {
    let mut t = [[0u8; 3]; 32];
    let mut i = 0;
    while i < 32 {
        t[i][0] = (i / 9) as u8;
        t[i][1] = ((i % 9) / 3) as u8;
        t[i][2] = ((i % 9) % 3) as u8;
        i += 1;
    }
    t
};

/// 3 values in 7 bits (exponent groups and bap=2 mantissas).
pub const UNGROUP_3_IN_7_BITS_TAB: [[u8; 3]; 128] = {
    let mut t = [[0u8; 3]; 128];
    let mut i = 0;
    while i < 128 {
        t[i][0] = (i / 25) as u8;
        t[i][1] = ((i % 25) / 5) as u8;
        t[i][2] = ((i % 25) % 5) as u8;
        i += 1;
    }
    t
};

pub const B1_MANTISSAS: [[i32; 3]; 32] = {
    let mut t = [[0i32; 3]; 32];
    let mut i = 0;
    while i < 32 {
        let mut k = 0;
        while k < 3 {
            t[i][k] = symmetric_dequant(UNGROUP_3_IN_5_BITS_TAB[i][k] as i32, 3);
            k += 1;
        }
        i += 1;
    }
    t
};

pub const B2_MANTISSAS: [[i32; 3]; 128] = {
    let mut t = [[0i32; 3]; 128];
    let mut i = 0;
    while i < 128 {
        let mut k = 0;
        while k < 3 {
            t[i][k] = symmetric_dequant(UNGROUP_3_IN_7_BITS_TAB[i][k] as i32, 5);
            k += 1;
        }
        i += 1;
    }
    t
};

pub const B3_MANTISSAS: [i32; 8] = {
    let mut t = [0i32; 8];
    let mut i = 0;
    while i < 7 {
        t[i] = symmetric_dequant(i as i32, 7);
        i += 1;
    }
    t
};

pub const B4_MANTISSAS: [[i32; 2]; 128] = {
    let mut t = [[0i32; 2]; 128];
    let mut i = 0;
    while i < 128 {
        t[i][0] = symmetric_dequant(i as i32 / 11, 11);
        t[i][1] = symmetric_dequant(i as i32 % 11, 11);
        i += 1;
    }
    t
};

pub const B5_MANTISSAS: [i32; 16] = {
    let mut t = [0i32; 16];
    let mut i = 0;
    while i < 15 {
        t[i] = symmetric_dequant(i as i32, 15);
        i += 1;
    }
    t
};

/// Gain levels for mix level codes (used only by the metadata parser).
pub const CRC16_TAB: [u16; 256] = {
    let mut t = [0u16; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = (i as u16) << 8;
        let mut k = 0;
        while k < 8 {
            c = if c & 0x8000 != 0 {
                (c << 1) ^ 0x8005
            } else {
                c << 1
            };
            k += 1;
        }
        t[i] = c;
        i += 1;
    }
    t
};

/// Initial state of FFmpeg's `av_lfg_init(&state, 0)` additive lagged
/// Fibonacci generator (slots 0..8 are never seeded and stay zero there).
pub const LFG_SEED0_STATE: [u32; 64] = [
    0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000,
    0x3b376b5e, 0xf56be763, 0xcba2a7fc, 0x9751b2c7, 0x5b1fe69a, 0x860b05af, 0x2b2650ca, 0x92a97fb0,
    0x7315e8eb, 0x8b6287a3, 0x2ddbd8b3, 0x1d098ca2, 0x6a65b478, 0xc9b22db4, 0x9637fbcf, 0xd4427934,
    0xb390e2b5, 0x92a84d32, 0x2110b086, 0x25a9182f, 0x239295f1, 0xc6f846f7, 0x5c5b3a51, 0xe6a7ba34,
    0xd6597325, 0xa537e3df, 0xcb80e534, 0xeaecf2dc, 0x9bbb6bd1, 0x2ea8bd11, 0x8c06ce60, 0x6859af1d,
    0xf159f64e, 0xc213bd06, 0x2a5d7785, 0xe621dc54, 0xb8cb9a8e, 0xe6a491f8, 0x85d7df25, 0x917d9aee,
    0xb156c513, 0x5bd929dd, 0xf6a9dab1, 0xc3ec5f2a, 0x5215230d, 0xe2b677d7, 0x701e36e7, 0x5405e2c7,
    0xddcb3ae2, 0x95b866ea, 0x1eb13c9a, 0x346646c2, 0x8ba48526, 0xb325e9bf, 0x29f07d49, 0xc986cf38,
];

/// Custom channel map locations for dependent substreams (Table E2.5):
/// (single-channel flag, mask of FFmpeg channel bits).
pub const CUSTOM_CHANNEL_MAP_LOCATIONS: [(bool, u64); 16] = [
    (true, 1 << 0),                       // L
    (true, 1 << 2),                       // C
    (true, 1 << 1),                       // R
    (true, 1 << 9),                       // Ls
    (true, 1 << 10),                      // Rs
    (false, (1 << 6) | (1 << 7)),         // Lc/Rc pair
    (false, (1 << 4) | (1 << 5)),         // Lrs/Rrs pair
    (false, 1 << 8),                      // Cs
    (false, 1 << 11),                     // Ts
    (false, (1u64 << 33) | (1u64 << 34)), // Lsd/Rsd pair
    (false, (1u64 << 31) | (1u64 << 32)), // Lw/Rw pair
    (false, (1 << 12) | (1 << 14)),       // Lvh/Rvh pair
    (false, 1 << 13),                     // Cvh
    (false, (1 << 15) | (1 << 17)),       // Lvs/Rvs pair
    (false, 1u64 << 35),                  // LFE2
    (true, 1 << 3),                       // LFE
];
