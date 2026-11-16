//! Object decoder: QMF analysis of the E-AC-3 downmix, JOC upmix with
//! temporal interpolation (clauses 6.6.5, 6.6.6) and QMF synthesis, plus the
//! OAMD metadata timeline.

use crate::emdf;
use crate::joc::{self, JocFrame, JocObject, SparseOffsetMode, MAX_OBJECTS, NUM_SUBBANDS, TIMESLOTS};
use crate::oamd::{self, ObjectState, OamdState, SpeakerLabel};
use crate::qmf::{QmfAnalysis, QmfSynthesis, QmfTables};
use aether_ac3::{Channel, DecodedFrame};

/// Samples per E-AC-3 frame with six blocks.
pub const FRAME_SAMPLES: usize = TIMESLOTS * NUM_SUBBANDS;
/// Delay of the QMF analysis + synthesis pair (measured, see tests): object
/// output sample `i + LATENCY_SAMPLES` corresponds to bed sample `i`.
pub const LATENCY_SAMPLES: usize = 577;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeError {
    /// The access unit carries no JOC payload.
    NoJoc,
    Joc(joc::JocError),
    Oamd(oamd::OamdError),
    /// Frame is not six blocks (1536 samples) or has no usable bed channels.
    UnsupportedFrame,
}

/// One OAMD metadata update of one object.
#[derive(Clone, Debug)]
pub struct MetadataUpdate {
    /// Index into [`ObjectFrame::pcm`] objects (JOC object index).
    pub object: usize,
    /// Index of the object in the OAMD payload (LFE objects are skipped by JOC).
    pub oamd_index: usize,
    /// Update start on the object PCM clock (samples since decoder start,
    /// already including [`LATENCY_SAMPLES`]).
    pub start_sample: u64,
    /// Same instant on the bed PCM clock (what OAMD signals).
    pub bed_start_sample: u64,
    /// Samples over which the properties ramp from the previous update.
    pub ramp_duration: u32,
    pub state: ObjectState,
    /// Speaker anchored position for bed objects.
    pub bed_label: Option<SpeakerLabel>,
}

/// Output of one access unit.
#[derive(Clone, Debug, Default)]
pub struct ObjectFrame {
    pub num_objects: usize,
    pub frames: usize,
    /// Planar object PCM, `pcm[obj * frames + i]`, delayed by
    /// [`LATENCY_SAMPLES`] against the bed.
    pub pcm: Vec<f32>,
    /// The bed's LFE channel bypassed through the same delay (empty if none).
    pub lfe: Vec<f32>,
    pub metadata: Vec<MetadataUpdate>,
    pub seq_count: u16,
    /// False if the frame had no (valid) JOC payload and the last matrices
    /// were held.
    pub joc_present: bool,
    pub dmx_config_idx: u8,
}

impl ObjectFrame {
    pub fn object(&self, i: usize) -> &[f32] {
        &self.pcm[i * self.frames..(i + 1) * self.frames]
    }
}

pub struct ObjectDecoder {
    tables: QmfTables,
    analysis: Vec<QmfAnalysis>,
    synthesis: Vec<QmfSynthesis>,
    lfe_delay: Vec<f32>,
    /// Previous dequantised matrix per object, `[obj][ch][sb]`.
    prev: Vec<f32>,
    prev_channels: usize,
    last_seq: Option<u16>,
    oamd_state: OamdState,
    frame_index: u64,
    sparse_mode: SparseOffsetMode,
    // scratch
    x_re: Vec<f32>,
    x_im: Vec<f32>,
    z_re: Vec<f32>,
    z_im: Vec<f32>,
}

impl Default for ObjectDecoder {
    fn default() -> Self {
        Self::new()
    }
}

