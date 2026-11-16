//! Audio decoding for the browser player. The crate root is the stateful TrueHD decoder over the
//! exact upstream Rust pin: it decodes PCM and retains parsed OAMD. `decoder` puts it, DTS (`dts`),
//! AC-3 / E-AC-3 (`aether-ac3`, objects through `joc`) and LPCM (`pcm`) behind one trait. The optional point-source renderer
//! mixes exact element signals into a chosen bed; it does not establish native
//! Atmos output. Input limits do not certify hostile-input safety.
use truehd::process::decode::{DecodedAccessUnit, Decoder};
use truehd::process::extract::Extractor;
use truehd::process::parse::Parser;
use truehd::process::{MAX_PRESENTATIONS, PresentationMap};
use truehd::structs::oamd::ObjectAudioMetadataPayload;

pub const MAX_PUSH_BYTES: usize = 64 * 1024;
pub const MAX_PENDING_BYTES: usize = 1024 * 1024;
#[derive(Debug, PartialEq, Eq)]
pub enum AudioError {
    InvalidPresentation,
    PushTooLarge,
    PendingLimit,
    Framing,
    Parse,
    Decode,
    InvalidOutput,
    Poisoned,
}
#[derive(Debug)]
pub struct AudioBlock {
    pub sample_rate: u32,
    pub frames: usize,
    pub channels: usize,
    pub presentation: usize,
    /// Signed 24-bit PCM represented as interleaved i32 values (not left-shifted).
    pub pcm: Vec<i32>,
    pub labels: Vec<String>,
    pub metadata: Vec<ObjectAudioMetadataPayload>,
    pub is_duplicate: bool,
    pub access_unit: u64,
    pub input_offset: u64,
    pub output_frame_offset: u64,
}
pub struct TrueHdDecoder {
    extractor: Extractor,
    parser: Box<Parser>,
    decoder: Box<Decoder>,
    decoded: Box<DecodedAccessUnit>,
    selection: usize,
    resolved: usize,
    pushed: u64,
    output_frames: u64,
    poisoned: bool,
    /// Channel assignment fields of the last major sync (labels fall back to them when the decoder states none).
    assignment: Option<Assignment>,
}
#[derive(Clone, Copy)]
struct Assignment {
    fbb: bool,
    sixch: u8,
    eightch: u16,
    flags: u16,
}
impl TrueHdDecoder {
    pub fn new(selection: usize) -> Result<Self, AudioError> {
        if selection >= MAX_PRESENTATIONS {
            return Err(AudioError::InvalidPresentation);
        }
        let mut parser = Box::new(Parser::default());
        let mut required = [false; MAX_PRESENTATIONS];
        required[selection] = true;
        parser.set_required_presentations(&required);
        parser.set_check_fifo(false);
        Ok(Self {
            extractor: Extractor::default(),
            parser,
            decoder: Box::new(Decoder::default()),
            decoded: Box::new(DecodedAccessUnit::default()),
            selection,
            resolved: selection,
            pushed: 0,
            output_frames: 0,
            poisoned: false,
            assignment: None,
        })
    }
    pub fn pending_bytes(&self) -> usize {
        self.pushed.saturating_sub(self.extractor.stream_position()) as usize
    }
    pub fn push(&mut self, bytes: &[u8]) -> Result<(), AudioError> {
        if self.poisoned {
            return Err(AudioError::Poisoned);
        }
        if bytes.len() > MAX_PUSH_BYTES {
            return Err(AudioError::PushTooLarge);
        }
        if self.pending_bytes() + bytes.len() > MAX_PENDING_BYTES {
            return Err(AudioError::PendingLimit);
        }
        self.extractor.push_bytes(bytes);
        self.pushed += bytes.len() as u64;
        Ok(())
    }
    pub fn pull(&mut self) -> Result<Option<AudioBlock>, AudioError> {
        if self.poisoned {
            return Err(AudioError::Poisoned);
        }
        let result = self.pull_inner();
        if result.is_err() {
            self.poisoned = true;
        }
        result
    }
    fn pull_inner(&mut self) -> Result<Option<AudioBlock>, AudioError> {
        let errors_before = self.extractor.error_count();
        let frame = match self.extractor.next() {
            Some(Ok(frame)) => Some(frame),
            Some(Err(truehd::utils::errors::ExtractError::InsufficientData)) | None => None,
            Some(Err(_)) => return Err(AudioError::Framing),
        };
        if self.extractor.error_count() != errors_before {
            return Err(AudioError::Framing);
        }
        let Some(frame) = frame else {
            return Ok(None);
        };
        if frame.as_ref().len() > 8190 || frame.as_ref().len() < 8 {
            return Err(AudioError::Framing);
        }
        let au = self.parser.parse(&frame).map_err(|_| AudioError::Parse)?;
        drop(self.parser.take_branches());
        if let Some(sync) = &au.major_sync_info {
            self.assignment = Some(Assignment {
                fbb: sync.format_sync == truehd::structs::sync::MAJOR_SYNC_FBB,
                sixch: sync.format_info.sixch_decoder_channel_assignment,
                eightch: sync.format_info.eightch_decoder_channel_assignment,
                flags: sync.flags,
            });
            let map = PresentationMap::for_format_sync(
                sync.format_sync,
                sync.substream_info,
                sync.extended_substream_info,
            );
            // The dependency resolves unavailable presentations to the highest independent one.
            self.resolved = match map.presentation_type_by_index(self.selection) {
                truehd::process::PresentationType::Invalid => map
                    .max_independent_presentation()
                    .ok_or(AudioError::InvalidPresentation)?,
                truehd::process::PresentationType::CopyOf(i) => i,
                _ => self.selection,
            };
        }
        self.decoder
            .decode_presentation_into(&au, self.selection, &mut self.decoded)
            .map_err(|_| AudioError::Decode)?;
        let decoded = &self.decoded;
        if decoded.channel_count == 0
            || decoded.channel_count > 16
            || decoded.sample_length > 160
            || decoded.sampling_frequency == 0
        {
            return Err(AudioError::InvalidOutput);
        }
        let mut pcm = Vec::with_capacity(decoded.channel_count * decoded.sample_length);
        for row in decoded.pcm_data.iter().take(decoded.sample_length) {
            pcm.extend_from_slice(&row[..decoded.channel_count]);
        }
        let output_frame_offset = self.output_frames;
        self.output_frames += decoded.sample_length as u64;
        Ok(Some(AudioBlock {
            sample_rate: decoded.sampling_frequency,
            frames: decoded.sample_length,
            channels: decoded.channel_count,
            presentation: self.resolved,
            pcm,
            labels: labels(decoded.channel_labels.iter().map(|c| format!("{c:?}")).collect(), decoded.channel_count, self.resolved, self.assignment),
            metadata: decoded.oamd.clone(),
            is_duplicate: decoded.is_duplicate,
            access_unit: frame.index,
            input_offset: frame.offset,
            output_frame_offset,
        }))
    }
}
#[cfg(all(target_arch = "wasm32", feature = "legacy-wasm-exports"))]
mod wasm;

