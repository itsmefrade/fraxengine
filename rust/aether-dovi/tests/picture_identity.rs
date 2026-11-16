use aether_dovi::{hevc::Framing, picture_identity::*, source_association::*};
#[derive(Default)]
struct Writer {
    bits: Vec<bool>,
}
impl Writer {
    fn u(&mut self, v: u64, n: usize) {
        for b in (0..n).rev() {
            self.bits.push(v >> b & 1 != 0)
        }
    }
    fn ue(&mut self, v: u32) {
        let x = u64::from(v) + 1;
        let bits = 64 - x.leading_zeros() as usize;
        self.u(0, bits - 1);
        self.u(x, bits)
    }
    fn nal(mut self, t: u8) -> Vec<u8> {
        self.u(1, 1);
        while !self.bits.len().is_multiple_of(8) {
            self.u(0, 1)
        }
        let mut raw = vec![];
        for chunk in self.bits.chunks(8) {
            raw.push(chunk.iter().fold(0, |a, &b| a << 1 | u8::from(b)))
        }
        let mut out = vec![t << 1, 1];
        let mut zeros = 0;
        for b in raw {
            if zeros >= 2 && b <= 3 {
                out.push(3);
                zeros = 0
            }
            out.push(b);
            zeros = if b == 0 { zeros + 1 } else { 0 };
        }
        out
    }
}
// Identity-prefix controls, not decodable full parameter sets or CABAC payloads.
fn sps(bits: u8) -> Vec<u8> {
    let mut w = Writer::default();
    w.u(0, 4);
    w.u(0, 3);
    w.u(1, 1);
    w.u(0, 64);
    w.u(0, 32);
    w.ue(0);
    w.ue(1);
    w.ue(128);
    w.ue(128);
    w.u(0, 1);
    w.ue(2);
    w.ue(2);
    w.ue(u32::from(bits) - 4);
    w.u(1, 1);
    w.ue(3);
    w.ue(2);
    w.ue(0);
    w.ue(0);
    w.ue(3);
    w.nal(33)
}
fn pps() -> Vec<u8> {
    let mut w = Writer::default();
    w.ue(0);
    w.ue(0);
    w.u(1, 1);
    w.u(0, 1);
    w.u(0, 3);
    w.nal(34)
}
fn slice(
    t: u8,
    lsb: u32,
    bits: u8,
    first: bool,
    dependent: bool,
    address: u32,
    no_prior: bool,
) -> Vec<u8> {
    let mut w = Writer::default();
    w.u(u64::from(first), 1);
    if (16..=23).contains(&t) {
        w.u(u64::from(no_prior), 1)
    }
    w.ue(0);
    if !first {
        w.u(u64::from(dependent), 1);
        w.u(u64::from(address), 2)
    }
    if !dependent {
        w.ue(2);
        if !matches!(t, 19 | 20) {
            w.u(u64::from(lsb), bits as usize)
        }
    }
    w.nal(t)
}
fn tracker() -> PictureIdentityTracker {
    let mut t = PictureIdentityTracker::new(0);
    t.push_nal(&sps(8), 0).unwrap();
    t.push_nal(&pps(), 0).unwrap();
    t
}
fn started(t: &mut PictureIdentityTracker, nal: &[u8]) -> PictureIdentity {
    match t.push_nal(nal, 0).unwrap() {
        IdentityEvent::PictureStart(v) => v,
        _ => panic!("not picture"),
    }
}
#[test]
fn wrap_and_checked_extremes() {
    assert_eq!(derive_poc(15, 1, 4, false), Ok(17));
    assert_eq!(derive_poc(17, 15, 4, false), Ok(15));
    assert_eq!(derive_poc(0, 240, 8, true), Ok(240));
    assert_eq!(derive_poc(0, 240, 8, false), Ok(-16));
    assert!(derive_poc(i32::MAX, 0, 16, false).is_err());
    assert!(derive_poc(0, 65536, 16, false).is_err());
}
#[test]
fn multislice_counts_once_and_rejects_changed_identity() {
    let mut t = tracker();
    let id = started(&mut t, &slice(19, 0, 8, true, false, 0, false));
    assert_eq!(id.decode_index, 0);
    assert_eq!(
        t.push_nal(&slice(19, 0, 8, false, true, 1, false), 0),
        Ok(IdentityEvent::SliceContinuation(id))
    );
    assert_eq!(
        t.push_nal(&slice(19, 0, 8, false, false, 2, false), 0),
        Ok(IdentityEvent::SliceContinuation(id))
    );
    t.end_access_unit(0).unwrap();
    assert_eq!(
        started(&mut t, &slice(1, 1, 8, true, false, 0, false)).decode_index,
        1
    );
    assert_eq!(
        t.push_nal(&slice(1, 2, 8, false, false, 1, false), 0),
        Err(IdentityError::MultiSlice)
    );
    assert_eq!(t.end_access_unit(0), Err(IdentityError::Poisoned));
}
#[test]
fn cra_rasl_suppression_and_midstream_cra_continuation() {
    let mut t = tracker();
    let first = started(&mut t, &slice(21, 240, 8, true, false, 0, false));
    assert!(first.starts_sequence && first.no_rasl_output);
    t.end_access_unit(0).unwrap();
    let rasl = started(&mut t, &slice(8, 238, 8, true, false, 0, false));
    assert!(rasl.suppressed_rasl && !rasl.output_expected);
    assert_eq!(rasl.poc, 238);
    t.end_access_unit(0).unwrap();
    let next = started(&mut t, &slice(1, 241, 8, true, false, 0, false));
    assert_eq!(next.poc, 241);
    t.end_access_unit(0).unwrap();
    let cra = started(&mut t, &slice(21, 248, 8, true, false, 0, true));
    assert!(!cra.starts_sequence && !cra.drop_prior_output);
    t.end_access_unit(0).unwrap();
    let leading = started(&mut t, &slice(8, 246, 8, true, false, 0, false));
    assert!(leading.output_expected && !leading.suppressed_rasl);
    assert_eq!(leading.sequence, 0);
}
#[test]
fn idr_eos_and_epoch_boundaries_are_explicit() {
    let mut t = tracker();
    started(&mut t, &slice(19, 0, 8, true, false, 0, false));
    t.end_access_unit(0).unwrap();
    let idr = started(&mut t, &slice(19, 0, 8, true, false, 0, true));
    assert_eq!(idr.sequence, 1);
    assert!(idr.drop_prior_output);
    t.end_access_unit(0).unwrap();
    t.push_nal(&[36 << 1, 1, 0x80], 0).unwrap();
    assert_eq!(
        t.push_nal(&slice(1, 1, 8, true, false, 0, false), 0),
        Err(IdentityError::AwaitingIrap)
    );
    let cra = started(&mut t, &slice(21, 200, 8, true, false, 0, false));
    assert_eq!(cra.sequence, 2);
    assert!(cra.drop_prior_output);
    t.reset(1).unwrap();
    assert_eq!(t.push_nal(&pps(), 0), Err(IdentityError::Epoch));
    assert_eq!(
        t.push_nal(&slice(19, 0, 8, true, false, 0, false), 1),
        Err(IdentityError::MissingParameterSet)
    );
}
fn au(bl: &[u8], el: &[u8], rpu: bool) -> Vec<u8> {
    let mut bytes = vec![];
    let wrapped = [&[0x7e, 1][..], el].concat();
    let mut nals = vec![bl.to_vec(), wrapped];
    if rpu {
        let raw = include_bytes!("data/fel-template.bin");
        nals.insert(0, [&[0x7c, 1][..], &raw[4..]].concat())
    }
    for nal in nals {
        bytes.extend((nal.len() as u32).to_be_bytes());
        bytes.extend(nal)
    }
    bytes
}
fn source(n: u64) -> SourceUnit {
    SourceUnit {
        epoch: 0,
        token: n,
        pts: SourceTime {
            ticks: 42_042_000 + n as i64 * 41_708,
            numerator: 1,
            denominator: 1_000_000,
        },
    }
}
fn associated() -> SourceAssociationTracker {
    let mut a = SourceAssociationTracker::new(0);
    for el in [false, true] {
        a.seed(el, &sps(8), 0).unwrap();
        a.seed(el, &pps(), 0).unwrap();
    }
    a
}
#[test]
fn different_poc_origins_and_rpu_bind_to_base_source_not_el() {
    let mut a = associated();
    let o = a
        .observe(
            &au(
                &slice(21, 240, 8, true, false, 0, false),
                &slice(19, 0, 8, true, false, 0, false),
                true,
            ),
            Framing::LengthPrefixed(4),
            source(0),
            true,
        )
        .unwrap();
    assert_eq!(o.base.unwrap().canonical, o.enhancement.unwrap().canonical);
    assert_eq!(o.rpu_nal_index, Some(0));
    assert!(o.rpu_crc32.is_some());
    let next = a
        .observe(
            &au(
                &slice(1, 241, 8, true, false, 0, false),
                &slice(1, 3, 8, true, false, 0, false),
                true,
            ),
            Framing::LengthPrefixed(4),
            source(1),
            false,
        )
        .unwrap();
    assert_eq!(next.base.unwrap().canonical.unwrap().relative_poc, 1);
    assert_eq!(next.enhancement.unwrap().canonical.unwrap().relative_poc, 3);
    assert_eq!(next.base.unwrap().source.pts, source(1).pts);
    assert_eq!(next.rpu_nal_index, Some(0));
}
#[test]
fn unproved_sequence_change_never_falls_back_to_au_index() {
    let mut a = associated();
    a.observe(
        &au(
            &slice(19, 0, 8, true, false, 0, false),
            &slice(19, 0, 8, true, false, 0, false),
            false,
        ),
        Framing::LengthPrefixed(4),
        source(0),
        true,
    )
    .unwrap();
    assert_eq!(
        a.observe(
            &au(
                &slice(1, 1, 8, true, false, 0, false),
                &slice(19, 0, 8, true, false, 0, false),
                false
            ),
            Framing::LengthPrefixed(4),
            source(1),
            false
        ),
        Err(AssociationError::SequenceDiscontinuity)
    );
    assert_eq!(
        a.observe(
            &au(
                &slice(1, 2, 8, true, false, 0, false),
                &slice(1, 1, 8, true, false, 0, false),
                false
            ),
            Framing::LengthPrefixed(4),
            source(2),
            false
        ),
        Err(AssociationError::Poisoned)
    );
}
#[test]
fn canonical_pairer_preserves_overlapping_irap_anchors_and_bounded_epochs() {
    use aether_dovi::pairing::{CanonicalLayerPairer, Layer, PairError};
    let mut p = CanonicalLayerPairer::new(0, 2).unwrap();
    let old = CanonicalKey {
        epoch: 0,
        anchor: 0,
        relative_poc: 0,
    };
    let new = CanonicalKey { anchor: 1, ..old };
    assert!(p.push(old, Layer::Base, "oldBL").unwrap().is_none());
    assert!(p.push(new, Layer::Enhancement, "newEL").unwrap().is_none());
    assert!(matches!(
        p.push(
            CanonicalKey {
                relative_poc: 1,
                ..new
            },
            Layer::Base,
            "over"
        ),
        Err(PairError::CapacityExceeded)
    ));
    let oldpair = p.push(old, Layer::Enhancement, "oldEL").unwrap().unwrap();
    assert_eq!((oldpair.base, oldpair.enhancement), ("oldBL", "oldEL"));
    let newpair = p.push(new, Layer::Base, "newBL").unwrap().unwrap();
    assert_eq!((newpair.base, newpair.enhancement), ("newBL", "newEL"));
    p.reset(1);
    assert!(matches!(
        p.push(old, Layer::Base, "stale"),
        Err(PairError::StaleEpoch)
    ));
}
#[test]
fn source_association_rejects_unbound_or_oversized_rpu_and_preserves_signed_ticks() {
    let mut a = associated();
    let mut stamp = source(0);
    stamp.pts.ticks = i64::MIN;
    let packet = au(
        &slice(19, 0, 8, true, false, 0, false),
        &slice(19, 0, 8, true, false, 0, false),
        true,
    );
    let observed = a
        .observe(&packet, Framing::LengthPrefixed(4), stamp, true)
        .unwrap();
    assert_eq!(observed.base.unwrap().source.pts.ticks, i64::MIN);
    let mut missing = associated();
    assert_eq!(
        missing.observe(
            &au(
                &[35 << 1, 1, 0x50],
                &slice(19, 0, 8, true, false, 0, false),
                true
            ),
            Framing::LengthPrefixed(4),
            source(0),
            false
        ),
        Err(AssociationError::RpuWithoutBase)
    );
    let raw = include_bytes!("data/fel-template.bin");
    let mut rpu = [&[0x7c, 1][..], &raw[4..]].concat();
    rpu.resize(65537, 0);
    assert!(aether_dovi::rpu::crc_is_valid(&rpu));
    let mut bytes = (rpu.len() as u32).to_be_bytes().to_vec();
    bytes.extend(rpu);
    let mut oversized = associated();
    assert_eq!(
        oversized.observe(&bytes, Framing::LengthPrefixed(4), source(0), false),
        Err(AssociationError::Framing)
    );
}
#[test]
fn header_only_eos_and_eob_match_pinned_decoder_sequence_rules() {
    let mut eos = tracker();
    started(&mut eos, &slice(19, 0, 8, true, false, 0, false));
    eos.end_access_unit(0).unwrap();
    assert_eq!(eos.push_nal(&[36 << 1, 1], 0), Ok(IdentityEvent::Boundary));
    let cra = started(&mut eos, &slice(21, 5, 8, true, false, 0, false));
    assert!(cra.starts_sequence && cra.drop_prior_output);
    assert_eq!(cra.sequence, 1);
    let mut eob = tracker();
    started(&mut eob, &slice(19, 0, 8, true, false, 0, false));
    eob.end_access_unit(0).unwrap();
    assert_eq!(eob.push_nal(&[37 << 1, 1], 0), Ok(IdentityEvent::Boundary));
    let cra = started(&mut eob, &slice(21, 5, 8, true, false, 0, false));
    assert!(!cra.starts_sequence && !cra.drop_prior_output);
    assert_eq!(cra.sequence, 0);
}
