use aether_dovi::pairing::{Layer, LayerPairer, PairError, PictureKey};
fn key(epoch: u64, picture: i64) -> PictureKey {
    PictureKey { epoch, picture }
}
#[test]
fn out_of_order_layers_pair_by_canonical_picture_not_arrival_order() {
    let mut p = LayerPairer::new(1, 2);
    assert!(
        p.push(key(1, 2), Layer::Enhancement, "el2")
            .unwrap()
            .is_none()
    );
    assert!(p.push(key(1, 1), Layer::Base, "bl1").unwrap().is_none());
    let pair = p.push(key(1, 2), Layer::Base, "bl2").unwrap().unwrap();
    assert_eq!(
        (pair.key.picture, pair.base, pair.enhancement),
        (2, "bl2", "el2")
    );
    let pair = p
        .push(key(1, 1), Layer::Enhancement, "el1")
        .unwrap()
        .unwrap();
    assert_eq!(
        (pair.key.picture, pair.base, pair.enhancement),
        (1, "bl1", "el1")
    );
    assert!(p.is_empty());
}
#[test]
fn bounded_queue_rejects_overflow_without_losing_existing_pair() {
    let mut p = LayerPairer::new(3, 1);
    p.push(key(3, 1), Layer::Base, 1).unwrap();
    assert_eq!(
        p.push(key(3, 2), Layer::Base, 2).unwrap_err(),
        PairError::CapacityExceeded
    );
    assert_eq!(
        p.push(key(3, 1), Layer::Base, 9).unwrap_err(),
        PairError::DuplicateLayer
    );
    assert_eq!(
        p.push(key(3, 1), Layer::Enhancement, 3)
            .unwrap()
            .unwrap()
            .base,
        1
    );
}
#[test]
fn seek_epoch_flushes_old_frames_and_refuses_stale_callbacks() {
    let mut p = LayerPairer::new(1, 2);
    p.push(key(1, 4), Layer::Base, 1).unwrap();
    p.reset(2);
    assert!(p.is_empty());
    assert_eq!(
        p.push(key(1, 4), Layer::Enhancement, 2).unwrap_err(),
        PairError::StaleEpoch
    );
    assert!(p.push(key(2, 4), Layer::Enhancement, 2).unwrap().is_none());
    p.discard(4);
    assert!(p.is_empty());
}
