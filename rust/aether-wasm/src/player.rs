//! Single-owner, pointer-free session and clock ABI. Export names carry the `aether_player_` prefix.
use aether_player::clock::{MediaClock, Timebase};
use aether_player::session::{Outcome, Phase, Session};
use std::cell::RefCell;

struct Owner {
    session: Session,
    clock: Option<MediaClock>,
    result: i64,
}
thread_local! { static OWNER: RefCell<Option<Owner>> = const { RefCell::new(None) }; static DEAD: std::cell::Cell<bool> = const { std::cell::Cell::new(false) }; }
// 0 success, 1 absent, 2 busy/invalid state, 3 stale generation, 4 stale epoch,
// 5 resource/limit error, 6 invalid argument/timebase, 7 arithmetic/host error.
fn with_owner(f: impl FnOnce(&mut Owner) -> i32) -> i32 {
    OWNER.with(|slot| match slot.borrow_mut().as_mut() {
        Some(o) => f(o),
        None => 1,
    })
}
fn check_generation(o: &Owner, g: u64) -> Result<(), i32> {
    if g == 0 || o.session.generation() != g {
        Err(3)
    } else if o.session.phase() != Phase::Active {
        Err(2)
    } else {
        Ok(())
    }
}
fn sesserr(e: aether_player::session::SessionError) -> i32 {
    use aether_player::session::SessionError::*;
    match e {
        StaleGeneration => 3,
        Busy | InvalidPhase => 2,
        GenerationOverflow => 7,
        _ => 5,
    }
}
#[unsafe(no_mangle)]
pub extern "C" fn aether_player_create() -> i32 {
    OWNER.with(|s| {
        let mut s = s.borrow_mut();
        if s.is_some() || DEAD.with(|d| d.get()) {
            return 2;
        }
        *s = Some(Owner {
            session: Session::new(16),
            clock: None,
            result: 0,
        });
        0
    })
}
#[unsafe(no_mangle)]
pub extern "C" fn aether_player_destroy() -> i32 {
    OWNER.with(|s| {
        let mut s = s.borrow_mut();
        match s.as_ref() {
            None => 1,
            Some(o) if matches!(o.session.phase(), Phase::Active | Phase::Cleaning(_)) => 2,
            Some(_) => {
                *s = None;
                DEAD.with(|d| d.set(true));
                0
            }
        }
    })
}
#[unsafe(no_mangle)]
pub extern "C" fn aether_player_result() -> i64 {
    OWNER.with(|s| s.borrow().as_ref().map_or(0, |o| o.result))
}
#[unsafe(no_mangle)]
pub extern "C" fn aether_player_start() -> i32 {
    with_owner(|o| match o.session.start() {
        Ok(g) => {
            o.clock = None;
            o.result = g as i64;
            0
        }
        Err(e) => sesserr(e),
    })
}
#[unsafe(no_mangle)]
pub extern "C" fn aether_player_acquire(g: u64, id: u64) -> i32 {
    with_owner(|o| o.session.acquire(g, id).map_or_else(sesserr, |_| 0))
}
#[unsafe(no_mangle)]
pub extern "C" fn aether_player_release(g: u64, id: u64) -> i32 {
    with_owner(|o| o.session.confirm_release(g, id).map_or_else(sesserr, |_| 0))
}
#[unsafe(no_mangle)]
pub extern "C" fn aether_player_finish(g: u64, kind: u32) -> i32 {
    with_owner(|o| {
        let outcome = match kind {
            1 => Outcome::Cancelled,
            2 => Outcome::Eof,
            3 => Outcome::Error,
            _ => return 6,
        };
        o.session.finish(g, outcome).map_or_else(sesserr, |_| 0)
    })
}
#[unsafe(no_mangle)]
pub extern "C" fn aether_player_phase() -> i32 {
    OWNER.with(|s| match s.borrow().as_ref().map(|o| o.session.phase()) {
        None => -1,
        Some(Phase::Idle) => 0,
        Some(Phase::Active) => 1,
        Some(Phase::Cleaning(_)) => 2,
        Some(Phase::Finished(_)) => 3,
    })
}
#[unsafe(no_mangle)]
pub extern "C" fn aether_player_convert(ticks: i64, num: u32, den: u32) -> i32 {
    with_owner(|o| {
        let v = Timebase::new(num, den)
            .and_then(|t| t.to_nanos(ticks))
            .and_then(|v| i64::try_from(v).map_err(|_| aether_player::clock::ClockError::Overflow));
        match v {
            Ok(v) => {
                o.result = v;
                0
            }
            Err(aether_player::clock::ClockError::InvalidTimebase) => 6,
            Err(_) => 7,
        }
    })
}
#[unsafe(no_mangle)]
pub extern "C" fn aether_player_clock_create(g: u64, media: i64, host: u64) -> i32 {
    with_owner(|o| {
        if let Err(e) = check_generation(o, g) {
            return e;
        }
        if o.clock.is_some() {
            return 2;
        }
        o.clock = Some(MediaClock::new(media as i128, host));
        o.result = 0;
        0
    })
}
// Each operation uses a clone: a failed narrowing never commits host/state changes.
// op: 0 position, 1 pause, 2 resume, 3 seek/discontinuity.
#[unsafe(no_mangle)]
pub extern "C" fn aether_player_clock_op(g: u64, epoch: u64, op: u32, host: u64, media: i64) -> i32 {
    with_owner(|o| {
        if let Err(e) = check_generation(o, g) {
            return e;
        }
        let Some(existing) = o.clock.as_ref() else {
            return 2;
        };
        if existing.epoch() != epoch {
            return 4;
        }
        let mut c = existing.clone();
        let value = match op {
            0 => c.position(host),
            1 => c.pause(host).and_then(|_| c.position(host)),
            2 => c.resume(host).and_then(|_| c.position(host)),
            3 => c.reanchor(media as i128, host).map(|e| e as i128),
            _ => return 6,
        };
        match value
            .and_then(|v| i64::try_from(v).map_err(|_| aether_player::clock::ClockError::Overflow))
        {
            Ok(v) => {
                o.clock = Some(c);
                o.result = v;
                0
            }
            Err(aether_player::clock::ClockError::InvalidState) => 2,
            Err(_) => 7,
        }
    })
}
