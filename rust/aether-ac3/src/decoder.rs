//! AC-3 / E-AC-3 decoder core.
//!
//! This is a step-by-step translation of FFmpeg's float decoder
//! (`ac3dec.c`, `eac3dec.c`, `ac3.c`, `ac3dsp.c`), kept close to the original
//! so that the output matches it. Channel 0 of the per-channel arrays is the
//! coupling channel, 1..=fbw are the full-bandwidth channels and the LFE
//! channel (if any) follows them.

use crate::bits::BitReader;
use crate::error::Error;
use crate::header::{self, find_syncword, parse_header, FrameInfo};
use crate::imdct::{kbd_window, Dct4};
use crate::layout::{acmod_mask, ChannelLayout, LFE_MASK};
use crate::lfg::Lfg;
use crate::tables::*;
use crate::tables_gen::*;

const MAX_CH: usize = AC3_MAX_CHANNELS;
const BLOCK: usize = AC3_BLOCK_SIZE;
const FRAME_BUFFER_SIZE: usize = 32768;
const OUTPUT_LFEON: u8 = 8;
/// Output buffer slot offset used for dependent substreams.
const DEP_OFFSET: usize = AC3_MAX_CHANNELS;

/// COEFF_0 = lrint(M_SQRT2 * cos(2 pi / 12) * (1 << 23))
const COEFF_0: i64 = 10273905;
/// COEFF_1 = lrint(M_SQRT2 * (1 << 23))
const COEFF_1: i64 = 11863283;
/// COEFF_2 = lrint(M_SQRT2 * cos(5 pi / 12) * (1 << 23))
const COEFF_2: i64 = 3070444;

/// Information about Dolby Atmos (Joint Object Coding) signalling in a frame.
/// Objects are not decoded; the decoder only reports what the stream says.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct JocInfo {
    /// `flag_ec3_extension_type_a` in the additional bitstream info of the
    /// independent frame (what FFmpeg uses to report "Dolby Atmos").
    pub extension_type_a: bool,
    /// `complexity_index_type_a` accompanying the flag.
    pub complexity_index: Option<u8>,
    /// An EMDF container with a JOC payload (id 14) was found after the audio
    /// blocks.
    pub emdf_joc_payload: bool,
    /// An EMDF container with an OAMD payload (id 11) was found.
    pub emdf_oamd_payload: bool,
}

impl JocInfo {
    /// True if the frame signals the Atmos object extension in any way.
    pub fn is_joc(&self) -> bool {
        self.extension_type_a || self.emdf_joc_payload
    }
}

/// One decoded access unit: 256 samples per audio block, planar `f32`.
#[derive(Clone, Debug, Default)]
pub struct DecodedFrame {
    pub sample_rate: u32,
    /// Samples per channel (`blocks * 256`, normally 1536).
    pub frames: usize,
    /// Channel order of the planes.
    pub layout: ChannelLayout,
    /// Plane-major samples: plane `c` is `planar[c * frames..(c + 1) * frames]`.
    pub planar: Vec<f32>,
    /// Atmos / JOC signalling; the object audio itself is not decoded.
    pub joc: JocInfo,
    /// Convenience copy of `joc.is_joc()`.
    pub is_joc: bool,
    /// True when every frame in the access unit passed its CRC check.
    pub crc_ok: bool,
    /// Bytes of the packet that were consumed.
    pub bytes_consumed: usize,
    /// `bsid` of the independent frame (16 for E-AC-3).
    pub bitstream_id: u8,
    /// Number of dependent frames merged into this access unit.
    pub dependent_frames: usize,
}

impl DecodedFrame {
    pub fn channels(&self) -> usize {
        self.layout.len()
    }

    /// Samples of plane `i`.
    pub fn channel(&self, i: usize) -> &[f32] {
        &self.planar[i * self.frames..(i + 1) * self.frames]
    }

    /// Writes the samples interleaved (`frames * channels` values).
    pub fn interleave_into(&self, out: &mut [f32]) {
        let ch = self.channels();
        for c in 0..ch {
            let plane = self.channel(c);
            for (i, &s) in plane.iter().enumerate() {
                out[i * ch + c] = s;
            }
        }
    }
}

/// Counters of the coding tools seen so far; useful to check which parts of
/// the format a stream exercises.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub frames: u64,
    pub dependent_frames: u64,
    pub blocks: u64,
    /// Blocks with coupling in use.
    pub coupling_blocks: u64,
    /// Blocks with spectral extension in use.
    pub spx_blocks: u64,
    /// (channel, frame) pairs coded with the adaptive hybrid transform.
    pub aht_channels: u64,
    /// (channel, block) pairs using the short transform.
    pub short_blocks: u64,
    /// Blocks with stereo rematrixing flags set.
    pub rematrix_blocks: u64,
    /// Blocks with delta bit allocation.
    pub dba_blocks: u64,
}

#[derive(Clone, Copy, Default)]
struct BitAllocParams {
    sr_code: usize,
    sr_shift: u32,
    slow_gain: i32,
    slow_decay: i32,
    fast_decay: i32,
    db_per_bit: i32,
    floor: i32,
    cpl_fast_leak: i32,
    cpl_slow_leak: i32,
}

#[derive(Default)]
struct MantGroups {
    b1_mant: [i32; 2],
    b2_mant: [i32; 2],
    b4_mant: i32,
    b1: usize,
    b2: usize,
    b4: usize,
}

/// Big per-channel work arrays, boxed to keep `Decoder` small.
struct Work {
    pre_mantissa: [[[i32; 6]; BLOCK]; MAX_CH],
    fixed_coeffs: [[i32; BLOCK]; MAX_CH],
    transform_coeffs: [[f32; BLOCK]; MAX_CH],
    delay: [[f32; BLOCK]; EAC3_MAX_CHANNELS],
    tmp_output: [f32; BLOCK],
    bap: [[u8; BLOCK]; MAX_CH],
    psd: [[i16; BLOCK]; MAX_CH],
    dexps: [[i8; BLOCK]; MAX_CH],
}

/// AC-3 / E-AC-3 decoder. One instance decodes one stream; feed it access
/// units in order.
pub struct Decoder {
    // Options.
    drc_scale: f32,
    strict_crc: bool,
    ffmpeg_compat: bool,
    /// FFmpeg's `downmixed` flag (starts true, see `upmix_delay_quirk`).
    downmixed: bool,

    // Static tables built at construction.
    window: Vec<f32>,
    dct256: Dct4,
    dct128: Dct4,
    dynamic_range_tab: [f32; 256],

    // Frame header state.
    frame_type: u8,
    substreamid: u8,
    frame_size: usize,
    sample_rate: u32,
    num_blocks: usize,
    bitstream_id: u8,
    channel_mode: u8,
    lfe_on: bool,
    eac3: bool,
    channel_map: u32,
    output_mode: u8,
    prev_output_mode: u8,
    channels: usize,
    fbw_channels: usize,
    lfe_ch: usize,
    ext_type_a: bool,
    complexity_index: Option<u8>,

    // Frame syntax parameters.
    snr_offset_strategy: u32,
    block_switch_syntax: bool,
    dither_flag_syntax: bool,
    bit_allocation_syntax: bool,
    fast_gain_syntax: bool,
    dba_syntax: bool,
    skip_syntax: bool,

    // Coupling.
    cpl_in_use: [bool; AC3_MAX_BLOCKS],
    cpl_strategy_exists: [bool; AC3_MAX_BLOCKS],
    channel_in_cpl: [bool; MAX_CH],
    phase_flags_in_use: bool,
    phase_flags: [bool; AC3_MAX_CPL_BANDS],
    num_cpl_bands: usize,
    cpl_band_struct: [u8; AC3_MAX_CPL_BANDS],
    cpl_band_sizes: [u8; AC3_MAX_CPL_BANDS],
    first_cpl_coords: [bool; MAX_CH],
    cpl_coords: [[i32; AC3_MAX_CPL_BANDS]; MAX_CH],

    // Spectral extension.
    spx_in_use: bool,
    channel_uses_spx: [bool; MAX_CH],
    spx_atten_code: [i8; MAX_CH],
    spx_src_start_freq: usize,
    spx_dst_end_freq: usize,
    spx_dst_start_freq: usize,
    num_spx_bands: usize,
    spx_band_struct: [u8; SPX_MAX_BANDS],
    spx_band_sizes: [u8; SPX_MAX_BANDS],
    first_spx_coords: [bool; MAX_CH],
    spx_noise_blend: [[f32; SPX_MAX_BANDS]; MAX_CH],
    spx_signal_blend: [[f32; SPX_MAX_BANDS]; MAX_CH],

    // AHT.
    channel_uses_aht: [bool; MAX_CH],

    // Bandwidth.
    start_freq: [usize; MAX_CH],
    end_freq: [usize; MAX_CH],

    // Rematrixing.
    num_rematrixing_bands: usize,
    rematrixing_flags: [bool; 4],

    // Exponents.
    num_exp_groups: [usize; MAX_CH],
    exp_strategy: [[u8; MAX_CH]; AC3_MAX_BLOCKS],

    // Bit allocation.
    ba: BitAllocParams,
    first_cpl_leak: bool,
    snr_offset: [i32; MAX_CH],
    fast_gain: [i32; MAX_CH],
    band_psd: [[i16; AC3_CRITICAL_BANDS]; MAX_CH],
    mask: [[i16; AC3_CRITICAL_BANDS]; MAX_CH],
    dba_mode: [u8; MAX_CH],
    dba_nsegs: [usize; MAX_CH],
    dba_offsets: [[u8; 8]; MAX_CH],
    dba_lengths: [[u8; 8]; MAX_CH],
    dba_values: [[u8; 8]; MAX_CH],

    // Dither.
    dither_flag: [bool; MAX_CH],
    dith_state: Lfg,

    // Dynamic range.
    dynamic_range: [f32; 2],

    // Transform.
    block_switch: [bool; MAX_CH],

    stats: Stats,
    work: Box<Work>,

    /// Final output buffers: 16 slots of 6 blocks.
    output_buffer: Vec<f32>,
    /// Output slot of each bitstream channel for the frame being decoded.
    dst_slot: [usize; MAX_CH],

    input: Vec<u8>,
    skip_regions: Vec<(usize, usize)>,
}

impl Default for Decoder {
    fn default() -> Self {
        Self::new()
    }
}

#[inline]
fn mulh(a: i32, b: i32) -> i32 {
    ((a as i64 * b as i64) >> 32) as i32
}

