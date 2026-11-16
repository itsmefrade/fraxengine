use aether_player::clock::{ClockError, ClockState, MediaClock, Timebase};

#[test]
fn rational_pts_dts_and_long_duration() {
    let mpeg = Timebase::new(1, 90_000).unwrap();
    assert_eq!(mpeg.to_nanos(90_000).unwrap(), 1_000_000_000);
    assert_eq!(mpeg.to_nanos(-4_500).unwrap(), -50_000_000);
    assert_eq!(mpeg.to_nanos(3).unwrap(), 33_333);
    assert_eq!(mpeg.to_nanos(-3).unwrap(), -33_333);
    assert_eq!(
        mpeg.to_nanos(90_000 * 86_400 * 365).unwrap(),
        31_536_000_000_000_000
    );
    let audio = Timebase::new(1, 48_000).unwrap();
    assert_eq!(mpeg.rescale(90_000, audio).unwrap(), 48_000);
    assert_eq!(audio.rescale(-24_000, mpeg).unwrap(), -45_000);
    assert_eq!(Timebase::new(0, 1), Err(ClockError::InvalidTimebase));
    assert_eq!(Timebase::new(1, 0), Err(ClockError::InvalidTimebase));
    assert_eq!(
        Timebase::new(u32::MAX, 1)
            .unwrap()
            .rescale(i64::MAX, Timebase::new(1, u32::MAX).unwrap()),
        Err(ClockError::Overflow)
    );
}
#[test]
fn host_pause_seek_and_discontinuity_are_monotonic_and_transactional() {
    let mut c = MediaClock::new(-500, 100);
    assert_eq!(c.position(200).unwrap(), -400);
    c.pause(250).unwrap();
    assert_eq!(c.position(1000).unwrap(), -350);
    assert_eq!(c.resume(999), Err(ClockError::HostWentBackwards));
    assert_eq!(c.state(), ClockState::Paused);
    assert_eq!(c.reanchor(12_000, 1000).unwrap(), 1);
    assert_eq!(c.position(1100).unwrap(), 12_000);
    c.resume(1200).unwrap();
    assert_eq!(c.position(1250).unwrap(), 12_050);
    assert_eq!(c.reanchor(-1000, 1249), Err(ClockError::HostWentBackwards));
    assert_eq!(c.epoch(), 1);
    assert_eq!(c.reanchor(-1000, 1300).unwrap(), 2);
    assert_eq!(c.position(1301).unwrap(), -999);
    let mut overflow = MediaClock::new(i128::MAX, 0);
    assert_eq!(overflow.position(1), Err(ClockError::Overflow));
    assert_eq!(overflow.position(0).unwrap(), i128::MAX);
}

#[test]
fn pause_overflow_does_not_advance_state_or_host() {
    let mut c = MediaClock::new(i128::MAX, 10);
    assert_eq!(c.pause(11), Err(ClockError::Overflow));
    assert_eq!(c.state(), ClockState::Running);
    assert_eq!(c.position(10), Ok(i128::MAX));
}
