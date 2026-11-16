//! One packet-in, planar-PCM-out interface over the TrueHD, DTS, AC-3 / E-AC-3 and LPCM decoders.

use crate::dts::{Decoded, DtsSession};
use crate::layout;
use crate::pcm::{PcmDecoder, PcmParams};
use crate::{AudioError, TrueHdDecoder};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Codec {
    /// `selection` is the TrueHD presentation index (3 is the usual full object presentation).
    TrueHd { presentation: u8 },
    Dts,
    /// DTS-HD MA with the standard 7.1.4 height presentation (DTS:X).
    DtsX,
    /// AC-3 (Dolby Digital), decoded by `aether-ac3`.
    Ac3,
    /// E-AC-3 (Dolby Digital Plus, incl. the 5.1/7.1 bed of Atmos JOC), decoded by `aether-ac3`.
    Eac3,
    /// LPCM, see [`crate::pcm`].
    Pcm(PcmParams),
}

/// Decoded samples, one `Vec` per channel in `labels` order.
#[derive(Clone, Debug, PartialEq)]
pub enum Planar {
    /// TrueHD and DTS-HD MA lossless output: signed 24-bit values, not left-shifted.
    I32(Vec<Vec<i32>>),
    F32(Vec<Vec<f32>>),
}

#[derive(Clone, Debug, PartialEq)]
pub struct PcmBlock {
    pub sample_rate: u32,
    pub frames: usize,
    pub labels: Vec<String>,
    pub samples: Planar,
    /// True when the f32 values are an exact image of the coded samples (24-bit integers, f32).
    pub lossless: bool,
    /// E-AC-3 only: the stream signals Dolby Atmos (JOC).
    pub joc: bool,
    /// E-AC-3 JOC: number of objects decoded for this block (0 when the bed was returned).
    pub joc_objects: usize,
    /// E-AC-3 JOC: the block is the objects rendered to 7.1.4 (`labels` are the 7.1.4 speakers) and not the bed.
    pub joc_rendered: bool,
}

impl PcmBlock {
    pub fn channels(&self) -> usize {
        self.labels.len()
    }
    /// Normalised f32 planes (24-bit integers divided by 2^23).
    pub fn to_f32(&self) -> Vec<Vec<f32>> {
        match &self.samples {
            Planar::F32(planes) => planes.clone(),
            Planar::I32(planes) => planes
                .iter()
                .map(|p| p.iter().map(|&v| v as f32 / 8_388_608.0).collect())
                .collect(),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum DecodeError {
    TrueHd(AudioError),
    Dts(String),
    Pcm(String),
    Ac3(String),
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DecodeError::TrueHd(e) => write!(f, "TrueHD {e:?}"),
            DecodeError::Dts(m) => write!(f, "DTS {m}"),
            DecodeError::Pcm(m) => write!(f, "PCM {m}"),
            DecodeError::Ac3(m) => write!(f, "AC-3 {m}"),
        }
    }
}

impl std::error::Error for DecodeError {}

pub trait AudioDecoder {
    /// Switches E-AC-3 JOC object rendering on or off (default on). Takes effect with the next packet and
    /// changes the block layout (7.1.4 or the bed). Returns false for decoders without JOC.
    fn set_joc(&mut self, _enabled: bool) -> bool {
        false
    }
    /// Feeds one container packet. Packets must be fed in stream order.
    fn push_packet(&mut self, data: &[u8]) -> Result<(), DecodeError>;
    /// Next decoded block, or `None` when more input is needed.
    fn pull(&mut self) -> Result<Option<PcmBlock>, DecodeError>;
}

pub fn open(codec: Codec) -> Result<Box<dyn AudioDecoder>, DecodeError> {
    Ok(match codec {
        Codec::TrueHd { presentation } => Box::new(TrueHd(
            TrueHdDecoder::new(usize::from(presentation)).map_err(DecodeError::TrueHd)?,
        )),
        Codec::Dts => Box::new(Dts { session: DtsSession::new(), ready: None }),
        Codec::DtsX => Box::new(Dts { session: DtsSession::with_heights(), ready: None }),
        Codec::Ac3 | Codec::Eac3 => Box::new(Ac3 { decoder: aether_ac3::Decoder::new(), ready: Default::default(), joc_enabled: true, joc: None }),
        Codec::Pcm(params) => Box::new(PcmDecoder::new(params)?),
    })
}

struct TrueHd(TrueHdDecoder);

impl AudioDecoder for TrueHd {
    fn push_packet(&mut self, data: &[u8]) -> Result<(), DecodeError> {
        self.0.push(data).map_err(DecodeError::TrueHd)
    }
    fn pull(&mut self) -> Result<Option<PcmBlock>, DecodeError> {
        let Some(b) = self.0.pull().map_err(DecodeError::TrueHd)? else {
            return Ok(None);
        };
        let planes = (0..b.channels)
            .map(|c| b.pcm.iter().skip(c).step_by(b.channels).copied().collect())
            .collect();
        Ok(Some(PcmBlock {
            sample_rate: b.sample_rate,
            frames: b.frames,
            labels: b.labels.iter().map(|l| layout::canonical(l).unwrap_or(l.as_str()).to_owned()).collect(),
            samples: Planar::I32(planes),
            lossless: true,
            joc: false,
            joc_objects: 0,
            joc_rendered: false,
        }))
    }
}

struct Dts {
    session: DtsSession,
    ready: Option<Decoded>,
}

impl AudioDecoder for Dts {
    fn push_packet(&mut self, data: &[u8]) -> Result<(), DecodeError> {
        if self.ready.is_some() {
            return Err(DecodeError::Dts("pull the previous block before pushing".into()));
        }
        // The decoder batches packets as `u32le length || packet`.
        let mut framed = Vec::with_capacity(data.len() + 4);
        framed.extend((data.len() as u32).to_le_bytes());
        framed.extend_from_slice(data);
        let out = self.session.push(&framed, crate::dts::MAX_SAMPLES).map_err(DecodeError::Dts)?;
        if !out.pcm.is_empty() {
            self.ready = Some(out);
        }
        Ok(())
    }
    fn pull(&mut self) -> Result<Option<PcmBlock>, DecodeError> {
        let Some(d) = self.ready.take() else {
            return Ok(None);
        };
        let channels = d.channel_labels.len();
        let frames = d.pcm.len() / channels;
        let lossless = d.integer24.is_some();
        let samples = match d.integer24 {
            Some(ints) => Planar::I32(
                (0..channels).map(|c| ints.iter().skip(c).step_by(channels).copied().collect()).collect(),
            ),
            None => Planar::F32(
                (0..channels).map(|c| d.pcm.iter().skip(c).step_by(channels).copied().collect()).collect(),
            ),
        };
        Ok(Some(PcmBlock {
            sample_rate: d.sample_rate,
            frames,
            labels: d.channel_labels.iter().map(|s| (*s).to_owned()).collect(),
            samples,
            lossless,
            joc: false,
            joc_objects: 0,
            joc_rendered: false,
        }))
    }
}

struct Ac3 {
    decoder: aether_ac3::Decoder,
    ready: std::collections::VecDeque<PcmBlock>,
    joc_enabled: bool,
    joc: Option<crate::joc::JocRenderer>,
}

/// Canonical label of an `aether-ac3` channel (its bit numbers are FFmpeg's `AV_CH_*`).
fn ac3_label(c: aether_ac3::Channel) -> &'static str {
    match c.bit() {
        b if (b as usize) < layout::WAVE_MASK_BITS => layout::WAVE_ORDER[b as usize],
        31 => "WL",
        32 => "WR",
        33 => "SDL",
        34 => "SDR",
        _ => "LFE2",
    }
}