impl Decoder {
    /// Creates a decoder with dynamic range control disabled (full dynamic
    /// range, matching `ffmpeg -drc_scale 0`).
    pub fn new() -> Self {
        let mut dynamic_range_tab = [0f32; 256];
        for (i, slot) in dynamic_range_tab.iter_mut().enumerate() {
            let v = (i as i32 >> 5) - ((i as i32 >> 7) << 3) - 5;
            *slot = 2.0f32.powi(v) * ((i & 0x1F) | 0x20) as f32;
        }
        let work = Box::new(Work {
            pre_mantissa: [[[0; 6]; BLOCK]; MAX_CH],
            fixed_coeffs: [[0; BLOCK]; MAX_CH],
            transform_coeffs: [[0.0; BLOCK]; MAX_CH],
            delay: [[0.0; BLOCK]; EAC3_MAX_CHANNELS],
            tmp_output: [0.0; BLOCK],
            bap: [[0; BLOCK]; MAX_CH],
            psd: [[0; BLOCK]; MAX_CH],
            dexps: [[0; BLOCK]; MAX_CH],
        });
        Decoder {
            drc_scale: 0.0,
            strict_crc: false,
            ffmpeg_compat: false,
            downmixed: true,
            window: kbd_window(5.0, 256),
            dct256: Dct4::new(256),
            dct128: Dct4::new(128),
            dynamic_range_tab,
            frame_type: 0,
            substreamid: 0,
            frame_size: 0,
            sample_rate: 0,
            num_blocks: 0,
            bitstream_id: 0,
            channel_mode: 0,
            lfe_on: false,
            eac3: false,
            channel_map: 0,
            output_mode: 0,
            prev_output_mode: 0,
            channels: 0,
            fbw_channels: 0,
            lfe_ch: 0,
            ext_type_a: false,
            complexity_index: None,
            snr_offset_strategy: 0,
            block_switch_syntax: false,
            dither_flag_syntax: false,
            bit_allocation_syntax: false,
            fast_gain_syntax: false,
            dba_syntax: false,
            skip_syntax: false,
            cpl_in_use: [false; AC3_MAX_BLOCKS],
            cpl_strategy_exists: [false; AC3_MAX_BLOCKS],
            channel_in_cpl: [false; MAX_CH],
            phase_flags_in_use: false,
            phase_flags: [false; AC3_MAX_CPL_BANDS],
            num_cpl_bands: 0,
            cpl_band_struct: [0; AC3_MAX_CPL_BANDS],
            cpl_band_sizes: [0; AC3_MAX_CPL_BANDS],
            first_cpl_coords: [false; MAX_CH],
            cpl_coords: [[0; AC3_MAX_CPL_BANDS]; MAX_CH],
            spx_in_use: false,
            channel_uses_spx: [false; MAX_CH],
            spx_atten_code: [0; MAX_CH],
            spx_src_start_freq: 0,
            spx_dst_end_freq: 0,
            spx_dst_start_freq: 0,
            num_spx_bands: 0,
            spx_band_struct: [0; SPX_MAX_BANDS],
            spx_band_sizes: [0; SPX_MAX_BANDS],
            first_spx_coords: [false; MAX_CH],
            spx_noise_blend: [[0.0; SPX_MAX_BANDS]; MAX_CH],
            spx_signal_blend: [[0.0; SPX_MAX_BANDS]; MAX_CH],
            channel_uses_aht: [false; MAX_CH],
            start_freq: [0; MAX_CH],
            end_freq: [0; MAX_CH],
            num_rematrixing_bands: 0,
            rematrixing_flags: [false; 4],
            num_exp_groups: [0; MAX_CH],
            exp_strategy: [[0; MAX_CH]; AC3_MAX_BLOCKS],
            ba: BitAllocParams::default(),
            first_cpl_leak: false,
            snr_offset: [0; MAX_CH],
            fast_gain: [0; MAX_CH],
            band_psd: [[0; AC3_CRITICAL_BANDS]; MAX_CH],
            mask: [[0; AC3_CRITICAL_BANDS]; MAX_CH],
            dba_mode: [0; MAX_CH],
            dba_nsegs: [0; MAX_CH],
            dba_offsets: [[0; 8]; MAX_CH],
            dba_lengths: [[0; 8]; MAX_CH],
            dba_values: [[0; 8]; MAX_CH],
            dither_flag: [false; MAX_CH],
            dith_state: Lfg::new(),
            dynamic_range: [0.0; 2],
            block_switch: [false; MAX_CH],
            stats: Stats::default(),
            work,
            output_buffer: vec![0.0; EAC3_MAX_CHANNELS * BLOCK * AC3_MAX_BLOCKS],
            dst_slot: [0; MAX_CH],
            input: Vec::new(),
            skip_regions: Vec::new(),
        }
    }

    /// Sets the dynamic range compression scale (0.0 = off, the default;
    /// 1.0 = apply as encoded). Same meaning as FFmpeg's `drc_scale` option.
    pub fn set_drc_scale(&mut self, scale: f32) {
        self.drc_scale = scale.clamp(0.0, 6.0);
    }

    /// When enabled, a CRC mismatch makes `decode` fail with [`Error::Crc`]
    /// instead of decoding the frame anyway (FFmpeg's default behaviour).
    pub fn set_strict_crc(&mut self, strict: bool) {
        self.strict_crc = strict;
    }

    /// Coding tool counters accumulated since construction or [`reset`](Self::reset).
    pub fn stats(&self) -> Stats {
        self.stats
    }

    /// Reproduces a quirk of FFmpeg's decoder (default: off).
    ///
    /// FFmpeg, the first time a block mixes long and short transforms across
    /// channels, runs its "upmix delay" step even though no downmix was ever
    /// applied. That zeroes or copies the overlap buffers of some channels
    /// and produces a one-off glitch of a block or two. Enable this only to
    /// compare output with FFmpeg bit for bit.
    pub fn set_ffmpeg_bug_compat(&mut self, on: bool) {
        self.ffmpeg_compat = on;
    }

    /// Forgets all stream state (overlap, dither, exponents). Call after a seek.
    pub fn reset(&mut self) {
        let drc = self.drc_scale;
        let strict = self.strict_crc;
        let compat = self.ffmpeg_compat;
        *self = Decoder::new();
        self.drc_scale = drc;
        self.strict_crc = strict;
        self.ffmpeg_compat = compat;
    }

    /// Decodes one access unit (an independent frame plus the dependent
    /// frames that follow it) into a new frame.
    pub fn decode(&mut self, packet: &[u8]) -> Result<DecodedFrame, Error> {
        let mut out = DecodedFrame::default();
        self.decode_into(packet, &mut out)?;
        Ok(out)
    }

    /// Like [`decode`](Self::decode) but reuses the buffers in `out`.
    ///
    /// If the packet holds no decodable audio (for instance only a secondary
    /// program substream) the call succeeds with `out.frames == 0`.
    pub fn decode_into(&mut self, packet: &[u8], out: &mut DecodedFrame) -> Result<(), Error> {
        out.frames = 0;
        out.planar.clear();
        out.layout = ChannelLayout::default();
        out.joc = JocInfo::default();
        out.is_joc = false;
        out.crc_ok = true;
        out.bytes_consumed = packet.len();
        out.dependent_frames = 0;

        let sync = find_syncword(packet).ok_or(Error::Sync)?;
        if sync > 10 {
            return Err(Error::Sync);
        }
        let buf = &packet[sync..];
        let mut input = std::mem::take(&mut self.input);
        input.clear();
        input.extend_from_slice(&buf[..buf.len().min(FRAME_BUFFER_SIZE)]);
        let valid_len = input.len();
        input.extend_from_slice(&[0u8; 16]);

        let r = self.decode_frames(&input, valid_len, sync, out);
        self.input = input;
        r
    }

    fn decode_frames(
        &mut self,
        input: &[u8],
        valid_len: usize,
        sync: usize,
        out: &mut DecodedFrame,
    ) -> Result<(), Error> {
        let mut pos = 0usize;
        let mut got_independent = false;
        let mut joc = JocInfo::default();
        let mut crc_ok = true;
        let mut dependent_frames = 0usize;
        let mut first_bsid = 0u8;

        loop {
            let frame = &input[pos..];
            let buf_size = valid_len - pos;

            // Peek at the sync header first so that frames we skip do not
            // disturb the decoder state.
            let info = {
                let mut peek = BitReader::new(frame, buf_size * 8);
                parse_header(&mut peek)?
            };
            if info.is_eac3 && info.substreamid != 0 {
                // Additional programs are not decoded.
                if !got_independent {
                    out.bytes_consumed = sync + pos + info.frame_size.min(buf_size);
                    return Ok(());
                }
                break;
            }
            if info.frame_size > buf_size {
                return Err(Error::Incomplete);
            }
            let mut br = BitReader::new(frame, info.frame_size * 8);
            self.skip_regions.clear();
            let frame_crc = header::frame_crc_ok(frame, info.frame_size);
            if !frame_crc {
                if self.strict_crc {
                    return Err(Error::Crc);
                }
                crc_ok = false;
            }
            if info.frame_type == FRAME_TYPE_DEPENDENT && !got_independent {
                // A dependent frame without its independent frame: ignore.
                out.bytes_consumed = sync + pos + info.frame_size;
                return Ok(());
            }

            if !got_independent {
                self.ext_type_a = false;
                self.complexity_index = None;
                first_bsid = info.bitstream_id;
            }
            self.parse_frame_header(&mut br)?;
            joc.extension_type_a = self.ext_type_a;
            joc.complexity_index = self.complexity_index;

            // Channel configuration.
            self.output_mode = self.channel_mode;
            if self.lfe_on {
                self.output_mode |= OUTPUT_LFEON;
            }
            let offset = if self.frame_type == FRAME_TYPE_DEPENDENT {
                DEP_OFFSET
            } else {
                0
            };
            let acmod = (self.output_mode & !OUTPUT_LFEON) as usize;
            let channel_map = &DEC_CHANNEL_MAP[acmod][self.lfe_on as usize];
            for (i, &c) in channel_map.iter().enumerate().take(self.channels) {
                self.dst_slot[c as usize] = offset + i;
            }

            // Audio blocks.
            for blk in 0..self.num_blocks {
                self.decode_audio_block(&mut br, blk, offset)?;
            }

            {
                // Look for an EMDF container (Atmos / JOC).
                // Dolby puts it in the skip field of a block; some streams
                // use the auxiliary data at the end of the frame instead.
                let mut regions = std::mem::take(&mut self.skip_regions);
                regions.push((br.pos(), info.frame_size * 8 - 16));
                for &(from, to) in &regions {
                    let (j, o) = scan_emdf(frame, from, to);
                    joc.emdf_joc_payload |= j;
                    joc.emdf_oamd_payload |= o;
                }
                regions.clear();
                self.skip_regions = regions;
            }

            self.stats.frames += 1;
            self.stats.dependent_frames += (self.frame_type == FRAME_TYPE_DEPENDENT) as u64;
            pos += self.frame_size;
            if self.frame_type != FRAME_TYPE_DEPENDENT {
                got_independent = true;
            }

            // Is there a dependent frame next?
            let remaining = valid_len - pos;
            if remaining > 0 {
                if remaining <= 16 {
                    pos = valid_len;
                    break;
                }
                let mut peek = BitReader::new(&input[pos..], remaining * 8);
                let Ok(nh) = parse_header(&mut peek) else {
                    break;
                };
                if nh.frame_type == FRAME_TYPE_DEPENDENT
                    && nh.substreamid == 0
                    && nh.num_blocks as usize == self.num_blocks
                    && nh.sample_rate == self.sample_rate
                {
                    self.prev_output_mode = self.output_mode;
                    dependent_frames += 1;
                    continue;
                }
            }
            break;
        }

        let _ = first_bsid;
        out.bytes_consumed = (sync + pos).min(sync + valid_len);
        self.assemble_output(out)?;
        out.sample_rate = self.sample_rate;
        out.joc = joc;
        out.is_joc = joc.is_joc();
        out.crc_ok = crc_ok;
        out.bitstream_id = first_bsid;
        out.dependent_frames = dependent_frames;
        Ok(())
    }

