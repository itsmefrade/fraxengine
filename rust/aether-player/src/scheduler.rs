//! PTS ordered bounded video scheduler. Payloads must be RAII resource leases.
use crate::{
    clock::{ClockError, MediaClock, Timebase},
    session::{Outcome, Session, SessionError},
};
use std::collections::VecDeque;

#[must_use = "frame owns its payload; release it or return it to its owner"]
#[derive(Debug)]
pub struct Frame<T> {
    pub epoch: u64,
    pub pts: i64,
    pub duration: i64,
    pub bytes: usize,
    pub payload: T,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reject {
    StaleEpoch,
    Closed,
    DuplicatePts,
    BehindPresented,
    InvalidDuration,
    Time(ClockError),
    Full,
}
#[derive(Debug)]
pub struct Rejected<T> {
    pub reason: Reject,
    pub frame: Frame<T>,
}
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Counters {
    pub enqueued: u64,
    pub delivered: u64,
    pub late_dropped: u64,
    pub rejected: u64,
    pub flushed: u64,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LatePolicy {
    Deliver,
    DropBeyond { nanos: u64 },
}
#[must_use = "poll contains owned frames that must be released by the caller"]
#[derive(Debug)]
pub struct Poll<T> {
    pub discarded: Vec<Frame<T>>,
    pub due: Option<Frame<T>>,
    pub state: PollState,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PollState {
    Waiting,
    Due,
    Drained,
    Cancelled,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Clock(ClockError),
    Session(SessionError),
    InvalidCapacity,
    InvalidState,
}
impl From<ClockError> for Error {
    fn from(e: ClockError) -> Self {
        Self::Clock(e)
    }
}
impl From<SessionError> for Error {
    fn from(e: SessionError) -> Self {
        Self::Session(e)
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Open,
    Eof,
    Finished,
    Cancelled,
}
struct Entry<T> {
    frame: Frame<T>,
    nanos: i128,
}
/// Do not abandon a scheduler with queued leases unless payload Drop releases them.
/// Explicitly call cancel and dispose returned frames, including on owner shutdown.
pub struct Scheduler<T> {
    clock: MediaClock,
    timebase: Timebase,
    session: Session,
    generation: u64,
    queue: VecDeque<Entry<T>>,
    max_frames: usize,
    max_bytes: usize,
    bytes: usize,
    policy: LatePolicy,
    state: State,
    last_delivered: Option<i64>,
    counters: Counters,
}
impl<T> Scheduler<T> {
    pub fn new(
        timebase: Timebase,
        media_nanos: i128,
        host_nanos: u64,
        max_frames: usize,
        max_bytes: usize,
        policy: LatePolicy,
    ) -> Result<Self, Error> {
        if max_frames == 0 || max_bytes == 0 {
            return Err(Error::InvalidCapacity);
        }
        let mut session = Session::new(0);
        let generation = session.start()?;
        Ok(Self {
            clock: MediaClock::new(media_nanos, host_nanos),
            timebase,
            session,
            generation,
            queue: VecDeque::new(),
            max_frames,
            max_bytes,
            bytes: 0,
            policy,
            state: State::Open,
            last_delivered: None,
            counters: Counters::default(),
        })
    }
    pub fn epoch(&self) -> u64 {
        self.clock.epoch()
    }
    pub fn queued_frames(&self) -> usize {
        self.queue.len()
    }
    pub fn queued_bytes(&self) -> usize {
        self.bytes
    }
    pub fn counters(&self) -> Counters {
        self.counters
    }
    pub fn enqueue(&mut self, frame: Frame<T>) -> Result<(), Rejected<T>> {
        let reason = if self.state != State::Open {
            Some(Reject::Closed)
        } else if frame.epoch != self.epoch() {
            Some(Reject::StaleEpoch)
        } else if frame.duration <= 0 || frame.bytes == 0 {
            Some(Reject::InvalidDuration)
        } else if self.last_delivered.is_some_and(|p| frame.pts <= p) {
            Some(Reject::BehindPresented)
        } else if self.queue.iter().any(|e| e.frame.pts == frame.pts) {
            Some(Reject::DuplicatePts)
        } else if self.queue.len() >= self.max_frames
            || frame.bytes > self.max_bytes.saturating_sub(self.bytes)
        {
            Some(Reject::Full)
        } else {
            None
        };
        if let Some(reason) = reason {
            self.counters.rejected = self.counters.rejected.saturating_add(1);
            return Err(Rejected { reason, frame });
        }
        let nanos = match self.timebase.to_nanos(frame.pts) {
            Ok(n) => n,
            Err(e) => {
                self.counters.rejected = self.counters.rejected.saturating_add(1);
                return Err(Rejected {
                    reason: Reject::Time(e),
                    frame,
                });
            }
        };
        let at = self.queue.partition_point(|e| e.frame.pts < frame.pts);
        self.bytes += frame.bytes;
        self.queue.insert(at, Entry { frame, nanos });
        self.counters.enqueued = self.counters.enqueued.saturating_add(1);
        Ok(())
    }
    /// No state or queue changes on a backwards host clock observation.
    pub fn poll(&mut self, host: u64) -> Result<Poll<T>, Error> {
        if self.state == State::Cancelled {
            return Ok(Poll {
                discarded: vec![],
                due: None,
                state: PollState::Cancelled,
            });
        }
        let now = self.clock.position(host)?;
        let mut discarded = Vec::new();
        while let Some(e) = self.queue.front() {
            if e.nanos > now {
                break;
            }
            let late = now.saturating_sub(e.nanos);
            let drop = match self.policy {
                LatePolicy::Deliver => false,
                LatePolicy::DropBeyond { nanos } => late > nanos as i128,
            };
            if !drop {
                let e = self.queue.pop_front().unwrap();
                self.bytes -= e.frame.bytes;
                self.last_delivered = Some(e.frame.pts);
                self.counters.delivered = self.counters.delivered.saturating_add(1);
                return Ok(Poll {
                    discarded,
                    due: Some(e.frame),
                    state: PollState::Due,
                });
            }
            let e = self.queue.pop_front().unwrap();
            self.bytes -= e.frame.bytes;
            self.last_delivered = Some(e.frame.pts);
            self.counters.late_dropped = self.counters.late_dropped.saturating_add(1);
            discarded.push(e.frame)
        }
        if self.state == State::Eof && self.queue.is_empty() {
            self.session.finish(self.generation, Outcome::Eof)?;
            self.state = State::Finished
        }
        let state = if self.state == State::Finished {
            PollState::Drained
        } else {
            PollState::Waiting
        };
        Ok(Poll {
            discarded,
            due: None,
            state,
        })
    }
    /// Reanchor only while active; failed reanchors leave all queued leases intact.
    pub fn seek(&mut self, media: i128, host: u64) -> Result<Vec<Frame<T>>, Error> {
        if !matches!(self.state, State::Open | State::Eof) {
            return Err(Error::InvalidState);
        }
        self.clock.reanchor(media, host)?;
        let out = self.flush();
        self.state = State::Open;
        self.last_delivered = None;
        Ok(out)
    }
    pub fn eof(&mut self) -> Result<(), Error> {
        if self.state != State::Open {
            return Err(Error::InvalidState);
        }
        self.state = State::Eof;
        Ok(())
    }
    pub fn cancel(&mut self) -> Result<Vec<Frame<T>>, Error> {
        if !matches!(self.state, State::Open | State::Eof) {
            return Err(Error::InvalidState);
        }
        self.session.finish(self.generation, Outcome::Cancelled)?;
        self.state = State::Cancelled;
        Ok(self.flush())
    }
    fn flush(&mut self) -> Vec<Frame<T>> {
        let out: Vec<_> = self.queue.drain(..).map(|e| e.frame).collect();
        self.bytes = 0;
        self.counters.flushed = self.counters.flushed.saturating_add(out.len() as u64);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn s(policy: LatePolicy) -> Scheduler<u64> {
        Scheduler::new(Timebase::new(1, 1000).unwrap(), 0, 100, 3, 10, policy).unwrap()
    }
    fn f(epoch: u64, pts: i64, bytes: usize) -> Frame<u64> {
        Frame {
            epoch,
            pts,
            duration: 1,
            bytes,
            payload: pts as u64,
        }
    }
    #[test]
    fn reorder_duplicate_late_and_lease_return() {
        let mut q = s(LatePolicy::DropBeyond { nanos: 1_000_000 });
        q.enqueue(f(0, 4, 2)).unwrap();
        q.enqueue(f(0, 1, 2)).unwrap();
        q.enqueue(f(0, 2, 2)).unwrap();
        let r = q.enqueue(f(0, 2, 2)).unwrap_err();
        assert_eq!(r.reason, Reject::DuplicatePts);
        assert_eq!(r.frame.pts, 2);
        let p = q.poll(4_000_100).unwrap();
        assert_eq!(
            p.discarded.iter().map(|f| f.pts).collect::<Vec<_>>(),
            vec![1, 2]
        );
        assert_eq!(p.due.unwrap().pts, 4);
        assert_eq!(q.queued_bytes(), 0);
        assert_eq!(q.counters().late_dropped, 2);
        assert_eq!(
            q.enqueue(f(0, 3, 1)).unwrap_err().reason,
            Reject::BehindPresented
        )
    }
    #[test]
    fn caps_and_backpressure_return_ownership() {
        let mut q = s(LatePolicy::Deliver);
        q.enqueue(f(0, 1, 6)).unwrap();
        let e = q.enqueue(f(0, 2, 5)).unwrap_err();
        assert_eq!(e.reason, Reject::Full);
        assert_eq!(e.frame.bytes, 5);
        q.enqueue(f(0, 2, 1)).unwrap();
        q.enqueue(f(0, 3, 1)).unwrap();
        assert_eq!(q.enqueue(f(0, 4, 1)).unwrap_err().reason, Reject::Full);
        assert_eq!(q.queued_bytes(), 8);
    }
    #[test]
    fn seek_failure_and_epoch_fence() {
        let mut q = s(LatePolicy::Deliver);
        q.enqueue(f(0, 5, 2)).unwrap();
        assert_eq!(
            q.seek(0, 99).unwrap_err(),
            Error::Clock(ClockError::HostWentBackwards)
        );
        assert_eq!(q.queued_frames(), 1);
        assert_eq!(q.seek(10, 100).unwrap()[0].pts, 5);
        assert_eq!(q.epoch(), 1);
        assert_eq!(
            q.enqueue(f(0, 1, 2)).unwrap_err().reason,
            Reject::StaleEpoch
        );
        q.enqueue(f(1, 1, 2)).unwrap();
        assert_eq!(q.cancel().unwrap()[0].pts, 1);
        assert_eq!(q.poll(100).unwrap().state, PollState::Cancelled);
        assert_eq!(q.enqueue(f(1, 2, 2)).unwrap_err().reason, Reject::Closed);
        assert_eq!(q.cancel().unwrap_err(), Error::InvalidState)
    }
    #[test]
    fn eof_drains_due_and_rejects_push() {
        let mut q = s(LatePolicy::Deliver);
        q.enqueue(f(0, 3, 1)).unwrap();
        q.eof().unwrap();
        assert_eq!(q.enqueue(f(0, 4, 1)).unwrap_err().reason, Reject::Closed);
        assert_eq!(q.poll(100).unwrap().state, PollState::Waiting);
        assert_eq!(q.poll(3_000_100).unwrap().due.unwrap().pts, 3);
        assert_eq!(q.poll(3_000_100).unwrap().state, PollState::Drained);
        assert_eq!(q.seek(0, 3_000_100).unwrap_err(), Error::InvalidState)
    }
    #[test]
    fn backwards_time_does_not_pop() {
        let mut q = s(LatePolicy::Deliver);
        q.enqueue(f(0, 2, 1)).unwrap();
        assert_eq!(
            q.poll(99).unwrap_err(),
            Error::Clock(ClockError::HostWentBackwards)
        );
        assert_eq!(q.queued_frames(), 1);
        assert_eq!(q.poll(2_000_100).unwrap().due.unwrap().pts, 2)
    }
    #[test]
    fn irregular_pts_and_negative() {
        let mut q = s(LatePolicy::Deliver);
        for n in [7, -2, 3] {
            q.enqueue(f(0, n, 1)).unwrap()
        }
        assert_eq!(q.poll(100).unwrap().due.unwrap().pts, -2);
        assert_eq!(q.poll(1_000_100).unwrap().state, PollState::Waiting);
        assert_eq!(q.poll(3_000_100).unwrap().due.unwrap().pts, 3);
        assert_eq!(q.poll(7_000_100).unwrap().due.unwrap().pts, 7)
    }
    #[test]
    fn exact_long_rational_23976_and_60() {
        for (num, den) in [(1001, 24000), (1, 60)] {
            let tb = Timebase::new(num, den).unwrap();
            for n in [0, 1, 2, 3, 23, 24, 60, 1_000_000, 2_000_000_000] {
                assert_eq!(
                    tb.to_nanos(n).unwrap(),
                    (n as i128) * (num as i128) * 1_000_000_000 / (den as i128)
                )
            }
            let mut q = Scheduler::new(tb, 0, 0, 2, 2, LatePolicy::Deliver).unwrap();
            q.enqueue(f(0, 1, 1)).unwrap();
            let due = tb.to_nanos(1).unwrap() as u64;
            assert_eq!(q.poll(due - 1).unwrap().state, PollState::Waiting);
            assert_eq!(q.poll(due).unwrap().due.unwrap().pts, 1)
        }
    }
    #[test]
    fn extreme_media_anchor_no_subtraction_overflow() {
        let mut q = Scheduler::new(
            Timebase::new(1, 1).unwrap(),
            i128::MAX,
            0,
            2,
            2,
            LatePolicy::DropBeyond { nanos: 0 },
        )
        .unwrap();
        q.enqueue(f(0, i64::MIN, 1)).unwrap();
        assert_eq!(q.poll(0).unwrap().discarded.len(), 1)
    }
    #[test]
    fn raii_leases_survive_reject_and_return_then_release() {
        use std::{cell::Cell, rc::Rc};
        struct Lease(Rc<Cell<u32>>);
        impl Drop for Lease {
            fn drop(&mut self) {
                self.0.set(self.0.get() + 1)
            }
        }
        let releases = Rc::new(Cell::new(0));
        let mut q = Scheduler::new(
            Timebase::new(1, 1).unwrap(),
            0,
            0,
            1,
            1,
            LatePolicy::Deliver,
        )
        .unwrap();
        let make = |pts| Frame {
            epoch: 0,
            pts,
            duration: 1,
            bytes: 1,
            payload: Lease(releases.clone()),
        };
        q.enqueue(make(1)).ok().unwrap();
        let rejected = q.enqueue(make(2)).err().unwrap();
        assert_eq!(releases.get(), 0);
        drop(rejected.frame);
        assert_eq!(releases.get(), 1);
        let returned = q.cancel().unwrap();
        assert_eq!(releases.get(), 1);
        drop(returned);
        assert_eq!(releases.get(), 2)
    }
}
