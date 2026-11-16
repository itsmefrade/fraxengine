//! Bit-closeness against FFmpeg's float decoder (`-drc_scale 0`).
//!
//! Fixtures are produced by `tests/gen_fixtures.sh` (needs FFmpeg) and are not
//! committed. Tests that find their fixture missing print a note and pass, so
//! a fresh checkout still runs; set `AETHER_AC3_REQUIRE_FIXTURES=1` to turn a
//! missing fixture into a failure.

use aether_ac3::{access_units, Decoder};
use std::path::PathBuf;

const MAX_ABS_ERR: f32 = 1e-4;

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

struct Report {
    channels: usize,
    layout: String,
    max_err: f32,
    samples: usize,
    frames: usize,
}

/// Decodes `<name>.<ext>` and compares against `<name>.ref.f32le`.
fn compare(name: &str, ext: &str) -> Option<Report> {
    let dir = fixtures_dir();
    let stream_path = dir.join(format!("{name}.{ext}"));
    let ref_path = dir.join(format!("{name}.ref.f32le"));
    if !stream_path.exists() || !ref_path.exists() {
        if std::env::var_os("AETHER_AC3_REQUIRE_FIXTURES").is_some() {
            panic!("missing fixture {name}; run tests/gen_fixtures.sh");
        }
        eprintln!("skipping {name}: fixture missing (run tests/gen_fixtures.sh)");
        return None;
    }
    let stream = std::fs::read(&stream_path).unwrap();
    let reference: Vec<f32> = std::fs::read(&ref_path)
        .unwrap()
        .chunks_exact(4)
        .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .collect();

    let mut dec = Decoder::new();
    dec.set_ffmpeg_bug_compat(true);
    let mut produced: Vec<f32> = Vec::with_capacity(reference.len());
    let mut channels = 0;
    let mut layout = String::new();
    let mut frames = 0;
    for au in access_units(&stream) {
        let f = dec
            .decode(au)
            .unwrap_or_else(|e| panic!("{name}: decode failed: {e}"));
        if f.frames == 0 {
            continue;
        }
        assert!(f.crc_ok, "{name}: CRC failure in frame {frames}");
        if channels == 0 {
            channels = f.channels();
            layout = f.layout.describe();
        }
        assert_eq!(f.channels(), channels, "{name}: channel count changed");
        let start = produced.len();
        produced.resize(start + f.frames * channels, 0.0);
        f.interleave_into(&mut produced[start..]);
        frames += 1;
    }
    assert_eq!(
        produced.len(),
        reference.len(),
        "{name}: sample count differs (got {}, FFmpeg {})",
        produced.len(),
        reference.len()
    );
    let mut max_err = 0f32;
    let mut worst = 0usize;
    for (i, (a, b)) in produced.iter().zip(reference.iter()).enumerate() {
        let e = (a - b).abs();
        if e > max_err || e.is_nan() {
            max_err = e;
            worst = i;
        }
    }
    eprintln!(
        "{name}: {frames} frames, {channels} ch ({layout}), {} samples, max abs err {max_err:e} (at sample {} ch {})",
        produced.len(),
        worst / channels,
        worst % channels
    );
    assert!(
        max_err <= MAX_ABS_ERR,
        "{name}: max abs error {max_err:e} exceeds {MAX_ABS_ERR:e}"
    );
    eprintln!("  tools: {:?}", dec.stats());
    Some(Report {
        channels,
        layout,
        max_err,
        samples: produced.len(),
        frames,
    })
}

macro_rules! fixture_test {
    ($fn_name:ident, $name:expr, $ext:expr) => {
        #[test]
        fn $fn_name() {
            let _ = compare($name, $ext).map(|r| {
                assert!(r.frames > 0 && r.samples > 0 && r.channels > 0 && !r.layout.is_empty());
                assert!(r.max_err.is_finite());
            });
        }
    };
}

fixture_test!(ac3_mono, "ac3-mono", "ac3");
fixture_test!(ac3_stereo, "ac3-stereo", "ac3");
fixture_test!(ac3_stereo_lowrate, "ac3-stereo-lowrate", "ac3");
fixture_test!(ac3_51, "ac3-51", "ac3");
fixture_test!(ac3_51_coupled, "ac3-51-coupled", "ac3");
fixture_test!(eac3_stereo, "eac3-stereo", "eac3");
fixture_test!(eac3_51, "eac3-51", "eac3");
fixture_test!(eac3_51_coupled, "eac3-51-coupled", "eac3");
fixture_test!(film_51_ac3, "film-51", "ac3");
fixture_test!(joc_51_eac3, "joc-51", "eac3");
fixture_test!(joc_signalling_eac3, "joc-signalling", "eac3");
fixture_test!(real_71_dd_plus, "real-71-dd-plus", "eac3");
fixture_test!(real_71_channel_check, "real-71-channel-check", "eac3");
fixture_test!(real_joc_atmos_mkv, "real-joc-atmos-mkv", "eac3");
fixture_test!(real_joc_hdr10plus, "real-joc-hdr10plus", "eac3");
fixture_test!(real_joc_71_amaze, "real-joc-71-amaze", "eac3");
fixture_test!(real_ac3_amaze, "real-ac3-amaze", "ac3");
fixture_test!(real_ac3_canyon, "real-ac3-canyon", "ac3");
fixture_test!(spx_synth_mono, "spx-synth-mono", "eac3");
fixture_test!(spx_synth_stereo, "spx-synth-stereo", "eac3");