    /// Builds the planar output from the per-slot buffers of the access unit.
    fn assemble_output(&mut self, out: &mut DecodedFrame) -> Result<(), Error> {
        let frames = self.num_blocks * BLOCK;
        let slot = BLOCK * AC3_MAX_BLOCKS;
        let layout;
        let mut ext_map = [0usize; EAC3_MAX_CHANNELS];
        for (i, e) in ext_map.iter_mut().enumerate() {
            *e = i;
        }
        if self.frame_type == FRAME_TYPE_DEPENDENT {
            let acmod_prev = (self.prev_output_mode & !OUTPUT_LFEON) as u8;
            let mut ich_layout = acmod_mask(acmod_prev);
            if self.prev_output_mode & OUTPUT_LFEON != 0 {
                ich_layout |= LFE_MASK;
            }
            let acmod = (self.output_mode & !OUTPUT_LFEON) as usize;
            let channel_map = &DEC_CHANNEL_MAP[acmod][self.lfe_on as usize];
            let channel_map_size = CHANNELS_TAB[acmod] as usize + self.lfe_on as usize;
            let mut channel_layout = ich_layout;
            for ch in 0..16 {
                if self.channel_map & (1 << (EAC3_MAX_CHANNELS - ch - 1)) != 0 {
                    channel_layout |= CUSTOM_CHANNEL_MAP_LOCATIONS[ch].1;
                }
            }
            if channel_layout.count_ones() as usize > EAC3_MAX_CHANNELS {
                return Err(Error::Invalid("too many channels"));
            }
            layout = ChannelLayout::from_mask(channel_layout);
            let mut extend = 0usize;
            'outer: for ch in 0..EAC3_MAX_CHANNELS {
                if self.channel_map & (1 << (EAC3_MAX_CHANNELS - ch - 1)) == 0 {
                    continue;
                }
                let (single, mask) = CUSTOM_CHANNEL_MAP_LOCATIONS[ch];
                if single {
                    let bit = mask.trailing_zeros();
                    let Some(index) = layout.channels().iter().position(|c| c.bit() == bit) else {
                        return Err(Error::Invalid("channel map"));
                    };
                    if extend >= channel_map_size {
                        break 'outer;
                    }
                    ext_map[index] = DEP_OFFSET + channel_map[extend] as usize;
                    extend += 1;
                } else {
                    for i in 0..64u32 {
                        if mask & (1u64 << i) != 0 {
                            let Some(index) = layout.channels().iter().position(|c| c.bit() == i)
                            else {
                                return Err(Error::Invalid("channel map"));
                            };
                            if extend >= channel_map_size {
                                break;
                            }
                            ext_map[index] = DEP_OFFSET + channel_map[extend] as usize;
                            extend += 1;
                        }
                    }
                }
            }
        } else {
            let acmod = (self.output_mode & !OUTPUT_LFEON) as u8;
            let mut mask = acmod_mask(acmod);
            if self.output_mode & OUTPUT_LFEON != 0 {
                mask |= LFE_MASK;
            }
            layout = ChannelLayout::from_mask(mask);
        }

