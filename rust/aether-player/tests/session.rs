use aether_player::session::{Outcome, Phase, Session, SessionError};

#[test]
fn cancellation_waits_for_every_confirmed_resource_and_fences_old_callbacks() {
    let mut s = Session::new(2);
    let first = s.start().unwrap();
    s.acquire(first, 10).unwrap();
    s.acquire(first, 11).unwrap();
    assert_eq!(s.acquire(first, 12), Err(SessionError::ResourceLimit));
    s.finish(first, Outcome::Cancelled).unwrap();
    assert_eq!(s.phase(), Phase::Cleaning(Outcome::Cancelled));
    assert_eq!(s.start(), Err(SessionError::Busy));
    assert_eq!(
        s.finish(first, Outcome::Eof),
        Err(SessionError::InvalidPhase)
    );
    s.confirm_release(first, 11).unwrap();
    assert_eq!(s.start(), Err(SessionError::Busy));
    assert_eq!(
        s.confirm_release(first, 11),
        Err(SessionError::UnknownResource)
    );
    s.confirm_release(first, 10).unwrap();
    assert_eq!(s.phase(), Phase::Finished(Outcome::Cancelled));
    let second = s.start().unwrap();
    s.acquire(second, 10).unwrap();
    assert_eq!(
        s.confirm_release(first, 10),
        Err(SessionError::StaleGeneration)
    );
    assert_eq!(
        s.finish(first, Outcome::Error),
        Err(SessionError::StaleGeneration)
    );
    assert_eq!(s.owned_resources().collect::<Vec<_>>(), vec![10]);
}
#[test]
fn normal_release_and_terminal_outcomes_preserve_ownership() {
    for outcome in [Outcome::Eof, Outcome::Error] {
        let mut s = Session::new(2);
        let g = s.start().unwrap();
        s.acquire(g, 1).unwrap();
        assert_eq!(s.acquire(g, 1), Err(SessionError::DuplicateResource));
        s.confirm_release(g, 1).unwrap();
        assert_eq!(s.acquire(g, 1), Err(SessionError::DuplicateResource));
        s.acquire(g, 2).unwrap();
        s.finish(g, outcome).unwrap();
        assert_eq!(s.acquire(g, 3), Err(SessionError::InvalidPhase));
        assert_eq!(s.phase(), Phase::Cleaning(outcome));
        s.confirm_release(g, 2).unwrap();
        assert_eq!(s.phase(), Phase::Finished(outcome));
    }
    let mut empty = Session::new(0);
    let g = empty.start().unwrap();
    assert_eq!(empty.acquire(g, 1), Err(SessionError::ResourceLimit));
    empty.finish(g, Outcome::Eof).unwrap();
    assert_eq!(empty.phase(), Phase::Finished(Outcome::Eof));
}

#[test]
fn released_ids_still_count_against_generation_limit() {
    let mut s = Session::new(1);
    let g = s.start().unwrap();
    s.acquire(g, 10).unwrap();
    s.confirm_release(g, 10).unwrap();
    assert_eq!(s.acquire(g, 11), Err(SessionError::ResourceLimit));
    s.finish(g, Outcome::Eof).unwrap();
    let next = s.start().unwrap();
    s.acquire(next, 11).unwrap();
}
