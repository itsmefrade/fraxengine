//! Experimental explicit-selection, owned same-AU TrueHD pair. Not a native companion oracle.
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use truehd::process::{MAX_PRESENTATIONS, PresentationMap, PresentationType};
use truehd::process::{decode::Decoder, extract::Extractor, parse::Parser};
use truehd::structs::oamd::ObjectAudioMetadataPayload;

pub const MAX_PUSH: usize = 64 * 1024;
pub const MAX_PENDING: usize = 1024 * 1024;
pub const MAX_OUTSTANDING_BLOCKS: usize = 8;
const MAX_AU: usize = 8190;
#[derive(Debug, Clone)]
pub struct Selection {
    pub bed_id: usize,
    pub elements_id: usize,
    /// Canonical upstream debug labels, in interleaved PCM order; all bed channels must be named.
    pub bed_labels: Vec<String>,
    pub element_channels: usize,
    /// Decoder's actual labels, potentially shorter than element_channels; no invented labels.
    pub element_labels: Vec<String>,
}
#[derive(Debug, PartialEq, Eq)]
pub enum Error {
    InvalidSelection,
    PushLimit,
    PendingLimit,
    Framing,
    Parse,
    UnsupportedMap,
    LayoutChanged,
    InvalidBlock,
    Decode,
    Overflow,
    Poisoned,
    OutstandingLimit,
    Finished,
    IncompleteInput,
    MissingMajorSync,
}
#[derive(Debug)]
pub struct PlaneSet {
    pub presentation_id: usize,
    pub channels: usize,
    pub labels: Vec<String>,
    pub pcm: Vec<i32>,
    pub duplicate: bool,
}
#[derive(Debug)]
pub struct PairedBlock {
    _lease: Lease,
    pub generation: u64,
    pub access_unit: u64,
    pub input_offset_bytes: u64,
    pub output_frame_offset: u64,
    pub sample_rate: u32,
    pub frames: usize,
    pub bed: PlaneSet,
    pub elements: PlaneSet,
    /// Exact payloads belonging to this element result/AU, including original timing fields.
    pub metadata: Vec<ObjectAudioMetadataPayload>,
}
#[derive(Debug)]
struct Lease(Arc<AtomicUsize>);
impl Drop for Lease {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}
#[derive(Debug)]
pub enum Pull {
    Block(PairedBlock),
    NeedInput,
    Eof,
}
pub struct PairedDecoder {
    leases: Arc<AtomicUsize>,
    selection: Selection,
    extractor: Extractor,
    parser: Box<Parser>,
    decoder: Box<Decoder>,
    required: [bool; MAX_PRESENTATIONS],
    map: Option<[PresentationType; MAX_PRESENTATIONS]>,
    rate: Option<u32>,
    generation: u64,
    pushed: u64,
    frames: u64,
    next_au: u64,
    next_byte: u64,
    poisoned: bool,
    finished: bool,
}
impl PairedDecoder {
    pub fn new(selection: Selection) -> Result<Self, Error> {
        const ROLES: &[&str] = &[
            "L", "R", "C", "LFE", "Ls", "Rs", "Lb", "Rb", "Ltf", "Rtf", "Ltr", "Rtr", "Lw", "Rw",
            "Cs", "Ts",
        ];
        if selection.bed_id >= MAX_PRESENTATIONS
            || selection.elements_id >= MAX_PRESENTATIONS
            || selection.bed_id == selection.elements_id
            || selection.bed_labels.is_empty()
            || selection.bed_labels.len() > 16
            || selection.element_channels == 0
            || selection.element_channels > 16
            || selection.element_labels.len() > selection.element_channels
            || selection
                .bed_labels
                .iter()
                .any(|s| !ROLES.contains(&s.as_str()))
            || selection
                .bed_labels
                .iter()
                .enumerate()
                .any(|(i, x)| selection.bed_labels[..i].contains(x))
            || selection.element_labels.iter().any(|s| s.len() > 16)
        {
            return Err(Error::InvalidSelection);
        }
        let mut required = [false; MAX_PRESENTATIONS];
        required[selection.bed_id] = true;
        required[selection.elements_id] = true;
        let mut parser = Box::new(Parser::default());
        parser.set_required_presentations(&required);
        parser.set_check_fifo(false);
        Ok(Self {
            selection,
            leases: Arc::new(AtomicUsize::new(0)),
            extractor: Extractor::default(),
            parser,
            decoder: Box::new(Decoder::default()),
            required,
            map: None,
            rate: None,
            generation: 0,
            pushed: 0,
            frames: 0,
            next_au: 0,
            next_byte: 0,
            poisoned: false,
            finished: false,
        })
    }
    pub fn generation(&self) -> u64 {
        self.generation
    }
    pub fn pending_bytes(&self) -> usize {
        self.pushed.saturating_sub(self.extractor.stream_position()) as usize
    }
    /// Rejected pushes do not consume bytes. The caller must pull before pushing past the limit.
    pub fn push(&mut self, bytes: &[u8]) -> Result<(), Error> {
        if self.poisoned {
            return Err(Error::Poisoned);
        }
        if self.finished {
            return Err(Error::Finished);
        }
        if bytes.len() > MAX_PUSH {
            return Err(Error::PushLimit);
        }
        if self
            .pending_bytes()
            .checked_add(bytes.len())
            .ok_or(Error::Overflow)?
            > MAX_PENDING
        {
            return Err(Error::PendingLimit);
        }
        let next = self
            .pushed
            .checked_add(bytes.len() as u64)
            .ok_or(Error::Overflow)?;
        self.extractor.push_bytes(bytes);
        self.pushed = next;
        Ok(())
    }
    /// Drops all buffered input and codec state; offsets restart at zero in a new generation.
    pub fn reset(&mut self) -> Result<(), Error> {
        let next = self.generation.checked_add(1).ok_or(Error::Overflow)?;
        let mut fresh = Self::new(self.selection.clone())?;
        fresh.generation = next;
        fresh.leases = Arc::clone(&self.leases);
        *self = fresh;
        Ok(())
    }
    /// Declare that no more compressed input will arrive; repeated finish is harmless.
    pub fn finish(&mut self) -> Result<(), Error> {
        if self.poisoned {
            return Err(Error::Poisoned);
        }
        self.finished = true;
        Ok(())
    }
    /// EOF is returned only once every pushed byte belongs to a published AU.
    /// Subsequent pulls after EOF continue to return EOF.
    pub fn pull(&mut self) -> Result<Pull, Error> {
        if self.poisoned {
            return Err(Error::Poisoned);
        }
        if self.leases.load(Ordering::Acquire) >= MAX_OUTSTANDING_BLOCKS {
            return Err(Error::OutstandingLimit);
        }
        match self.pull_inner() {
            Ok(Some(block)) => Ok(Pull::Block(block)),
            Ok(None) if !self.finished => Ok(Pull::NeedInput),
            Ok(None) if self.next_au == 0 => {
                self.poisoned = true;
                Err(Error::MissingMajorSync)
            }
            Ok(None) if self.next_byte != self.pushed => {
                self.poisoned = true;
                Err(Error::IncompleteInput)
            }
            Ok(None) => Ok(Pull::Eof),
            Err(e) => {
                self.poisoned = true;
                Err(e)
            }
        }
    }
    fn pull_inner(&mut self) -> Result<Option<PairedBlock>, Error> {
        let before = self.extractor.error_count();
        let extracted = self.extractor.next();
        if self.extractor.error_count() != before {
            return Err(Error::Framing);
        }
        let frame = match extracted {
            Some(Ok(f)) => f,
            Some(Err(truehd::utils::errors::ExtractError::InsufficientData)) | None => {
                return Ok(None);
            }
            Some(Err(_)) => return Err(Error::Framing),
        };
        if self.extractor.error_count() != before
            || frame.as_ref().len() < 8
            || frame.as_ref().len() > MAX_AU
            || frame.index != self.next_au
            || frame.offset != self.next_byte
        {
            return Err(Error::Framing);
        }
        let au = self.parser.parse(&frame).map_err(|_| Error::Parse)?;
        drop(self.parser.take_branches());
        if let Some(sync) = &au.major_sync_info {
            let m = PresentationMap::for_format_sync(
                sync.format_sync,
                sync.substream_info,
                sync.extended_substream_info,
            );
            let types = std::array::from_fn(|i| m.presentation_type_by_index(i));
            if self.map.is_some_and(|old| old != types) {
                return Err(Error::LayoutChanged);
            }
            if types[self.selection.elements_id] != PresentationType::Independent {
                return Err(Error::UnsupportedMap);
            }
            let mut id = self.selection.bed_id;
            let mut found = false;
            for _ in 0..MAX_PRESENTATIONS {
                match types[id] {
                    PresentationType::DownmixOf(next) if next < MAX_PRESENTATIONS => {
                        id = next;
                        if id == self.selection.elements_id {
                            found = true;
                            break;
                        }
                    }
                    _ => break,
                }
            }
            if !found {
                return Err(Error::UnsupportedMap);
            }
            self.map = Some(types)
        }
        if self.map.is_none() {
            return Err(Error::UnsupportedMap);
        }
        let mut ds = self
            .decoder
            .decode_presentations(&au, &self.required)
            .map_err(|_| Error::Decode)?;
        let bed = ds[self.selection.bed_id]
            .take()
            .ok_or(Error::InvalidBlock)?;
        let elements = ds[self.selection.elements_id]
            .take()
            .ok_or(Error::InvalidBlock)?;
        let bed_labels: Vec<String> = bed
            .channel_labels
            .iter()
            .map(|x| format!("{x:?}"))
            .collect();
        let element_labels: Vec<String> = elements
            .channel_labels
            .iter()
            .map(|x| format!("{x:?}"))
            .collect();
        if bed.substream_info_changed || elements.substream_info_changed {
            return Err(Error::LayoutChanged);
        }
        if bed.sampling_frequency == 0
            || bed.sampling_frequency != elements.sampling_frequency
            || bed.sample_length == 0
            || bed.sample_length > 160
            || bed.sample_length != elements.sample_length
            || bed.channel_count != self.selection.bed_labels.len()
            || elements.channel_count != self.selection.element_channels
            || bed_labels != self.selection.bed_labels
            || element_labels != self.selection.element_labels
            || !bed.oamd.is_empty()
            || elements.oamd.len() > 64
            || elements
                .oamd
                .iter()
                .any(|m| m.object_count > 128 || m.oa_element_md.len() > 64)
        {
            return Err(Error::InvalidBlock);
        }
        check_rate(self.rate, bed.sampling_frequency)?;
        let next_frames = self
            .frames
            .checked_add(bed.sample_length as u64)
            .ok_or(Error::Overflow)?;
        let next_au = self.next_au.checked_add(1).ok_or(Error::Overflow)?;
        let next_byte = self
            .next_byte
            .checked_add(frame.as_ref().len() as u64)
            .ok_or(Error::Overflow)?;
        let copy = |id: usize,
                    d: &truehd::process::decode::DecodedAccessUnit,
                    labels: Vec<String>| PlaneSet {
            presentation_id: id,
            channels: d.channel_count,
            labels,
            pcm: d
                .pcm_data
                .iter()
                .take(d.sample_length)
                .flat_map(|row| row[..d.channel_count].iter().copied())
                .collect(),
            duplicate: d.is_duplicate,
        };
        self.leases.fetch_add(1, Ordering::AcqRel);
        let result = PairedBlock {
            _lease: Lease(Arc::clone(&self.leases)),
            generation: self.generation,
            access_unit: frame.index,
            input_offset_bytes: frame.offset,
            output_frame_offset: self.frames,
            sample_rate: bed.sampling_frequency,
            frames: bed.sample_length,
            bed: copy(self.selection.bed_id, &bed, bed_labels),
            elements: copy(self.selection.elements_id, &elements, element_labels),
            metadata: elements.oamd,
        };
        self.rate = Some(result.sample_rate);
        self.frames = next_frames;
        self.next_au = next_au;
        self.next_byte = next_byte;
        Ok(Some(result))
    }
}

fn check_rate(previous: Option<u32>, incoming: u32) -> Result<(), Error> {
    if previous.is_some_and(|rate| rate != incoming) {
        Err(Error::LayoutChanged)
    } else {
        Ok(())
    }
}
#[cfg(test)]
mod rate_tests {
    use super::*;
    #[test]
    fn changed_rate_requires_new_generation() {
        assert_eq!(check_rate(None, 48000), Ok(()));
        assert_eq!(check_rate(Some(48000), 48000), Ok(()));
        assert_eq!(check_rate(Some(48000), 96000), Err(Error::LayoutChanged));
    }
}