fn read_stream(name: &str, ext: &str) -> Option<Vec<u8>> {
    let p = fixtures_dir().join(format!("{name}.{ext}"));
    if !p.exists() {
        eprintln!("skipping {name}: fixture missing (run tests/gen_fixtures.sh)");
        return None;
    }
    Some(std::fs::read(p).unwrap())
}

/// Without the FFmpeg-compat switch the output differs from FFmpeg only in the
/// single block where FFmpeg's mistaken "upmix delay" step corrupts the
/// overlap buffers.
#[test]
fn default_mode_differs_from_ffmpeg_only_at_its_delay_glitch() {
    let Some(stream) = read_stream("real-ac3-canyon", "ac3") else {
        return;
    };
    let reference: Vec<f32> = std::fs::read(fixtures_dir().join("real-ac3-canyon.ref.f32le"))
        .unwrap()
        .chunks_exact(4)
        .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .collect();
    let mut dec = Decoder::new();
    let mut out = Vec::new();
    for au in access_units(&stream) {
        let f = dec.decode(au).unwrap();
        let start = out.len();
        out.resize(start + f.frames * f.channels(), 0.0);
        f.interleave_into(&mut out[start..]);
    }
    assert_eq!(out.len(), reference.len());
    let bad = out
        .iter()
        .zip(&reference)
        .filter(|(a, b)| (**a - **b).abs() > MAX_ABS_ERR)
        .count();
    assert!(
        bad > 0,
        "the glitch FFmpeg shows on this fixture should be visible"
    );
    assert!(
        bad <= 256 * 6,
        "{bad} samples differ, more than one block of the glitching channels"
    );
}

#[test]
fn joc_signalling_is_detected_on_atmos_streams_only() {
    if let Some(stream) = read_stream("joc-51", "eac3") {
        let mut dec = Decoder::new();
        let f = dec.decode(access_units(&stream)[0]).unwrap();
        assert!(f.is_joc);
        assert!(f.joc.extension_type_a);
        assert!(f.joc.emdf_joc_payload, "EMDF JOC payload not found");
        assert!(f.joc.emdf_oamd_payload);
        assert_eq!(f.joc.complexity_index, Some(16));
        assert_eq!(f.channels(), 6);
    }
    if let Some(stream) = read_stream("eac3-51", "eac3") {
        let mut dec = Decoder::new();
        let f = dec.decode(access_units(&stream)[0]).unwrap();
        assert!(!f.is_joc);
        assert_eq!(f.joc, aether_ac3::JocInfo::default());
    }
}

#[test]
fn dependent_substream_gives_seven_one() {
    let Some(stream) = read_stream("real-71-channel-check", "eac3") else {
        return;
    };
    let mut dec = Decoder::new();
    let f = dec.decode(access_units(&stream)[0]).unwrap();
    assert_eq!(f.layout.describe(), "L R C LFE Lb Rb Ls Rs");
    assert_eq!(f.dependent_frames, 1);
    assert_eq!(f.frames, 1536);
    assert_eq!(f.planar.len(), 8 * 1536);
}

#[test]
fn crc_is_checked() {
    let Some(stream) = read_stream("ac3-51", "ac3") else {
        return;
    };
    let au = access_units(&stream)[3].to_vec();
    let mut dec = Decoder::new();
    assert!(dec.decode(&au).unwrap().crc_ok);

    let mut broken = au.clone();
    let n = broken.len();
    broken[n - 6] ^= 0x10; // tail padding: decodes fine, CRC no longer matches
    assert!(!dec.decode(&broken).unwrap().crc_ok);

    let mut strict = Decoder::new();
    strict.set_strict_crc(true);
    assert_eq!(strict.decode(&broken).unwrap_err(), aether_ac3::Error::Crc);
    assert!(strict.decode(&au).is_ok());
}

#[test]
fn bad_input_is_an_error_not_a_panic() {
    let mut dec = Decoder::new();
    assert_eq!(dec.decode(&[]).unwrap_err(), aether_ac3::Error::Sync);
    assert_eq!(dec.decode(&[0u8; 64]).unwrap_err(), aether_ac3::Error::Sync);
    let Some(stream) = read_stream("eac3-51", "eac3") else {
        return;
    };
    let au = access_units(&stream)[0];
    assert_eq!(
        dec.decode(&au[..au.len() / 2]).unwrap_err(),
        aether_ac3::Error::Incomplete
    );
    // Random corruption must never panic.
    let mut x = 0x1234_5678u32;
    for _ in 0..200 {
        let mut bad = au.to_vec();
        for _ in 0..8 {
            x = x.wrapping_mul(1664525).wrapping_add(1013904223);
            let i = 4 + (x >> 8) as usize % (bad.len() - 4);
            bad[i] ^= (x >> 3) as u8;
        }
        let _ = dec.decode(&bad);
    }
}
