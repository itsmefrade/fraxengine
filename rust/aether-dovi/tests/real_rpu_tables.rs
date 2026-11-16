//! GPU tables for real Dolby Vision RPUs (P5, P7 MEL, P8.1). The RPUs are extracted from local
//! media by `tests/data/regen-real-rpus.sh` into git-ignored `tests/data/real/`; a missing file
//! skips its test. Every RPU goes through `MappingHistory`, which runs the CRC and `rpu_guard`
//! checks before libdovi parses it.
use aether_dovi::fel::{GPU_TABLE_WORDS, MappingHistory, gpu_tables_for_rpu};
use aether_dovi::semantic::parse_rpu_checked;
use dolby_vision::rpu::rpu_data_mapping::DoviMappingMethod;
use std::path::PathBuf;

fn load(name: &str) -> Option<Vec<Vec<u8>>> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/data/real")
        .join(format!("{name}.rpu"));
    let Ok(data) = std::fs::read(&path) else {
        eprintln!("skipped: {} missing (run tests/data/regen-real-rpus.sh)", path.display());
        return None;
    };
    let starts: Vec<usize> = (0..data.len().saturating_sub(3))
        .filter(|&i| data[i..i + 4] == [0, 0, 0, 1])
        .collect();
    let nals: Vec<Vec<u8>> = starts
        .iter()
        .enumerate()
        .map(|(n, &s)| {
            let end = starts.get(n + 1).copied().unwrap_or(data.len());
            [&[0x7c, 1][..], &data[s + 4..end]].concat()
        })
        .collect();
    assert!(!nals.is_empty());
    Some(nals)
}

/// Independent model: prediction table from libdovi's own integer/fraction fields.
fn check_tables(nal: &[u8], expect_profile: u8) {
    let rpu = parse_rpu_checked(nal).unwrap();
    assert_eq!(rpu.dovi_profile, expect_profile);
    let mut history = MappingHistory::default();
    history.reset(1);
    let tables = gpu_tables_for_rpu(&mut history, 1, nal).unwrap();
    let w = tables.words();
    assert_eq!(w.len(), GPU_TABLE_WORDS);
    let denom = rpu.header.coefficient_log2_denom as u32;
    assert_eq!(w[3], denom as i32);
    assert_eq!(w[4], rpu.header.vdr_bit_depth_minus8 as i32 + 8);
    assert_eq!(w[5], 1, "mapping only: residual disabled");
    assert_eq!(w[6], 0, "polynomial fixtures need no MMR");
    assert!(w[3760..3760 + 3 * 1024].iter().all(|&v| v == 0), "no residual");
    let mapping = rpu.rpu_data_mapping.as_ref().unwrap();
    for (c, curve) in mapping.curves.iter().enumerate() {
        assert_eq!(curve.mapping_idc, DoviMappingMethod::Polynomial);
        let poly = curve.polynomial.as_ref().unwrap();
        let mut pivots = Vec::new();
        let mut acc = 0_i64;
        for p in &curve.pivots {
            acc += i64::from(*p);
            pivots.push(acc);
        }
        let d = 64 + c * 32;
        assert_eq!(w[d] as usize, pivots.len());
        for (i, p) in pivots.iter().enumerate() {
            assert_eq!(i64::from(w[d + 2 + i]), *p);
        }
        for code in 0..1024_i128 {
            let piece = (0..pivots.len() - 1)
                .rev()
                .find(|&i| code >= i128::from(pivots[i]))
                .unwrap_or(0);
            let order = poly.poly_order_minus1[piece] as usize + 1;
            let q = |k: usize| {
                i128::from(poly.poly_coef_int[piece][k]) * (1_i128 << denom)
                    + i128::from(poly.poly_coef[piece][k])
            };
            let x = code.clamp(pivots[0].into(), (*pivots.last().unwrap()).into());
            let mut sum = (q(0) << 20) + (q(1) * x << 10);
            if order == 2 {
                sum += q(2) * x * x;
            }
            let expect = (sum.max(0) >> (denom + 4)).min(65535) as i32;
            assert_eq!(w[688 + c * 1024 + code as usize], expect, "curve {c} code {code}");
        }
    }
}

#[test]
fn p5_rpus_produce_mapping_only_tables() {
    let Some(nals) = load("p5") else { return };
    for nal in &nals {
        let rpu = parse_rpu_checked(nal).unwrap();
        assert!(rpu.el_type.is_none() && !rpu.header.el_spatial_resampling_filter_flag);
        check_tables(nal, 5);
    }
}

#[test]
fn p7_mel_with_explicit_zero_nlq_is_mapping_only() {
    let Some(nals) = load("p7-mel") else { return };
    for nal in &nals {
        let rpu = parse_rpu_checked(nal).unwrap();
        assert!(!rpu.header.disable_residual_flag, "fixture carries explicit zero NLQ");
        check_tables(nal, 7);
    }
}

#[test]
fn p81_rpus_without_el_filter_flag_produce_mapping_only_tables() {
    let Some(nals) = load("p81") else { return };
    for nal in &nals {
        assert!(!parse_rpu_checked(nal).unwrap().header.el_spatial_resampling_filter_flag);
        check_tables(nal, 8);
    }
}

#[test]
fn corrupted_real_rpu_is_still_rejected_by_the_guard() {
    let Some(nals) = load("p5") else { return };
    let mut bad = nals[0].clone();
    bad[12] ^= 0x40;
    let mut history = MappingHistory::default();
    history.reset(1);
    assert!(gpu_tables_for_rpu(&mut history, 1, &bad).is_err());
}