/// Maps decoder planes to JOC input channels `[L R C Ls Rs (Lb Rb | Tfl Tfr)]`.
fn joc_channel_planes(bed: &DecodedFrame, cfg: u8) -> Option<[Option<usize>; 7]> {
    let l = &bed.layout;
    let side_or_back = |side: Channel, back: Channel| l.index_of(side).or_else(|| l.index_of(back));
    let mut m = [None; 7];
    m[0] = l.index_of(Channel::FrontLeft);
    m[1] = l.index_of(Channel::FrontRight);
    m[2] = l.index_of(Channel::FrontCenter);
    m[3] = side_or_back(Channel::SideLeft, Channel::BackLeft);
    m[4] = side_or_back(Channel::SideRight, Channel::BackRight);
    match cfg {
        1 => {
            m[5] = l.index_of(Channel::BackLeft);
            m[6] = l.index_of(Channel::BackRight);
        }
        2 | 4 => {
            m[5] = l.index_of(Channel::TopFrontLeft);
            m[6] = l.index_of(Channel::TopFrontRight);
        }
        _ => {}
    }
    if m[0].is_none() || m[1].is_none() {
        return None;
    }
    Some(m)
}

impl ObjectDecoder {
    pub fn new() -> Self {
        Self {
            tables: QmfTables::new(),
            analysis: vec![QmfAnalysis::default(); 7],
            synthesis: vec![QmfSynthesis::default(); MAX_OBJECTS],
            lfe_delay: vec![0.0; LATENCY_SAMPLES],
            prev: vec![0.0; MAX_OBJECTS * 7 * NUM_SUBBANDS],
            prev_channels: 5,
            last_seq: None,
            oamd_state: OamdState::default(),
            frame_index: 0,
            sparse_mode: SparseOffsetMode::ZeroPoint,
            x_re: vec![0.0; 7 * TIMESLOTS * NUM_SUBBANDS],
            x_im: vec![0.0; 7 * TIMESLOTS * NUM_SUBBANDS],
            z_re: vec![0.0; TIMESLOTS * NUM_SUBBANDS],
            z_im: vec![0.0; TIMESLOTS * NUM_SUBBANDS],
        }
    }

    pub fn set_sparse_offset_mode(&mut self, mode: SparseOffsetMode) {
        self.sparse_mode = mode;
    }

    pub fn reset(&mut self) {
        self.analysis.iter_mut().for_each(|a| a.reset());
        self.synthesis.iter_mut().for_each(|s| s.reset());
        self.lfe_delay.iter_mut().for_each(|v| *v = 0.0);
        self.prev.iter_mut().for_each(|v| *v = 0.0);
        self.last_seq = None;
        self.oamd_state.reset();
        self.frame_index = 0;
    }

    /// Decodes the objects of one access unit. `au` is the raw E-AC-3 access
    /// unit (independent frame plus dependent frames) that was given to
    /// `aether_ac3::Decoder::decode`, and `bed` its result.
    pub fn process(&mut self, au: &[u8], bed: &DecodedFrame) -> Result<ObjectFrame, DecodeError> {
        let payloads = emdf::extract_from_access_unit(au);
        self.process_payloads(payloads.joc.as_deref(), payloads.oamd.as_deref(), bed)
    }

