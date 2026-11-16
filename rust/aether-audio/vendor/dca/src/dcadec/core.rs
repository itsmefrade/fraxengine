// SPDX-License-Identifier: LGPL-2.1-or-later
//
// DTS Coherent Acoustics core decoder — subband-domain decode. Ported from
// ffmpeg's dca_core.c (HEADER_CORE and HEADER_XXCH paths, plus the XBR and
// EXSS X96 extensions) + dcadsp.c kernels + dcaadpcm.h. Handles the primary channel set (the 5.1 lossy bed), the XXCH
// extension set a DTS-HD stream adds to it (the 7.1 rears), and a bare
// extension set coded in the same syntax without a frame header of its own
// (the DTS:X heights of a lossy carrier). XBR adds a residual to the base
// subbands, X96 supplies a second set of 64-band samples for the 96 kHz
// synthesis (`x96_subband`). Not handled: XCH and the core-embedded X96. The
// subband samples produced here are fed to the QMF synthesis in `synth.rs`.

use super::huffman::core_vlcs;
use super::tables::{
    ADPCM_VB, BIT_RATES, DMIXTABLE, HIGH_FREQ_VQ, INV_DMIXTABLE, JOINT_SCALE_FACTORS,
    LOSSLESS_QUANT, LOSSY_QUANT, QUANT_INDEX_GROUP_SIZE, QUANT_INDEX_SEL_NBITS, QUANT_LEVELS,
    SCALE_FACTOR_ADJ, SCALE_FACTOR_QUANT6, SCALE_FACTOR_QUANT7,
};
use super::xll::crc16_ccitt;
use crate::bitstream::BitReader;
use crate::parser::{AudioMode, FrameInfo};
use crate::types::BedChannel;
use std::ops::{Index, IndexMut};

pub(crate) const DCA_SUBBANDS: usize = 32;
/// `DCA_SUBBANDS_X96`: bands of the 64-band (96 kHz) filter bank.
pub(crate) const DCA_SUBBANDS_X96: usize = 64;
pub(crate) const DCA_SUBBAND_SAMPLES: usize = 8;
const DCA_ADPCM_COEFFS: usize = 4;
pub(crate) const DCA_LFE_HISTORY: usize = 8;
const DCA_ABITS_MAX: i32 = 26;
const DCA_CODE_BOOKS: usize = 10;
/// Channels the decoder holds at once: the primary set (at most five), an
/// XXCH set (at most two, as ffmpeg) and a bare extension set of up to four.
const DCA_CHANNELS: usize = 12;
/// `DCA_XXCH_CHANNELS_MAX`.
const DCA_XXCH_CHANNELS_MAX: usize = 2;
/// `DCA_CORE_CHANNELS_MAX`: bounds the XXCH downmix coefficient list.
const DCA_CORE_CHANNELS_MAX: usize = 6;
const DCA_SYNCWORD_XXCH: u32 = 0x4700_4A03;
const DCA_SYNCWORD_XBR: u32 = 0x655E_315E;
const DCA_SYNCWORD_X96: u32 = 0x1D95_F262;
/// `DCA_EXSS_CHSETS_MAX`.
const DCA_EXSS_CHSETS_MAX: usize = 4;
/// `DCA_EXSS_CHANNELS_MAX`.
const DCA_EXSS_CHANNELS_MAX: usize = 8;

/// Per-band state wide enough for the X96 bands. `[T; 64]` has no `Default`.
#[derive(Clone, Copy)]
struct Wide<T>([T; DCA_SUBBANDS_X96]);

impl<T: Copy + Default> Default for Wide<T> {
    fn default() -> Self {
        Wide([T::default(); DCA_SUBBANDS_X96])
    }
}

impl<T> Index<usize> for Wide<T> {
    type Output = T;
    #[inline(always)]
    fn index(&self, band: usize) -> &T {
        &self.0[band]
    }
}

impl<T> IndexMut<usize> for Wide<T> {
    #[inline(always)]
    fn index_mut(&mut self, band: usize) -> &mut T {
        &mut self.0[band]
    }
}

const BLOCK_CODE_NBITS: [u8; 7] = [7, 10, 12, 13, 15, 17, 19];

// DCA speaker enum indices (`enum DCASpeaker`); mask bit = 1 << index.
pub(crate) const DCA_SPEAKER_C: usize = 0;
pub(crate) const DCA_SPEAKER_L: usize = 1;
pub(crate) const DCA_SPEAKER_R: usize = 2;
pub(crate) const DCA_SPEAKER_LS: usize = 3;
pub(crate) const DCA_SPEAKER_RS: usize = 4;
pub(crate) const DCA_SPEAKER_LFE1: usize = 5;
pub(crate) const DCA_SPEAKER_CS: usize = 6;
pub(crate) const DCA_SPEAKER_LSS: usize = 9;
pub(crate) const DCA_SPEAKER_RSS: usize = 10;
pub(crate) const DCA_SPEAKER_COUNT: usize = 32;

/// `audio_mode_ch_mask` — speaker layout mask (excluding LFE) per audio mode.
fn audio_mode_ch_mask(mode: AudioMode) -> u32 {
    let c = 1 << DCA_SPEAKER_C;
    let l = 1 << DCA_SPEAKER_L;
    let r = 1 << DCA_SPEAKER_R;
    let ls = 1 << DCA_SPEAKER_LS;
    let rs = 1 << DCA_SPEAKER_RS;
    let cs = 1 << DCA_SPEAKER_CS;
    let stereo = l | r;
    match mode {
        AudioMode::Mono => c,
        AudioMode::MonoDual
        | AudioMode::Stereo
        | AudioMode::StereoSumDiff
        | AudioMode::StereoTotal => stereo,
        AudioMode::ThreeF => stereo | c,
        AudioMode::TwoF1R => stereo | cs,
        AudioMode::ThreeF1R => stereo | c | cs,
        AudioMode::TwoF2R => stereo | ls | rs,
        AudioMode::ThreeF2R => stereo | c | ls | rs,
    }
}

/// `prm_ch_to_spkr_map[mode][ch]` — DCA speaker for each primary channel.
fn prm_ch_to_spkr(mode: AudioMode, ch: usize) -> usize {
    use AudioMode::*;
    let row: &[usize] = match mode {
        Mono | MonoDual => &[DCA_SPEAKER_C],
        Stereo | StereoSumDiff | StereoTotal => &[DCA_SPEAKER_L, DCA_SPEAKER_R],
        ThreeF => &[DCA_SPEAKER_C, DCA_SPEAKER_L, DCA_SPEAKER_R],
        TwoF1R => &[DCA_SPEAKER_L, DCA_SPEAKER_R, DCA_SPEAKER_CS],
        ThreeF1R => &[DCA_SPEAKER_C, DCA_SPEAKER_L, DCA_SPEAKER_R, DCA_SPEAKER_CS],
        TwoF2R => &[DCA_SPEAKER_L, DCA_SPEAKER_R, DCA_SPEAKER_LS, DCA_SPEAKER_RS],
        ThreeF2R => &[
            DCA_SPEAKER_C,
            DCA_SPEAKER_L,
            DCA_SPEAKER_R,
            DCA_SPEAKER_LS,
            DCA_SPEAKER_RS,
        ],
    };
    row[ch]
}

// ───────────────────────── fixed-point helpers (dcamath.h) ─────────────────

#[inline(always)]
fn clip23(a: i32) -> i32 {
    let lo = -(1 << 23);
    let hi = (1 << 23) - 1;
    a.clamp(lo, hi)
}

#[inline(always)]
fn norm(a: i64, bits: u32) -> i32 {
    if bits > 0 {
        ((a + (1i64 << (bits - 1))) >> bits) as i32
    } else {
        a as i32
    }
}

#[inline(always)]
fn mul(a: i32, b: i32, bits: u32) -> i32 {
    norm(a as i64 * b as i64, bits)
}

sample_loop! {
    /// `samples = mul(samples, scale, 16)`.
    fn scale_samples(samples: &mut [i32], scale: i32) {
        for x in samples {
            *x = mul(*x, scale, 16);
        }
    }
}

sample_loop! {
    /// `dst -= mul(src, coeff, 15)`.
    fn sub_scaled(dst: &mut [i32], src: &[i32], coeff: i32) {
        for (d, &s) in dst.iter_mut().zip(src) {
            *d = d.wrapping_sub(mul(s, coeff, 15));
        }
    }
}

/// `ff_dcaadpcm_predict`.
#[inline(always)]
fn adpcm_predict(pred_vq_index: usize, input: &[i32]) -> i32 {
    let coeff = &ADPCM_VB[pred_vq_index];
    let mut pred = 0i64;
    for i in 0..DCA_ADPCM_COEFFS {
        pred += input[DCA_ADPCM_COEFFS - 1 - i] as i64 * coeff[i] as i64;
    }
    clip23(norm(pred, 13))
}

/// True when the `nbytes` of `data` at bit position `pos` (byte aligned)
/// carry a valid trailing CRC16 (`ff_dca_check_crc`).
fn crc_clean(data: &[u8], pos: usize, nbytes: usize) -> bool {
    if pos % 8 != 0 {
        return false;
    }
    data.get(pos / 8..pos / 8 + nbytes)
        .is_some_and(|bytes| crc16_ccitt(bytes) == 0)
}

/// Two distinct slots of the speaker-indexed output, borrowed apart.
fn two_slots(
    samples: &mut [Option<Vec<i32>>],
    src: usize,
    dst: usize,
) -> (Option<&[i32]>, Option<&mut Vec<i32>>) {
    debug_assert_ne!(src, dst);
    if src < dst {
        let (head, tail) = samples.split_at_mut(dst);
        (head[src].as_deref(), tail[0].as_mut())
    } else {
        let (head, tail) = samples.split_at_mut(src);
        (tail[0].as_deref(), head[dst].as_mut())
    }
}

/// Which channel set a coding header describes (`enum HeaderType`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Header {
    /// The primary set, from the core frame's own coding header.
    Core,
    /// An XXCH set (`HEADER_XXCH`): channels beyond the core's, with the
    /// downmix the encoder folded into the core.
    Xxch,
    /// A bare set coded in the core syntax without a frame header of its
    /// own: the DTS:X extension of a lossy carrier.
    Extension,
}

/// `ff_dca_core_dequantize`: `residual` adds to the samples (XBR) instead
/// of replacing them.
#[inline(always)]
fn dequantize(output: &mut [i32], input: &[i32], step_size: i32, scale: i32, residual: bool) {
    let mut step_scale = step_size as i64 * scale as i64;
    let mut shift = 0u32;
    if step_scale > (1 << 23) {
        shift = (63 - (step_scale >> 23).leading_zeros()) + 1; // av_log2(x)+1
        step_scale >>= shift;
    }
    // A step scale past 2^45 shifts by more than the 22 bits there are to
    // drop (ffmpeg's shift goes negative there): what is left moves back
    // into the scale. No change below that.
    let bits = 22u32.saturating_sub(shift);
    step_scale <<= shift.saturating_sub(22);
    if residual {
        for n in 0..output.len() {
            output[n] = output[n].wrapping_add(clip23(norm(input[n] as i64 * step_scale, bits)));
        }
    } else {
        for n in 0..output.len() {
            output[n] = clip23(norm(input[n] as i64 * step_scale, bits));
        }
    }
}

// ───────────────────────── decoder state ──────────────────────────────────