impl AudioDecoder for Ac3 {
    fn set_joc(&mut self, enabled: bool) -> bool {
        if self.joc_enabled != enabled {
            self.joc_enabled = enabled;
            // A cold renderer on re-enable: the object history belongs to the stretch that was skipped.
            self.joc = None;
        }
        true
    }
    /// A packet is one access unit (independent frame plus its dependent frames); a packet holding
    /// several is decoded in turn.
    fn push_packet(&mut self, data: &[u8]) -> Result<(), DecodeError> {
        let mut rest = data;
        while !rest.is_empty() {
            let rest_before = rest;
            let f = self.decoder.decode(rest).map_err(|e| DecodeError::Ac3(e.to_string()))?;
            let used = f.bytes_consumed.max(1).min(rest.len());
            rest = &rest[used..];
            if f.frames == 0 {
                continue;
            }
            let au = &rest_before[..used];
            if f.is_joc && self.joc_enabled && f.frames == aether_joc::FRAME_SAMPLES {
                let r = self.joc.get_or_insert_with(crate::joc::JocRenderer::new).process(au, &f);
                if let Ok(Some(r)) = r {
                    self.ready.push_back(PcmBlock {
                        sample_rate: f.sample_rate,
                        frames: r.planes[0].len(),
                        labels: crate::joc::LAYOUT.labels().iter().map(|l| layout::canonical(l).unwrap_or(l).to_owned()).collect(),
                        samples: Planar::F32(r.planes),
                        lossless: false,
                        joc: true,
                        joc_objects: r.objects,
                        joc_rendered: true,
                    });
                    continue;
                }
                // JOC side information that does not decode: this block falls back to the bed.
            }
            let labels = f.layout.channels().iter().map(|c| ac3_label(*c).to_owned()).collect();
            let planes = (0..f.channels()).map(|c| f.channel(c).to_vec()).collect();
            self.ready.push_back(PcmBlock {
                sample_rate: f.sample_rate,
                frames: f.frames,
                labels,
                samples: Planar::F32(planes),
                lossless: false,
                joc: f.is_joc,
                joc_objects: 0,
                joc_rendered: false,
            });
        }
        Ok(())
    }
    fn pull(&mut self) -> Result<Option<PcmBlock>, DecodeError> {
        Ok(self.ready.pop_front())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truehd_rejects_out_of_range_presentation() {
        assert!(open(Codec::TrueHd { presentation: 200 }).is_err());
    }

    #[test]
    fn dts_garbage_is_an_error_not_silence() {
        let mut d = open(Codec::Dts).unwrap();
        assert!(d.push_packet(&[0xff; 16]).is_err());
    }

    #[test]
    fn nothing_to_pull_before_input() {
        let mut d = open(Codec::TrueHd { presentation: 0 }).unwrap();
        assert_eq!(d.pull().unwrap(), None);
    }
}