pub mod bed_regression;
pub mod decoder;
pub mod joc;
pub mod dts;
pub mod layout;
pub mod output;
pub mod pcm;
pub mod paired;
pub mod render;

/// Labels of a block, always one per plane. The decoder states them for TrueHD (FBA) streams, but it leaves the order
/// unstated for an MLP (FBB, DVD-Audio) arrangement it has not measured, so a raw `.mlp` stream would arrive without any.
/// Fall back to the stream's channel assignment fields (the 6 and 8 channel bitmasks, in the TrueHD speaker order),
/// then to the WAVE default for the channel count. Names are canonical (`layout::WAVE_ORDER`) where they are not TrueHD
/// names; the TrueHD names are canonicalised by `decoder::open`.
fn labels(stated: Vec<String>, channels: usize, presentation: usize, assignment: Option<Assignment>) -> Vec<String> {
    use truehd::structs::channel::ChannelLabel;
    if stated.len() == channels {
        return stated;
    }
    let derived = assignment.filter(|a| !a.fbb).and_then(|a| match presentation {
        0 => Some(if channels == 1 { vec![ChannelLabel::C] } else { vec![ChannelLabel::L, ChannelLabel::R] }),
        1 => ChannelLabel::from_sixch_channel(a.sixch).ok(),
        2 => ChannelLabel::from_eightch_channel(a.eightch, a.flags).ok(),
        _ => None,
    });
    if let Some(d) = derived.filter(|d| d.len() == channels) {
        return d.iter().map(|c| format!("{c:?}")).collect();
    }
    match crate::layout::default_for_channels(channels) {
        Some(v) => v.into_iter().map(str::to_owned).collect(),
        None => crate::layout::WAVE_ORDER.iter().take(channels).map(|s| (*s).to_owned()).collect(),
    }
}

#[cfg(test)]
mod label_tests {
    use super::*;
    fn a(fbb: bool, sixch: u8, eightch: u16) -> Option<Assignment> {
        Some(Assignment { fbb, sixch, eightch, flags: 0 })
    }
    #[test]
    fn stated_labels_pass_through() {
        assert_eq!(labels(vec!["L".into(), "R".into()], 2, 0, None), ["L", "R"]);
    }
    #[test]
    fn missing_mlp_labels_fall_back_to_the_wave_default_for_the_count() {
        // FBB arrangement the decoder does not state (e.g. raw .mlp): never an empty label list.
        assert_eq!(labels(vec![], 2, 0, a(true, 0, 0)), ["FL", "FR"]);
        assert_eq!(labels(vec![], 6, 1, a(true, 0, 0)), ["FL", "FR", "FC", "LFE", "BL", "BR"]);
        assert_eq!(labels(vec![], 8, 2, a(true, 0, 0)), ["FL", "FR", "FC", "LFE", "BL", "BR", "SL", "SR"]);
        assert_eq!(labels(vec![], 10, 2, None).len(), 10);
    }
    #[test]
    fn missing_truehd_labels_come_from_the_assignment_masks() {
        // sixch bits 0 (L R), 1 (C), 2 (LFE), 3 (Ls Rs) = 5.1
        assert_eq!(labels(vec![], 6, 1, a(false, 0b01111, 0)), ["L", "R", "C", "LFE", "Ls", "Rs"]);
        // eightch adds Lb Rb (bit 6): 7.1
        assert_eq!(labels(vec![], 8, 2, a(false, 0, 0b100_1111)), ["L", "R", "C", "LFE", "Ls", "Rs", "Lb", "Rb"]);
        // a mask that disagrees with the plane count is not trusted
        assert_eq!(labels(vec![], 6, 1, a(false, 0b00011, 0)), ["FL", "FR", "FC", "LFE", "BL", "BR"]);
    }
    #[test]
    fn canonical_names_pass_decoder_canonicalisation() {
        for l in labels(vec![], 8, 2, a(false, 0, 0b100_1111)) {
            assert!(crate::layout::canonical(&l).is_some(), "{l}");
        }
    }
}