        out.planar.clear();
        out.planar.reserve(frames * layout.len());
        for k in 0..layout.len() {
            let s = ext_map[k] * slot;
            out.planar
                .extend_from_slice(&self.output_buffer[s..s + frames]);
        }
        out.frames = frames;
        out.layout = layout;
        Ok(())
    }

    // ---------------------------------------------------------------------
    // Header parsing
    // ---------------------------------------------------------------------

    fn parse_frame_header(&mut self, br: &mut BitReader<'_>) -> Result<FrameInfo, Error> {
        let hdr = parse_header(br)?;
        self.ba.sr_code = hdr.sr_code as usize;
        self.bitstream_id = hdr.bitstream_id;
        self.channel_mode = hdr.channel_mode;
        self.lfe_on = hdr.lfe_on;
        self.ba.sr_shift = hdr.sr_shift as u32;
        self.sample_rate = hdr.sample_rate;
        self.channels = hdr.channels as usize;
        self.fbw_channels = self.channels - self.lfe_on as usize;
        self.lfe_ch = self.fbw_channels + 1;
        self.frame_size = hdr.frame_size;
        self.num_blocks = hdr.num_blocks as usize;
        self.frame_type = hdr.frame_type;
        self.substreamid = hdr.substreamid;

        if self.lfe_on {
            let l = self.lfe_ch;
            self.start_freq[l] = 0;
            self.end_freq[l] = 7;
            self.num_exp_groups[l] = 2;
            self.channel_in_cpl[l] = false;
        }

        if self.bitstream_id <= 10 {
            self.eac3 = false;
            self.snr_offset_strategy = 2;
            self.block_switch_syntax = true;
            self.dither_flag_syntax = true;
            self.bit_allocation_syntax = true;
            self.fast_gain_syntax = false;
            self.first_cpl_leak = false;
            self.dba_syntax = true;
            self.skip_syntax = true;
            self.channel_uses_aht = [false; MAX_CH];
            self.ac3_parse_bsi(br);
        } else {
            self.eac3 = true;
            self.eac3_parse_header(br)?;
        }
        Ok(hdr)
    }

    /// Remaining `bsi()` fields of plain AC-3.
    fn ac3_parse_bsi(&mut self, br: &mut BitReader<'_>) {
        let mut i = (self.channel_mode == 0) as i32;
        loop {
            br.skip(5); // dialnorm
            if br.get_bit() != 0 {
                br.skip(8); // compr
            }
            if br.get_bit() != 0 {
                br.skip(8); // language code
            }
            if br.get_bit() != 0 {
                br.skip(7); // audio production information
            }
            if i == 0 {
                break;
            }
            i -= 1;
        }
        br.skip(2); // copyright, original

        // Timecodes, or for bsid 6 the alternate bit stream syntax: both are
        // two optional 14-bit fields that do not affect decoding.
        if br.get_bit() != 0 {
            br.skip(14);
        }
        if br.get_bit() != 0 {
            br.skip(14);
        }

        if br.get_bit() != 0 {
            let n = br.get_bits(6) as usize;
            br.skip(8 * (n + 1));
        }
    }

    fn eac3_parse_header(&mut self, br: &mut BitReader<'_>) -> Result<(), Error> {
        if self.frame_type == FRAME_TYPE_RESERVED {
            return Err(Error::FrameType);
        }
        if self.substreamid != 0 {
            return Err(Error::FrameType);
        }
        if self.ba.sr_code == 3 {
            return Err(Error::Unsupported("reduced sampling rate"));
        }
        br.skip(5); // bitstream id

        // Volume control parameters.
        let nch_dn = if self.channel_mode != 0 { 1 } else { 2 };
        for _ in 0..nch_dn {
            br.skip(5); // dialnorm
            if br.get_bit() != 0 {
                br.skip(8); // compr
            }
        }

        // Dependent stream channel map.
        if self.frame_type == FRAME_TYPE_DEPENDENT && br.get_bit() != 0 {
            let channel_map = br.get_bits(16);
            let mut channel_layout = 0u64;
            for i in 0..16 {
                if channel_map & (1 << (EAC3_MAX_CHANNELS - i - 1)) != 0 {
                    channel_layout |= CUSTOM_CHANNEL_MAP_LOCATIONS[i].1;
                }
            }
            if channel_layout.count_ones() as usize > EAC3_MAX_CHANNELS {
                return Err(Error::Invalid("too many channels in channel map"));
            }
            self.channel_map = channel_map;
        }

        // Mixing metadata.
        if br.get_bit() != 0 {
            if self.channel_mode > CHMODE_STEREO {
                br.skip(2); // dmixmod
                if self.channel_mode & 1 != 0 {
                    br.skip(6); // ltrtcmixlev, lorocmixlev
                }
                if self.channel_mode & 4 != 0 {
                    br.skip(6); // ltrtsurmixlev, lorosurmixlev
                }
            }
            if self.lfe_on && br.get_bit() != 0 {
                br.skip(5); // lfemixlevcod
            }
            if self.frame_type == FRAME_TYPE_INDEPENDENT {
                for _ in 0..nch_dn {
                    if br.get_bit() != 0 {
                        br.skip(6); // pgmscl
                    }
                }
                if br.get_bit() != 0 {
                    br.skip(6); // extpgmscl
                }
                match br.get_bits(2) {
                    1 => br.skip(5),
                    2 => br.skip(12),
                    3 => {
                        let mix_data_size = ((br.get_bits(5) as usize) + 2) << 3;
                        br.skip(mix_data_size);
                    }
                    _ => {}
                }
                if self.channel_mode < CHMODE_STEREO {
                    for _ in 0..nch_dn {
                        if br.get_bit() != 0 {
                            br.skip(14); // panmean + paninfo reserved
                        }
                    }
                }
                if br.get_bit() != 0 {
                    for _ in 0..self.num_blocks {
                        if self.num_blocks == 1 || br.get_bit() != 0 {
                            br.skip(5);
                        }
                    }
                }
            }
        }

        // Informational metadata.
        if br.get_bit() != 0 {
            br.skip(3); // bsmod
            br.skip(2); // copyright, original
            if self.channel_mode == CHMODE_STEREO {
                br.skip(4); // dsurmod, dheadphonmod
            }
            if self.channel_mode >= 6 {
                br.skip(2); // dsurexmod
            }
            for _ in 0..nch_dn {
                if br.get_bit() != 0 {
                    br.skip(8);
                }
            }
            if self.ba.sr_code != 3 {
                br.skip(1); // sourcefscod
            }
        }

        // Converter synchronization flag.
        if self.frame_type == FRAME_TYPE_INDEPENDENT && self.num_blocks != 6 {
            br.skip(1);
        }

        // Original frame size code if converted from AC-3.
        if self.frame_type == FRAME_TYPE_AC3_CONVERT && (self.num_blocks == 6 || br.get_bit() != 0)
        {
            br.skip(6);
        }

        // Additional bitstream info.
        // The access unit's flags are reset by the caller before the first
        // frame; like FFmpeg, the flag counts if any frame of the unit sets it.
        if br.get_bit() != 0 {
            let addbsil = br.get_bits(6) as usize;
            for i in 0..=addbsil {
                if i == 0 {
                    br.skip(7);
                    if br.get_bit() != 0 {
                        self.ext_type_a = true;
                    }
                } else if i == 1 {
                    let v = br.get_bits(8) as u8;
                    if self.ext_type_a {
                        self.complexity_index = Some(v);
                    }
                } else {
                    br.skip(8);
                }
            }
        }

        // Audio frame syntax flags, strategy data and per-frame data.
        let (ac3_exponent_strategy, parse_aht_info);
        if self.num_blocks == 6 {
            ac3_exponent_strategy = br.get_bit() != 0;
            parse_aht_info = br.get_bit() != 0;
        } else {
            ac3_exponent_strategy = true;
            parse_aht_info = false;
        }

        self.snr_offset_strategy = br.get_bits(2);
        let parse_transient_proc_info = br.get_bit() != 0;

        self.block_switch_syntax = br.get_bit() != 0;
        if !self.block_switch_syntax {
            self.block_switch = [false; MAX_CH];
        }

        self.dither_flag_syntax = br.get_bit() != 0;
        if !self.dither_flag_syntax {
            for ch in 1..=self.fbw_channels {
                self.dither_flag[ch] = true;
            }
        }
        self.dither_flag[CPL_CH] = false;
        self.dither_flag[self.lfe_ch] = false;

        self.bit_allocation_syntax = br.get_bit() != 0;
        if !self.bit_allocation_syntax {
            self.ba.slow_decay = SLOW_DECAY_TAB[2];
            self.ba.fast_decay = FAST_DECAY_TAB[1];
            self.ba.slow_gain = SLOW_GAIN_TAB[1];
            self.ba.db_per_bit = DB_PER_BIT_TAB[2];
            self.ba.floor = FLOOR_TAB[7];
        }

        self.fast_gain_syntax = br.get_bit() != 0;
        self.dba_syntax = br.get_bit() != 0;
        self.skip_syntax = br.get_bit() != 0;
        let parse_spx_atten_data = br.get_bit() != 0;

        // Coupling strategy occurrence and coupling use per block.
        let mut num_cpl_blocks = 0;
        if self.channel_mode > 1 {
            for blk in 0..self.num_blocks {
                self.cpl_strategy_exists[blk] = blk == 0 || br.get_bit() != 0;
                if self.cpl_strategy_exists[blk] {
                    self.cpl_in_use[blk] = br.get_bit() != 0;
                } else {
                    self.cpl_in_use[blk] = self.cpl_in_use[blk - 1];
                }
                num_cpl_blocks += self.cpl_in_use[blk] as usize;
            }
        } else {
            self.cpl_in_use = [false; AC3_MAX_BLOCKS];
        }

        // Exponent strategy data.
        if ac3_exponent_strategy {
            for blk in 0..self.num_blocks {
                let first = if self.cpl_in_use[blk] { 0 } else { 1 };
                for ch in first..=self.fbw_channels {
                    self.exp_strategy[blk][ch] = br.get_bits(2) as u8;
                }
            }
        } else {
            let first = if self.channel_mode > 1 && num_cpl_blocks != 0 {
                0
            } else {
                1
            };
            for ch in first..=self.fbw_channels {
                let frmchexpstr = br.get_bits(5) as usize;
                for blk in 0..6 {
                    self.exp_strategy[blk][ch] = FRM_EXPSTR[frmchexpstr][blk];
                }
            }
        }
        // LFE exponent strategy.
        if self.lfe_on {
            for blk in 0..self.num_blocks {
                self.exp_strategy[blk][self.lfe_ch] = br.get_bit() as u8;
            }
        }
        // Original exponent strategies if converted from AC-3.
        if self.frame_type == FRAME_TYPE_INDEPENDENT && (self.num_blocks == 6 || br.get_bit() != 0)
        {
            br.skip(5 * self.fbw_channels);
        }

        // Which channels use AHT.
        if parse_aht_info {
            self.channel_uses_aht[CPL_CH] = false;
            let first = if num_cpl_blocks != 6 { 1 } else { 0 };
            for ch in first..=self.channels {
                let mut use_aht = true;
                for blk in 1..6 {
                    if self.exp_strategy[blk][ch] != EXP_REUSE
                        || (ch == 0 && self.cpl_strategy_exists[blk])
                    {
                        use_aht = false;
                        break;
                    }
                }
                self.channel_uses_aht[ch] = use_aht && br.get_bit() != 0;
            }
        } else {
            self.channel_uses_aht = [false; MAX_CH];
        }

        // Per-frame SNR offset.
        if self.snr_offset_strategy == 0 {
            let csnroffst = (br.get_bits(6) as i32 - 15) << 4;
            let snroffst = (csnroffst + br.get_bits(4) as i32) << 2;
            for ch in 0..=self.channels {
                self.snr_offset[ch] = snroffst;
            }
        }

        // Transient pre-noise processing data (parsed, not applied; FFmpeg
        // does not apply it either).
        if parse_transient_proc_info {
            for _ in 1..=self.fbw_channels {
                if br.get_bit() != 0 {
                    br.skip(10);
                    br.skip(8);
                }
            }
        }

        // Spectral extension attenuation data.
        for ch in 1..=self.fbw_channels {
            if parse_spx_atten_data && br.get_bit() != 0 {
                self.spx_atten_code[ch] = br.get_bits(5) as i8;
            } else {
                self.spx_atten_code[ch] = -1;
            }
        }

        // Block start information.
        if self.num_blocks > 1 && br.get_bit() != 0 {
            let log2 = (usize::BITS - 1 - (self.frame_size - 2).leading_zeros()) as usize;
            let block_start_bits = (self.num_blocks - 1) * (4 + log2);
            br.skip(block_start_bits);
        }

        // Syntax state initialization.
        for ch in 1..=self.fbw_channels {
            self.first_spx_coords[ch] = true;
            self.first_cpl_coords[ch] = true;
        }
        self.first_cpl_leak = true;
        Ok(())
    }

    // ---------------------------------------------------------------------
    // Exponents, bit allocation
    // ---------------------------------------------------------------------

    fn decode_exponents(
        br: &mut BitReader<'_>,
        exp_strategy: u8,
        ngrps: usize,
        absexp: i32,
        dexps: &mut [i8],
    ) -> Result<(), Error> {
        let group_size = exp_strategy as usize + (exp_strategy == EXP_D45) as usize;
        let mut dexp = [0u8; 3 * 128];
        let mut i = 0;
        for _ in 0..ngrps {
            let expacc = br.get_bits(7) as usize;
            if expacc >= 125 {
                return Err(Error::Invalid("exponent group out of range"));
            }
            let g = UNGROUP_3_IN_7_BITS_TAB[expacc];
            if i + 3 > dexp.len() {
                return Err(Error::Invalid("too many exponent groups"));
            }
            dexp[i] = g[0];
            dexp[i + 1] = g[1];
            dexp[i + 2] = g[2];
            i += 3;
        }
        let mut prevexp = absexp;
        let mut j = 0;
        for &d in &dexp[..ngrps * 3] {
            prevexp += d as i32 - 2;
            if !(0..=24).contains(&prevexp) {
                return Err(Error::Invalid("exponent out of range"));
            }
            for _ in 0..group_size {
                if let Some(slot) = dexps.get_mut(j) {
                    *slot = prevexp as i8;
                }
                j += 1;
            }
        }
        Ok(())
    }

    fn calc_psd(&mut self, ch: usize) {
        let start = self.start_freq[ch];
        let end = self.end_freq[ch];
        let exp = &self.work.dexps[ch];
        let psd = &mut self.work.psd[ch];
        let band_psd = &mut self.band_psd[ch];
        for bin in start..end {
            psd[bin] = (3072 - ((exp[bin] as i32) << 7)) as i16;
        }
        let mut bin = start;
        let mut band = BIN_TO_BAND_TAB[start] as usize;
        loop {
            let mut v = psd[bin] as i32;
            bin += 1;
            let band_end = (BAND_START_TAB[band + 1] as usize).min(end);
            while bin < band_end {
                let p = psd[bin] as i32;
                let max = v.max(p);
                let adr = (max - ((v + p + 1) >> 1)).min(255);
                v = max + LOG_ADD_TAB[adr as usize] as i32;
                bin += 1;
            }
            band_psd[band] = v as i16;
            band += 1;
            if end <= BAND_START_TAB[band] as usize {
                break;
            }
        }
    }

    fn calc_mask(&mut self, ch: usize) -> Result<(), Error> {
        let start = self.start_freq[ch];
        let end = self.end_freq[ch];
        let fast_gain = self.fast_gain[ch];
        let is_lfe = ch == self.lfe_ch;
        let dba_mode = self.dba_mode[ch];
        let band_psd = &self.band_psd[ch];
        let p = self.ba;
        let mut excite = [0i32; AC3_CRITICAL_BANDS];

        if end == 0 {
            return Err(Error::Invalid("empty channel bandwidth"));
        }

        fn calc_lowcomp1(a: i32, b0: i32, b1: i32, c: i32) -> i32 {
            if b0 + 256 == b1 {
                c
            } else if b0 > b1 {
                (a - 64).max(0)
            } else {
                a
            }
        }
        fn calc_lowcomp(a: i32, b0: i32, b1: i32, bin: usize) -> i32 {
            if bin < 7 {
                calc_lowcomp1(a, b0, b1, 384)
            } else if bin < 20 {
                calc_lowcomp1(a, b0, b1, 320)
            } else {
                (a - 128).max(0)
            }
        }

        let band_start = BIN_TO_BAND_TAB[start] as usize;
        let band_end = BIN_TO_BAND_TAB[end - 1] as usize + 1;
        let bp = |b: usize| band_psd[b] as i32;
        let begin;
        let mut fastleak;
        let mut slowleak;

        if band_start == 0 {
            let mut lowcomp = 0;
            lowcomp = calc_lowcomp1(lowcomp, bp(0), bp(1), 384);
            excite[0] = bp(0) - fast_gain - lowcomp;
            lowcomp = calc_lowcomp1(lowcomp, bp(1), bp(2), 384);
            excite[1] = bp(1) - fast_gain - lowcomp;
            let mut begin_ = 7;
            fastleak = 0;
            slowleak = 0;
            for band in 2..7 {
                if !(is_lfe && band == 6) {
                    lowcomp = calc_lowcomp1(lowcomp, bp(band), bp(band + 1), 384);
                }
                fastleak = bp(band) - fast_gain;
                slowleak = bp(band) - p.slow_gain;
                excite[band] = fastleak - lowcomp;
                if !(is_lfe && band == 6) && bp(band) <= bp(band + 1) {
                    begin_ = band + 1;
                    break;
                }
            }
            let end1 = band_end.min(22);
            for band in begin_..end1 {
                if !(is_lfe && band == 6) {
                    lowcomp = calc_lowcomp(lowcomp, bp(band), bp(band + 1), band);
                }
                fastleak = (fastleak - p.fast_decay).max(bp(band) - fast_gain);
                slowleak = (slowleak - p.slow_decay).max(bp(band) - p.slow_gain);
                excite[band] = (fastleak - lowcomp).max(slowleak);
            }
            begin = 22;
        } else {
            // Coupling channel.
            begin = band_start;
            fastleak = (p.cpl_fast_leak << 8) + 768;
            slowleak = (p.cpl_slow_leak << 8) + 768;
        }

        for band in begin..band_end {
            fastleak = (fastleak - p.fast_decay).max(bp(band) - fast_gain);
            slowleak = (slowleak - p.slow_decay).max(bp(band) - p.slow_gain);
            excite[band] = fastleak.max(slowleak);
        }

        // Masking curve.
        let mask = &mut self.mask[ch];
        for band in band_start..band_end {
            let tmp = p.db_per_bit - bp(band);
            if tmp > 0 {
                excite[band] += tmp >> 2;
            }
            let ht = HEARING_THRESHOLD_TAB[band >> p.sr_shift][p.sr_code] as i32;
            mask[band] = ht.max(excite[band]) as i16;
        }

        // Delta bit allocation.
        if dba_mode == DBA_REUSE || dba_mode == DBA_NEW {
            let nsegs = self.dba_nsegs[ch];
            if nsegs > 8 {
                return Err(Error::Invalid("delta bit allocation"));
            }
            let mut band = band_start;
            for seg in 0..nsegs {
                band += self.dba_offsets[ch][seg] as usize;
                let len = self.dba_lengths[ch][seg] as usize;
                if band >= AC3_CRITICAL_BANDS || len > AC3_CRITICAL_BANDS - band {
                    return Err(Error::Invalid("delta bit allocation"));
                }
                let v = self.dba_values[ch][seg] as i32;
                let delta = if v >= 4 { (v - 3) * 128 } else { (v - 4) * 128 };
                for _ in 0..len {
                    mask[band] = (mask[band] as i32 + delta) as i16;
                    band += 1;
                }
            }
        }
        Ok(())
    }

    fn calc_bap(&mut self, ch: usize) {
        let start = self.start_freq[ch];
        let end = self.end_freq[ch];
        let snr_offset = self.snr_offset[ch];
        let floor = self.ba.floor;
        let bap = &mut self.work.bap[ch];
        if snr_offset == -960 {
            *bap = [0; BLOCK];
            return;
        }
        let bap_tab: &[u8; 64] = if self.channel_uses_aht[ch] {
            &HEBAP_TAB
        } else {
            &BAP_TAB
        };
        let mask = &self.mask[ch];
        let psd = &self.work.psd[ch];
        let mut bin = start;
        let mut band = BIN_TO_BAND_TAB[start] as usize;
        loop {
            let m = ((mask[band] as i32 - snr_offset - floor).max(0) & 0x1FE0) + floor;
            band += 1;
            let band_end = (BAND_START_TAB[band] as usize).min(end);
            while bin < band_end {
                let address = ((psd[bin] as i32 - m) >> 5).clamp(0, 63);
                bap[bin] = bap_tab[address as usize];
                bin += 1;
            }
            if end <= band_end {
                break;
            }
        }
    }

    // ---------------------------------------------------------------------
    // Band structures, spectral extension, coupling
    // ---------------------------------------------------------------------

    #[allow(clippy::too_many_arguments)]
    fn decode_band_structure(
        br: &mut BitReader<'_>,
        blk: usize,
        eac3: bool,
        ecpl: bool,
        start_subband: usize,
        end_subband: usize,
        default_band_struct: &[u8],
        band_struct: &mut [u8],
        num_bands: &mut usize,
        band_sizes: &mut [u8],
    ) {
        let n_subbands = end_subband - start_subband;
        if blk == 0 {
            band_struct[..default_band_struct.len()].copy_from_slice(default_band_struct);
        }
        let bs = &mut band_struct[start_subband + 1..];
        if !eac3 || br.get_bit() != 0 {
            for b in bs.iter_mut().take(n_subbands - 1) {
                *b = br.get_bit() as u8;
            }
        }
        let mut bnd_sz = [0u8; 22];
        let mut n_bands = n_subbands;
        bnd_sz[0] = if ecpl { 6 } else { 12 };
        let mut bnd = 0;
        for subbnd in 1..n_subbands {
            let subbnd_size = if ecpl && subbnd < 4 { 6 } else { 12 };
            if bs[subbnd - 1] != 0 {
                n_bands -= 1;
                bnd_sz[bnd] += subbnd_size;
            } else {
                bnd += 1;
                bnd_sz[bnd] = subbnd_size;
            }
        }
        *num_bands = n_bands;
        band_sizes[..n_bands].copy_from_slice(&bnd_sz[..n_bands]);
    }

    fn spx_strategy(&mut self, br: &mut BitReader<'_>, blk: usize) -> Result<(), Error> {
        let fbw = self.fbw_channels;
        if self.channel_mode == CHMODE_MONO {
            self.channel_uses_spx[1] = true;
        } else {
            for ch in 1..=fbw {
                self.channel_uses_spx[ch] = br.get_bit() != 0;
            }
        }
        let mut dst_start_freq = br.get_bits(2) as usize;
        let mut start_subband = br.get_bits(3) as usize + 2;
        if start_subband > 7 {
            start_subband += start_subband - 7;
        }
        let mut end_subband = br.get_bits(3) as usize + 5;
        if end_subband > 7 {
            end_subband += end_subband - 7;
        }
        dst_start_freq = dst_start_freq * 12 + 25;
        let src_start_freq = start_subband * 12 + 25;
        let dst_end_freq = end_subband * 12 + 25;

        if start_subband >= end_subband {
            return Err(Error::Invalid("spectral extension range"));
        }
        if dst_start_freq >= src_start_freq {
            return Err(Error::Invalid("spectral extension copy start"));
        }
        self.spx_dst_start_freq = dst_start_freq;
        self.spx_src_start_freq = src_start_freq;
        self.spx_dst_end_freq = dst_end_freq;

        let mut num = 0;
        Self::decode_band_structure(
            br,
            blk,
            self.eac3,
            false,
            start_subband,
            end_subband,
            &EAC3_DEFAULT_SPX_BAND_STRUCT,
            &mut self.spx_band_struct,
            &mut num,
            &mut self.spx_band_sizes,
        );
        self.num_spx_bands = num;
        Ok(())
    }

    fn spx_coordinates(&mut self, br: &mut BitReader<'_>) {
        for ch in 1..=self.fbw_channels {
            if self.channel_uses_spx[ch] {
                if self.first_spx_coords[ch] || br.get_bit() != 0 {
                    self.first_spx_coords[ch] = false;
                    let spx_blend = br.get_bits(5) as f32 * (1.0f32 / 32.0);
                    let master_spx_coord = br.get_bits(2) as i32 * 3;

                    let mut bin = self.spx_src_start_freq;
                    for bnd in 0..self.num_spx_bands {
                        let bandsize = self.spx_band_sizes[bnd] as usize;
                        let mut nratio = ((bin + (bandsize >> 1)) as f32
                            / self.spx_dst_end_freq as f32)
                            - spx_blend;
                        nratio = nratio.clamp(0.0, 1.0);
                        let nblend = (3.0f32 * nratio).sqrt();
                        let sblend = (1.0f32 - nratio).sqrt();
                        bin += bandsize;

                        let spx_coord_exp = br.get_bits(4) as i32;
                        let mut spx_coord_mant = br.get_bits(2) as i32;
                        if spx_coord_exp == 15 {
                            spx_coord_mant <<= 1;
                        } else {
                            spx_coord_mant += 4;
                        }
                        let sh = 25 - spx_coord_exp - master_spx_coord;
                        // The shift is non-negative for any valid stream.
                        spx_coord_mant = if sh >= 0 {
                            spx_coord_mant.wrapping_shl(sh as u32)
                        } else {
                            spx_coord_mant >> (-sh).min(31)
                        };
                        let spx_coord = spx_coord_mant as f32 * (1.0f32 / (1 << 23) as f32);
                        self.spx_noise_blend[ch][bnd] = nblend * spx_coord;
                        self.spx_signal_blend[ch][bnd] = sblend * spx_coord;
                    }
                }
            } else {
                self.first_spx_coords[ch] = true;
            }
        }
    }

    fn coupling_strategy(
        &mut self,
        br: &mut BitReader<'_>,
        blk: usize,
        bit_alloc_stages: &mut [u8; MAX_CH],
    ) -> Result<(), Error> {
        let fbw = self.fbw_channels;
        let channel_mode = self.channel_mode;
        *bit_alloc_stages = [3; MAX_CH];
        if !self.eac3 {
            self.cpl_in_use[blk] = br.get_bit() != 0;
        }
        if self.cpl_in_use[blk] {
            if channel_mode < CHMODE_STEREO {
                return Err(Error::Invalid("coupling in mono or dual-mono"));
            }
            if self.eac3 && br.get_bit() != 0 {
                return Err(Error::Unsupported("enhanced coupling"));
            }
            if self.eac3 && self.channel_mode == CHMODE_STEREO {
                self.channel_in_cpl[1] = true;
                self.channel_in_cpl[2] = true;
            } else {
                for ch in 1..=fbw {
                    self.channel_in_cpl[ch] = br.get_bit() != 0;
                }
            }
            if channel_mode == CHMODE_STEREO {
                self.phase_flags_in_use = br.get_bit() != 0;
            }
            let cpl_start_subband = br.get_bits(4) as usize;
            let cpl_end_subband = if self.spx_in_use {
                (self.spx_src_start_freq as isize - 37).div_euclid(12) as usize
            } else {
                br.get_bits(4) as usize + 3
            };
            if cpl_start_subband >= cpl_end_subband {
                return Err(Error::Invalid("coupling range"));
            }
            self.start_freq[CPL_CH] = cpl_start_subband * 12 + 37;
            self.end_freq[CPL_CH] = cpl_end_subband * 12 + 37;
            let mut num = 0;
            Self::decode_band_structure(
                br,
                blk,
                self.eac3,
                false,
                cpl_start_subband,
                cpl_end_subband,
                &EAC3_DEFAULT_CPL_BAND_STRUCT,
                &mut self.cpl_band_struct,
                &mut num,
                &mut self.cpl_band_sizes,
            );
            self.num_cpl_bands = num;
        } else {
            for ch in 1..=fbw {
                self.channel_in_cpl[ch] = false;
                self.first_cpl_coords[ch] = true;
            }
            self.first_cpl_leak = self.eac3;
            self.phase_flags_in_use = false;
        }
        Ok(())
    }

    fn coupling_coordinates(&mut self, br: &mut BitReader<'_>, blk: usize) -> Result<(), Error> {
        let mut cpl_coords_exist = false;
        for ch in 1..=self.fbw_channels {
            if self.channel_in_cpl[ch] {
                if (self.eac3 && self.first_cpl_coords[ch]) || br.get_bit() != 0 {
                    self.first_cpl_coords[ch] = false;
                    cpl_coords_exist = true;
                    let master_cpl_coord = 3 * br.get_bits(2) as i32;
                    for bnd in 0..self.num_cpl_bands {
                        let cpl_coord_exp = br.get_bits(4) as i32;
                        let cpl_coord_mant = br.get_bits(4) as i32;
                        let v = if cpl_coord_exp == 15 {
                            cpl_coord_mant << 22
                        } else {
                            (cpl_coord_mant + 16) << 21
                        };
                        self.cpl_coords[ch][bnd] = v >> (cpl_coord_exp + master_cpl_coord).min(31);
                    }
                } else if blk == 0 {
                    return Err(Error::Invalid("coupling coordinates missing in block 0"));
                }
            } else {
                self.first_cpl_coords[ch] = true;
            }
        }
        if self.channel_mode == CHMODE_STEREO && cpl_coords_exist {
            for bnd in 0..self.num_cpl_bands {
                self.phase_flags[bnd] = if self.phase_flags_in_use {
                    br.get_bit() != 0
                } else {
                    false
                };
            }
        }
        Ok(())
    }

    // ---------------------------------------------------------------------
    // Mantissas
    // ---------------------------------------------------------------------

    fn calc_transform_coeffs_cpl(&mut self) {
        let mut bin = self.start_freq[CPL_CH];
        for band in 0..self.num_cpl_bands {
            let band_start = bin;
            let band_end = bin + self.cpl_band_sizes[band] as usize;
            for ch in 1..=self.fbw_channels {
                if self.channel_in_cpl[ch] {
                    let cpl_coord = self.cpl_coords[ch][band] << 5;
                    let (cpl_row, rest) = self.work.fixed_coeffs.split_at_mut(1);
                    let src = &cpl_row[0];
                    let dst = &mut rest[ch - 1];
                    for b in band_start..band_end.min(BLOCK) {
                        dst[b] = mulh(src[b].wrapping_mul(1 << 4), cpl_coord);
                    }
                    if ch == 2 && self.phase_flags[band] {
                        for b in band_start..band_end.min(BLOCK) {
                            dst[b] = dst[b].wrapping_neg();
                        }
                    }
                }
            }
            bin = band_end;
        }
    }

    fn decode_transform_coeffs_ch_plain(
        &mut self,
        br: &mut BitReader<'_>,
        ch_index: usize,
        m: &mut MantGroups,
    ) {
        let start_freq = self.start_freq[ch_index];
        let end_freq = self.end_freq[ch_index].min(BLOCK);
        let dither = ch_index == CPL_CH || self.dither_flag[ch_index];
        let baps = &self.work.bap[ch_index];
        let exps = &self.work.dexps[ch_index];
        let coeffs = &mut self.work.fixed_coeffs[ch_index];
        let dith = &mut self.dith_state;

        for freq in start_freq..end_freq {
            let bap = baps[freq] as usize;
            let mantissa: i32 = match bap {
                0 => {
                    if dither {
                        let r = dith.get();
                        (((r >> 8).wrapping_mul(181)) >> 8).wrapping_sub(5931008) as i32
                    } else {
                        0
                    }
                }
                1 => {
                    if m.b1 > 0 {
                        m.b1 -= 1;
                        m.b1_mant[m.b1]
                    } else {
                        let bits = br.get_bits(5) as usize;
                        m.b1_mant[1] = B1_MANTISSAS[bits][1];
                        m.b1_mant[0] = B1_MANTISSAS[bits][2];
                        m.b1 = 2;
                        B1_MANTISSAS[bits][0]
                    }
                }
                2 => {
                    if m.b2 > 0 {
                        m.b2 -= 1;
                        m.b2_mant[m.b2]
                    } else {
                        let bits = br.get_bits(7) as usize;
                        m.b2_mant[1] = B2_MANTISSAS[bits][1];
                        m.b2_mant[0] = B2_MANTISSAS[bits][2];
                        m.b2 = 2;
                        B2_MANTISSAS[bits][0]
                    }
                }
                3 => B3_MANTISSAS[br.get_bits(3) as usize],
                4 => {
                    if m.b4 > 0 {
                        m.b4 = 0;
                        m.b4_mant
                    } else {
                        let bits = br.get_bits(7) as usize;
                        m.b4_mant = B4_MANTISSAS[bits][1];
                        m.b4 = 1;
                        B4_MANTISSAS[bits][0]
                    }
                }
                5 => B5_MANTISSAS[br.get_bits(4) as usize],
                _ => {
                    let bap = bap.min(15);
                    let q = QUANTIZATION_TAB[bap] as u32;
                    ((br.get_sbits(q) as u32) << (24 - q)) as i32
                }
            };
            coeffs[freq] = mantissa >> exps[freq];
        }
    }

    fn remove_dithering(&mut self) {
        for ch in 1..=self.fbw_channels {
            if !self.dither_flag[ch] && self.channel_in_cpl[ch] {
                let (cpl_bap, fixed) = (&self.work.bap[CPL_CH], &mut self.work.fixed_coeffs[ch]);
                for i in self.start_freq[CPL_CH]..self.end_freq[CPL_CH].min(BLOCK) {
                    if cpl_bap[i] == 0 {
                        fixed[i] = 0;
                    }
                }
            }
        }
    }

    fn decode_transform_coeffs_ch(
        &mut self,
        br: &mut BitReader<'_>,
        blk: usize,
        ch: usize,
        m: &mut MantGroups,
    ) {
        if !self.channel_uses_aht[ch] {
            self.decode_transform_coeffs_ch_plain(br, ch, m);
        } else {
            // With AHT the mantissas of all blocks sit in the first block.
            if blk == 0 {
                self.decode_aht_ch(br, ch);
            }
            let end = self.end_freq[ch].min(BLOCK);
            for bin in self.start_freq[ch]..end {
                self.work.fixed_coeffs[ch][bin] =
                    self.work.pre_mantissa[ch][bin][blk] >> self.work.dexps[ch][bin];
            }
        }
    }

    fn decode_transform_coeffs(&mut self, br: &mut BitReader<'_>, blk: usize) {
        let mut got_cplchan = false;
        let mut m = MantGroups::default();
        for ch in 1..=self.channels {
            self.decode_transform_coeffs_ch(br, blk, ch, &mut m);
            let mut end;
            if self.channel_in_cpl[ch] {
                if !got_cplchan {
                    self.decode_transform_coeffs_ch(br, blk, CPL_CH, &mut m);
                    self.calc_transform_coeffs_cpl();
                    got_cplchan = true;
                }
                end = self.end_freq[CPL_CH];
            } else {
                end = self.end_freq[ch];
            }
            let row = &mut self.work.fixed_coeffs[ch];
            while end < BLOCK {
                row[end] = 0;
                end += 1;
            }
        }
        self.remove_dithering();
    }

    fn do_rematrixing(&mut self) {
        let end = self.end_freq[1].min(self.end_freq[2]);
        let (a, b) = self.work.fixed_coeffs.split_at_mut(2);
        let c1 = &mut a[1];
        let c2 = &mut b[0];
        for bnd in 0..self.num_rematrixing_bands {
            if self.rematrixing_flags[bnd] {
                let bndend = end.min(REMATRIX_BAND_TAB[bnd + 1] as usize);
                for i in REMATRIX_BAND_TAB[bnd] as usize..bndend {
                    let tmp0 = c1[i];
                    c1[i] = c1[i].wrapping_add(c2[i]);
                    c2[i] = tmp0.wrapping_sub(c2[i]);
                }
            }
        }
    }

    // ---------------------------------------------------------------------
    // AHT
    // ---------------------------------------------------------------------

    fn idct6(pre_mant: &mut [i32; 6]) {
        let p = |i: usize| pre_mant[i];
        let odd1 = p(1).wrapping_sub(p(3)).wrapping_sub(p(5));
        let even2 = ((p(2) as i64 * COEFF_0) >> 23) as i32;
        let tmp = ((p(4) as i64 * COEFF_1) >> 23) as i32;
        let odd0 = (((p(1).wrapping_add(p(5))) as i64 * COEFF_2) >> 23) as i32;

        let even0 = p(0).wrapping_add(tmp >> 1);
        let even1 = p(0).wrapping_sub(tmp);

        let t = even0;
        let even0 = t.wrapping_add(even2);
        let even2 = t.wrapping_sub(even2);

        let t = odd0;
        let odd0 = t.wrapping_add(p(1)).wrapping_add(p(3));
        let odd2 = t.wrapping_add(p(5)).wrapping_sub(p(3));

        pre_mant[0] = even0.wrapping_add(odd0);
        pre_mant[1] = even1.wrapping_add(odd1);
        pre_mant[2] = even2.wrapping_add(odd2);
        pre_mant[3] = even2.wrapping_sub(odd2);
        pre_mant[4] = even1.wrapping_sub(odd1);
        pre_mant[5] = even0.wrapping_sub(odd0);
    }

    fn decode_aht_ch(&mut self, br: &mut BitReader<'_>, ch: usize) {
        let gaq_mode = br.get_bits(2);
        let end_bap = if gaq_mode < 2 { 12 } else { 17 };
        let start = self.start_freq[ch];
        let end = self.end_freq[ch].min(BLOCK);
        let mut gaq_gain = [0i32; BLOCK];

        let mut gs = 0;
        if gaq_mode == 1 || gaq_mode == 2 {
            // 1-bit GAQ gain codes.
            for bin in start..end {
                let b = self.work.bap[ch][bin];
                if b > 7 && b < end_bap {
                    gaq_gain[gs] = (br.get_bit() as i32) << (gaq_mode - 1);
                    gs += 1;
                }
            }
        } else if gaq_mode == 3 {
            // 1.67-bit GAQ gain codes (three codes in five bits).
            let mut gc = 2;
            for bin in start..end {
                let b = self.work.bap[ch][bin];
                if b > 7 && b < 17 {
                    let g = gc;
                    gc += 1;
                    if g == 2 {
                        let group_code = (br.get_bits(5) as usize).min(26);
                        let u = UNGROUP_3_IN_5_BITS_TAB[group_code];
                        gaq_gain[gs] = u[0] as i32;
                        gaq_gain[gs + 1] = u[1] as i32;
                        gaq_gain[gs + 2] = u[2] as i32;
                        gs += 3;
                        gc = 0;
                    }
                }
            }
        }

        gs = 0;
        for bin in start..end {
            let hebap = self.work.bap[ch][bin] as usize;
            let bits = BITS_VS_HEBAP[hebap] as u32;
            let pre = &mut self.work.pre_mantissa[ch][bin];
            if hebap == 0 {
                // Zero-mantissa dithering.
                for blk in 0..6 {
                    pre[blk] = ((self.dith_state.get() & 0x7FFFFF) as i32).wrapping_sub(0x400000);
                }
            } else if hebap < 8 {
                // Vector quantisation.
                let v = br.get_bits(bits) as usize;
                let row: &[i16; 6] = match hebap {
                    1 => &VQ_HEBAP1[v & 3],
                    2 => &VQ_HEBAP2[v & 7],
                    3 => &VQ_HEBAP3[v & 15],
                    4 => &VQ_HEBAP4[v & 31],
                    5 => &VQ_HEBAP5[v & 127],
                    6 => &VQ_HEBAP6[v & 255],
                    _ => &VQ_HEBAP7[v & 511],
                };
                for blk in 0..6 {
                    pre[blk] = (row[blk] as i32) * (1 << 8);
                }
            } else {
                // Gain adaptive quantisation.
                let log_gain: i32 = if gaq_mode != 0 && (hebap as u8) < end_bap {
                    let g = gaq_gain[gs];
                    gs += 1;
                    g
                } else {
                    0
                };
                let gbits = bits as i32 - log_gain;
                for blk in 0..6 {
                    let mut mant = br.get_sbits(gbits as u32);
                    if log_gain != 0 && mant == -(1 << (gbits - 1)) {
                        // Large mantissa.
                        let mbits = bits as i32 - (2 - log_gain);
                        mant = br.get_sbits(mbits as u32);
                        mant = ((mant as u32) << (23 - (mbits - 1))) as i32;
                        let b = if mant >= 0 {
                            1i32 << (23 - log_gain)
                        } else {
                            GAQ_REMAP_2_4_B[hebap - 8][(log_gain - 1) as usize] as i32 * (1 << 8)
                        };
                        let a = GAQ_REMAP_2_4_A[hebap - 8][(log_gain - 1) as usize] as i64;
                        mant = mant
                            .wrapping_add(((a * mant as i64) >> 15) as i32)
                            .wrapping_add(b);
                    } else {
                        // Small mantissa, no GAQ, or Gk = 1.
                        mant = mant.wrapping_mul(1 << (24 - bits));
                        if log_gain == 0 {
                            let r = GAQ_REMAP_1[hebap - 8] as i64;
                            mant = mant.wrapping_add(((r * mant as i64) >> 15) as i32);
                        }
                    }
                    pre[blk] = mant;
                }
            }
            Self::idct6(pre);
        }
    }

    // ---------------------------------------------------------------------
    // Spectral extension
    // ---------------------------------------------------------------------

    fn apply_spectral_extension(&mut self) {
        let mut wrapflag = [0u8; SPX_MAX_BANDS];
        wrapflag[0] = 1;
        let mut copy_sizes = [0usize; SPX_MAX_BANDS + 2];
        let mut num_copy_sections = 0;
        let mut rms_energy = [0f32; SPX_MAX_BANDS];

        // Copy index mapping table and wrap flags (notch filter positions).
        let mut bin = self.spx_dst_start_freq;
        for bnd in 0..self.num_spx_bands {
            let bandsize = self.spx_band_sizes[bnd] as usize;
            if bin + bandsize > self.spx_src_start_freq {
                copy_sizes[num_copy_sections] = bin - self.spx_dst_start_freq;
                num_copy_sections += 1;
                bin = self.spx_dst_start_freq;
                wrapflag[bnd] = 1;
            }
            let mut i = 0;
            while i < bandsize {
                if bin == self.spx_src_start_freq {
                    copy_sizes[num_copy_sections] = bin - self.spx_dst_start_freq;
                    num_copy_sections += 1;
                    bin = self.spx_dst_start_freq;
                }
                let copysize = (bandsize - i).min(self.spx_src_start_freq - bin);
                bin += copysize;
                i += copysize;
            }
        }
        copy_sizes[num_copy_sections] = bin - self.spx_dst_start_freq;
        num_copy_sections += 1;

        for ch in 1..=self.fbw_channels {
            if !self.channel_uses_spx[ch] {
                continue;
            }
            let tc = &mut self.work.transform_coeffs[ch];

            // Copy coefficients from normal bands to extension bands.
            let mut bin = self.spx_src_start_freq;
            for &sz in &copy_sizes[..num_copy_sections] {
                let src = self.spx_dst_start_freq;
                if bin + sz <= BLOCK {
                    tc.copy_within(src..src + sz, bin);
                }
                bin += sz;
            }

            // RMS energy of each SPX band.
            let mut bin = self.spx_src_start_freq;
            for bnd in 0..self.num_spx_bands {
                let bandsize = self.spx_band_sizes[bnd] as usize;
                let mut accum = 0.0f32;
                for _ in 0..bandsize {
                    let coeff = tc[bin];
                    accum += coeff * coeff;
                    bin += 1;
                }
                rms_energy[bnd] = (accum / bandsize as f32).sqrt();
            }

            // Notch filter at transitions between normal and extension bands
            // and at all wrap points.
            if self.spx_atten_code[ch] >= 0 {
                let atten_tab = &SPX_ATTEN_TAB[self.spx_atten_code[ch] as usize];
                let mut bin = self.spx_src_start_freq - 2;
                for bnd in 0..self.num_spx_bands {
                    if wrapflag[bnd] != 0 {
                        tc[bin] *= atten_tab[0];
                        tc[bin + 1] *= atten_tab[1];
                        tc[bin + 2] *= atten_tab[2];
                        tc[bin + 3] *= atten_tab[1];
                        tc[bin + 4] *= atten_tab[0];
                    }
                    bin += self.spx_band_sizes[bnd] as usize;
                }
            }

            // Noise-blended coefficient scaling.
            let mut bin = self.spx_src_start_freq;
            for bnd in 0..self.num_spx_bands {
                let nscale =
                    self.spx_noise_blend[ch][bnd] * rms_energy[bnd] * (1.0f32 / -2147483648.0f32);
                let sscale = self.spx_signal_blend[ch][bnd];
                for _ in 0..self.spx_band_sizes[bnd] {
                    let r = self.dith_state.get() as i32;
                    let noise = nscale * (r as f32);
                    tc[bin] *= sscale;
                    tc[bin] += noise;
                    bin += 1;
                }
            }
        }
    }

    // ---------------------------------------------------------------------
    // Block decode
    // ---------------------------------------------------------------------

    fn decode_audio_block(
        &mut self,
        br: &mut BitReader<'_>,
        blk: usize,
        delay_offset: usize,
    ) -> Result<(), Error> {
        let fbw = self.fbw_channels;
        let channel_mode = self.channel_mode;
        let mut bit_alloc_stages = [0u8; MAX_CH];

        // Block switch flags.
        let mut different_transforms = false;
        if self.block_switch_syntax {
            for ch in 1..=fbw {
                self.block_switch[ch] = br.get_bit() != 0;
                if ch > 1 && self.block_switch[ch] != self.block_switch[1] {
                    different_transforms = true;
                }
            }
        }

        // Dithering flags.
        if self.dither_flag_syntax {
            for ch in 1..=fbw {
                self.dither_flag[ch] = br.get_bit() != 0;
            }
        }

        // Dynamic range.
        let mut i = (channel_mode == 0) as usize;
        loop {
            if br.get_bit() != 0 {
                let range_bits = br.get_bits(8) as usize;
                let range = self.dynamic_range_tab[range_bits];
                if range_bits <= 127 || self.drc_scale <= 1.0 {
                    self.dynamic_range[i] = range.powf(self.drc_scale);
                } else {
                    self.dynamic_range[i] = range;
                }
            } else if blk == 0 {
                self.dynamic_range[i] = 1.0;
            }
            if i == 0 {
                break;
            }
            i -= 1;
        }

        // Spectral extension strategy.
        if self.eac3 && (blk == 0 || br.get_bit() != 0) {
            self.spx_in_use = br.get_bit() != 0;
            if self.spx_in_use {
                self.spx_strategy(br, blk)?;
            }
        }
        if !self.eac3 || !self.spx_in_use {
            self.spx_in_use = false;
            for ch in 1..=fbw {
                self.channel_uses_spx[ch] = false;
                self.first_spx_coords[ch] = true;
            }
        }

        // Spectral extension coordinates.
        if self.spx_in_use {
            self.spx_coordinates(br);
        }

        // Coupling strategy.
        let cpl_strategy_exists = if self.eac3 {
            self.cpl_strategy_exists[blk]
        } else {
            br.get_bit() != 0
        };
        if cpl_strategy_exists {
            self.coupling_strategy(br, blk, &mut bit_alloc_stages)?;
        } else if !self.eac3 {
            if blk == 0 {
                return Err(Error::Invalid("coupling strategy missing in block 0"));
            }
            self.cpl_in_use[blk] = self.cpl_in_use[blk - 1];
        }
        let cpl_in_use = self.cpl_in_use[blk];
        let first_ch = if cpl_in_use { 0 } else { 1 };

        // Coupling coordinates.
        if cpl_in_use {
            self.coupling_coordinates(br, blk)?;
        }

        // Stereo rematrixing strategy and band structure.
        if channel_mode == CHMODE_STEREO {
            if (self.eac3 && blk == 0) || br.get_bit() != 0 {
                self.num_rematrixing_bands = 4;
                if cpl_in_use && self.start_freq[CPL_CH] <= 61 {
                    self.num_rematrixing_bands -= 1 + (self.start_freq[CPL_CH] == 37) as usize;
                } else if self.spx_in_use && self.spx_src_start_freq <= 61 {
                    self.num_rematrixing_bands -= 1;
                }
                for bnd in 0..self.num_rematrixing_bands {
                    self.rematrixing_flags[bnd] = br.get_bit() != 0;
                }
            } else if blk == 0 {
                self.num_rematrixing_bands = 0;
            }
        }

        // Exponent strategies for each channel.
        for ch in first_ch..=self.channels {
            if !self.eac3 {
                let bits = if ch == self.lfe_ch { 1 } else { 2 };
                self.exp_strategy[blk][ch] = br.get_bits(bits) as u8;
            }
            if self.exp_strategy[blk][ch] != EXP_REUSE {
                bit_alloc_stages[ch] = 3;
            }
        }

        // Channel bandwidth.
        for ch in 1..=fbw {
            self.start_freq[ch] = 0;
            if self.exp_strategy[blk][ch] != EXP_REUSE {
                let prev = self.end_freq[ch];
                if self.channel_in_cpl[ch] {
                    self.end_freq[ch] = self.start_freq[CPL_CH];
                } else if self.channel_uses_spx[ch] {
                    self.end_freq[ch] = self.spx_src_start_freq;
                } else {
                    let bandwidth_code = br.get_bits(6) as usize;
                    if bandwidth_code > 60 {
                        return Err(Error::Invalid("bandwidth code above 60"));
                    }
                    self.end_freq[ch] = bandwidth_code * 3 + 73;
                }
                let group_size = 3usize << (self.exp_strategy[blk][ch] - 1);
                self.num_exp_groups[ch] = (self.end_freq[ch] + group_size - 4) / group_size;
                if blk > 0 && self.end_freq[ch] != prev {
                    bit_alloc_stages = [3; MAX_CH];
                }
            }
        }
        if cpl_in_use && self.exp_strategy[blk][CPL_CH] != EXP_REUSE {
            self.num_exp_groups[CPL_CH] = (self.end_freq[CPL_CH] - self.start_freq[CPL_CH])
                / (3usize << (self.exp_strategy[blk][CPL_CH] - 1));
        }

        // Decode exponents for each channel.
        for ch in first_ch..=self.channels {
            if self.exp_strategy[blk][ch] != EXP_REUSE {
                let shift = (ch == 0) as u32;
                let absexp = (br.get_bits(4) << shift) as i32;
                self.work.dexps[ch][0] = absexp as i8;
                let start = self.start_freq[ch] + (ch != 0) as usize;
                Self::decode_exponents(
                    br,
                    self.exp_strategy[blk][ch],
                    self.num_exp_groups[ch],
                    absexp,
                    &mut self.work.dexps[ch][start..],
                )?;
                if ch != CPL_CH && ch != self.lfe_ch {
                    br.skip(2); // gainrng
                }
            }
        }

        // Bit allocation information.
        if self.bit_allocation_syntax {
            if br.get_bit() != 0 {
                self.ba.slow_decay = SLOW_DECAY_TAB[br.get_bits(2) as usize] >> self.ba.sr_shift;
                self.ba.fast_decay = FAST_DECAY_TAB[br.get_bits(2) as usize] >> self.ba.sr_shift;
                self.ba.slow_gain = SLOW_GAIN_TAB[br.get_bits(2) as usize];
                self.ba.db_per_bit = DB_PER_BIT_TAB[br.get_bits(2) as usize];
                self.ba.floor = FLOOR_TAB[br.get_bits(3) as usize];
                for ch in first_ch..=self.channels {
                    bit_alloc_stages[ch] = bit_alloc_stages[ch].max(2);
                }
            } else if blk == 0 {
                return Err(Error::Invalid("bit allocation info missing in block 0"));
            }
        }

        // SNR offsets and fast gains.
        if !self.eac3 || blk == 0 {
            if self.snr_offset_strategy != 0 && br.get_bit() != 0 {
                let mut snr = 0;
                let csnr = (br.get_bits(6) as i32 - 15) << 4;
                let i = first_ch;
                for ch in first_ch..=self.channels {
                    if ch == i || self.snr_offset_strategy == 2 {
                        snr = (csnr + br.get_bits(4) as i32) << 2;
                    }
                    if blk > 0 && self.snr_offset[ch] != snr {
                        bit_alloc_stages[ch] = bit_alloc_stages[ch].max(1);
                    }
                    self.snr_offset[ch] = snr;

                    if !self.eac3 {
                        let prev = self.fast_gain[ch];
                        self.fast_gain[ch] = FAST_GAIN_TAB[br.get_bits(3) as usize];
                        if blk > 0 && prev != self.fast_gain[ch] {
                            bit_alloc_stages[ch] = bit_alloc_stages[ch].max(2);
                        }
                    }
                }
            } else if !self.eac3 && blk == 0 {
                return Err(Error::Invalid("snr offsets missing in block 0"));
            }
        }

        // Fast gain (E-AC-3 only).
        if self.fast_gain_syntax && br.get_bit() != 0 {
            for ch in first_ch..=self.channels {
                let prev = self.fast_gain[ch];
                self.fast_gain[ch] = FAST_GAIN_TAB[br.get_bits(3) as usize];
                if blk > 0 && prev != self.fast_gain[ch] {
                    bit_alloc_stages[ch] = bit_alloc_stages[ch].max(2);
                }
            }
        } else if self.eac3 && blk == 0 {
            for ch in first_ch..=self.channels {
                self.fast_gain[ch] = FAST_GAIN_TAB[4];
            }
        }

        // E-AC-3 to AC-3 converter SNR offset.
        if self.frame_type == FRAME_TYPE_INDEPENDENT {
            if br.get_bit() != 0 {
                br.skip(10);
            }
        }

        // Coupling leak information.
        if cpl_in_use {
            if self.first_cpl_leak || br.get_bit() != 0 {
                let fl = br.get_bits(3) as i32;
                let sl = br.get_bits(3) as i32;
                if blk > 0 && (fl != self.ba.cpl_fast_leak || sl != self.ba.cpl_slow_leak) {
                    bit_alloc_stages[CPL_CH] = bit_alloc_stages[CPL_CH].max(2);
                }
                self.ba.cpl_fast_leak = fl;
                self.ba.cpl_slow_leak = sl;
            } else if !self.eac3 && blk == 0 {
                return Err(Error::Invalid("coupling leak info missing in block 0"));
            }
            self.first_cpl_leak = false;
        }

        // Delta bit allocation information.
        if self.dba_syntax && br.get_bit() != 0 {
            for ch in first_ch..=fbw {
                self.dba_mode[ch] = br.get_bits(2) as u8;
                if self.dba_mode[ch] == DBA_RESERVED {
                    return Err(Error::Invalid("reserved delta bit allocation strategy"));
                }
                bit_alloc_stages[ch] = bit_alloc_stages[ch].max(2);
            }
            for ch in first_ch..=fbw {
                if self.dba_mode[ch] == DBA_NEW {
                    self.dba_nsegs[ch] = br.get_bits(3) as usize + 1;
                    for seg in 0..self.dba_nsegs[ch] {
                        self.dba_offsets[ch][seg] = br.get_bits(5) as u8;
                        self.dba_lengths[ch][seg] = br.get_bits(4) as u8;
                        self.dba_values[ch][seg] = br.get_bits(3) as u8;
                    }
                    bit_alloc_stages[ch] = bit_alloc_stages[ch].max(2);
                }
            }
        } else if blk == 0 {
            for ch in 0..=self.channels {
                self.dba_mode[ch] = DBA_NONE;
            }
        }

        // Bit allocation.
        for ch in first_ch..=self.channels {
            if bit_alloc_stages[ch] > 2 {
                self.calc_psd(ch);
            }
            if bit_alloc_stages[ch] > 1 {
                self.calc_mask(ch)?;
            }
            if bit_alloc_stages[ch] > 0 {
                self.calc_bap(ch);
            }
        }

        // Unused dummy data.
        if self.skip_syntax && br.get_bit() != 0 {
            let skipl = br.get_bits(9) as usize;
            if skipl > 0 {
                self.skip_regions.push((br.pos(), br.pos() + 8 * skipl));
            }
            br.skip(8 * skipl);
        }

        // Unpack the transform coefficients (also uncouples channels).
        self.decode_transform_coeffs(br, blk);

        // Recover coefficients if rematrixing is in use.
        if channel_mode == CHMODE_STEREO {
            self.do_rematrixing();
        }

        // Scaling (headroom, dynamic range).
        for ch in 1..=self.channels {
            let mut audio_channel = 0;
            if channel_mode == CHMODE_DUALMONO && ch <= 2 {
                audio_channel = 2 - ch;
            }
            let gain = self.dynamic_range[audio_channel] * (1.0 / 4194304.0f32);
            let src = &self.work.fixed_coeffs[ch];
            let dst = &mut self.work.transform_coeffs[ch];
            for (d, &s) in dst.iter_mut().zip(src.iter()) {
                *d = s as f32 * gain;
            }
        }

        self.stats.blocks += 1;
        self.stats.coupling_blocks += cpl_in_use as u64;
        self.stats.spx_blocks += self.spx_in_use as u64;
        self.stats.short_blocks += (1..=fbw).filter(|&c| self.block_switch[c]).count() as u64;
        self.stats.rematrix_blocks += (channel_mode == CHMODE_STEREO
            && self.rematrixing_flags[..self.num_rematrixing_bands]
                .iter()
                .any(|&f| f)) as u64;
        self.stats.dba_blocks += (first_ch..=fbw).any(|c| {
            self.dba_mode[c] == DBA_NEW || self.dba_mode[c] == DBA_REUSE && self.dba_nsegs[c] > 0
        }) as u64;
        if blk == 0 {
            self.stats.aht_channels += (0..=self.channels)
                .filter(|&c| self.channel_uses_aht[c])
                .count() as u64;
        }

        // Spectral extension.
        if self.spx_in_use {
            self.apply_spectral_extension();
        }

        if br.overread() {
            return Err(Error::Invalid("frame data ends early"));
        }

        // IMDCT.
        if different_transforms && self.ffmpeg_compat && self.downmixed {
            self.downmixed = false;
            self.upmix_delay_quirk();
        }
        self.do_imdct(blk, delay_offset);
        Ok(())
    }

    /// FFmpeg `ac3_upmix_delay`, which it runs by mistake on the first block
    /// that mixes transform sizes (the `downmixed` flag starts out set).
    fn upmix_delay_quirk(&mut self) {
        let d = &mut self.work.delay;
        match self.channel_mode {
            0 | 2 => d[1] = d[0],
            6 => {
                d[3] = [0.0; BLOCK];
                d[2] = [0.0; BLOCK];
            }
            4 => d[2] = [0.0; BLOCK],
            7 => {
                d[4] = [0.0; BLOCK];
                d[3] = [0.0; BLOCK];
                d[2] = d[1];
                d[1] = [0.0; BLOCK];
            }
            5 => {
                d[3] = [0.0; BLOCK];
                d[2] = d[1];
                d[1] = [0.0; BLOCK];
            }
            3 => {
                d[2] = d[1];
                d[1] = [0.0; BLOCK];
            }
            _ => {}
        }
    }

    fn do_imdct(&mut self, blk: usize, delay_offset: usize) {
        let slot_len = BLOCK * AC3_MAX_BLOCKS;
        for ch in 1..=self.channels {
            let dst_start = self.dst_slot[ch - 1] * slot_len + blk * BLOCK;
            let out = &mut self.output_buffer[dst_start..dst_start + BLOCK];
            let delay = &mut self.work.delay[ch - 1 + delay_offset];
            let coeffs = &self.work.transform_coeffs[ch];
            let tmp = &mut self.work.tmp_output;
            if self.block_switch[ch] {
                let mut x = [0f32; 128];
                for i in 0..128 {
                    x[i] = coeffs[2 * i];
                }
                imdct_half(&self.dct128, &x, &mut tmp[..128]);
                fmul_window(out, &delay[..128], &tmp[..128], &self.window);
                for i in 0..128 {
                    x[i] = coeffs[2 * i + 1];
                }
                imdct_half(&self.dct128, &x, &mut delay[..128]);
            } else {
                imdct_half(&self.dct256, coeffs, &mut tmp[..]);
                fmul_window(out, &delay[..128], &tmp[..128], &self.window);
                delay[..128].copy_from_slice(&tmp[128..256]);
            }
        }
    }
}

