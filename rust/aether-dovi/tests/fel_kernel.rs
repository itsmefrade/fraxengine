use aether_dovi::fel::*;
use std::sync::atomic::AtomicBool;
fn identity() -> Curve {
    Curve::new(
        vec![0, 1023],
        vec![Piece::Polynomial {
            order: 1,
            coefficients: [0, 1 << 23, 0],
        }],
    )
    .unwrap()
}
fn params(nlq: Nlq) -> FelFrameParams {
    FelFrameParams::new(
        [identity(), identity(), identity()],
        [nlq; 3],
        23,
        12,
        false,
    )
    .unwrap()
}
#[test]
fn every_nlq_code_against_separate_wide_integer_formulas() {
    for offset in [0, 1, 512, 1023] {
        for (slope, threshold, max_q) in [
            (2048, 0, 1 << 20),
            (33077, 9123, 912399),
            (0, 301, 12000),
            (i32::MAX, i32::MAX, 100000),
        ] {
            let n = Nlq::new(offset, slope, threshold, max_q, 23).unwrap();
            for code in 0..1024 {
                let delta = i128::from(code) - i128::from(offset);
                let ref_value = if delta == 0 {
                    0
                } else {
                    let magnitude =
                        (delta.abs() * 2 - 1) * i128::from(slope) + 2 * i128::from(threshold);
                    let signed = magnitude * delta.signum();
                    let ceiling = i128::from(max_q) * 2;
                    // div_euclid expresses floor without copying a signed right shift.
                    signed.clamp(-ceiling, ceiling).div_euclid(256) as i32
                };
                assert_eq!(
                    n.residual_reference16(code).unwrap(),
                    ref_value,
                    "offset{offset} code{code}"
                );
                let upstream = if delta == 0 || slope == 0 || max_q >> 11 == 0 {
                    0
                } else {
                    let value = ((delta.abs() * 2 - 1) * i128::from(slope)
                        + 2 * i128::from(threshold))
                        / 4096;
                    (value.min(i128::from(max_q >> 11)) * delta.signum()) as i32
                };
                assert_eq!(n.residual_upstream(code, 12).unwrap(), upstream);
            }
        }
    }
}
#[test]
fn preserves_reference16_fraction_until_combining() {
    let p = params(Nlq::new(512, 2048, 0, 1 << 20, 23).unwrap());
    let pred = p
        .prediction(0, 512, [512; 3], ReconstructionMode::Reference16)
        .unwrap();
    assert_eq!(pred, 32768);
    for (code, expected) in [(511, 2048), (512, 2048), (513, 2049)] {
        let residual = Nlq::new(512, 2048, 0, 1 << 20, 23)
            .unwrap()
            .residual_reference16(code)
            .unwrap();
        assert_eq!(
            quantize_reference16((i32::from(pred) + residual) as u16, 12).unwrap(),
            expected
        );
        assert_eq!(
            p.prediction(0, 512, [512; 3], ReconstructionMode::UpstreamVdrF64)
                .unwrap()
                + Nlq::new(512, 2048, 0, 1 << 20, 23)
                    .unwrap()
                    .residual_upstream(code, 12)
                    .unwrap() as u16,
            2048
        );
    }
}
#[test]
fn duplicate_pivots_choose_last_piece_and_inputs_are_clamped() {
    let c = Curve::new(
        vec![64, 512, 512, 940],
        vec![
            Piece::Polynomial {
                order: 1,
                coefficients: [0, 0, 0],
            },
            Piece::Polynomial {
                order: 1,
                coefficients: [1 << 21, 0, 0],
            },
            Piece::Polynomial {
                order: 1,
                coefficients: [1 << 22, 0, 0],
            },
        ],
    )
    .unwrap();
    let p = FelFrameParams::new(
        [c, identity(), identity()],
        [Nlq::new(0, 0, 0, 0, 23).unwrap(); 3],
        23,
        12,
        true,
    )
    .unwrap();
    assert_eq!(
        p.prediction(0, 511, [511; 3], ReconstructionMode::Reference16)
            .unwrap(),
        0
    );
    assert_eq!(
        p.prediction(0, 512, [512; 3], ReconstructionMode::Reference16)
            .unwrap(),
        32768
    );
    assert_eq!(
        p.prediction(0, 1023, [1023; 3], ReconstructionMode::Reference16)
            .unwrap(),
        32768
    );
}
#[test]
fn mmr_all_terms_and_orders_use_staged_q20() {
    let input = [721, 317, 899];
    let [y, u, v] = input.map(i128::from);
    let first = [
        y * 1024,
        u * 1024,
        v * 1024,
        y * u,
        y * v,
        u * v,
        (y * u * v) / 1024,
    ];
    let second = [
        y * y,
        u * u,
        v * v,
        first[3] * first[3] / 1048576,
        first[4] * first[4] / 1048576,
        first[5] * first[5] / 1048576,
        first[6] * first[6] / 1048576,
    ];
    let third = std::array::from_fn::<_, 7, _>(|i| first[i] * second[i] / 1048576);
    for (order, terms) in [first, second, third].iter().enumerate() {
        for (term, &value) in terms.iter().enumerate() {
            let mut coefficients = [[0; 7]; 3];
            coefficients[order][term] = 1 << 23;
            let mmr = Curve::new(
                vec![0, 1023],
                vec![Piece::Mmr {
                    order: order as u8 + 1,
                    constant: 0,
                    coefficients,
                }],
            )
            .unwrap();
            let p = FelFrameParams::new(
                [identity(), mmr, identity()],
                [Nlq::new(0, 0, 0, 0, 23).unwrap(); 3],
                23,
                12,
                true,
            )
            .unwrap();
            assert_eq!(
                p.prediction(1, input[1], input, ReconstructionMode::Reference16)
                    .unwrap(),
                (value / 16) as u16,
                "order{order} term{term}"
            );
        }
    }
}
#[test]
fn planes_validate_last_row_padding_codes_and_overflow() {
    assert!(Plane::new(&[1, 2, 50000, 3, 4], 2, 2, 3).is_ok());
    assert_eq!(Plane::new(&[1, 2, 3], 2, 2, 2).unwrap_err(), Error::Buffer);
    assert_eq!(
        Plane::new(&[1024], 1, 1, 1).unwrap_err(),
        Error::SampleRange
    );
    assert_eq!(
        Plane::new(&[], 2, 2, usize::MAX).unwrap_err(),
        Error::Overflow
    );
    assert_eq!(Plane::new(&[], 0, 1, 0).unwrap_err(), Error::Dimensions);
}
#[test]
fn full_odd_frame_identity_and_cancellation() {
    let y = [
        0, 20, 100, 200, 1023, 512, 10, 30, 40, 50, 1000, 900, 800, 700, 600,
    ];
    let u = [10, 20, 30, 40, 50, 60];
    let v = [700; 6];
    let bl = Frame420::new(
        Plane::new(&y, 5, 3, 5).unwrap(),
        Plane::new(&u, 3, 2, 3).unwrap(),
        Plane::new(&v, 3, 2, 3).unwrap(),
    )
    .unwrap();
    let p = params(Nlq::new(512, 0, 0, 0, 23).unwrap());
    let mut oy = [0; 15];
    let mut ou = [0; 6];
    let mut ov = [0; 6];
    compose(
        &p,
        bl,
        bl,
        Output420 {
            y: &mut oy,
            cb: &mut ou,
            cr: &mut ov,
        },
        ReconstructionMode::Reference16,
        SpatialPolicy::EngineBilinearLumaAnnexBChromaClip10,
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(oy, y.map(|v| v * 64));
    assert_eq!(ou, u.map(|v| v * 64));
    assert_eq!(ov, v.map(|v| v * 64));
    assert_eq!(
        compose(
            &p,
            bl,
            bl,
            Output420 {
                y: &mut oy,
                cb: &mut ou,
                cr: &mut ov
            },
            ReconstructionMode::Reference16,
            SpatialPolicy::EngineBilinearLumaAnnexBChromaClip10,
            &AtomicBool::new(true)
        ),
        Err(Error::Cancelled)
    );
}
#[test]
fn rejects_unsupported_and_malformed_typed_params() {
    assert!(
        Curve::new(
            vec![100, 50],
            vec![Piece::Polynomial {
                order: 1,
                coefficients: [0; 3]
            }]
        )
        .is_err()
    );
    assert!(
        Curve::new(
            vec![0, 1023],
            vec![Piece::Mmr {
                order: 4,
                constant: 0,
                coefficients: [[0; 7]; 3]
            }]
        )
        .is_err()
    );
    assert!(Nlq::new(1024, 0, 0, 0, 23).is_err());
    assert!(Nlq::new(512, -1, 0, 0, 23).is_err());
    assert!(Nlq::new(512, 0, 0, i64::MAX, 23).is_err());
    assert_eq!(
        Nlq::new(512, 1, 0, 100, 12)
            .unwrap()
            .residual_reference16(513),
        Err(Error::Unsupported)
    );
}
#[test]
fn guarded_rpu_path_rejects_crc_mutation_and_epoch_reuse() {
    let data = include_bytes!("data/fel-template.bin");
    let nal = [&[0x7c, 1][..], &data[4..]].concat();
    let mut history = MappingHistory::default();
    assert!(matches!(history.resolve(1, &nal), Err(Error::Epoch)));
    history.reset(1);
    let result = history.resolve(1, &nal);
    assert!(result.is_ok(), "{result:?}");
    let mut bad = nal.to_vec();
    bad[10] ^= 1;
    assert!(matches!(history.resolve(1, &bad), Err(Error::Rpu)));
    history.reset(2);
    assert!(matches!(history.resolve(1, &nal), Err(Error::Epoch)));
}
#[test]
fn guarded_mapping_reuse_residual_flag_nonzero_id_and_corruption_reset() {
    let data = include_bytes!("data/fel-template.bin");
    let full = [&[0x7c, 1][..], &data[4..]].concat();
    // Trusted fixture transformation only; every resulting NAL enters the guarded API.
    let mut trusted = aether_dovi::semantic::parse_rpu_checked(&full).unwrap();
    trusted.header.use_prev_vdr_rpu_flag = true;
    trusted.header.prev_vdr_rpu_id = 0;
    trusted.header.disable_residual_flag = true;
    trusted.rpu_data_mapping = None;
    trusted.modified = true;
    let reuse = trusted.write_hevc_unspec62_nalu().unwrap();
    let mut history = MappingHistory::default();
    history.reset(10);
    assert!(matches!(
        history.resolve(10, &reuse),
        Err(Error::MissingHistory)
    ));
    let original = history.resolve(10, &full).unwrap();
    let disabled = history.resolve(10, &reuse).unwrap();
    assert!(disabled.disable_residual());
    assert_eq!(disabled.curves()[0].pivots(), original.curves()[0].pivots());
    trusted.header.disable_residual_flag = false;
    let enabled = trusted.write_hevc_unspec62_nalu().unwrap();
    assert!(!history.resolve(10, &enabled).unwrap().disable_residual());
    trusted.header.prev_vdr_rpu_id = 1;
    assert!(matches!(
        history.resolve(10, &trusted.write_hevc_unspec62_nalu().unwrap()),
        Err(Error::Unsupported)
    ));
    assert!(matches!(
        history.resolve(10, &enabled),
        Err(Error::MissingHistory)
    ));
    history.resolve(10, &full).unwrap();
    assert!(matches!(history.resolve(10, &[0; 4]), Err(Error::Rpu)));
    assert!(matches!(
        history.resolve(10, &enabled),
        Err(Error::MissingHistory)
    ));
}
#[test]
fn mmr_inputs_clamp_to_each_components_own_pivots() {
    let y = Curve::new(
        vec![100, 900],
        vec![Piece::Polynomial {
            order: 1,
            coefficients: [0, 1 << 23, 0],
        }],
    )
    .unwrap();
    let mut coeff = [[0; 7]; 3];
    coeff[0][0] = 1 << 23;
    let u = Curve::new(
        vec![0, 1023],
        vec![Piece::Mmr {
            order: 1,
            constant: 0,
            coefficients: coeff,
        }],
    )
    .unwrap();
    let p = FelFrameParams::new(
        [y, u, identity()],
        [Nlq::new(0, 0, 0, 0, 23).unwrap(); 3],
        23,
        12,
        true,
    )
    .unwrap();
    for (input, expected) in [([0, 512, 512], 100 * 64), ([1023, 512, 512], 900 * 64)] {
        assert_eq!(
            p.prediction(1, 512, input, ReconstructionMode::Reference16)
                .unwrap(),
            expected
        );
    }
}
#[test]
fn half_up_quantizer_is_separate_from_upstream_ties_even() {
    let c = Curve::new(
        vec![0, 1023],
        vec![Piece::Polynomial {
            order: 1,
            coefficients: [1024, 0, 0],
        }],
    )
    .unwrap();
    let p = FelFrameParams::new(
        [c, identity(), identity()],
        [Nlq::new(0, 0, 0, 0, 23).unwrap(); 3],
        23,
        12,
        true,
    )
    .unwrap();
    assert_eq!(
        p.prediction(0, 0, [0; 3], ReconstructionMode::UpstreamVdrF64)
            .unwrap(),
        0
    );
    assert_eq!(
        quantize_reference16(
            p.prediction(0, 0, [0; 3], ReconstructionMode::Reference16)
                .unwrap(),
            12
        )
        .unwrap(),
        1
    );
    assert_eq!(quantize_reference16(65535, 12).unwrap(), 4095);
    assert_eq!(quantize_reference16(65535, 16).unwrap(), 65535);
    assert!(quantize_reference16(0, 9).is_err());
}
#[test]
fn clamp_prediction_before_residual_and_final_signal_after_residual() {
    let c = Curve::new(
        vec![0, 1023],
        vec![Piece::Polynomial {
            order: 1,
            coefficients: [-(1 << 23), 0, 0],
        }],
    )
    .unwrap();
    let p = FelFrameParams::new(
        [c, identity(), identity()],
        [Nlq::new(512, 2048, 0, 1 << 20, 23).unwrap(); 3],
        23,
        12,
        false,
    )
    .unwrap();
    let y = [512];
    let uv = [512];
    let ey = [513];
    let bl = Frame420::new(
        Plane::new(&y, 1, 1, 1).unwrap(),
        Plane::new(&uv, 1, 1, 1).unwrap(),
        Plane::new(&uv, 1, 1, 1).unwrap(),
    )
    .unwrap();
    let el = Frame420::new(
        Plane::new(&ey, 1, 1, 1).unwrap(),
        Plane::new(&uv, 1, 1, 1).unwrap(),
        Plane::new(&uv, 1, 1, 1).unwrap(),
    )
    .unwrap();
    let mut out = [0];
    let mut u = [0];
    let mut v = [0];
    compose(
        &p,
        bl,
        el,
        Output420 {
            y: &mut out,
            cb: &mut u,
            cr: &mut v,
        },
        ReconstructionMode::Reference16,
        SpatialPolicy::EngineBilinearLumaAnnexBChromaClip10,
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(out, [8]);
}
#[test]
fn gpu_tables_keep_cross_component_mmr_and_exact_polynomial_nlq() {
    let mut coeff = [[0; 7]; 3];
    coeff[0][0] = 1 << 23;
    coeff[2][6] = -(1 << 22);
    let mmr = Curve::new(
        vec![0, 512, 1023],
        vec![
            Piece::Polynomial {
                order: 1,
                coefficients: [0, 1 << 23, 0],
            },
            Piece::Mmr {
                order: 3,
                constant: 1234,
                coefficients: coeff,
            },
        ],
    )
    .unwrap();
    let p = FelFrameParams::new(
        [identity(), mmr, identity()],
        [Nlq::new(512, 2048, 0, 1 << 20, 23).unwrap(); 3],
        23,
        12,
        false,
    )
    .unwrap();
    let tables = GpuFrameTables::from_params(&p).unwrap();
    let w = tables.words();
    assert_eq!(w.len(), 6832);
    assert!(tables.requires_mmr());
    assert_eq!(w[6], 2);
    assert_eq!(w[96], 3);
    assert_eq!(&w[98..101], &[0, 512, 1023]);
    assert_eq!(w[96 + 11], 0);
    assert_eq!(w[96 + 12], 1);
    assert_eq!(w[96 + 20], 3);
    let co = 160 + (8 + 1) * 22;
    assert_eq!(w[co], 1234);
    assert_eq!(w[co + 1], 1 << 23);
    assert_eq!(w[co + 21], -(1 << 22));
    for code in 0..1024 {
        assert_eq!(w[688 + code], (code * 64) as i32);
        assert_eq!(
            w[3760 + code],
            Nlq::new(512, 2048, 0, 1 << 20, 23)
                .unwrap()
                .residual_reference16(code as u16)
                .unwrap()
        );
        assert_eq!(
            w[688 + 1024 + code],
            if code < 512 { (code * 64) as i32 } else { 0 }
        );
    }
    assert_ne!(
        p.prediction(1, 600, [100, 600, 512], ReconstructionMode::Reference16)
            .unwrap(),
        p.prediction(1, 600, [900, 600, 512], ReconstructionMode::Reference16)
            .unwrap()
    );
}
#[test]
fn p7_syntax_mapping_only_exception_and_p8_without_el_filter() {
    let data = include_bytes!("data/fel-template.bin");
    let full = [&[0x7c, 1][..], &data[4..]].concat();
    let seed = aether_dovi::semantic::parse_rpu_checked(&full).unwrap();
    let mut only = seed.clone();
    only.header.disable_residual_flag = true;
    only.dovi_profile = 8;
    only.el_type = None;
    let m = only.rpu_data_mapping.as_mut().unwrap();
    m.nlq = None;
    m.nlq_method_idc = None;
    m.nlq_num_pivots_minus2 = None;
    m.nlq_pred_pivot_value = None;
    only.modified = true;
    let nal = only.write_hevc_unspec62_nalu().unwrap();
    let parsed = aether_dovi::semantic::parse_rpu_checked(&nal).unwrap();
    assert_eq!(parsed.dovi_profile, 8);
    assert!(parsed.header.el_spatial_resampling_filter_flag);
    let mut h = MappingHistory::default();
    h.reset(0);
    let params = h.resolve(0, &nal).unwrap();
    assert!(params.disable_residual());
    let tables = GpuFrameTables::from_params(&params).unwrap();
    assert_eq!(tables.words()[5], 1);
    assert!(tables.words()[3760..].iter().all(|&v| v == 0));
    // P8.x has no EL, so the EL resampling flag is not a gate (real P8.1 streams clear it).
    only.header.el_spatial_resampling_filter_flag = false;
    assert!(h.resolve(0, &only.write_hevc_unspec62_nalu().unwrap()).is_ok());
    // A residual-enabled P8 header is still rejected.
    only.header.disable_residual_flag = false;
    if let Ok(nal) = only.write_hevc_unspec62_nalu() {
        assert!(h.resolve(0, &nal).is_err());
    }
    let mut mel = seed;
    mel.convert_with_mode(dolby_vision::rpu::ConversionMode::ToMel)
        .unwrap();
    let nal = mel.write_hevc_unspec62_nalu().unwrap();
    assert_eq!(
        aether_dovi::semantic::parse_rpu_checked(&nal)
            .unwrap()
            .el_type,
        Some(dolby_vision::rpu::rpu_data_nlq::DoviELType::MEL)
    );
    // The ToMel conversion zeroes the NLQ, so MEL is accepted as mapping only.
    let params = h.resolve(0, &nal).unwrap();
    assert!(params.disable_residual());
    assert_eq!(GpuFrameTables::from_params(&params).unwrap().words()[5], 1);
}
#[test]
fn gpu_signed_addition_gate_preserves_unsigned16_prediction_range() {
    let zero = Curve::new(
        vec![0, 1023],
        vec![Piece::Polynomial {
            order: 1,
            coefficients: [0; 3],
        }],
    )
    .unwrap();
    let make = |maximum| {
        FelFrameParams::new(
            [zero.clone(), zero.clone(), zero.clone()],
            [Nlq::new(0, i32::MAX, 0, maximum, 15).unwrap(); 3],
            15,
            12,
            false,
        )
        .unwrap()
    };
    // Residual alone fits i32, but adding a 65535 prediction would overflow it.
    let rejected = make(1_073_741_823);
    assert_eq!(
        rejected
            .nlq_tables(ReconstructionMode::Reference16)
            .unwrap()[0][1],
        i32::MAX - 1
    );
    assert!(matches!(
        GpuFrameTables::from_params(&rejected),
        Err(Error::Unsupported)
    ));
    let allowed = make(1_073_709_056);
    let tables = GpuFrameTables::from_params(&allowed).unwrap();
    assert_eq!(tables.words()[3761], i32::MAX - 65535);
}
/// A stream the reconstruction is not exact for gets no tables at all, so the player shows the base
/// layer alone (dv-path.js: tables null, single layer) instead of a residual computed wrong: an
/// 8-bit enhancement layer, or coefficients with fewer than 15 fraction bits (the Reference16
/// kernel keeps 15).
#[test]
fn eight_bit_el_and_short_coefficient_fraction_fall_back_to_base_layer() {
    let data = include_bytes!("data/fel-template.bin");
    let full = [&[0x7c, 1][..], &data[4..]].concat();
    let seed = aether_dovi::semantic::parse_rpu_checked(&full).unwrap();
    let tables = |rpu: &dolby_vision::rpu::dovi_rpu::DoviRpu| {
        // libdovi's own validation refuses some of these before the kernel sees them: no tables either way
        let nal = rpu.write_hevc_unspec62_nalu().map_err(|_| Error::Rpu)?;
        let mut h = MappingHistory::default();
        h.reset(0);
        aether_dovi::fel::gpu_tables_for_rpu(&mut h, 0, &nal).map(|_| ())
    };
    assert!(tables(&seed).is_ok());
    let mut el8 = seed.clone();
    el8.header.el_bit_depth_minus8 = 0;
    el8.modified = true;
    assert!(tables(&el8).is_err());
    for denom in [12, 14] {
        let mut short = seed.clone();
        short.header.coefficient_log2_denom = denom;
        // the fractions were written for the template's denominator: keep them inside the new one
        let m = short.rpu_data_mapping.as_mut().unwrap();
        for curve in &mut m.curves {
            if let Some(p) = curve.polynomial.as_mut() {
                for row in &mut p.poly_coef { for v in row.iter_mut() { *v &= (1 << denom) - 1; } }
            }
        }
        short.modified = true;
        assert!(tables(&short).is_err(), "denominator {denom}");
    }
}
