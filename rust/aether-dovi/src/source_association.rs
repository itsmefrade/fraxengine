//! Explicit combined-stream source-AU identity. RPU follows the unique BL coded
//! picture in the original container AU, not the co-carried EL or display order.
//! This interpretation is scoped to the combined single-track carriage used by
//! the fixtures; it is not an automatic dual-PID demultiplexer.
use crate::{
    hevc::{self, Framing, Kind},
    picture_identity::{IdentityError, IdentityEvent, PictureIdentity, PictureIdentityTracker},
};
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceTime {
    pub ticks: i64,
    pub numerator: u32,
    pub denominator: u32,
}
impl SourceTime {
    pub fn validate(self) -> Result<Self, AssociationError> {
        if self.numerator == 0 || self.denominator == 0 {
            Err(AssociationError::Timebase)
        } else {
            Ok(self)
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceUnit {
    pub epoch: u32,
    pub token: u64,
    pub pts: SourceTime,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct CanonicalKey {
    pub epoch: u32,
    pub anchor: u32,
    pub relative_poc: i64,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PictureDescriptor {
    pub identity: PictureIdentity,
    pub source: SourceUnit,
    pub canonical: Option<CanonicalKey>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssociationError {
    Epoch,
    Timebase,
    TokenOrder,
    Framing,
    Identity(IdentityError),
    MultiplePictures,
    MultipleRpu,
    RpuWithoutBase,
    UnprovenAnchor,
    SequenceDiscontinuity,
    Overflow,
    Poisoned,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AccessUnitObservation {
    pub source: SourceUnit,
    pub base: Option<PictureDescriptor>,
    pub enhancement: Option<PictureDescriptor>,
    /// NAL index into the original framed AU, before wrapper stripping.
    pub rpu_nal_index: Option<usize>,
    pub rpu_crc32: Option<u32>,
    pub new_anchor: bool,
}
#[derive(Debug, Clone, Copy)]
struct Anchor {
    id: u32,
    base_sequence: u32,
    el_sequence: u32,
    base_poc: i32,
    el_poc: i32,
}
#[derive(Clone)]
pub struct SourceAssociationTracker {
    epoch: u32,
    base: PictureIdentityTracker,
    enhancement: PictureIdentityTracker,
    last_token: Option<u64>,
    timebase: Option<(u32, u32)>,
    anchor: Option<Anchor>,
    poisoned: bool,
}
impl SourceAssociationTracker {
    pub fn new(epoch: u32) -> Self {
        Self {
            epoch,
            base: PictureIdentityTracker::new(epoch),
            enhancement: PictureIdentityTracker::new(epoch),
            last_token: None,
            timebase: None,
            anchor: None,
            poisoned: false,
        }
    }
    pub fn reset(&mut self, epoch: u32) -> Result<(), AssociationError> {
        if epoch <= self.epoch {
            return Err(AssociationError::Epoch);
        }
        *self = Self::new(epoch);
        Ok(())
    }
    /// External hvcC parameter sets only. VCL/RPU input is never accepted here.
    pub fn seed(
        &mut self,
        enhancement: bool,
        nal: &[u8],
        epoch: u32,
    ) -> Result<(), AssociationError> {
        if epoch != self.epoch {
            return Err(AssociationError::Epoch);
        }
        if self.poisoned {
            return Err(AssociationError::Poisoned);
        }
        let h = hevc::Header::parse(nal).map_err(|_| AssociationError::Framing)?;
        if !matches!(h.nal_type, 32..=34) {
            return Err(AssociationError::Framing);
        }
        let tracker = if enhancement {
            &mut self.enhancement
        } else {
            &mut self.base
        };
        tracker
            .push_nal(nal, epoch)
            .map_err(AssociationError::Identity)?;
        Ok(())
    }
    /// Establishing an anchor is explicit: the caller must certify that the
    /// co-carried joint IRAP is the same source presentation instant. Both local
    /// origins are retained; sourceAU index is never substituted for POC.
    pub fn observe(
        &mut self,
        bytes: &[u8],
        framing: Framing,
        source: SourceUnit,
        prove_joint_irap: bool,
    ) -> Result<AccessUnitObservation, AssociationError> {
        if source.epoch != self.epoch {
            return Err(AssociationError::Epoch);
        }
        if self.poisoned {
            return Err(AssociationError::Poisoned);
        }
        let result = self.observe_inner(bytes, framing, source, prove_joint_irap);
        if result.is_err() {
            self.poisoned = true;
            self.anchor = None;
        }
        result
    }
    fn observe_inner(
        &mut self,
        bytes: &[u8],
        framing: Framing,
        source: SourceUnit,
        prove: bool,
    ) -> Result<AccessUnitObservation, AssociationError> {
        source.pts.validate()?;
        if self.last_token.is_some_and(|last| source.token <= last) {
            return Err(AssociationError::TokenOrder);
        }
        let timebase = (source.pts.numerator, source.pts.denominator);
        if self.timebase.is_some_and(|previous| previous != timebase) {
            return Err(AssociationError::Timebase);
        }
        let nals = hevc::split(bytes, framing, hevc::Limits::default())
            .map_err(|_| AssociationError::Framing)?;
        let (mut base, mut el, mut rpu, mut crc) = (None, None, None, None);
        for (index, nal) in nals.iter().enumerate() {
            if nal.kind == Kind::Rpu {
                if nal.bytes.len() > 65536 || nal.header.layer_id != 0 {
                    return Err(AssociationError::Framing);
                }
                if rpu.replace(index).is_some() {
                    return Err(AssociationError::MultipleRpu);
                }
                // CRC/envelope validation here establishes byte identity only.
                // Semantic consumers must still call the checked semantic parser.
                let raw =
                    crate::rpu::verified_unescaped(nal.bytes).ok_or(AssociationError::Framing)?;
                crc = Some(u32::from_be_bytes(
                    raw[raw.len() - 5..raw.len() - 1]
                        .try_into()
                        .map_err(|_| AssociationError::Framing)?,
                ));
                continue;
            }
            let tracker = if nal.kind == Kind::Enhancement {
                &mut self.enhancement
            } else {
                &mut self.base
            };
            let event = tracker
                .push_nal(nal.decode_bytes(), source.epoch)
                .map_err(AssociationError::Identity)?;
            if let IdentityEvent::PictureStart(id) = event {
                let target = if nal.kind == Kind::Enhancement {
                    &mut el
                } else {
                    &mut base
                };
                if target.replace(id).is_some() {
                    return Err(AssociationError::MultiplePictures);
                }
            }
        }
        self.base
            .end_access_unit(source.epoch)
            .map_err(AssociationError::Identity)?;
        self.enhancement
            .end_access_unit(source.epoch)
            .map_err(AssociationError::Identity)?;
        if rpu.is_some() && base.is_none() {
            return Err(AssociationError::RpuWithoutBase);
        }
        let mut new_anchor = false;
        if prove {
            let (b, e) = (
                base.ok_or(AssociationError::UnprovenAnchor)?,
                el.ok_or(AssociationError::UnprovenAnchor)?,
            );
            if !b.irap || !e.irap || !b.output_expected || !e.output_expected {
                return Err(AssociationError::UnprovenAnchor);
            }
            // Reanchoring is allowed only at a true CVS boundary in both layers.
            if !(b.starts_sequence && e.starts_sequence) {
                return Err(AssociationError::UnprovenAnchor);
            }
            let id = self
                .anchor
                .map(|a| a.id.checked_add(1).ok_or(AssociationError::Overflow))
                .transpose()?
                .unwrap_or(0);
            self.anchor = Some(Anchor {
                id,
                base_sequence: b.sequence,
                el_sequence: e.sequence,
                base_poc: b.poc,
                el_poc: e.poc,
            });
            new_anchor = true;
        }
        if let Some(a) = self.anchor
            && (base.is_some_and(|b| b.sequence != a.base_sequence)
                || el.is_some_and(|e| e.sequence != a.el_sequence))
        {
            return Err(AssociationError::SequenceDiscontinuity);
        }
        let descriptor = |identity: PictureIdentity, enhancement: bool| {
            let canonical = self.anchor.map(|a| CanonicalKey {
                epoch: source.epoch,
                anchor: a.id,
                relative_poc: i64::from(identity.poc)
                    - i64::from(if enhancement { a.el_poc } else { a.base_poc }),
            });
            PictureDescriptor {
                identity,
                source,
                canonical,
            }
        };
        self.last_token = Some(source.token);
        self.timebase = Some(timebase);
        Ok(AccessUnitObservation {
            source,
            base: base.map(|b| descriptor(b, false)),
            enhancement: el.map(|e| descriptor(e, true)),
            rpu_nal_index: rpu,
            rpu_crc32: crc,
            new_anchor,
        })
    }
}