#[derive(Default)]
pub(crate) struct CoreDecoder {
    npcmblocks: usize,
    sample_rate: u32,
    // coding header
    nsubframes: usize,
    nchannels: usize,
    lfe_present: u8,
    bit_rate_lossless: bool,
    es_format: bool,
    predictor_history: bool,
    filter_perfect: bool,
    crc_present: bool,
    sync_ssf: bool,
    audio_mode: AudioMode,
    /// Channels of the primary set; extension sets are appended after it.
    nchannels_core: usize,
    /// First channel of the bare extension set (`nchannels` when none).
    extension_base: usize,

    // XXCH set of the frame (`parse_xxch_frame`); reset per core frame.
    xxch_present: bool,
    xxch_crc_present: bool,
    xxch_mask_nbits: usize,
    xxch_core_mask: u32,
    xxch_spkr_mask: u32,
    xxch_dmix_embedded: bool,
    xxch_dmix_scale_inv: i32,
    xxch_dmix_mask: [u32; DCA_XXCH_CHANNELS_MAX],
    xxch_dmix_coeff: [i32; DCA_XXCH_CHANNELS_MAX * DCA_CORE_CHANNELS_MAX],

    // X96 extension of the frame (`parse_x96_frame_exss`); reset per core frame.
    /// Whether this frame's X96 component decoded: `x96_subband` is valid.
    x96_present: bool,
    x96_rev_no: u32,
    x96_crc_present: bool,
    /// Channels `[0, x96_nchannels)` carry X96 samples.
    x96_nchannels: usize,
    x96_high_res: bool,
    x96_subband_start: usize,
    /// State of the generator for unallocated X96 subbands (`x96_rand`); it
    /// starts at 1 and is never reset, as in ffmpeg.
    x96_rand: u32,
    x96_rand_seeded: bool,

    nsubbands: [usize; DCA_CHANNELS],
    subband_vq_start: [usize; DCA_CHANNELS],
    joint_intensity_index: [usize; DCA_CHANNELS],
    transition_mode_sel: [usize; DCA_CHANNELS],
    scale_factor_sel: [usize; DCA_CHANNELS],
    bit_allocation_sel: [usize; DCA_CHANNELS],
    quant_index_sel: [[usize; DCA_CODE_BOOKS]; DCA_CHANNELS],
    scale_factor_adj: [[i32; DCA_CODE_BOOKS]; DCA_CHANNELS],
    joint_scale_sel: [usize; DCA_CHANNELS],

    nsubsubframes: [usize; 16],
    // The per-band arrays are 64 wide: the X96 header reuses them for its
    // own bands (as ffmpeg does).
    prediction_mode: [Wide<bool>; DCA_CHANNELS],
    prediction_vq_index: [Wide<usize>; DCA_CHANNELS],
    bit_allocation: [Wide<i32>; DCA_CHANNELS],
    transition_mode: Vec<[[i32; DCA_SUBBANDS]; DCA_CHANNELS]>, // [sf][ch][band]
    scale_factors: [[[i32; 2]; DCA_SUBBANDS]; DCA_CHANNELS],
    joint_scale_factors: [Wide<i32>; DCA_CHANNELS],

    // persistent sample buffers
    /// Subband samples, one row per channel and band (`[ch][band]`, see
    /// `band_range`): `DCA_ADPCM_COEFFS` history words followed by the
    /// frame's `npcmblocks` decoded samples. The history persists across
    /// frames for ADPCM.
    bands: Vec<i32>,
    /// Length of a row of `bands`.
    band_len: usize,
    /// The X96 counterpart of `bands`: 64 rows per channel (`x96_band_range`).
    x96_bands: Vec<i32>,
    lfe_samples: Vec<i32>, // DCA_LFE_HISTORY + npcmblocks/2
}

