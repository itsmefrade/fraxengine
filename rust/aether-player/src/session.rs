//! Generation-fenced session lifecycle with explicit resource release confirmation.
//!
//! The caller owns the actual resources and performs cleanup. A terminal event
//! requests cleanup; it never implies that cancellation released a resource.
//! All asynchronous operations must carry the generation returned by `start`.
use std::collections::BTreeSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Cancelled,
    Eof,
    Error,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Idle,
    Active,
    Cleaning(Outcome),
    Finished(Outcome),
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionError {
    Busy,
    StaleGeneration,
    InvalidPhase,
    DuplicateResource,
    UnknownResource,
    ResourceLimit,
    GenerationOverflow,
}

/// A generation may register at most `resource_limit` resource IDs in total.
/// IDs are caller-defined and must be unique for the entire generation. Register
/// pending acquisitions before starting asynchronous work, and confirm release only
/// after the work and its resource have actually been disposed.
#[derive(Debug)]
pub struct Session {
    generation: u64,
    phase: Phase,
    resources: BTreeSet<u64>,
    seen_resources: BTreeSet<u64>,
    resource_limit: usize,
}
impl Session {
    pub fn new(resource_limit: usize) -> Self {
        Self {
            generation: 0,
            phase: Phase::Idle,
            resources: BTreeSet::new(),
            seen_resources: BTreeSet::new(),
            resource_limit,
        }
    }
    pub fn phase(&self) -> Phase {
        self.phase
    }
    pub fn generation(&self) -> u64 {
        self.generation
    }
    pub fn owned_resources(&self) -> impl Iterator<Item = u64> + '_ {
        self.resources.iter().copied()
    }
    pub fn start(&mut self) -> Result<u64, SessionError> {
        if !matches!(self.phase, Phase::Idle | Phase::Finished(_)) || !self.resources.is_empty() {
            return Err(SessionError::Busy);
        }
        let next = self
            .generation
            .checked_add(1)
            .ok_or(SessionError::GenerationOverflow)?;
        self.generation = next;
        self.seen_resources.clear();
        self.phase = Phase::Active;
        Ok(next)
    }
    fn check(&self, generation: u64) -> Result<(), SessionError> {
        if generation != self.generation || generation == 0 {
            Err(SessionError::StaleGeneration)
        } else {
            Ok(())
        }
    }
    pub fn acquire(&mut self, generation: u64, resource: u64) -> Result<(), SessionError> {
        self.check(generation)?;
        if self.phase != Phase::Active {
            return Err(SessionError::InvalidPhase);
        }
        if self.seen_resources.contains(&resource) {
            return Err(SessionError::DuplicateResource);
        }
        if self.seen_resources.len() >= self.resource_limit {
            return Err(SessionError::ResourceLimit);
        }
        self.resources.insert(resource);
        self.seen_resources.insert(resource);
        Ok(())
    }
    /// Request terminal cleanup. A repeated terminal event is rejected, preserving the first outcome.
    pub fn finish(&mut self, generation: u64, outcome: Outcome) -> Result<(), SessionError> {
        self.check(generation)?;
        if self.phase != Phase::Active {
            return Err(SessionError::InvalidPhase);
        }
        self.phase = if self.resources.is_empty() {
            Phase::Finished(outcome)
        } else {
            Phase::Cleaning(outcome)
        };
        Ok(())
    }
    /// Confirm actual release, including during normal operation or terminal cleanup.
    pub fn confirm_release(&mut self, generation: u64, resource: u64) -> Result<(), SessionError> {
        self.check(generation)?;
        if !matches!(self.phase, Phase::Active | Phase::Cleaning(_)) {
            return Err(SessionError::InvalidPhase);
        }
        if !self.resources.remove(&resource) {
            return Err(SessionError::UnknownResource);
        }
        if let Phase::Cleaning(outcome) = self.phase {
            if self.resources.is_empty() {
                self.phase = Phase::Finished(outcome);
            }
        }
        Ok(())
    }
}
