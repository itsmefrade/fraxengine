//! ACE frame and stream set headers (ETSI TS 103 491 clauses 9.3 to 9.5).

use crate::bitreader::BitReader;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StreamSetHeader {
    pub id: u32,
    pub is_predictive: bool,
    pub num_lfe_streams: usize,
    pub num_mono_streams: usize,
    pub num_stereo_streams: usize,
    pub num_lfe_channels: Vec<usize>,
    pub mono_bandwidth_mode: Vec<usize>,
    pub stereo_bandwidth_mode: Vec<usize>,
    pub lfe_sizes: Vec<usize>,
    pub mono_sizes: Vec<usize>,
    pub stereo_sizes: Vec<usize>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AceFrameHeader {
    pub is_sync: bool,
    pub sampling_rate: u32,
    pub frame_duration: u32,
    pub enable_deemphasis: bool,
    pub has_padding: bool,
    pub streamsets: Vec<StreamSetHeader>,
    /// Byte offset of the first stream payload within the ACE frame.
    pub header_bytes: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AceHeaderError {
    Truncated,
    Reserved(&'static str),
    Unsupported(&'static str),
}

/// Persistent header state (previous frame).
#[derive(Debug, Clone, Default)]
pub struct AceHeaderState {
    pub valid: bool,
    pub sampling_rate: u32,
    pub frame_duration: u32,
    pub enable_deemphasis: bool,
    pub streamsets: Vec<StreamSetHeader>,
}

fn read_stream_information(
    br: &mut BitReader<'_>,
    h: &mut StreamSetHeader,
) -> Result<(), AceHeaderError> {
    let code = br.read_limits_vlc_default();
    let (lfe, mono, stereo) = match code {
        0 => (0, 1, 0),
        1 => (0, 0, 1),
        2 => (1, 1, 2),
        3 => (1, 1, 3),
        4 => (1, 1, 5),
        5 => (0, 1, 2),
        6 => (0, 0, 4),
        7 => (1, 1, 4),
        8 => (1, 2, 3),
        9 => (1, 2, 4),
        10 => {
            let n = br.read_limits_vlc_default() as usize;
            if n > 16 {
                return Err(AceHeaderError::Unsupported("too many LFE streams"));
            }
            h.num_lfe_channels = (0..n)
                .map(|_| 1 + br.read_limits_vlc_default() as usize)
                .collect();
            let mono = br.read_limits_vlc_default() as usize;
            let stereo = br.read_limits_vlc_default() as usize;
            h.num_lfe_streams = n;
            h.num_mono_streams = mono.min(64);
            h.num_stereo_streams = stereo.min(64);
            return Ok(());
        }
        _ => return Err(AceHeaderError::Reserved("stream information code")),
    };
    h.num_lfe_streams = lfe;
    h.num_mono_streams = mono;
    h.num_stereo_streams = stereo;
    h.num_lfe_channels = vec![1; lfe];
    Ok(())
}

fn read_bandwidth_modes(br: &mut BitReader<'_>, h: &mut StreamSetHeader) {
    let (m, s) = (h.num_mono_streams, h.num_stereo_streams);
    h.mono_bandwidth_mode = vec![0; m];
    h.stereo_bandwidth_mode = vec![0; s];
    if m + s == 1 {
        let bw = br.read_uint(2) as usize;
        if m == 1 {
            h.mono_bandwidth_mode[0] = bw;
        }
        if s == 1 {
            h.stereo_bandwidth_mode[0] = bw;
        }
    } else if !br.read_bool() {
        let mb = br.read_uint(2) as usize;
        let sb = br.read_uint(2) as usize;
        h.mono_bandwidth_mode.iter_mut().for_each(|v| *v = mb);
        h.stereo_bandwidth_mode.iter_mut().for_each(|v| *v = sb);
    } else {
        for v in h.mono_bandwidth_mode.iter_mut() {
            *v = br.read_uint(2) as usize;
        }
        for v in h.stereo_bandwidth_mode.iter_mut() {
            *v = br.read_uint(2) as usize;
        }
    }
}

/// Parses an ACE frame header from the start of `data`.
pub fn parse_ace_frame_header(
    data: &[u8],
    state: &mut AceHeaderState,
) -> Result<AceFrameHeader, AceHeaderError> {
    let mut br = BitReader::new(data);
    let is_sync = br.read_bool();
    let mut h = AceFrameHeader {
        is_sync,
        ..Default::default()
    };
    if is_sync {
        // enable_deemphasis: ones terminated by a zero
        let mut coded = 0;
        while coded <= 3 {
            if br.read_bit() == 0 {
                break;
            }
            coded += 1;
        }
        match coded {
            0 => h.enable_deemphasis = true,
            1 => h.enable_deemphasis = false,
            _ => return Err(AceHeaderError::Reserved("enable_deemphasis")),
        }
        if br.read_unary(2) != 0 {
            return Err(AceHeaderError::Reserved("frame duration"));
        }
        h.frame_duration = 1024;
        h.sampling_rate = if br.read_unary(2) == 0 { 48000 } else { 44100 };
    } else {
        if !state.valid {
            return Err(AceHeaderError::Unsupported(
                "non-sync ACE frame without state",
            ));
        }
        h.frame_duration = state.frame_duration;
        h.sampling_rate = state.sampling_rate;
        h.enable_deemphasis = state.enable_deemphasis;
    }
    // Observed in real streams: the number of stream sets and the stream set identifier are
    // each coded as a single '1' bit when there is one stream set with identifier 0. Other
    // values are not decodable from the published text, see DESIGN.md.
    if br.read_bit() != 1 {
        return Err(AceHeaderError::Unsupported("more than one ACE stream set"));
    }
    h.has_padding = br.read_bool();
    let mut sh = StreamSetHeader::default();
    if br.read_bit() != 1 {
        return Err(AceHeaderError::Unsupported(
            "ACE stream set identifier other than zero",
        ));
    }
    sh.id = 0;
    let prev = if is_sync {
        None
    } else {
        state.streamsets.iter().find(|s| s.id == sh.id)
    };
    if let Some(p) = prev {
        sh.is_predictive = true;
        sh.num_lfe_streams = p.num_lfe_streams;
        sh.num_mono_streams = p.num_mono_streams;
        sh.num_stereo_streams = p.num_stereo_streams;
        sh.num_lfe_channels = p.num_lfe_channels.clone();
        sh.mono_bandwidth_mode = p.mono_bandwidth_mode.clone();
        sh.stereo_bandwidth_mode = p.stereo_bandwidth_mode.clone();
    } else {
        read_stream_information(&mut br, &mut sh)?;
        read_bandwidth_modes(&mut br, &mut sh);
    }
    sh.lfe_sizes = (0..sh.num_lfe_streams)
        .map(|_| br.read_non_uniform_five_ten() as usize)
        .collect();
    sh.mono_sizes = (0..sh.num_mono_streams)
        .map(|_| br.read_vlc(&[2, 7, 9, 13], &[24, 32]) as usize)
        .collect();
    sh.stereo_sizes = (0..sh.num_stereo_streams)
        .map(|_| br.read_vlc(&[2, 7, 9, 13], &[24, 32]) as usize)
        .collect();
    if br.overrun() {
        return Err(AceHeaderError::Truncated);
    }
    br.align();
    h.header_bytes = br.bits_used() / 8;
    h.streamsets.push(sh);
    state.valid = true;
    state.sampling_rate = h.sampling_rate;
    state.frame_duration = h.frame_duration;
    state.enable_deemphasis = h.enable_deemphasis;
    state.streamsets = h.streamsets.clone();
    Ok(h)
}