#[derive(Debug)]
pub(crate) enum CoreError {
    Bitstream,
    Invalid(&'static str),
    Unsupported(&'static str),
}

type R<T> = Result<T, CoreError>;

#[inline]
fn rb(gb: &mut BitReader, n: usize) -> R<u32> {
    gb.read_bits(n).ok_or(CoreError::Bitstream)
}
#[inline]
fn rb1(gb: &mut BitReader) -> R<bool> {
    gb.read_bit().ok_or(CoreError::Bitstream)
}
#[inline]
fn rsb(gb: &mut BitReader, n: usize) -> R<i32> {
    gb.read_signed_bits(n).ok_or(CoreError::Bitstream)
}

impl CoreDecoder {
    pub(crate) fn reset(&mut self) {
        self.bands.fill(0);
        self.x96_bands.fill(0);
        self.lfe_samples.iter_mut().for_each(|x| *x = 0);
    }

    /// Where the row of channel `ch`, band `band` lies in `bands`.
    #[inline(always)]
    fn band_range(&self, ch: usize, band: usize) -> std::ops::Range<usize> {
        let start = (ch * DCA_SUBBANDS + band) * self.band_len;
        start..start + self.band_len
    }

    /// The rows of `band` in two distinct channels: `src` to read, `dst`
    /// to write.
    #[inline(always)]
    fn band_pair(&mut self, src_ch: usize, dst_ch: usize, band: usize) -> (&[i32], &mut [i32]) {
        debug_assert_ne!(src_ch, dst_ch);
        let (src, dst) = (self.band_range(src_ch, band), self.band_range(dst_ch, band));
        split_rows(&mut self.bands, src, dst)
    }

    fn alloc_buffers(&mut self) {
        let band_len = DCA_ADPCM_COEFFS + self.npcmblocks;
        if self.band_len != band_len || self.bands.len() != DCA_CHANNELS * DCA_SUBBANDS * band_len {
            self.bands = vec![0i32; DCA_CHANNELS * DCA_SUBBANDS * band_len];
            self.band_len = band_len;
        }
        let lfe_len = DCA_LFE_HISTORY + self.npcmblocks / 2;
        if self.lfe_samples.len() != lfe_len {
            self.lfe_samples = vec![0i32; lfe_len];
        }
        if !self.predictor_history {
            for row in self.bands.chunks_exact_mut(band_len) {
                row[..DCA_ADPCM_COEFFS].fill(0);
            }
        }
    }

    /// Decode one core access unit (header already validated by the caller).
    pub(crate) fn decode_frame(&mut self, info: &FrameInfo, data: &[u8]) -> R<()> {
        self.npcmblocks = info.npcmblocks as usize;
        self.sample_rate = info.sample_rate;
        self.lfe_present = info.lfe_present;
        self.es_format = info.es_format;
        self.predictor_history = info.predictor_history;
        self.crc_present = info.crc_present;
        self.audio_mode = info.audio_mode;
        // Set from the header's rate code by skip_frame_header below.
        self.bit_rate_lossless = false;
        // A new access unit: any extension set is decoded after it.
        self.xxch_present = false;
        self.x96_present = false;
        self.x96_nchannels = 0;
        self.nchannels = 0;
        self.nchannels_core = 0;
        self.extension_base = 0;

        self.alloc_buffers();

        // Start parsing right after the 32-bit syncword; re-walk the header to
        // reach the coding header position deterministically by re-reading it.
        let mut gb = BitReader::new(data);
        self.skip_frame_header(&mut gb)?;

        self.parse_frame_data(&mut gb, data, Header::Core, 0)?;
        self.nchannels_core = self.nchannels;
        self.extension_base = self.nchannels;
        Ok(())
    }

    /// Advance `gb` past the core frame header to the coding header. Mirrors the
    /// field order of ff_dca_parse_core_frame_header.
    fn skip_frame_header(&mut self, gb: &mut BitReader) -> R<()> {
        rb(gb, 32)?; // sync
        rb1(gb)?; // normal_frame
        rb(gb, 5)?; // deficit
        let crc_present = rb1(gb)?;
        rb(gb, 7)?; // npcmblocks
        rb(gb, 14)?; // frame_size
        rb(gb, 6)?; // audio_mode
        rb(gb, 4)?; // sr_code
        let br_code = rb(gb, 5)?;
        // The lossless quantiser is selected by the rate the code names, not
        // by the code: BIT_RATES[31] is the "lossless" sentinel 3, while code 3
        // is 96 kb/s (ffmpeg dca_core.c: `s->bit_rate == 3`).
        self.bit_rate_lossless = BIT_RATES[br_code as usize] == 3;
        rb1(gb)?; // reserved
        rb1(gb)?; // drc
        let _ts = rb1(gb)?;
        let _aux = rb1(gb)?;
        rb1(gb)?; // hdcd
        rb(gb, 3)?; // ext_audio_type
        rb1(gb)?; // ext_audio_present
        self.sync_ssf = rb1(gb)?; // sync_ssf
        rb(gb, 2)?; // lfe
        rb1(gb)?; // predictor_history
        if crc_present {
            rb(gb, 16)?;
        }
        self.filter_perfect = rb1(gb)?; // filter_perfect
        rb(gb, 4)?; // encoder_rev
        rb(gb, 2)?; // copy_hist
        rb(gb, 3)?; // pcmr
        rb1(gb)?; // sumdiff_front
        rb1(gb)?; // sumdiff_surround
        rb(gb, 4)?; // dn_code
        self.crc_present = crc_present;
        Ok(())
    }

    fn parse_coding_header(
        &mut self,
        gb: &mut BitReader,
        data: &[u8],
        header: Header,
        xch_base: usize,
    ) -> R<()> {
        let header_pos = gb.position();
        let mut header_size = 0usize;
        match header {
            Header::Core => {
                self.nsubframes = rb(gb, 4)? as usize + 1;
                self.nchannels = rb(gb, 3)? as usize + 1;
                let expect = self.audio_mode.channel_count();
                if self.nchannels != expect {
                    return Err(CoreError::Invalid("nchannels mismatch"));
                }
            }
            Header::Xxch => {
                header_size = rb(gb, 7)? as usize + 1;
                let nchannels = rb(gb, 3)? as usize + 1;
                if nchannels > DCA_XXCH_CHANNELS_MAX {
                    return Err(CoreError::Unsupported("xxch channel count"));
                }
                self.nchannels = xch_base + nchannels;
                let mask = rb(gb, self.xxch_mask_nbits - DCA_SPEAKER_CS)? << DCA_SPEAKER_CS;
                if mask.count_ones() as usize != nchannels {
                    return Err(CoreError::Invalid("xxch speaker layout mask"));
                }
                if self.xxch_core_mask & mask != 0 {
                    return Err(CoreError::Invalid("xxch speaker mask overlaps core"));
                }
                self.xxch_spkr_mask = mask;
                if rb1(gb)? {
                    self.xxch_dmix_embedded = rb1(gb)?;
                    // `get_bits(6) * 4 - FF_DCA_DMIXTABLE_OFFSET - 3`, with the
                    // offset the difference of the two table sizes (41).
                    let index =
                        rb(gb, 6)? as i64 * 4 - (DMIXTABLE.len() - INV_DMIXTABLE.len()) as i64 - 3;
                    if !(0..INV_DMIXTABLE.len() as i64).contains(&index) {
                        return Err(CoreError::Invalid("xxch downmix scale index"));
                    }
                    self.xxch_dmix_scale_inv = INV_DMIXTABLE[index as usize] as i32;
                    for ch in 0..nchannels {
                        let mask = rb(gb, self.xxch_mask_nbits)?;
                        if mask & self.xxch_core_mask != mask {
                            return Err(CoreError::Invalid("xxch downmix mapping mask"));
                        }
                        self.xxch_dmix_mask[ch] = mask;
                    }
                    let mut n = 0usize;
                    for ch in 0..nchannels {
                        for bit in 0..self.xxch_mask_nbits {
                            if self.xxch_dmix_mask[ch] & (1 << bit) == 0 {
                                continue;
                            }
                            let code = rb(gb, 7)? as i32;
                            let sign = (code >> 6) - 1;
                            let code = code & 63;
                            let coeff = if code != 0 {
                                let index = code as usize * 4 - 3;
                                if index >= DMIXTABLE.len() {
                                    return Err(CoreError::Invalid(
                                        "xxch downmix coefficient index",
                                    ));
                                }
                                (DMIXTABLE[index] as i32 ^ sign) - sign
                            } else {
                                0
                            };
                            if n >= self.xxch_dmix_coeff.len() {
                                return Err(CoreError::Invalid("xxch downmix coefficient count"));
                            }
                            self.xxch_dmix_coeff[n] = coeff;
                            n += 1;
                        }
                    }
                } else {
                    self.xxch_dmix_embedded = false;
                }
            }
            Header::Extension => {
                // The bare set's header: its byte size, three bits observed
                // zero, the common fields, padding, then a CRC16 over the
                // whole header. The channel count is the caller's (the
                // wrapper states it), not the header's.
                header_size = rb(gb, 8)? as usize + 1;
                if !crc_clean(data, header_pos, header_size) {
                    return Err(CoreError::Invalid("extension set header crc"));
                }
                if rb(gb, 3)? != 0 {
                    return Err(CoreError::Unsupported("extension set header flags"));
                }
            }
        }

        if header == Header::Extension {
            // The bare set writes its per-band sections over all 32 subbands
            // (prediction flags, scale factors) and codes the bands from the
            // first five-bit field on by VQ; the second five-bit field's
            // meaning is not established (27, 15, 15, 15 across the corpus)
            // and nothing here depends on it.
            for ch in xch_base..self.nchannels {
                self.subband_vq_start[ch] = rb(gb, 5)? as usize + 1;
                self.nsubbands[ch] = DCA_SUBBANDS;
            }
            for _ in xch_base..self.nchannels {
                rb(gb, 5)?;
            }
        } else {
            for ch in xch_base..self.nchannels {
                let n = rb(gb, 5)? as usize + 2;
                if n > DCA_SUBBANDS {
                    return Err(CoreError::Invalid("subband activity"));
                }
                self.nsubbands[ch] = n;
            }
            for ch in xch_base..self.nchannels {
                self.subband_vq_start[ch] = rb(gb, 5)? as usize + 1;
            }
        }
        for ch in xch_base..self.nchannels {
            let mut n = rb(gb, 3)? as usize;
            match header {
                Header::Core => {}
                // An XXCH set's joint index counts from its own first
                // channel (`n += xch_base - 1` in ffmpeg).
                Header::Xxch if n != 0 => n += xch_base - 1,
                Header::Xxch => {}
                // The bare set carries this field (3, 0, 0, 0 across the
                // corpus) but no joint codebook select after it: whatever
                // it means, its channels are not joint-coded.
                Header::Extension => n = 0,
            }
            if n > self.nchannels {
                return Err(CoreError::Invalid("joint intensity"));
            }
            self.joint_intensity_index[ch] = n;
        }
        for ch in xch_base..self.nchannels {
            self.transition_mode_sel[ch] = rb(gb, 2)? as usize;
        }
        for ch in xch_base..self.nchannels {
            let sel = rb(gb, 3)? as usize;
            if sel == 7 {
                return Err(CoreError::Invalid("scale factor codebook"));
            }
            self.scale_factor_sel[ch] = sel;
        }
        for ch in xch_base..self.nchannels {
            let sel = rb(gb, 3)? as usize;
            if sel == 7 {
                return Err(CoreError::Invalid("bit allocation select"));
            }
            self.bit_allocation_sel[ch] = sel;
        }
        for n in 0..DCA_CODE_BOOKS {
            for ch in xch_base..self.nchannels {
                self.quant_index_sel[ch][n] = rb(gb, QUANT_INDEX_SEL_NBITS[n] as usize)? as usize;
            }
        }
        for n in 0..DCA_CODE_BOOKS {
            for ch in xch_base..self.nchannels {
                if self.quant_index_sel[ch][n] < QUANT_INDEX_GROUP_SIZE[n] as usize {
                    self.scale_factor_adj[ch][n] = SCALE_FACTOR_ADJ[rb(gb, 2)? as usize] as i32;
                }
            }
        }
        match header {
            Header::Core => {
                if self.crc_present {
                    rb(gb, 16)?;
                }
            }
            // Reserved bits, byte alignment and the header CRC16.
            Header::Xxch | Header::Extension => {
                let end = header_pos + header_size * 8;
                if gb.position() > end || !gb.seek(end) {
                    return Err(CoreError::Invalid("channel set header size"));
                }
            }
        }
        Ok(())
    }

    fn parse_scale(&self, gb: &mut BitReader, scale_index: &mut i32, sel: usize) -> R<i32> {
        let (table, size): (&[u32], usize) = if sel > 5 {
            (&SCALE_FACTOR_QUANT7, SCALE_FACTOR_QUANT7.len())
        } else {
            (&SCALE_FACTOR_QUANT6, SCALE_FACTOR_QUANT6.len())
        };
        if sel < 5 {
            *scale_index += core_vlcs().scale_factor[sel]
                .get(gb)
                .ok_or(CoreError::Bitstream)?;
        } else {
            *scale_index = rb(gb, sel + 1)? as i32;
        }
        if (*scale_index as usize) >= size {
            return Err(CoreError::Invalid("scale factor index"));
        }
        Ok(table[*scale_index as usize] as i32)
    }

    fn parse_joint_scale(&self, gb: &mut BitReader, sel: usize) -> R<i32> {
        let mut scale_index = if sel < 5 {
            core_vlcs().scale_factor[sel]
                .get(gb)
                .ok_or(CoreError::Bitstream)?
        } else {
            rb(gb, sel + 1)? as i32
        };
        scale_index += 64;
        if (scale_index as usize) >= JOINT_SCALE_FACTORS.len() {
            return Err(CoreError::Invalid("joint scale index"));
        }
        Ok(JOINT_SCALE_FACTORS[scale_index as usize] as i32)
    }

    fn parse_subframe_header(
        &mut self,
        gb: &mut BitReader,
        sf: usize,
        header: Header,
        xch_base: usize,
    ) -> R<()> {
        // The subsubframe layout is the primary set's; extension sets share it.
        if header == Header::Core {
            self.nsubsubframes[sf] = rb(gb, 2)? as usize + 1;
            rb(gb, 3)?; // partial subsubframe sample count
        }

        for ch in xch_base..self.nchannels {
            for band in 0..self.nsubbands[ch] {
                self.prediction_mode[ch][band] = rb1(gb)?;
            }
        }
        for ch in xch_base..self.nchannels {
            for band in 0..self.nsubbands[ch] {
                if self.prediction_mode[ch][band] {
                    self.prediction_vq_index[ch][band] = rb(gb, 12)? as usize;
                }
            }
        }
        // Bit allocation index
        for ch in xch_base..self.nchannels {
            let sel = self.bit_allocation_sel[ch];
            for band in 0..self.subband_vq_start[ch] {
                let abits = if sel < 5 {
                    core_vlcs().bit_allocation[sel]
                        .get(gb)
                        .ok_or(CoreError::Bitstream)?
                } else {
                    rb(gb, sel - 1)? as i32
                };
                if abits > DCA_ABITS_MAX {
                    return Err(CoreError::Invalid("bit allocation index"));
                }
                self.bit_allocation[ch][band] = abits;
            }
        }
        // Transition mode
        for ch in xch_base..self.nchannels {
            self.transition_mode[sf][ch] = [0; DCA_SUBBANDS];
            if self.nsubsubframes[sf] > 1 {
                let sel = self.transition_mode_sel[ch];
                for band in 0..self.subband_vq_start[ch] {
                    if self.bit_allocation[ch][band] != 0 {
                        self.transition_mode[sf][ch][band] = core_vlcs().transition_mode[sel]
                            .get(gb)
                            .ok_or(CoreError::Bitstream)?;
                    }
                }
            }
        }
        // Scale factors
        for ch in xch_base..self.nchannels {
            let sel = self.scale_factor_sel[ch];
            let mut scale_index = 0i32;
            for band in 0..self.subband_vq_start[ch] {
                if self.bit_allocation[ch][band] != 0 {
                    let s = self.parse_scale(gb, &mut scale_index, sel)?;
                    self.scale_factors[ch][band][0] = s;
                    if self.transition_mode[sf][ch][band] != 0 {
                        let s2 = self.parse_scale(gb, &mut scale_index, sel)?;
                        self.scale_factors[ch][band][1] = s2;
                    }
                } else {
                    self.scale_factors[ch][band][0] = 0;
                }
            }
            for band in self.subband_vq_start[ch]..self.nsubbands[ch] {
                let s = self.parse_scale(gb, &mut scale_index, sel)?;
                self.scale_factors[ch][band][0] = s;
            }
        }
        // Joint subband codebook select
        for ch in xch_base..self.nchannels {
            if self.joint_intensity_index[ch] != 0 {
                let sel = rb(gb, 3)? as usize;
                if sel == 7 {
                    return Err(CoreError::Invalid("joint scale codebook"));
                }
                self.joint_scale_sel[ch] = sel;
            }
        }
        // Scale factors for joint subband coding
        for ch in xch_base..self.nchannels {
            let src_ch = self.joint_intensity_index[ch] as i32 - 1;
            if src_ch >= 0 {
                let src_ch = src_ch as usize;
                let sel = self.joint_scale_sel[ch];
                for band in self.nsubbands[ch]..self.nsubbands[src_ch] {
                    self.joint_scale_factors[ch][band] = self.parse_joint_scale(gb, sel)?;
                }
            }
        }
        // Dynamic range coefficient (drc_present) — drc flag not retained; the
        // header parser skipped it. The core path here assumes drc absent.
        if self.crc_present {
            rb(gb, 16)?;
        }
        Ok(())
    }

    /// `extract_audio` — returns (huffman_used, samples[8]).
    #[inline(always)]
    fn extract_audio(&self, gb: &mut BitReader, abits: i32, ch: usize) -> R<(bool, [i32; 8])> {
        let mut audio = [0i32; 8];
        if abits == 0 {
            return Ok((false, audio));
        }
        if abits as usize <= DCA_CODE_BOOKS {
            let sel = self.quant_index_sel[ch][abits as usize - 1];
            if sel < QUANT_INDEX_GROUP_SIZE[abits as usize - 1] as usize {
                let vlc = &core_vlcs().quant_index[abits as usize - 1][sel];
                for a in &mut audio {
                    *a = vlc.get(gb).ok_or(CoreError::Bitstream)?;
                }
                return Ok((true, audio));
            }
            if abits <= 7 {
                self.parse_block_codes(gb, &mut audio, abits)?;
                return Ok((false, audio));
            }
        }
        // No further encoding: abits-3 signed bits each.
        for a in &mut audio {
            *a = rsb(gb, abits as usize - 3)?;
        }
        Ok((false, audio))
    }

    #[inline(always)]
    fn parse_block_codes(&self, gb: &mut BitReader, audio: &mut [i32; 8], abits: i32) -> R<()> {
        let nbits = BLOCK_CODE_NBITS[abits as usize - 1] as usize;
        let code1 = rb(gb, nbits)?;
        let code2 = rb(gb, nbits)?;
        if !decode_blockcodes(code1, code2, abits as usize, audio) {
            return Err(CoreError::Invalid("block code"));
        }
        Ok(())
    }

    #[inline(always)]
    fn parse_subframe_audio(
        &mut self,
        gb: &mut BitReader,
        sf: usize,
        header: Header,
        xch_base: usize,
        sub_pos: &mut usize,
        lfe_pos: &mut usize,
    ) -> R<()> {
        let nsamples = self.nsubsubframes[sf] * DCA_SUBBAND_SAMPLES;
        if *sub_pos + nsamples > self.npcmblocks {
            return Err(CoreError::Invalid("subband overflow"));
        }

        // VQ encoded subbands
        for ch in xch_base..self.nchannels {
            let mut vq_index = [0i32; DCA_SUBBANDS];
            for band in self.subband_vq_start[ch]..self.nsubbands[ch] {
                vq_index[band] = rb(gb, 10)? as i32;
            }
            if self.subband_vq_start[ch] < self.nsubbands[ch] {
                self.decode_hf(ch, &vq_index, *sub_pos, nsamples);
            }
        }

        // LFE: the primary set's only.
        if self.lfe_present != 0 && header == Header::Core {
            let nlfesamples = 2 * self.lfe_present as usize * self.nsubsubframes[sf];
            let mut audio = [0i32; 16];
            for a in audio.iter_mut().take(nlfesamples) {
                *a = rsb(gb, 8)?;
            }
            let index = rb(gb, 8)? as usize;
            if index >= SCALE_FACTOR_QUANT7.len() {
                return Err(CoreError::Invalid("lfe scale"));
            }
            let mut scale = SCALE_FACTOR_QUANT7[index] as i32;
            scale = mul(4_697_620, scale, 23); // 0.035 * (1<<27)
            let mut ofs = *lfe_pos;
            for &a in audio.iter().take(nlfesamples) {
                self.lfe_samples[ofs] = clip23(((a as i64 * scale as i64) >> 4) as i32);
                ofs += 1;
            }
            *lfe_pos = ofs;
        }

        // Audio data
        let mut ofs = *sub_pos;
        for ssf in 0..self.nsubsubframes[sf] {
            for ch in xch_base..self.nchannels {
                for band in 0..self.subband_vq_start[ch] {
                    let abits = self.bit_allocation[ch][band];
                    let (huff, audio) = self.extract_audio(gb, abits, ch)?;
                    let step_size = if self.bit_rate_lossless {
                        LOSSLESS_QUANT[abits as usize] as i32
                    } else {
                        LOSSY_QUANT[abits as usize] as i32
                    };
                    let trans_ssf = self.transition_mode[sf][ch][band];
                    let mut scale = if trans_ssf == 0 || (ssf as i32) < trans_ssf {
                        self.scale_factors[ch][band][0]
                    } else {
                        self.scale_factors[ch][band][1]
                    };
                    if huff {
                        let adj = self.scale_factor_adj[ch][abits as usize - 1] as i64;
                        scale = clip23((adj * scale as i64 >> 22) as i32);
                    }
                    let base = DCA_ADPCM_COEFFS + ofs;
                    let row = self.band_range(ch, band);
                    let buf = &mut self.bands[row][base..base + DCA_SUBBAND_SAMPLES];
                    dequantize(buf, &audio, step_size, scale, false);
                }
            }
            // DSYNC
            if (ssf == self.nsubsubframes[sf] - 1 || self.sync_ssf) && rb(gb, 16)? != 0xffff {
                return Err(CoreError::Invalid("dsync"));
            }
            ofs += DCA_SUBBAND_SAMPLES;
        }

        // Inverse ADPCM
        for ch in xch_base..self.nchannels {
            self.inverse_adpcm(ch, *sub_pos, nsamples);
        }

        // Joint subband coding
        for ch in xch_base..self.nchannels {
            let src_ch = self.joint_intensity_index[ch] as i32 - 1;
            if src_ch >= 0 {
                self.decode_joint(ch, src_ch as usize, *sub_pos, nsamples);
            }
        }

        *sub_pos = ofs;
        Ok(())
    }

    #[inline(always)]
    fn decode_hf(&mut self, ch: usize, vq_index: &[i32], ofs: usize, len: usize) {
        for i in self.subband_vq_start[ch]..self.nsubbands[ch] {
            let coeff = &HIGH_FREQ_VQ[vq_index[i] as usize];
            let scale = self.scale_factors[ch][i][0];
            let base = DCA_ADPCM_COEFFS + ofs;
            let row = self.band_range(ch, i);
            let buf = &mut self.bands[row][base..base + len];
            for (x, &c) in buf.iter_mut().zip(&coeff[..len]) {
                *x = clip23(((c as i32 * scale) + (1 << 3)) >> 4);
            }
        }
    }

    #[inline(always)]
    fn inverse_adpcm(&mut self, ch: usize, sub_pos: usize, len: usize) {
        for band in 0..self.nsubbands[ch] {
            if self.prediction_mode[ch][band] {
                let pred_id = self.prediction_vq_index[ch][band];
                let row = self.band_range(ch, band);
                adpcm_row(&mut self.bands[row], pred_id, sub_pos, len);
            }
        }
    }

    #[inline(always)]
    fn decode_joint(&mut self, ch: usize, src_ch: usize, ofs: usize, len: usize) {
        for band in self.nsubbands[ch]..self.nsubbands[src_ch] {
            let scale = self.joint_scale_factors[ch][band];
            let base = DCA_ADPCM_COEFFS + ofs;
            let (src, dst) = self.band_pair(src_ch, ch, band);
            for (d, &s) in dst[base..base + len].iter_mut().zip(&src[base..base + len]) {
                *d = clip23(mul(s, scale, 17));
            }
        }
    }

    /// Parse one channel set — the primary set from the core's coding
    /// header, or an extension set whose channels start at `xch_base` — and
    /// decode its subband samples for the frame.
    fn parse_frame_data(
        &mut self,
        gb: &mut BitReader,
        data: &[u8],
        header: Header,
        xch_base: usize,
    ) -> R<()> {
        #[cfg(target_arch = "x86_64")]
        if crate::cpu::has_avx512() {
            // SAFETY: `has_avx512` covers the features the function is
            // compiled for.
            return unsafe { self.parse_frame_data_avx512(gb, data, header, xch_base) };
        }
        #[cfg(target_arch = "x86_64")]
        if crate::cpu::has_avx2() {
            // SAFETY: `has_avx2` covers the features the function is
            // compiled for.
            return unsafe { self.parse_frame_data_avx2(gb, data, header, xch_base) };
        }
        self.parse_frame_data_body(gb, data, header, xch_base)
    }

    /// `parse_frame_data_body` compiled for AVX-512: the AVX2 build's
    /// operations on sixteen lanes, with the 64-bit shifts and products the
    /// AVX2 instruction set has to spell out.
    #[cfg(target_arch = "x86_64")]
    #[target_feature(enable = "avx2,bmi1,bmi2,lzcnt,avx512f,avx512bw,avx512dq,avx512vl")]
    fn parse_frame_data_avx512(
        &mut self,
        gb: &mut BitReader,
        data: &[u8],
        header: Header,
        xch_base: usize,
    ) -> R<()> {
        self.parse_frame_data_body(gb, data, header, xch_base)
    }

    /// `parse_frame_data_body` compiled for AVX2: the same code, whose
    /// dequantization, vector-quantized bands and joint bands then use the
    /// 24-bit clips and 64-bit products the baseline target lacks.
    #[cfg(target_arch = "x86_64")]
    #[target_feature(enable = "avx2,bmi1,bmi2,lzcnt")]
    fn parse_frame_data_avx2(
        &mut self,
        gb: &mut BitReader,
        data: &[u8],
        header: Header,
        xch_base: usize,
    ) -> R<()> {
        self.parse_frame_data_body(gb, data, header, xch_base)
    }

    #[inline(always)]
    fn parse_frame_data_body(
        &mut self,
        gb: &mut BitReader,
        data: &[u8],
        header: Header,
        xch_base: usize,
    ) -> R<()> {
        self.parse_coding_header(gb, data, header, xch_base)?;
        // transition_mode needs nsubframes slots; kept across frames.
        if self.transition_mode.len() != self.nsubframes {
            self.transition_mode
                .resize(self.nsubframes, [[0i32; DCA_SUBBANDS]; DCA_CHANNELS]);
        }

        let mut sub_pos = 0usize;
        let mut lfe_pos = DCA_LFE_HISTORY;
        for sf in 0..self.nsubframes {
            self.parse_subframe_header(gb, sf, header, xch_base)?;
            self.parse_subframe_audio(gb, sf, header, xch_base, &mut sub_pos, &mut lfe_pos)?;
        }

        // Update ADPCM history & clear inactive subbands.
        for ch in xch_base..self.nchannels {
            let mut nsubbands = self.nsubbands[ch];
            if self.joint_intensity_index[ch] != 0 {
                nsubbands = nsubbands.max(self.nsubbands[self.joint_intensity_index[ch] - 1]);
            }
            for band in 0..nsubbands {
                let row = self.band_range(ch, band);
                let buf = &mut self.bands[row];
                // history (first 4) = last 4 decoded samples
                buf.copy_within(self.npcmblocks..self.npcmblocks + DCA_ADPCM_COEFFS, 0);
            }
            let inactive = self.band_range(ch, nsubbands).start..self.band_range(ch + 1, 0).start;
            self.bands[inactive].fill(0);
        }
        Ok(())
    }

    /// Decode an XXCH frame (`parse_xxch_frame`): the extra channels a DTS-HD
    /// stream carries beyond the core's 5.1, from the EXSS XXCH component
    /// bytes (`data` starts at the 0x47004A03 syncword). Appends them after
    /// the primary channels; must follow `decode_frame` of the same core
    /// access unit, whose subframe layout the set shares. On error the
    /// decoder falls back to the primary set alone.
    pub(crate) fn decode_xxch(&mut self, data: &[u8]) -> R<()> {
        let result = self.parse_xxch_frame(data);
        if result.is_err() {
            self.drop_extension_sets();
        }
        result
    }

    fn parse_xxch_frame(&mut self, data: &[u8]) -> R<()> {
        let mut gb = BitReader::new(data);
        if rb(&mut gb, 32)? != DCA_SYNCWORD_XXCH {
            return Err(CoreError::Invalid("xxch sync"));
        }
        let header_size = rb(&mut gb, 6)? as usize + 1;
        self.xxch_crc_present = rb1(&mut gb)?;
        self.xxch_mask_nbits = rb(&mut gb, 5)? as usize + 1;
        if self.xxch_mask_nbits <= DCA_SPEAKER_CS {
            return Err(CoreError::Invalid("xxch speaker mask width"));
        }
        if rb(&mut gb, 2)? != 0 {
            return Err(CoreError::Unsupported("xxch channel sets"));
        }
        let frame_size = rb(&mut gb, 14)? as usize + 1;
        self.xxch_core_mask = rb(&mut gb, self.xxch_mask_nbits)?;

        // The set's view of the core must agree with the core, allowing the
        // side-surround naming of the core's surrounds.
        let mut mask = self.core_ch_mask();
        if mask & (1 << DCA_SPEAKER_LS) != 0 && self.xxch_core_mask & (1 << DCA_SPEAKER_LSS) != 0 {
            mask = (mask & !(1 << DCA_SPEAKER_LS)) | (1 << DCA_SPEAKER_LSS);
        }
        if mask & (1 << DCA_SPEAKER_RS) != 0 && self.xxch_core_mask & (1 << DCA_SPEAKER_RSS) != 0 {
            mask = (mask & !(1 << DCA_SPEAKER_RS)) | (1 << DCA_SPEAKER_RSS);
        }
        if mask != self.xxch_core_mask {
            return Err(CoreError::Invalid("xxch core speaker mask"));
        }
        if !gb.seek(header_size * 8) {
            return Err(CoreError::Invalid("xxch frame header size"));
        }
        if header_size + frame_size > data.len() {
            return Err(CoreError::Invalid("xxch channel set size"));
        }
        self.parse_frame_data(&mut gb, data, Header::Xxch, self.nchannels_core)?;
        self.xxch_present = true;
        // AETHER: the XXCH channels are speaker channels, not a bare extension set.
        self.extension_base = self.nchannels;
        Ok(())
    }

    /// Decode a bare channel set of `nchannels` channels coded in the core
    /// syntax without a frame header of its own (the DTS:X extension of a
    /// lossy carrier), appended after the sets already decoded for this
    /// frame. Returns the bytes consumed up to the end of the last DSYNC
    /// marker. On error the set is dropped.
    pub(crate) fn decode_extension_set(&mut self, data: &[u8], nchannels: usize) -> R<usize> {
        let base = self.nchannels;
        if nchannels == 0 || base + nchannels > DCA_CHANNELS {
            return Err(CoreError::Unsupported("extension set channel count"));
        }
        self.nchannels = base + nchannels;
        self.extension_base = base;
        let mut gb = BitReader::new(data);
        match self.parse_frame_data(&mut gb, data, Header::Extension, base) {
            Ok(()) => Ok(gb.position().div_ceil(8)),
            Err(e) => {
                self.nchannels = base;
                self.extension_base = base;
                let dropped =
                    self.band_range(base, 0).start..self.band_range(base + nchannels, 0).start;
                self.bands[dropped].fill(0);
                Err(e)
            }
        }
    }

    /// Forget every extension set of this frame: the primary set alone.
    fn drop_extension_sets(&mut self) {
        self.nchannels = self.nchannels_core;
        self.extension_base = self.nchannels_core;
        self.xxch_present = false;
    }

    /// Undo the downmix an XXCH encoder folded into the core's channels, in
    /// the PCM domain after synthesis (`ff_dca_core_filter_fixed`), on the
    /// speaker-indexed 24-bit output.
    pub(crate) fn undo_xxch_dmix(&self, samples: &mut [Option<Vec<i32>>]) {
        if !self.xxch_present || !self.xxch_dmix_embedded {
            return;
        }
        let scale_inv = self.xxch_dmix_scale_inv;
        for spkr in 0..self.xxch_mask_nbits {
            if self.xxch_core_mask & (1 << spkr) == 0 {
                continue;
            }
            if let Some(buf) = samples
                .get_mut(self.output_slot(spkr))
                .and_then(Option::as_mut)
            {
                scale_samples(buf, scale_inv);
            }
        }
        let mut coeff = self.xxch_dmix_coeff.iter();
        for ch in self.nchannels_core..self.nchannels {
            if ch >= self.extension_base {
                break;
            }
            let Some(src) = self.speaker_for(ch) else {
                break;
            };
            let mask = self.xxch_dmix_mask[ch - self.nchannels_core];
            for spkr in 0..self.xxch_mask_nbits {
                if mask & (1 << spkr) == 0 {
                    continue;
                }
                let Some(&c) = coeff.next() else {
                    return;
                };
                let c = mul(c, scale_inv, 16);
                let dst = self.output_slot(spkr);
                if c == 0 || dst == src {
                    continue;
                }
                let (src_buf, dst_buf) = two_slots(samples, src, dst);
                let (Some(src_buf), Some(dst_buf)) = (src_buf, dst_buf) else {
                    continue;
                };
                sub_scaled(dst_buf, src_buf, c);
            }
        }
    }

    /// The output slot a speaker of the XXCH masks lands in: the core's
    /// surrounds keep their Ls/Rs slots when the set names them Lss/Rss.
    fn output_slot(&self, spkr: usize) -> usize {
        let core = audio_mode_ch_mask(self.audio_mode);
        match spkr {
            DCA_SPEAKER_LSS if core & (1 << DCA_SPEAKER_LS) != 0 => DCA_SPEAKER_LS,
            DCA_SPEAKER_RSS if core & (1 << DCA_SPEAKER_RS) != 0 => DCA_SPEAKER_RS,
            s => s,
        }
    }

    /// The DCA speaker channel `ch` plays through: a primary channel's from
    /// the audio mode, an XXCH channel's from the set's layout mask; `None`
    /// for a bare extension set's channels, which have no speaker.
    pub(crate) fn speaker_for(&self, ch: usize) -> Option<usize> {
        if ch < self.nchannels_core {
            return Some(prm_ch_to_spkr(self.audio_mode, ch));
        }
        if ch >= self.extension_base {
            return None;
        }
        let mut index = ch - self.nchannels_core;
        for spkr in DCA_SPEAKER_CS..self.xxch_mask_nbits {
            if self.xxch_spkr_mask & (1 << spkr) != 0 {
                if index == 0 {
                    return Some(spkr);
                }
                index -= 1;
            }
        }
        None
    }

    /// Decoded samples for channel `ch`, band `band` (npcmblocks long).
    pub(crate) fn subband(&self, ch: usize, band: usize) -> &[i32] {
        &self.bands[self.band_range(ch, band)][DCA_ADPCM_COEFFS..]
    }

    pub(crate) fn lfe(&self) -> &[i32] {
        &self.lfe_samples
    }

    /// Shift the last `DCA_LFE_HISTORY` decimated LFE samples to the front,
    /// matching the post-synthesis history update in ff_dca_core_filter_fixed.
    pub(crate) fn shift_lfe_history(&mut self, nlfesamples: usize) {
        for n in 0..DCA_LFE_HISTORY {
            self.lfe_samples[n] = self.lfe_samples[nlfesamples + n];
        }
    }

    pub(crate) fn filter_perfect(&self) -> bool {
        self.filter_perfect
    }

    /// The primary set's speaker layout mask (incl. LFE), as `ff_dca_core`
    /// builds it before any extension.
    fn core_ch_mask(&self) -> u32 {
        let mut mask = audio_mode_ch_mask(self.audio_mode);
        if self.lfe_present != 0 {
            mask |= 1 << DCA_SPEAKER_LFE1;
        }
        mask
    }

    /// Speaker layout mask of the frame's output (incl. LFE): the primary
    /// set's, plus the XXCH set's speakers when one decoded. The core's
    /// surrounds keep their Ls/Rs slots (see `output_slot`).
    pub(crate) fn ch_mask(&self) -> u32 {
        let mut mask = self.core_ch_mask();
        if self.xxch_present {
            mask |= self.xxch_spkr_mask;
        }
        mask
    }

    /// Speaker layout mask of the frame's output as the carrier names it:
    /// [`Self::ch_mask`] with the core's surrounds under the side-surround
    /// names (Lss/Rss) when the XXCH set calls them that.
    pub(crate) fn coded_mask(&self) -> u32 {
        let mut mask = self.core_ch_mask();
        if self.xxch_present {
            if mask & (1 << DCA_SPEAKER_LS) != 0
                && self.xxch_core_mask & (1 << DCA_SPEAKER_LSS) != 0
            {
                mask = (mask & !(1 << DCA_SPEAKER_LS)) | (1 << DCA_SPEAKER_LSS);
            }
            if mask & (1 << DCA_SPEAKER_RS) != 0
                && self.xxch_core_mask & (1 << DCA_SPEAKER_RSS) != 0
            {
                mask = (mask & !(1 << DCA_SPEAKER_RS)) | (1 << DCA_SPEAKER_RSS);
            }
            mask |= self.xxch_spkr_mask;
        }
        mask
    }

    pub(crate) fn nchannels(&self) -> usize {
        self.nchannels
    }
    pub(crate) fn npcmblocks(&self) -> usize {
        self.npcmblocks
    }
    pub(crate) fn sample_rate(&self) -> u32 {
        self.sample_rate
    }
    pub(crate) fn lfe_present(&self) -> u8 {
        self.lfe_present
    }
}

/// The rows `src` and `dst` of one band buffer, borrowed apart: `src` to
/// read, `dst` to write.
#[inline(always)]
fn split_rows(
    bands: &mut [i32],
    src: std::ops::Range<usize>,
    dst: std::ops::Range<usize>,
) -> (&[i32], &mut [i32]) {
    if src.start < dst.start {
        let (head, tail) = bands.split_at_mut(dst.start);
        (&head[src], &mut tail[..dst.len()])
    } else {
        let (head, tail) = bands.split_at_mut(src.start);
        (&tail[..src.len()], &mut head[dst])
    }
}

/// `inverse_adpcm` for one band row: `row` holds `DCA_ADPCM_COEFFS` history
/// words, then the decoded samples; `sub_pos..sub_pos + len` are predicted.
#[inline(always)]
fn adpcm_row(row: &mut [i32], pred_id: usize, sub_pos: usize, len: usize) {
    for j in 0..len {
        let win = [
            row[sub_pos + j],
            row[sub_pos + j + 1],
            row[sub_pos + j + 2],
            row[sub_pos + j + 3],
        ];
        let x = adpcm_predict(pred_id, &win);
        let idx = DCA_ADPCM_COEFFS + sub_pos + j;
        row[idx] = clip23(row[idx] + x);
    }
}

// AETHER: the XBR and X96 extensions below are ported from ffmpeg's dca_core.c
// (parse_xbr_*, parse_x96_*), with its CRC checks (off unless asked for in
// ffmpeg) left out.
impl CoreDecoder {
    /// Where the row of X96 channel `ch`, band `band` lies in `x96_bands`.
    #[inline(always)]
    fn x96_band_range(&self, ch: usize, band: usize) -> std::ops::Range<usize> {
        let start = (ch * DCA_SUBBANDS_X96 + band) * self.band_len;
        start..start + self.band_len
    }

    /// Channels `[0, n)` that carry X96 samples this frame.
    pub(crate) fn x96_channels(&self) -> usize {
        if self.x96_present {
            self.x96_nchannels
        } else {
            0
        }
    }

    /// X96 samples of channel `ch`, band `band` (of 64; `npcmblocks` long).
    pub(crate) fn x96_subband(&self, ch: usize, band: usize) -> &[i32] {
        &self.x96_bands[self.x96_band_range(ch, band)][DCA_ADPCM_COEFFS..]
    }

    /// `alloc_x96_sample_buffer`.
    fn alloc_x96_buffers(&mut self) {
        let len = DCA_CHANNELS * DCA_SUBBANDS_X96 * self.band_len;
        if self.x96_bands.len() != len {
            self.x96_bands = vec![0i32; len];
        }
        if !self.predictor_history {
            for row in self.x96_bands.chunks_exact_mut(self.band_len) {
                row[..DCA_ADPCM_COEFFS].fill(0);
            }
        }
        if !self.x96_rand_seeded {
            self.x96_rand = 1;
            self.x96_rand_seeded = true;
        }
    }

    /// `rand_x96`: a pseudorandom integer in `[-2^30, 2^30 - 1]`.
    fn rand_x96(&mut self) -> i32 {
        self.x96_rand = self.x96_rand.wrapping_mul(1_103_515_245).wrapping_add(12_345);
        (self.x96_rand & 0x7fff_ffff) as i32 - 0x4000_0000
    }

    // ───────────────────────── XBR ────────────────────────────────────────

    /// Decode the EXSS XBR component (`parse_xbr_frame`; `data` starts at its
    /// syncword): a residual added to the base subbands of the frame's
    /// channels, after the primary and XXCH sets are decoded. On error the
    /// rows it reached keep what was added, as in ffmpeg.
    pub(crate) fn decode_xbr(&mut self, data: &[u8]) -> R<()> {
        let mut gb = BitReader::new(data);
        if rb(&mut gb, 32)? != DCA_SYNCWORD_XBR {
            return Err(CoreError::Invalid("xbr sync"));
        }
        let header_size = rb(&mut gb, 6)? as usize + 1;
        let nchsets = rb(&mut gb, 2)? as usize + 1;
        let mut frame_size = [0usize; DCA_EXSS_CHSETS_MAX];
        for size in frame_size.iter_mut().take(nchsets) {
            *size = rb(&mut gb, 14)? as usize + 1;
        }
        let transition_mode = rb1(&mut gb)?;
        let mut nchannels = [0usize; DCA_EXSS_CHSETS_MAX];
        let mut nsubbands = [0usize; DCA_EXSS_CHSETS_MAX * DCA_EXSS_CHANNELS_MAX];
        let mut ch2 = 0;
        for n in nchannels.iter_mut().take(nchsets) {
            *n = rb(&mut gb, 3)? as usize + 1;
            let band_nbits = rb(&mut gb, 2)? as usize + 5;
            for _ in 0..*n {
                let bands = rb(&mut gb, band_nbits)? as usize + 1;
                if bands > DCA_SUBBANDS {
                    return Err(CoreError::Invalid("xbr subband count"));
                }
                nsubbands[ch2] = bands;
                ch2 += 1;
            }
        }
        if !gb.seek(header_size * 8) {
            return Err(CoreError::Invalid("xbr frame header size"));
        }
        let mut base = 0;
        for i in 0..nchsets {
            let header_pos = gb.position();
            if base + nchannels[i] <= self.nchannels {
                let mut sub_pos = 0;
                for sf in 0..self.nsubframes {
                    self.parse_xbr_subframe(
                        &mut gb,
                        base,
                        base + nchannels[i],
                        &nsubbands,
                        transition_mode,
                        sf,
                        &mut sub_pos,
                    )?;
                }
            }
            base += nchannels[i];
            if !gb.seek(header_pos + frame_size[i] * 8) {
                return Err(CoreError::Invalid("xbr channel set size"));
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn parse_xbr_subframe(
        &mut self,
        gb: &mut BitReader,
        base: usize,
        end: usize,
        nsubbands: &[usize],
        transition_mode: bool,
        sf: usize,
        sub_pos: &mut usize,
    ) -> R<()> {
        if *sub_pos + self.nsubsubframes[sf] * DCA_SUBBAND_SAMPLES > self.npcmblocks {
            return Err(CoreError::Invalid("subband overflow"));
        }
        let mut nabits = [0usize; DCA_CHANNELS];
        let mut bit_allocation = [[0i32; DCA_SUBBANDS]; DCA_CHANNELS];
        let mut scale_nbits = [0usize; DCA_CHANNELS];
        let mut scale_factors = [[[0i32; 2]; DCA_SUBBANDS]; DCA_CHANNELS];

        for n in &mut nabits[base..end] {
            *n = rb(gb, 2)? as usize + 2;
        }
        for ch in base..end {
            for band in 0..nsubbands[ch] {
                let abits = rb(gb, nabits[ch])? as i32;
                if abits > DCA_ABITS_MAX {
                    return Err(CoreError::Invalid("xbr bit allocation"));
                }
                bit_allocation[ch][band] = abits;
            }
        }
        for n in &mut scale_nbits[base..end] {
            *n = rb(gb, 3)? as usize;
            if *n == 0 {
                return Err(CoreError::Invalid("xbr scale index width"));
            }
        }
        for ch in base..end {
            let table: &[u32] = if self.scale_factor_sel[ch] > 5 {
                &SCALE_FACTOR_QUANT7
            } else {
                &SCALE_FACTOR_QUANT6
            };
            for band in 0..nsubbands[ch] {
                if bit_allocation[ch][band] == 0 {
                    continue;
                }
                let index = rb(gb, scale_nbits[ch])? as usize;
                if index >= table.len() {
                    return Err(CoreError::Invalid("xbr scale index"));
                }
                scale_factors[ch][band][0] = table[index] as i32;
                if transition_mode && self.transition_mode[sf][ch][band] != 0 {
                    let index = rb(gb, scale_nbits[ch])? as usize;
                    if index >= table.len() {
                        return Err(CoreError::Invalid("xbr scale index"));
                    }
                    scale_factors[ch][band][1] = table[index] as i32;
                }
            }
        }

        let mut ofs = *sub_pos;
        for ssf in 0..self.nsubsubframes[sf] {
            for ch in base..end {
                for band in 0..nsubbands[ch] {
                    let abits = bit_allocation[ch][band];
                    let mut audio = [0i32; DCA_SUBBAND_SAMPLES];
                    if abits > 7 {
                        for a in &mut audio {
                            *a = rsb(gb, abits as usize - 3)?;
                        }
                    } else if abits > 0 {
                        self.parse_block_codes(gb, &mut audio, abits)?;
                    } else {
                        continue;
                    }
                    let step_size = LOSSLESS_QUANT[abits as usize] as i32;
                    let trans_ssf = if transition_mode {
                        self.transition_mode[sf][ch][band]
                    } else {
                        0
                    };
                    let scale = if trans_ssf == 0 || (ssf as i32) < trans_ssf {
                        scale_factors[ch][band][0]
                    } else {
                        scale_factors[ch][band][1]
                    };
                    let start = DCA_ADPCM_COEFFS + ofs;
                    let row = self.band_range(ch, band);
                    let buf = &mut self.bands[row][start..start + DCA_SUBBAND_SAMPLES];
                    dequantize(buf, &audio, step_size, scale, true);
                }
            }
            if (ssf == self.nsubsubframes[sf] - 1 || self.sync_ssf) && rb(gb, 16)? != 0xffff {
                return Err(CoreError::Invalid("xbr dsync"));
            }
            ofs += DCA_SUBBAND_SAMPLES;
        }
        *sub_pos = ofs;
        Ok(())
    }

    // ───────────────────────── X96 ────────────────────────────────────────

    /// Decode the EXSS X96 component (`parse_x96_frame_exss`; `data` starts
    /// at its syncword): the 64-band samples whose upper half, and the
    /// residual over the lower half, a 96 kHz synthesis adds to the base
    /// subbands. Valid through [`Self::x96_subband`] when it returns `Ok`.
    pub(crate) fn decode_x96_exss(&mut self, data: &[u8]) -> R<()> {
        self.x96_present = false;
        let result = self.parse_x96_frame_exss(data);
        self.x96_present = result.is_ok();
        result
    }

    fn parse_x96_frame_exss(&mut self, data: &[u8]) -> R<()> {
        let mut gb = BitReader::new(data);
        if rb(&mut gb, 32)? != DCA_SYNCWORD_X96 {
            return Err(CoreError::Invalid("x96 sync"));
        }
        let header_size = rb(&mut gb, 6)? as usize + 1;
        self.x96_rev_no = rb(&mut gb, 4)?;
        if !(1..=8).contains(&self.x96_rev_no) {
            return Err(CoreError::Invalid("x96 revision"));
        }
        self.x96_crc_present = rb1(&mut gb)?;
        let nchsets = rb(&mut gb, 2)? as usize + 1;
        let mut frame_size = [0usize; DCA_EXSS_CHSETS_MAX];
        for size in frame_size.iter_mut().take(nchsets) {
            *size = rb(&mut gb, 12)? as usize + 1;
        }
        let mut nchannels = [0usize; DCA_EXSS_CHSETS_MAX];
        for n in nchannels.iter_mut().take(nchsets) {
            *n = rb(&mut gb, 3)? as usize + 1;
        }
        if !gb.seek(header_size * 8) {
            return Err(CoreError::Invalid("x96 frame header size"));
        }
        self.alloc_x96_buffers();

        self.x96_nchannels = 0;
        let mut base = 0;
        for i in 0..nchsets {
            let header_pos = gb.position();
            if base + nchannels[i] <= self.nchannels {
                self.x96_nchannels = base + nchannels[i];
                self.parse_x96_frame_data(&mut gb, base)?;
            }
            base += nchannels[i];
            if !gb.seek(header_pos + frame_size[i] * 8) {
                return Err(CoreError::Invalid("x96 channel set size"));
            }
        }
        Ok(())
    }

    fn parse_x96_frame_data(&mut self, gb: &mut BitReader, base: usize) -> R<()> {
        self.parse_x96_coding_header(gb, base)?;
        let mut sub_pos = 0usize;
        for sf in 0..self.nsubframes {
            self.parse_x96_subframe_header(gb, base)?;
            self.parse_x96_subframe_audio(gb, sf, base, &mut sub_pos)?;
        }
        // Update ADPCM history and clear inactive subbands.
        for ch in base..self.x96_nchannels {
            let mut nsubbands = self.nsubbands[ch];
            if self.joint_intensity_index[ch] != 0 {
                nsubbands = nsubbands.max(self.nsubbands[self.joint_intensity_index[ch] - 1]);
            }
            for band in 0..DCA_SUBBANDS_X96 {
                let row = self.x96_band_range(ch, band);
                let buf = &mut self.x96_bands[row];
                if band >= self.x96_subband_start && band < nsubbands {
                    buf.copy_within(self.npcmblocks..self.npcmblocks + DCA_ADPCM_COEFFS, 0);
                } else {
                    buf.fill(0);
                }
            }
        }
        Ok(())
    }

    fn parse_x96_coding_header(&mut self, gb: &mut BitReader, base: usize) -> R<()> {
        let header_pos = gb.position();
        let header_size = rb(gb, 7)? as usize + 1;
        self.x96_high_res = rb1(gb)?;
        if self.x96_rev_no < 8 {
            self.x96_subband_start = rb(gb, 5)? as usize;
            if self.x96_subband_start > 27 {
                return Err(CoreError::Invalid("x96 subband start"));
            }
        } else {
            self.x96_subband_start = DCA_SUBBANDS;
        }
        for ch in base..self.x96_nchannels {
            let n = rb(gb, 6)? as usize + 1;
            if n < DCA_SUBBANDS {
                return Err(CoreError::Invalid("x96 subband activity"));
            }
            self.nsubbands[ch] = n;
        }
        for ch in base..self.x96_nchannels {
            let mut n = rb(gb, 3)? as usize;
            if n != 0 && base != 0 {
                n += base - 1;
            }
            if n > self.x96_nchannels {
                return Err(CoreError::Invalid("x96 joint intensity"));
            }
            self.joint_intensity_index[ch] = n;
        }
        for ch in base..self.x96_nchannels {
            let sel = rb(gb, 3)? as usize;
            if sel >= 6 {
                return Err(CoreError::Invalid("x96 scale factor codebook"));
            }
            self.scale_factor_sel[ch] = sel;
        }
        for ch in base..self.x96_nchannels {
            self.bit_allocation_sel[ch] = rb(gb, 3)? as usize;
        }
        for n in 0..6 + 4 * usize::from(self.x96_high_res) {
            for ch in base..self.x96_nchannels {
                self.quant_index_sel[ch][n] = rb(gb, QUANT_INDEX_SEL_NBITS[n] as usize)? as usize;
            }
        }
        // Reserved bits, byte alignment and the header CRC16.
        if !gb.seek(header_pos + header_size * 8) {
            return Err(CoreError::Invalid("x96 channel set header size"));
        }
        Ok(())
    }

    fn parse_x96_subframe_header(&mut self, gb: &mut BitReader, base: usize) -> R<()> {
        let start = self.x96_subband_start;
        let high_res = usize::from(self.x96_high_res);
        for ch in base..self.x96_nchannels {
            for band in start..self.nsubbands[ch] {
                self.prediction_mode[ch][band] = rb1(gb)?;
            }
        }
        for ch in base..self.x96_nchannels {
            for band in start..self.nsubbands[ch] {
                if self.prediction_mode[ch][band] {
                    self.prediction_vq_index[ch][band] = rb(gb, 12)? as usize;
                }
            }
        }
        // Bit allocation index
        for ch in base..self.x96_nchannels {
            let sel = self.bit_allocation_sel[ch];
            let mut abits = 0i32;
            for band in start..self.nsubbands[ch] {
                // A Huffman code carries the difference of abits.
                if sel < 7 {
                    abits += core_vlcs().quant_index[5 + 2 * high_res][sel]
                        .get(gb)
                        .ok_or(CoreError::Bitstream)?;
                } else {
                    abits = rb(gb, 3 + high_res)? as i32;
                }
                if abits < 0 || abits > 7 + 8 * high_res as i32 {
                    return Err(CoreError::Invalid("x96 bit allocation"));
                }
                self.bit_allocation[ch][band] = abits;
            }
        }
        // Scale factors, transmitted even for unallocated subbands.
        for ch in base..self.x96_nchannels {
            let sel = self.scale_factor_sel[ch];
            let mut scale_index = 0i32;
            for band in start..self.nsubbands[ch] {
                let s = self.parse_scale(gb, &mut scale_index, sel)?;
                self.scale_factors[ch][band >> 1][band & 1] = s;
            }
        }
        // Joint subband codebook select
        for ch in base..self.x96_nchannels {
            if self.joint_intensity_index[ch] != 0 {
                let sel = rb(gb, 3)? as usize;
                if sel == 7 {
                    return Err(CoreError::Invalid("x96 joint scale codebook"));
                }
                self.joint_scale_sel[ch] = sel;
            }
        }
        // Scale factors for joint subband coding
        for ch in base..self.x96_nchannels {
            let src_ch = self.joint_intensity_index[ch] as i32 - 1;
            if src_ch >= 0 {
                let src_ch = src_ch as usize;
                let sel = self.joint_scale_sel[ch];
                for band in self.nsubbands[ch]..self.nsubbands[src_ch] {
                    self.joint_scale_factors[ch][band] = self.parse_joint_scale(gb, sel)?;
                }
            }
        }
        // Side information CRC check word
        if self.crc_present {
            rb(gb, 16)?;
        }
        Ok(())
    }

    fn parse_x96_subframe_audio(
        &mut self,
        gb: &mut BitReader,
        sf: usize,
        base: usize,
        sub_pos: &mut usize,
    ) -> R<()> {
        let nsamples = self.nsubsubframes[sf] * DCA_SUBBAND_SAMPLES;
        if *sub_pos + nsamples > self.npcmblocks {
            return Err(CoreError::Invalid("subband overflow"));
        }
        let start = self.x96_subband_start;
        let first = DCA_ADPCM_COEFFS + *sub_pos;

        // VQ encoded or unallocated subbands
        for ch in base..self.x96_nchannels {
            for band in start..self.nsubbands[ch] {
                let scale = self.scale_factors[ch][band >> 1][band & 1];
                let row = self.x96_band_range(ch, band);
                match self.bit_allocation[ch][band] {
                    0 => {
                        if scale <= 1 {
                            self.x96_bands[row][first..first + nsamples].fill(0);
                        } else {
                            for n in 0..nsamples {
                                let r = self.rand_x96();
                                self.x96_bands[row.clone()][first + n] = mul(r, scale, 31);
                            }
                        }
                    }
                    1 => {
                        for ssf in 0..self.nsubsubframes[sf].div_ceil(2) {
                            let vq = &HIGH_FREQ_VQ[rb(gb, 10)? as usize];
                            let buf = &mut self.x96_bands[row.clone()];
                            for n in 0..(nsamples - ssf * 16).min(16) {
                                buf[first + ssf * 16 + n] = clip23(
                                    ((vq[n] as i32).wrapping_mul(scale).wrapping_add(1 << 3)) >> 4,
                                );
                            }
                        }
                    }
                    _ => {}
                }
            }
        }

        // Audio data
        let mut ofs = *sub_pos;
        for ssf in 0..self.nsubsubframes[sf] {
            for ch in base..self.x96_nchannels {
                for band in start..self.nsubbands[ch] {
                    let abits = self.bit_allocation[ch][band] - 1;
                    // Not VQ encoded or unallocated
                    if abits < 1 {
                        continue;
                    }
                    let (_, audio) = self.extract_audio(gb, abits, ch)?;
                    let step_size = if self.bit_rate_lossless {
                        LOSSLESS_QUANT[abits as usize] as i32
                    } else {
                        LOSSY_QUANT[abits as usize] as i32
                    };
                    let scale = self.scale_factors[ch][band >> 1][band & 1];
                    let at = DCA_ADPCM_COEFFS + ofs;
                    let row = self.x96_band_range(ch, band);
                    let buf = &mut self.x96_bands[row][at..at + DCA_SUBBAND_SAMPLES];
                    dequantize(buf, &audio, step_size, scale, false);
                }
            }
            if (ssf == self.nsubsubframes[sf] - 1 || self.sync_ssf) && rb(gb, 16)? != 0xffff {
                return Err(CoreError::Invalid("x96 dsync"));
            }
            ofs += DCA_SUBBAND_SAMPLES;
        }

        // Inverse ADPCM
        for ch in base..self.x96_nchannels {
            for band in start..self.nsubbands[ch] {
                if self.prediction_mode[ch][band] {
                    let pred_id = self.prediction_vq_index[ch][band];
                    let row = self.x96_band_range(ch, band);
                    adpcm_row(&mut self.x96_bands[row], pred_id, *sub_pos, nsamples);
                }
            }
        }

        // Joint subband coding
        for ch in base..self.x96_nchannels {
            let src_ch = self.joint_intensity_index[ch] as i32 - 1;
            if src_ch >= 0 {
                let src_ch = src_ch as usize;
                for band in self.nsubbands[ch]..self.nsubbands[src_ch] {
                    let scale = self.joint_scale_factors[ch][band];
                    let (src_rows, dst_rows) = (
                        self.x96_band_range(src_ch, band),
                        self.x96_band_range(ch, band),
                    );
                    let (src, dst) = split_rows(&mut self.x96_bands, src_rows, dst_rows);
                    for (d, &s) in dst[first..first + nsamples]
                        .iter_mut()
                        .zip(&src[first..first + nsamples])
                    {
                        *d = clip23(mul(s, scale, 17));
                    }
                }
            }
        }

        *sub_pos = ofs;
        Ok(())
    }
}

/// Bed-channel label for each primary channel, in DCA decode order
/// (`prm_ch_to_spkr_map`). The renderer places beds by label, so this order
/// (not WAV order) is what the synthesized `fullband_channels` follow.
pub(crate) fn primary_bed_layout(mode: AudioMode) -> &'static [BedChannel] {
    use crate::types::BedChannel::*;
    match mode {
        AudioMode::Mono => &[Center],
        AudioMode::MonoDual
        | AudioMode::Stereo
        | AudioMode::StereoSumDiff
        | AudioMode::StereoTotal => &[FrontLeft, FrontRight],
        AudioMode::ThreeF => &[Center, FrontLeft, FrontRight],
        AudioMode::TwoF1R => &[FrontLeft, FrontRight, RearCenter],
        AudioMode::ThreeF1R => &[Center, FrontLeft, FrontRight, RearCenter],
        AudioMode::TwoF2R => &[FrontLeft, FrontRight, SurroundLeft, SurroundRight],
        AudioMode::ThreeF2R => &[Center, FrontLeft, FrontRight, SurroundLeft, SurroundRight],
    }
}

/// What splits a block code of bit allocation `abits` (1 to 7) in two.
///
/// A code packs four samples as base-`levels` digits. Peeling them off one
/// division at a time chains four divisions; instead one multiplication
/// splits the code into two two-digit halves, each then read from a table.
#[derive(Clone, Copy)]
struct BlockCode {
    /// `levels * levels`: a half is below this.
    square: u32,
    /// `2^32 / square + 1`: `(code * reciprocal) >> 32` is `code / square`
    /// for every code of at most 19 bits.
    reciprocal: u64,
    /// Where the halves of this allocation start in `BLOCK_CODE_PAIRS`.
    first: usize,
}

const BLOCK_CODE_PAIR_COUNT: usize = 9 + 25 + 49 + 81 + 169 + 289 + 625;

const fn block_codes() -> ([BlockCode; 7], [[i8; 2]; BLOCK_CODE_PAIR_COUNT]) {
    let mut codes = [BlockCode {
        square: 0,
        reciprocal: 0,
        first: 0,
    }; 7];
    let mut pairs = [[0i8; 2]; BLOCK_CODE_PAIR_COUNT];
    let mut first = 0;
    let mut i = 0;
    while i < 7 {
        let levels = QUANT_LEVELS[i + 1];
        let square = levels * levels;
        codes[i] = BlockCode {
            square,
            reciprocal: (1u64 << 32) / square as u64 + 1,
            first,
        };
        let offset = (levels as i32 - 1) / 2;
        let mut half = 0;
        while half < square {
            pairs[first + half as usize] = [
                ((half % levels) as i32 - offset) as i8,
                ((half / levels) as i32 - offset) as i8,
            ];
            half += 1;
        }
        first += square as usize;
        i += 1;
    }
    assert!(first == BLOCK_CODE_PAIR_COUNT);
    (codes, pairs)
}

static BLOCK_CODES: [BlockCode; 7] = block_codes().0;
/// For each bit allocation, the two samples of every half code.
static BLOCK_CODE_PAIRS: [[i8; 2]; BLOCK_CODE_PAIR_COUNT] = block_codes().1;

/// `decode_blockcodes`: the eight samples of two block codes. False when a
/// code holds more than its four digits.
#[inline(always)]
fn decode_blockcodes(code1: u32, code2: u32, abits: usize, audio: &mut [i32; 8]) -> bool {
    let BlockCode {
        square,
        reciprocal,
        first,
    } = BLOCK_CODES[abits - 1];
    let pairs = &BLOCK_CODE_PAIRS[first..first + square as usize];
    let high1 = ((code1 as u64 * reciprocal) >> 32) as u32;
    let high2 = ((code2 as u64 * reciprocal) >> 32) as u32;
    if high1 >= square || high2 >= square {
        return false;
    }
    let halves = [code1 - high1 * square, high1, code2 - high2 * square, high2];
    for (samples, half) in audio.chunks_exact_mut(2).zip(halves) {
        let pair = pairs[half as usize];
        samples[0] = pair[0] as i32;
        samples[1] = pair[1] as i32;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ffmpeg's `decode_blockcodes`: one division per sample.
    fn reference(mut code1: i32, mut code2: i32, levels: i32, audio: &mut [i32; 8]) -> i32 {
        let offset = (levels - 1) / 2;
        for n in 0..DCA_SUBBAND_SAMPLES / 2 {
            let div = code1 / levels;
            audio[n] = code1 - div * levels - offset;
            code1 = div;
        }
        for n in DCA_SUBBAND_SAMPLES / 2..DCA_SUBBAND_SAMPLES {
            let div = code2 / levels;
            audio[n] = code2 - div * levels - offset;
            code2 = div;
        }
        code1 | code2
    }

    /// Every code of every allocation, in both positions, decodes to the
    /// samples the division chain gives and is refused when it is.
    #[test]
    fn block_codes_match_the_division_chain() {
        for abits in 1..=7usize {
            let levels = QUANT_LEVELS[abits] as i32;
            let nbits = BLOCK_CODE_NBITS[abits - 1];
            for code in 0..1u32 << nbits {
                let other = (code * 7 + 3) % (levels * levels * levels * levels) as u32;
                for (code1, code2) in [(code, other), (other, code)] {
                    let mut expected = [0i32; 8];
                    let valid = reference(code1 as i32, code2 as i32, levels, &mut expected) == 0;
                    let mut audio = [0i32; 8];
                    assert_eq!(
                        decode_blockcodes(code1, code2, abits, &mut audio),
                        valid,
                        "abits {abits}, codes {code1} {code2}"
                    );
                    if valid {
                        assert_eq!(audio, expected, "abits {abits}, codes {code1} {code2}");
                    }
                }
            }
        }
    }

    /// The largest step size times the largest scale factor needs a shift
    /// past 22 bits; the samples still come out scaled, then clipped,
    /// instead of an overflowed shift.
    #[test]
    fn dequantize_past_22_bits_of_shift_clips() {
        let step_size = LOSSY_QUANT[1] as i32;
        let scale = (1 << 23) - 1;
        let mut output = [7i32; 4];
        dequantize(&mut output, &[1, -1, 0, 3], step_size, scale, false);
        assert_eq!(output, [(1 << 23) - 1, -(1 << 23), 0, (1 << 23) - 1]);
    }

    /// A core frame that reaches that shift (found by the `core_frame` fuzz
    /// target): it must decode or fail, not panic.
    #[test]
    fn core_frame_with_an_oversized_step_scale_does_not_panic() {
        const FRAME: [u8; 124] = [
            0x7f, 0xfe, 0x80, 0x01, 0xfc, 0x3c, 0x1a, 0xb0, 0xb5, 0x20, 0x01, 0x38, 0x00, 0x03,
            0xef, 0x7f, 0xe0, 0x06, 0xc9, 0x1e, 0x00, 0x12, 0x49, 0xf7, 0x03, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x72, 0x55, 0x7e, 0x3f, 0xff,
            0xc0, 0xe7, 0x8c, 0x72, 0x55, 0x7e, 0x3f, 0xff, 0xc0, 0x72, 0x55, 0x7e, 0x3f, 0xff,
            0xc0, 0x11, 0x67, 0x5c, 0x69, 0xa0, 0xe7, 0x99, 0x66, 0x48, 0xbd, 0xd6, 0x72, 0x7e,
            0x3f, 0xff, 0x72, 0x55, 0x7e, 0x3f, 0xff, 0xc0, 0xff, 0x7e, 0x3f, 0xff, 0x72, 0x55,
            0x7e, 0x3f, 0xff, 0xc0, 0xff, 0xc0, 0xa0, 0xe7, 0x99, 0x66, 0x48, 0xbd, 0xd6, 0x72,
            0x7e, 0x3f, 0xff, 0x72, 0x55, 0x7e, 0x3f, 0xff, 0xc0, 0xff, 0xc0, 0x66, 0x48, 0xbd,
            0xd6, 0x72, 0x3f, 0xff, 0x72, 0x55, 0x7e, 0x3f, 0xff, 0xc0, 0xff, 0xc0,
        ];
        let _ = crate::PcmDecoder::new().push_access_unit(&FRAME);
    }

    /// A decoder holding one primary channel of two subsubframes in one
    /// subframe, ready for an extension component to be decoded over it.
    fn extension_fixture() -> CoreDecoder {
        let mut dec = CoreDecoder::default();
        dec.npcmblocks = 16;
        dec.nsubframes = 1;
        dec.nchannels = 1;
        dec.nchannels_core = 1;
        dec.nsubsubframes[0] = 2;
        dec.predictor_history = true;
        dec.transition_mode = vec![[[0; DCA_SUBBANDS]; DCA_CHANNELS]];
        dec.alloc_buffers();
        dec
    }

    /// Eight samples as two block codes of four base-3 digits (`abits` 1).
    fn block_codes_3(w: &mut crate::bitstream_writer::BitWriter, digits: [u32; 8]) {
        for half in digits.chunks(4) {
            w.write(7, half[0] + 3 * half[1] + 9 * half[2] + 27 * half[3]);
        }
    }

    /// The XBR component adds its residual to the base subbands of the
    /// frame: block-coded samples, a transition scale switch, DSYNC.
    #[test]
    fn xbr_residual_is_added_to_the_base_subbands() {
        use crate::bitstream_writer::BitWriter;
        let mut dec = extension_fixture();
        dec.transition_mode[0][0][0] = 1; // band 0 changes scale at ssf 1
        for ch_band in [0usize, 1] {
            let row = dec.band_range(0, ch_band);
            for (n, x) in dec.bands[row][DCA_ADPCM_COEFFS..].iter_mut().enumerate() {
                *x = 100 * (n as i32 + 1);
            }
        }
        let before = dec.bands.clone();

        let mut set = BitWriter::new();
        set.write(2, 0); // bits per bit allocation index: 2
        set.write(2, 1); // band 0: 1 bit allocated
        set.write(2, 0); // band 1: none
        set.write(3, 3); // bits per scale index
        set.write(3, 7); // band 0 scale index, ssf 0
        set.write(3, 6); // band 0 scale index after the transition
        block_codes_3(&mut set, [2, 0, 1, 2, 1, 1, 0, 2]);
        block_codes_3(&mut set, [0, 2, 2, 1, 2, 0, 1, 1]);
        set.write(16, 0xffff); // DSYNC
        let set = set.finish();

        let mut w = BitWriter::new();
        w.write(32, DCA_SYNCWORD_XBR);
        w.write(6, 8); // header size 9 bytes
        w.write(2, 0); // one channel set
        w.write(14, set.len() as u32 - 1);
        w.write(1, 1); // transition mode
        w.write(3, 0); // one channel
        w.write(2, 0); // 5 bits for the subband count
        w.write(5, 1); // two subbands
        let mut data = w.finish();
        data.resize(9, 0);
        data.extend_from_slice(&set);

        dec.decode_xbr(&data).expect("xbr");
        let samples = [
            [1, -1, 0, 1, 0, 0, -1, 1].map(|d| d * 7),
            [-1, 1, 1, 0, 1, -1, 0, 0].map(|d| d * 6),
        ]
        .concat();
        let row = dec.band_range(0, 0);
        for (n, &residual) in samples.iter().enumerate() {
            let at = DCA_ADPCM_COEFFS + n;
            assert_eq!(dec.bands[row.clone()][at], before[row.clone()][at] + residual);
        }
        // Everything else is untouched: the unallocated band, the history.
        let mut expected = before.clone();
        for (n, &residual) in samples.iter().enumerate() {
            expected[row.clone()][DCA_ADPCM_COEFFS + n] += residual;
        }
        assert_eq!(dec.bands, expected);

        let mut bad = data.clone();
        bad[0] ^= 1;
        assert!(dec.decode_xbr(&bad).is_err());
    }

    /// The X96 component of one channel: a random-filled band, a block-coded
    /// band, everything below untouched; the generator carries over.
    #[test]
    fn x96_component_fills_the_high_bands() {
        use crate::bitstream_writer::BitWriter;
        let mut dec = extension_fixture();

        let mut set = BitWriter::new();
        set.write(7, 4); // header size 5 bytes
        set.write(1, 0); // standard resolution
        set.write(6, 33); // 34 subbands (revision 8 codes none below 32)
        set.write(3, 0); // no joint coding
        set.write(3, 5); // scale factor codebook: 6-bit indices
        set.write(3, 7); // bit allocation codebook: 3-bit values
        for n in 0..6 {
            set.write(QUANT_INDEX_SEL_NBITS[n] as usize, 3); // no Huffman: block codes
        }
        let mut header = set.finish();
        header.resize(5, 0);
        let mut set = BitWriter::new();
        set.write(1, 0); // band 32: no prediction
        set.write(1, 0); // band 33: no prediction
        set.write(3, 0); // band 32: nothing allocated
        set.write(3, 2); // band 33: abits 1
        set.write(6, 10); // band 32 scale
        set.write(6, 20); // band 33 scale
        block_codes_3(&mut set, [2, 0, 1, 2, 1, 1, 0, 2]);
        block_codes_3(&mut set, [0, 2, 2, 1, 2, 0, 1, 1]);
        set.write(16, 0xffff);
        let body = set.finish();
        let size = header.len() + body.len();

        let mut w = BitWriter::new();
        w.write(32, DCA_SYNCWORD_X96);
        w.write(6, 7); // header size 8 bytes
        w.write(4, 8); // revision
        w.write(1, 0);
        w.write(2, 0); // one channel set
        w.write(12, size as u32 - 1);
        w.write(3, 0); // one channel
        let mut data = w.finish();
        data.resize(8, 0);
        data.extend_from_slice(&header);
        data.extend_from_slice(&body);

        // The generator, as ffmpeg seeds and steps it.
        let mut seed = 1u32;
        let mut random = move || {
            seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            (seed & 0x7fff_ffff) as i32 - 0x4000_0000
        };
        let scale = SCALE_FACTOR_QUANT6[10] as i32;
        let noise: Vec<i32> = (0..16).map(|_| mul(random(), scale, 31)).collect();
        let step_scale = LOSSY_QUANT[1] as i64 * SCALE_FACTOR_QUANT6[20] as i64;
        let shift = (63 - (step_scale >> 23).leading_zeros()) + 1;
        let step_scale = step_scale >> shift;
        let digits = [
            [2i64, 0, 1, 2, 1, 1, 0, 2],
            [0, 2, 2, 1, 2, 0, 1, 1],
        ]
        .concat();
        let coded: Vec<i32> = digits
            .iter()
            .map(|&d| clip23(norm((d - 1) * step_scale, 22 - shift)))
            .collect();

        dec.decode_x96_exss(&data).expect("x96");
        assert_eq!(dec.x96_channels(), 1);
        assert_eq!(dec.x96_subband(0, 32), noise);
        assert_eq!(dec.x96_subband(0, 33), coded);
        for band in (0..32).chain(34..64) {
            assert!(dec.x96_subband(0, band).iter().all(|&x| x == 0), "band {band}");
        }

        // The next frame continues the sequence.
        dec.decode_x96_exss(&data).expect("x96 again");
        let more: Vec<i32> = (0..16).map(|_| mul(random(), scale, 31)).collect();
        assert_eq!(dec.x96_subband(0, 32), more);
        assert_eq!(dec.x96_subband(0, 33), coded);

        // A truncated component leaves no X96 samples in use.
        assert!(dec.decode_x96_exss(&data[..data.len() - 6]).is_err());
        assert_eq!(dec.x96_channels(), 0);
    }
}
