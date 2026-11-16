//! Bit-exactness against FFmpeg on real Blu-ray style VC-1 Advanced Profile streams.
//!
//! The fixtures are extracted from media outside the repository by `tools/regen-fixtures.sh`
//! (elementary streams plus `ffmpeg -f framemd5` hashes).  Without fixtures the tests print a
//! notice and pass, so a plain `cargo test` works on a clean checkout.
mod common;
use aether_vc1::decoder::split_access_units;
use aether_vc1::Decoder;
use common::*;

fn hash_frame(f: &aether_vc1::Frame) -> String {
    let mut v = Vec::new();
    f.write_i420(&mut v);
    md5_hex(&v)
}

fn decode_hashes(es: &[u8], aus: &[(usize, usize)], max: usize) -> Vec<String> {
    let mut dec = Decoder::new();
    let mut out = Vec::new();
    for (i, (a, b)) in aus.iter().enumerate() {
        dec.decode(&es[*a..*b], i as i64).expect("decode");
        while let Some(f) = dec.take_frame() {
            out.push(hash_frame(&f));
        }
        if out.len() >= max {
            return out;
        }
    }
    dec.flush();
    while let Some(f) = dec.take_frame() {
        out.push(hash_frame(&f));
    }
    out
}

fn check_prefix(name: &str, frames: usize) {
    let Some((es, refs)) = load_fixture(name) else {
        eprintln!("SKIP {name}: fixtures missing (run tools/regen-fixtures.sh)");
        return;
    };
    let aus = split_access_units(&es);
    let got = decode_hashes(&es, &aus, frames);
    let n = frames.min(refs.len());
    assert!(got.len() >= n, "decoded only {} frames", got.len());
    for i in 0..n {
        assert_eq!(got[i], refs[i], "{name}: frame {i} differs from ffmpeg");
    }
}

fn check_mid_gop(name: &str) {
    let Some((es, refs)) = load_fixture(name) else {
        eprintln!("SKIP {name}: fixtures missing (run tools/regen-fixtures.sh)");
        return;
    };
    let aus = split_access_units(&es);
    // random access points: access units that start with a sequence header or entry point
    let rap: Vec<usize> = aus
        .iter()
        .enumerate()
        .filter(|(_, (a, _))| es[*a + 3] == 0x0e || es[*a + 3] == 0x0f)
        .map(|(i, _)| i)
        .collect();
    assert!(rap.len() > 4, "expected several entry points");
    let start = rap[rap.len() / 2];
    let mut dec = Decoder::new();
    let mut got = Vec::new();
    for (i, (a, b)) in aus.iter().enumerate().skip(start).take(60) {
        dec.decode(&es[*a..*b], i as i64).expect("decode");
        while let Some(f) = dec.take_frame() {
            got.push(hash_frame(&f));
        }
    }
    assert!(got.len() > 20);
    // The first output frame after the random access point is the anchor that opens the GOP; find
    // its position in the ffmpeg list and require the following frames to match exactly.
    let first = refs.iter().position(|r| *r == got[0]).expect("first frame of the GOP not found in reference");
    for (k, g) in got.iter().enumerate().take(40) {
        assert_eq!(*g, refs[first + k], "{name}: mid-file frame {k} (access unit {start}) differs");
    }
}

/// Matroska style: no start code in front of the frame data, headers only in CodecPrivate.
fn check_container_style(name: &str) {
    let Some((es, refs)) = load_fixture(name) else {
        eprintln!("SKIP {name}: fixtures missing (run tools/regen-fixtures.sh)");
        return;
    };
    let aus = split_access_units(&es);
    let first = &es[aus[0].0..aus[0].1];
    let fpos = first.windows(4).position(|w| w == [0, 0, 1, 0x0d]).unwrap();
    let mut dec = Decoder::new();
    dec.set_extradata(&first[..fpos]).expect("extradata");
    let info = dec.info().expect("stream info");
    assert_eq!((info.width, info.height), (1920, 1080));
    let mut got = Vec::new();
    for (i, (a, b)) in aus.iter().enumerate().take(120) {
        let au = &es[*a..*b];
        let p = au.windows(4).position(|w| w == [0, 0, 1, 0x0d]).unwrap() + 4;
        dec.decode(&au[p..], i as i64).expect("decode");
        while let Some(f) = dec.take_frame() {
            got.push(hash_frame(&f));
        }
    }
    for (i, g) in got.iter().enumerate() {
        assert_eq!(*g, refs[i], "{name}: container style frame {i}");
    }
    assert!(got.len() > 100);
}

#[test]
fn lpcm_first_300_frames_match_ffmpeg() {
    check_prefix("lpcm71", 300);
}
#[test]
fn dtshra_first_300_frames_match_ffmpeg() {
    check_prefix("dtshra51", 300);
}
#[test]
fn lpcm_mid_file_gop_matches_ffmpeg() {
    check_mid_gop("lpcm71");
}
#[test]
fn dtshra_mid_file_gop_matches_ffmpeg() {
    check_mid_gop("dtshra51");
}
#[test]
fn container_style_packets_match_ffmpeg() {
    check_container_style("lpcm71");
}
/// Whole files (slow in debug builds; run with `cargo test --release -- --ignored`).
#[test]
#[ignore]
fn full_files_match_ffmpeg() {
    for (name, n) in [("lpcm71", usize::MAX), ("dtshra51", usize::MAX)] {
        check_prefix(name, n);
    }
}