    /// Like [`process`](Self::process) with the EMDF payloads already extracted.
    pub fn process_payloads(
        &mut self,
        joc_payload: Option<&[u8]>,
        oamd_payload: Option<&[u8]>,
        bed: &DecodedFrame,
    ) -> Result<ObjectFrame, DecodeError> {
        if bed.frames != FRAME_SAMPLES {
            return Err(DecodeError::UnsupportedFrame);
        }
        let parsed = match joc_payload {
            Some(p) => Some(joc::parse_joc(p, self.sparse_mode).map_err(DecodeError::Joc)?),
            None => None,
        };
        let held_cfg = self.prev_channels;
        let (num_channels, cfg) = match &parsed {
            Some(f) => (f.num_channels, f.dmx_config_idx),
            None => (held_cfg, if held_cfg == 5 { 0 } else { 1 }),
        };
        let planes = joc_channel_planes(bed, cfg).ok_or(DecodeError::UnsupportedFrame)?;
        let num_objects = parsed.as_ref().map(|f| f.num_objects).unwrap_or(0);
        let mut out = ObjectFrame {
            num_objects,
            frames: FRAME_SAMPLES,
            joc_present: parsed.is_some(),
            dmx_config_idx: cfg,
            ..Default::default()
        };

        // Splice / first frame handling (clause 6.3.3.3).
        if let Some(f) = &parsed {
            let consecutive = match self.last_seq {
                Some(prev) => f.seq_count == prev.wrapping_add(1) || (prev == 1023 && f.seq_count == 1),
                None => true,
            };
            if f.seq_count == 0 || !consecutive || num_channels != self.prev_channels {
                self.prev.iter_mut().for_each(|v| *v = 0.0);
            }
            self.last_seq = Some(f.seq_count);
            self.prev_channels = num_channels;
            out.seq_count = f.seq_count;
        }
        let Some(parsed) = parsed else {
            // No payload: without side information nothing can be rebuilt.
            self.frame_index += 1;
            self.push_lfe(bed, &mut out);
            return Ok(out);
        };

        // QMF analysis of the downmix channels.
        for ch in 0..num_channels {
            let base = ch * TIMESLOTS * NUM_SUBBANDS;
            match planes[ch] {
                Some(p) => {
                    let pcm = bed.channel(p);
                    for ts in 0..TIMESLOTS {
                        let o = base + ts * NUM_SUBBANDS;
                        let (re, im) = (&mut self.x_re[o..o + NUM_SUBBANDS], &mut self.x_im[o..o + NUM_SUBBANDS]);
                        self.analysis[ch].process(&self.tables, &pcm[ts * 64..ts * 64 + 64], re, im);
                    }
                }
                None => {
                    self.x_re[base..base + TIMESLOTS * NUM_SUBBANDS].iter_mut().for_each(|v| *v = 0.0);
                    self.x_im[base..base + TIMESLOTS * NUM_SUBBANDS].iter_mut().for_each(|v| *v = 0.0);
                }
            }
        }

        out.pcm = vec![0.0; num_objects * FRAME_SAMPLES];
        for obj in 0..num_objects {
            self.upmix_object(&parsed, obj, num_channels);
            for ts in 0..TIMESLOTS {
                let o = ts * NUM_SUBBANDS;
                let dst = obj * FRAME_SAMPLES + ts * 64;
                self.synthesis[obj].process(
                    &self.tables,
                    &self.z_re[o..o + NUM_SUBBANDS],
                    &self.z_im[o..o + NUM_SUBBANDS],
                    &mut out.pcm[dst..dst + 64],
                );
            }
        }
        self.push_lfe(bed, &mut out);

        // Metadata.
        if let Some(p) = oamd_payload {
            if let Ok(o) = oamd::parse_oamd(p, &mut self.oamd_state) {
                self.append_metadata(&o, num_objects, &mut out);
            }
        }
        self.frame_index += 1;
        Ok(out)
    }

    fn push_lfe(&mut self, bed: &DecodedFrame, out: &mut ObjectFrame) {
        let Some(i) = bed.layout.index_of(Channel::LowFrequency) else { return };
        let x = bed.channel(i);
        let n = x.len();
        let d = LATENCY_SAMPLES;
        // y[k] = x[k - d]; the first d samples come from the carried history.
        let mut cat = Vec::with_capacity(d + n);
        cat.extend_from_slice(&self.lfe_delay);
        cat.extend_from_slice(x);
        let y = cat[..n].to_vec();
        self.lfe_delay.copy_from_slice(&cat[n..n + d]);
        out.lfe = y;
    }

