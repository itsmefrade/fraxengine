//! Bounded layer pairing after the caller has derived aligned picture order.
//!
//! POC parsing, wrap handling and IRAP alignment are not implemented here. The
//! caller must supply the same canonical picture number for matching BL/EL.

use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PictureKey {
    pub epoch: u64,
    pub picture: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layer {
    Base,
    Enhancement,
}

#[derive(Debug, PartialEq, Eq)]
pub enum PairError {
    StaleEpoch,
    DuplicateLayer,
    CapacityExceeded,
}

#[derive(Debug)]
pub struct Pair<T> {
    pub key: PictureKey,
    pub base: T,
    pub enhancement: T,
}

struct Pending<T> {
    base: Option<T>,
    enhancement: Option<T>,
}

pub struct LayerPairer<T> {
    epoch: u64,
    capacity: usize,
    pending: BTreeMap<i64, Pending<T>>,
}

impl<T> LayerPairer<T> {
    pub fn new(epoch: u64, capacity: usize) -> Self {
        Self {
            epoch,
            capacity,
            pending: BTreeMap::new(),
        }
    }
    pub fn len(&self) -> usize {
        self.pending.len()
    }
    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }
    /// Epoch must be supplied by the session owner; resetting drops every old buffer.
    pub fn reset(&mut self, epoch: u64) {
        self.pending.clear();
        self.epoch = epoch;
    }
    pub fn push(
        &mut self,
        key: PictureKey,
        layer: Layer,
        frame: T,
    ) -> Result<Option<Pair<T>>, PairError> {
        if key.epoch != self.epoch {
            return Err(PairError::StaleEpoch);
        }
        if !self.pending.contains_key(&key.picture) && self.pending.len() >= self.capacity {
            return Err(PairError::CapacityExceeded);
        }
        let entry = self.pending.entry(key.picture).or_insert(Pending {
            base: None,
            enhancement: None,
        });
        let target = match layer {
            Layer::Base => &mut entry.base,
            Layer::Enhancement => &mut entry.enhancement,
        };
        if target.is_some() {
            return Err(PairError::DuplicateLayer);
        }
        *target = Some(frame);
        if entry.base.is_some() && entry.enhancement.is_some() {
            let mut entry = self.pending.remove(&key.picture).unwrap();
            Ok(Some(Pair {
                key,
                base: entry.base.take().unwrap(),
                enhancement: entry.enhancement.take().unwrap(),
            }))
        } else {
            Ok(None)
        }
    }
    /// The scheduler chooses when a picture's enhancement deadline expires.
    pub fn discard(&mut self, picture: i64) {
        self.pending.remove(&picture);
    }
}

/// Composite-key pairer for source-aware identities. Different IRAP anchors can
/// coexist while old-sequence outputs drain; numeric POC alone never pairs them.
pub struct CanonicalPair<T> {
    pub key: crate::source_association::CanonicalKey,
    pub base: T,
    pub enhancement: T,
}
pub struct CanonicalLayerPairer<T> {
    epoch: u32,
    capacity: usize,
    pending: BTreeMap<crate::source_association::CanonicalKey, Pending<T>>,
}
impl<T> CanonicalLayerPairer<T> {
    pub fn new(epoch: u32, capacity: usize) -> Result<Self, PairError> {
        if !(1..=64).contains(&capacity) {
            return Err(PairError::CapacityExceeded);
        }
        Ok(Self {
            epoch,
            capacity,
            pending: BTreeMap::new(),
        })
    }
    pub fn len(&self) -> usize {
        self.pending.len()
    }
    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }
    pub fn reset(&mut self, epoch: u32) {
        self.pending.clear();
        self.epoch = epoch;
    }
    pub fn push(
        &mut self,
        key: crate::source_association::CanonicalKey,
        layer: Layer,
        value: T,
    ) -> Result<Option<CanonicalPair<T>>, PairError> {
        if key.epoch != self.epoch {
            return Err(PairError::StaleEpoch);
        }
        if !self.pending.contains_key(&key) && self.pending.len() >= self.capacity {
            return Err(PairError::CapacityExceeded);
        }
        let entry = self.pending.entry(key).or_insert(Pending {
            base: None,
            enhancement: None,
        });
        let slot = match layer {
            Layer::Base => &mut entry.base,
            Layer::Enhancement => &mut entry.enhancement,
        };
        if slot.is_some() {
            return Err(PairError::DuplicateLayer);
        }
        *slot = Some(value);
        if entry.base.is_some() && entry.enhancement.is_some() {
            let mut complete = self.pending.remove(&key).unwrap();
            Ok(Some(CanonicalPair {
                key,
                base: complete.base.take().unwrap(),
                enhancement: complete.enhancement.take().unwrap(),
            }))
        } else {
            Ok(None)
        }
    }
    pub fn discard(&mut self, key: crate::source_association::CanonicalKey) {
        self.pending.remove(&key);
    }
}
