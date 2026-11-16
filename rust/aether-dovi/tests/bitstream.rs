use aether_dovi::{
    hevc::{self, Error, Framing, Kind, Limits},
    rpu,
};

fn prefixed(nals: &[&[u8]], width: usize) -> Vec<u8> {
    let mut out = Vec::new();
    for nal in nals {
        out.extend_from_slice(&(nal.len() as u32).to_be_bytes()[4 - width..]);
        out.extend_from_slice(nal);
    }
    out
}
fn escaped_rpu(payload: &[u8]) -> Vec<u8> {
    let mut raw = vec![0x19];
    raw.extend_from_slice(payload);
    raw.extend_from_slice(&rpu::mpeg2_crc32(payload).to_be_bytes());
    raw.push(0x80);
    let mut out = vec![0x7c, 1];
    let mut zeros = 0;
    for byte in raw {
        if zeros >= 2 && byte <= 3 {
            out.push(3);
            zeros = 0;
        }
        out.push(byte);
        zeros = if byte == 0 { zeros + 1 } else { 0 };
    }
    out
}

#[test]
fn both_framings_keep_base_enhancement_and_rpu_bytes() {
    let base = [0x26, 1, 0x80, 9];
    let enhancement = [0x7e, 1, 0x26, 1, 0x80, 8];
    let rpu = escaped_rpu(&[0, 0, 1, 7]);
    let mut annex = vec![0, 0];
    for nal in [base.as_slice(), enhancement.as_slice(), rpu.as_slice()] {
        annex.extend_from_slice(&[0, 0, 0, 1]);
        annex.extend_from_slice(nal);
    }
    annex.extend_from_slice(&[0, 0]);
    for (data, framing) in [
        (annex, Framing::AnnexB),
        (
            prefixed(&[&base, &enhancement, &rpu], 4),
            Framing::LengthPrefixed(4),
        ),
    ] {
        let nals = hevc::split(&data, framing, Limits::default()).unwrap();
        assert_eq!(
            nals.iter().map(|n| n.kind).collect::<Vec<_>>(),
            [Kind::Base, Kind::Enhancement, Kind::Rpu]
        );
        assert_eq!(nals[0].bytes, base);
        assert_eq!(nals[1].decode_bytes(), &enhancement[2..]);
        assert_eq!(nals[2].bytes, rpu);
        let report = hevc::inspect(&data, framing, Limits::default()).unwrap();
        assert_eq!(
            (
                report.base,
                report.enhancement,
                report.rpu,
                report.crc_valid_rpu
            ),
            (1, 1, 1, 1)
        );
    }
}

#[test]
fn length_prefix_widths_are_explicit_and_checked() {
    for width in 1..=4 {
        let data = prefixed(&[&[0x40, 1, 0x80], &[0x42, 1, 0x80]], width);
        assert_eq!(
            hevc::split(
                &data,
                Framing::LengthPrefixed(width as u8),
                Limits::default()
            )
            .unwrap()
            .len(),
            2
        );
    }
    assert_eq!(
        hevc::split(&[0, 0, 0], Framing::LengthPrefixed(4), Limits::default()).unwrap_err(),
        Error::TruncatedLength
    );
    assert_eq!(
        hevc::split(
            &[0xff, 0xff, 0xff, 0xff],
            Framing::LengthPrefixed(4),
            Limits::default()
        )
        .unwrap_err(),
        Error::NalOverrun
    );
    assert_eq!(
        hevc::split(&[0], Framing::LengthPrefixed(1), Limits::default()).unwrap_err(),
        Error::EmptyNal
    );
    assert_eq!(
        hevc::split(&[0], Framing::LengthPrefixed(0), Limits::default()).unwrap_err(),
        Error::InvalidLengthSize
    );
}

#[test]
fn malformed_headers_and_wrappers_fail_closed() {
    for (nal, expected) in [
        (vec![0x26], Error::TruncatedHeader),
        (vec![0x80, 1], Error::ForbiddenZeroBit),
        (vec![0x26, 0], Error::InvalidTemporalId),
        (vec![0x7e, 1], Error::InvalidEnhancementHeader),
        (vec![0x7e, 1, 0x7e, 1], Error::InvalidEnhancementHeader),
    ] {
        assert_eq!(
            hevc::split(
                &prefixed(&[&nal], 4),
                Framing::LengthPrefixed(4),
                Limits::default()
            )
            .unwrap_err(),
            expected
        );
    }
    assert_eq!(
        hevc::split(&[9, 0, 0, 1, 0x26, 1], Framing::AnnexB, Limits::default()).unwrap_err(),
        Error::JunkBeforeStartCode
    );
    assert_eq!(
        hevc::split(
            &[0, 0, 1, 0, 0, 1, 0x26, 1],
            Framing::AnnexB,
            Limits::default()
        )
        .unwrap_err(),
        Error::EmptyNal
    );
}

#[test]
fn caller_limits_bound_access_unit_nal_and_count() {
    let data = prefixed(&[&[0x26, 1, 0x80], &[0x26, 1, 0x80]], 4);
    for (limits, error) in [
        (
            Limits {
                max_access_unit_bytes: 4,
                ..Limits::default()
            },
            Error::AccessUnitTooLarge,
        ),
        (
            Limits {
                max_nal_bytes: 2,
                ..Limits::default()
            },
            Error::NalTooLarge,
        ),
        (
            Limits {
                max_nals: 1,
                ..Limits::default()
            },
            Error::TooManyNals,
        ),
    ] {
        assert_eq!(
            hevc::split(&data, Framing::LengthPrefixed(4), limits).unwrap_err(),
            error
        );
    }
}

#[test]
fn rpu_crc_checks_known_polynomial_and_corruption() {
    assert_eq!(rpu::mpeg2_crc32(b"123456789"), 0x0376e6e7);
    let rpu = escaped_rpu(&[0, 0, 0, 1, 2, 3, 9]);
    assert!(rpu::crc_is_valid(&rpu));
    let mut corrupt = rpu.clone();
    *corrupt.last_mut().unwrap() = 0x81;
    assert!(!rpu::crc_is_valid(&corrupt));
    corrupt = rpu.clone();
    corrupt[3] ^= 0x40;
    assert!(!rpu::crc_is_valid(&corrupt));
    assert!(!rpu::crc_is_valid(&[0x7c, 1, 0x19, 0, 0, 3, 0xff, 0x80]));
}

#[test]
fn deterministic_hostile_byte_corpus_never_panics() {
    let mut state = 0x12345678u32;
    for len in 0..256 {
        let bytes: Vec<u8> = (0..len)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                state as u8
            })
            .collect();
        for framing in [
            Framing::AnnexB,
            Framing::LengthPrefixed(1),
            Framing::LengthPrefixed(2),
            Framing::LengthPrefixed(3),
            Framing::LengthPrefixed(4),
        ] {
            let _ = hevc::split(&bytes, framing, Limits::default());
        }
        let _ = rpu::crc_is_valid(&bytes);
    }
}