    /// Computes `z_re/z_im` (`[ts][sb]`) of object `obj` from `x_*` with the
    /// interpolated reconstruction matrix.
    fn upmix_object(&mut self, f: &JocFrame, obj: usize, nch: usize) {
        self.z_re.iter_mut().for_each(|v| *v = 0.0);
        self.z_im.iter_mut().for_each(|v| *v = 0.0);
        let o: &JocObject = &f.objects[obj];
        let sb_pb = if o.present { joc::sb_to_pb_table(o.num_bands_idx) } else { [0u8; NUM_SUBBANDS] };
        for ch in 0..nch {
            let prev_off = (obj * 7 + ch) * NUM_SUBBANDS;
            let mut p = [0f32; NUM_SUBBANDS];
            p.copy_from_slice(&self.prev[prev_off..prev_off + NUM_SUBBANDS]);
            let mut t0 = [0f32; NUM_SUBBANDS];
            let mut t1 = [0f32; NUM_SUBBANDS];
            if o.present {
                let nb = o.num_bands;
                for sb in 0..NUM_SUBBANDS {
                    let pb = sb_pb[sb] as usize;
                    t0[sb] = o.dq[ch * nb + pb];
                    t1[sb] = if o.num_dpoints > 1 { o.dq[(nch + ch) * nb + pb] } else { t0[sb] };
                }
            }
            let last = if o.present && o.num_dpoints > 1 { t1 } else { t0 };
            let all_zero = p.iter().all(|&v| v == 0.0) && last.iter().all(|&v| v == 0.0) && t0.iter().all(|&v| v == 0.0);
            if !all_zero {
                let xb = ch * TIMESLOTS * NUM_SUBBANDS;
                let slope = if o.present { o.slope_idx } else { 0 };
                let ndp = if o.present { o.num_dpoints } else { 1 };
                let mut coef = [0f32; NUM_SUBBANDS];
                for ts in 0..TIMESLOTS {
                    // Pseudocode 6.
                    if slope == 0 {
                        if ndp == 1 {
                            let w = (ts + 1) as f32 / TIMESLOTS as f32;
                            for sb in 0..NUM_SUBBANDS {
                                coef[sb] = p[sb] + w * (t0[sb] - p[sb]);
                            }
                        } else {
                            let ts2 = TIMESLOTS / 2;
                            if ts < ts2 {
                                let w = (ts + 1) as f32 / ts2 as f32;
                                for sb in 0..NUM_SUBBANDS {
                                    coef[sb] = p[sb] + w * (t0[sb] - p[sb]);
                                }
                            } else {
                                let w = (ts - ts2 + 1) as f32 / (TIMESLOTS - ts2) as f32;
                                for sb in 0..NUM_SUBBANDS {
                                    coef[sb] = t0[sb] + w * (t1[sb] - t0[sb]);
                                }
                            }
                        }
                    } else if ts < o.offset_ts[0] {
                        coef = p;
                    } else if ndp == 1 || ts < o.offset_ts[1] {
                        coef = t0;
                    } else {
                        coef = t1;
                    }
                    let xo = xb + ts * NUM_SUBBANDS;
                    let zo = ts * NUM_SUBBANDS;
                    let xr = &self.x_re[xo..xo + NUM_SUBBANDS];
                    let xi = &self.x_im[xo..xo + NUM_SUBBANDS];
                    let zr = &mut self.z_re[zo..zo + NUM_SUBBANDS];
                    let zi = &mut self.z_im[zo..zo + NUM_SUBBANDS];
                    for sb in 0..NUM_SUBBANDS {
                        zr[sb] += xr[sb] * coef[sb];
                        zi[sb] += xi[sb] * coef[sb];
                    }
                }
            }
            self.prev[prev_off..prev_off + NUM_SUBBANDS].copy_from_slice(&last);
        }
    }

    fn append_metadata(&mut self, o: &oamd::OamdFrame, num_joc_objects: usize, out: &mut ObjectFrame) {
        let base = self.frame_index * FRAME_SAMPLES as u64;
        // JOC objects are all OAMD objects except bed LFE channels.
        let mut joc_idx = 0usize;
        for (oi, blocks) in o.objects.iter().enumerate() {
            if blocks.first().map(|b| b.bed_label.map(|l| l.is_lfe()).unwrap_or(false)).unwrap_or(false) {
                continue;
            }
            if joc_idx >= num_joc_objects {
                break;
            }
            for (bi, b) in blocks.iter().enumerate() {
                let t = &o.timing[bi.min(o.timing.len() - 1)];
                let bed_start = base + t.start_sample as u64;
                out.metadata.push(MetadataUpdate {
                    object: joc_idx,
                    oamd_index: oi,
                    start_sample: bed_start + LATENCY_SAMPLES as u64,
                    bed_start_sample: bed_start,
                    ramp_duration: t.ramp_duration,
                    state: b.state,
                    bed_label: b.bed_label,
                });
            }
            joc_idx += 1;
        }
    }
}