/// Half of the IMDCT (the part the overlap-add needs): `out[n]` for
/// `n in 0..N` given `N` coefficients.
#[inline]
fn imdct_half(dct: &Dct4, coeffs: &[f32], out: &mut [f32]) {
    let n = coeffs.len();
    let mut d = [0f32; 256];
    dct.dct4(coeffs, &mut d[..n]);
    for i in 0..n {
        out[i] = IMDCT_SIGN * d[n - 1 - i];
    }
}

const IMDCT_SIGN: f32 = 1.0;

/// FFmpeg `vector_fmul_window` with `len == 128`: windowed overlap-add of the
/// previous block's delay samples with the new block's first half.
#[inline]
fn fmul_window(dst: &mut [f32], src0: &[f32], src1: &[f32], win: &[f32]) {
    const LEN: usize = 128;
    for k in 0..LEN {
        let s0 = src0[k];
        let s1 = src1[LEN - 1 - k];
        let wi = win[k];
        let wj = win[2 * LEN - 1 - k];
        dst[k] = s0 * wj - s1 * wi;
        dst[2 * LEN - 1 - k] = s0 * wi + s1 * wj;
    }
}

/// EMDF `variable_bits(n)` coding.
fn emdf_variable_bits(br: &mut BitReader<'_>, n: u32) -> u32 {
    let mut value = 0u32;
    loop {
        value = value.wrapping_add(br.get_bits(n));
        if br.get_bit() == 0 {
            break;
        }
        value = value.wrapping_shl(n).wrapping_add(1 << n);
        if value > (1 << 24) {
            break;
        }
    }
    value
}

