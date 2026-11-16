//! Top level DTS-UHD frame parser and decoder.
//!
//! `Decoder::parse_frame` walks the FTOC, metadata chunks and the ACE frame header and
//! never decodes audio. `Decoder::decode_frame` additionally runs the ACE LFE and
//! full bandwidth stream decoders and returns planar PCM.

use crate::ace::*;
use crate::aceframe::*;
use crate::ftoc::*;
use crate::layout::*;
use crate::lfe::LfeChannel;
use crate::metadata::*;

#[derive(Debug, Clone, PartialEq)]
pub struct AceInfo {
    pub sampling_rate: u32,
    pub enable_deemphasis: bool,
    pub num_lfe_streams: usize,
    pub num_mono_streams: usize,
    pub num_stereo_streams: usize,
    pub mono_bandwidth_mode: Vec<usize>,
    pub stereo_bandwidth_mode: Vec<usize>,
    /// Sizes in bytes of the stream payloads of this frame.
    pub lfe_sizes: Vec<usize>,
    pub mono_sizes: Vec<usize>,
    pub stereo_sizes: Vec<usize>,
    pub has_padding: bool,
    /// Padding length read from the variable length code that follows the stream payloads
    /// (it counts the code itself), when `has_padding` is set.
    pub padding_bytes: Option<usize>,
    /// True when the padding length equals the number of bytes left in the audio chunk.
    pub padding_consistent: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct StreamInfo {
    pub is_sync: bool,
    pub sample_rate: u32,
    pub frame_duration_samples: u32,
    pub full_channel_based_mix: bool,
    pub version: (u32, u32),
    pub num_audio_presentations: u32,
    pub presentation_selectable: Vec<bool>,
    pub num_metadata_chunks: usize,
    pub audio_chunks: Vec<AudioChunkNav>,
    pub metadata: Vec<Md01Chunk>,
    /// Channel activity mask of the first channel mask based object, if any.
    pub channel_mask: Option<u32>,
    pub channel_labels: Vec<&'static str>,
    pub layout_name: Option<String>,
    pub object_count: usize,
    pub ace: Option<AceInfo>,
    pub frame_bytes: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChannelStatus {
    /// Decoded from the bitstream.
    Decoded,
    /// The encoder switched the stream off for this frame.
    CodedOff,
    /// Nothing available yet (decoder delay) or after a reset.
    NoData,
    /// The stream used a tool the published specification does not define well enough.
    Unsupported(&'static str),
    /// A previous frame of this stream was not decodable; prediction state is lost until
    /// the next sync frame.
    Desynchronized,
}

#[derive(Debug, Clone)]
pub struct DecodedFrame {
    pub info: StreamInfo,
    /// Planar PCM, one vector per channel in `info.channel_labels` order, `frame_duration` samples
    /// each. The ACE codec has a delay of one frame, so these samples belong to the previous frame.
    pub pcm: Vec<Vec<f32>>,
    pub status: Vec<ChannelStatus>,
}

pub struct Decoder {
    ftoc: FtocState,
    ace_hdr: AceHeaderState,
    lfe: Vec<LfeChannel>,
    mono: Vec<AceStream>,
    stereo: Vec<AceStream>,
    poisoned_mono: Vec<bool>,
    poisoned_stereo: Vec<bool>,
    selectable: bool,
    channel_mask: Option<u32>,
}

impl Default for Decoder {
    fn default() -> Self {
        Self::new()
    }
}

/// Output scale of the ACE full bandwidth decoder (PCM is produced in 16 bit full scale units).
pub const ACE_PCM_SCALE: f32 = 1.0 / 32768.0;

impl Decoder {
    pub fn new() -> Self {
        Decoder {
            ftoc: FtocState::new(),
            ace_hdr: AceHeaderState::default(),
            lfe: vec![],
            mono: vec![],
            stereo: vec![],
            poisoned_mono: vec![],
            poisoned_stereo: vec![],
            selectable: true,
            channel_mask: None,
        }
    }

    /// Parses structure only.
    pub fn parse_frame(&mut self, data: &[u8]) -> Result<StreamInfo, ParseError> {
        let (info, _, _) = self.parse_inner(data)?;
        Ok(info)
    }

    fn parse_inner(
        &mut self,
        data: &[u8],
    ) -> Result<(StreamInfo, Ftoc, Option<(AceFrameHeader, usize, usize)>), ParseError> {
        let ft = parse_ftoc(data, &mut self.ftoc)?;
        if data.len() < ft.total_bytes {
            return Err(ParseError::Truncated);
        }
        let mut pos = ft.ftoc_bytes;
        let mut metadata = Vec::new();
        let selectable = ft.presentation_selectable.first().copied().unwrap_or(true);
        self.selectable = selectable;
        for nav in ft.md_chunks.iter() {
            let size = nav.size as usize;
            if pos + size > data.len() {
                return Err(ParseError::Truncated);
            }
            if let Some(c) = parse_md_chunk(
                &data[pos..pos + size],
                size,
                nav.crc_flag,
                ft.is_sync,
                ft.full_channel_based_mix,
                ft.frame_duration,
                selectable,
                ft.interact_obj_limits_present,
            )? {
                metadata.push(c);
            }
            pos += size;
        }
        let mut ace: Option<AceInfo> = None;
        let mut ace_loc: Option<(AceFrameHeader, usize, usize)> = None;
        for nav in ft.audio_chunks.iter() {
            let size = nav.size as usize;
            if pos + size > data.len() {
                return Err(ParseError::Truncated);
            }
            if nav.id == 0x01 && ace.is_none() {
                let chunk = &data[pos..pos + size];
                let h = parse_ace_frame_header(chunk, &mut self.ace_hdr).map_err(|e| match e {
                    AceHeaderError::Truncated => ParseError::Truncated,
                    AceHeaderError::Reserved(s) => ParseError::Invalid(s),
                    AceHeaderError::Unsupported(s) => ParseError::Unsupported(s),
                })?;
                let sh = &h.streamsets[0];
                let payload: usize = sh
                    .lfe_sizes
                    .iter()
                    .chain(sh.mono_sizes.iter())
                    .chain(sh.stereo_sizes.iter())
                    .sum();
                if h.header_bytes + payload > size {
                    return Err(ParseError::Invalid(
                        "ACE stream payloads exceed the audio chunk",
                    ));
                }
                let used = h.header_bytes + payload;
                let (padding_bytes, padding_consistent) = if h.has_padding {
                    let mut pb = crate::bitreader::BitReader::new(&data[pos + used..pos + size]);
                    let v = pb.read_vlc(&[7, 31], &[]) as usize;
                    (Some(v), v == size - used)
                } else {
                    (None, size == used)
                };
                ace = Some(AceInfo {
                    sampling_rate: h.sampling_rate,
                    enable_deemphasis: h.enable_deemphasis,
                    num_lfe_streams: sh.num_lfe_streams,
                    num_mono_streams: sh.num_mono_streams,
                    num_stereo_streams: sh.num_stereo_streams,
                    mono_bandwidth_mode: sh.mono_bandwidth_mode.clone(),
                    stereo_bandwidth_mode: sh.stereo_bandwidth_mode.clone(),
                    lfe_sizes: sh.lfe_sizes.clone(),
                    mono_sizes: sh.mono_sizes.clone(),
                    stereo_sizes: sh.stereo_sizes.clone(),
                    has_padding: h.has_padding,
                    padding_bytes,
                    padding_consistent,
                });
                ace_loc = Some((h, pos, size));
            }
            pos += size;
        }
        let mut channel_mask = None;
        let mut object_count = 0;
        for c in metadata.iter() {
            for o in c.objects.iter() {
                object_count += 1;
                if channel_mask.is_none() {
                    channel_mask = o.ch_mask;
                }
            }
        }
        if channel_mask.is_some() {
            self.channel_mask = channel_mask;
        } else if ft.full_channel_based_mix {
            channel_mask = self.channel_mask;
        }
        let info = StreamInfo {
            is_sync: ft.is_sync,
            sample_rate: ft.sample_rate,
            frame_duration_samples: ft.frame_duration * ft.sample_rate / ft.clock_rate.max(1),
            full_channel_based_mix: ft.full_channel_based_mix,
            version: (ft.major_version, ft.minor_revision),
            num_audio_presentations: ft.num_audio_presentations,
            presentation_selectable: ft.presentation_selectable.clone(),
            num_metadata_chunks: ft.md_chunks.len(),
            audio_chunks: ft.audio_chunks.clone(),
            metadata,
            channel_mask,
            channel_labels: channel_mask.map(labels_for_mask).unwrap_or_default(),
            layout_name: channel_mask.map(layout_name),
            object_count,
            ace,
            frame_bytes: ft.total_bytes,
        };
        Ok((info, ft, ace_loc))
    }

    /// Parses and decodes one frame.
    pub fn decode_frame(&mut self, data: &[u8]) -> Result<DecodedFrame, ParseError> {
        let (mut info, ft, ace_loc) = self.parse_inner(data)?;
        let n = info.frame_duration_samples as usize;
        let Some((h, chunk_pos, chunk_size)) = ace_loc else {
            return Err(ParseError::Unsupported(
                "no ACE audio chunk (only audio chunk type 0x01 is implemented)",
            ));
        };
        if n != 1024 {
            return Err(ParseError::Unsupported(
                "ACE frame duration other than 1024",
            ));
        }
        let sh = &h.streamsets[0];
        let chunk = &data[chunk_pos..chunk_pos + chunk_size];
        if h.is_sync {
            self.lfe.resize_with(sh.num_lfe_streams, LfeChannel::new);
            self.mono = (0..sh.num_mono_streams)
                .map(|_| AceStream::new(false))
                .collect();
            self.stereo = (0..sh.num_stereo_streams)
                .map(|_| AceStream::new(true))
                .collect();
            self.poisoned_mono = vec![false; sh.num_mono_streams];
            self.poisoned_stereo = vec![false; sh.num_stereo_streams];
        }
        if self.lfe.len() != sh.num_lfe_streams
            || self.mono.len() != sh.num_mono_streams
            || self.stereo.len() != sh.num_stereo_streams
        {
            return Err(ParseError::Invalid(
                "stream composition changed without a sync frame",
            ));
        }
        // channel slots in waveform order
        let labels = info.channel_labels.clone();
        let total_ch = sh.num_lfe_streams + sh.num_mono_streams + 2 * sh.num_stereo_streams;
        let mut pcm: Vec<Vec<f32>> = vec![vec![0.0; n]; total_ch];
        let mut status: Vec<ChannelStatus> = vec![ChannelStatus::NoData; total_ch];
        // slot assignment: LFE labels, single labels, pairs, each in mask order
        let mut lfe_slots = vec![];
        let mut mono_slots = vec![];
        let mut stereo_slots = vec![];
        {
            let mut idx = 0usize;
            let mut mask = info.channel_mask.unwrap_or(0);
            if labels.len() != total_ch || mask == 0 {
                mask = 0;
            }
            if mask != 0 {
                for m in MASK_BITS.iter() {
                    if mask & m.bit == 0 {
                        continue;
                    }
                    if m.bit == 0x8 || m.bit == 0x1000 {
                        lfe_slots.push(idx);
                    } else if m.labels.len() == 1 {
                        mono_slots.push(idx);
                    } else {
                        stereo_slots.push(idx);
                    }
                    idx += m.labels.len();
                }
            }
            if lfe_slots.len() != sh.num_lfe_streams
                || mono_slots.len() != sh.num_mono_streams
                || stereo_slots.len() != sh.num_stereo_streams
            {
                // fall back to stream order
                lfe_slots = (0..sh.num_lfe_streams).collect();
                mono_slots = (0..sh.num_mono_streams)
                    .map(|i| sh.num_lfe_streams + i)
                    .collect();
                stereo_slots = (0..sh.num_stereo_streams)
                    .map(|i| sh.num_lfe_streams + sh.num_mono_streams + 2 * i)
                    .collect();
                info.channel_labels.clear();
            }
        }
        let mut p = h.header_bytes;
        // LFE
        for (i, &sz) in sh.lfe_sizes.iter().enumerate() {
            let payload = &chunk[p..p + sz];
            p += sz;
            let mut br = crate::bitreader::BitReader::new(payload);
            let nch = sh.num_lfe_channels.get(i).copied().unwrap_or(1);
            for c in 0..nch {
                let cur = self.lfe[i].read_decode(&mut br, 1024);
                let out = self.lfe[i].push_and_upsample(&cur, 1024);
                let slot = lfe_slots[i] + c;
                if slot < total_ch {
                    pcm[slot] = out;
                    status[slot] = ChannelStatus::Decoded;
                }
            }
        }
        let ctx = |bw: usize| FrameCtx {
            sampling_rate: h.sampling_rate,
            is_predictive: sh.is_predictive,
            enable_deemphasis: h.enable_deemphasis,
            bandwidth_mode: bw,
        };
        for (i, &sz) in sh.mono_sizes.iter().enumerate() {
            let payload = &chunk[p..p + sz];
            p += sz;
            let slot = mono_slots[i];
            Self::run_stream(
                &mut self.mono[i],
                &mut self.poisoned_mono[i],
                payload,
                &ctx(sh.mono_bandwidth_mode[i]),
                h.is_sync,
                &mut pcm,
                &mut status,
                &[slot],
            );
        }
        for (i, &sz) in sh.stereo_sizes.iter().enumerate() {
            let payload = &chunk[p..p + sz];
            p += sz;
            let slot = stereo_slots[i];
            Self::run_stream(
                &mut self.stereo[i],
                &mut self.poisoned_stereo[i],
                payload,
                &ctx(sh.stereo_bandwidth_mode[i]),
                h.is_sync,
                &mut pcm,
                &mut status,
                &[slot, slot + 1],
            );
        }
        let _ = ft;
        Ok(DecodedFrame { info, pcm, status })
    }

    #[allow(clippy::too_many_arguments)]
    fn run_stream(
        s: &mut AceStream,
        poisoned: &mut bool,
        payload: &[u8],
        ctx: &FrameCtx,
        is_sync: bool,
        pcm: &mut [Vec<f32>],
        status: &mut [ChannelStatus],
        slots: &[usize],
    ) {
        if is_sync {
            s.reset_parse_state();
            *poisoned = false;
        }
        if *poisoned {
            for &sl in slots {
                if sl < status.len() {
                    status[sl] = ChannelStatus::Desynchronized;
                }
            }
            return;
        }
        match s.decode_frame(payload, ctx) {
            Ok(t) => {
                for (c, &sl) in slots.iter().enumerate() {
                    if sl >= pcm.len() {
                        continue;
                    }
                    if s.out_valid {
                        pcm[sl].copy_from_slice(
                            &s.out[c]
                                .iter()
                                .map(|v| v * ACE_PCM_SCALE)
                                .collect::<Vec<_>>(),
                        );
                        status[sl] = if t.coding_mode == CODING_OFF {
                            ChannelStatus::CodedOff
                        } else {
                            ChannelStatus::Decoded
                        };
                    } else {
                        status[sl] = ChannelStatus::NoData;
                    }
                }
            }
            Err(AceError::Unsupported(why)) => {
                *poisoned = true;
                for &sl in slots {
                    if sl < status.len() {
                        status[sl] = ChannelStatus::Unsupported(why);
                    }
                }
            }
            Err(AceError::Invalid(why)) => {
                *poisoned = true;
                for &sl in slots {
                    if sl < status.len() {
                        status[sl] = ChannelStatus::Unsupported(why);
                    }
                }
            }
        }
    }
}