/// Looks for an EMDF container (sync word 0x5838) between `from_bit` and the
/// CRC at the end of the frame and walks its payload list. Returns whether a
/// JOC payload (id 14) and an OAMD payload (id 11) are present. The range
/// `from_bit..to_bit` is searched.
fn scan_emdf(frame: &[u8], from_bit: usize, to_bit: usize) -> (bool, bool) {
    let frame_bits = to_bit;
    let mut joc = false;
    let mut oamd = false;
    let mut pos = from_bit;
    while pos + 48 <= to_bit {
        let mut br = BitReader::new(frame, frame_bits);
        br.skip(pos);
        if br.get_bits(16) != 0x5838 {
            pos += 1;
            continue;
        }
        let container_bytes = br.get_bits(16) as usize;
        let end = (br.pos() + container_bytes * 8).min(frame_bits);
        let mut version = br.get_bits(2);
        if version == 3 {
            version += emdf_variable_bits(&mut br, 2);
        }
        let _ = version;
        let mut key_id = br.get_bits(3);
        if key_id == 7 {
            key_id += emdf_variable_bits(&mut br, 3);
        }
        let _ = key_id;
        while br.pos() < end {
            let mut id = br.get_bits(5);
            if id == 0 {
                break;
            }
            if id == 31 {
                id += emdf_variable_bits(&mut br, 5);
            }
            // emdf_payload_config()
            let smploffste = br.get_bit() != 0;
            if smploffste {
                br.skip(12);
            }
            if br.get_bit() != 0 {
                emdf_variable_bits(&mut br, 11);
            }
            if br.get_bit() != 0 {
                emdf_variable_bits(&mut br, 2);
            }
            if br.get_bit() != 0 {
                br.skip(8);
            }
            let discard_unknown = br.get_bit() != 0;
            if !discard_unknown {
                let mut aligned = false;
                if !smploffste {
                    aligned = br.get_bit() != 0;
                    if aligned {
                        br.skip(2);
                    }
                }
                if smploffste || aligned {
                    br.skip(7);
                }
            }
            let size = emdf_variable_bits(&mut br, 8) as usize;
            match id {
                14 => joc = true,
                11 => oamd = true,
                _ => {}
            }
            br.skip(size * 8);
        }
        return (joc, oamd);
    }
    (joc, oamd)
}
