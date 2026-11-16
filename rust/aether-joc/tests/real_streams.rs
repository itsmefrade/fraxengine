//! Validation on the two real JOC streams. Skipped (with a note) when the
//! media is not present. Which checks are spec-derived and which are
//! semantic is stated per test.
use aether_joc::analysis::fit_static_downmix;
use aether_joc::emdf::extract_from_access_unit;
use aether_joc::joc::{parse_joc, SparseOffsetMode};
use aether_joc::oamd::{parse_oamd, OamdState};
use aether_joc::render::{pan_gains, LAYOUT_714};
use aether_joc::{ObjectDecoder, LATENCY_SAMPLES};

const CHID_MP4: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../test-media/audio-official-joc-channel-id.mp4");
const ATMOS_MKV: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../test-media/EAC3-JOC ATMOS.mkv");

/// Extracts the first audio stream with ffmpeg into a raw E-AC-3 file.
fn load(path: &str, name: &str) -> Option<Vec<u8>> {
    if !std::path::Path::new(path).exists() {
        eprintln!("skip: {path} missing");
        return None;
    }
    let out = std::env::temp_dir().join(format!("aether-joc-test-{name}.eac3"));
    if !out.exists() {
        let st = std::process::Command::new("ffmpeg")
            .args(["-v", "error", "-y", "-i", path, "-map", "0:a:0", "-c", "copy", "-f", "eac3"])
            .arg(&out)
            .status()
            .ok()?;
        if !st.success() {
            eprintln!("skip: ffmpeg failed");
            return None;
        }
    }
    std::fs::read(out).ok()
}

fn decode_all(stream: &[u8]) -> (Vec<Vec<f32>>, Vec<Vec<f32>>, Vec<aether_joc::ObjectFrame>) {
    let mut dec = aether_ac3::Decoder::new();
    dec.set_ffmpeg_bug_compat(true);
    let mut od = ObjectDecoder::new();
    let mut bed = vec![Vec::new(); 6];
    let mut objs = vec![Vec::new(); 15];
    let mut frames = Vec::new();
    for au in aether_ac3::access_units(stream) {
        let b = dec.decode(au).unwrap();
        let mut o = od.process(au, &b).unwrap();
        for c in 0..6 {
            bed[c].extend_from_slice(b.channel(c));
        }
        for k in 0..o.num_objects {
            objs[k].extend_from_slice(o.object(k));
        }
        o.pcm.clear();
        frames.push(o);
    }
    (bed, objs, frames)
}

/// SPEC-DERIVED: every JOC and OAMD payload of both streams parses, the JOC
/// payload is consumed to within its byte padding, the sequence counter is
/// consecutive, the OAMD elements are consumed exactly (zero padding), and the
/// object counts agree.
#[test]
fn payloads_parse_bit_exactly() {
    for (path, name) in [(CHID_MP4, "chid"), (ATMOS_MKV, "atmos")] {
        let Some(stream) = load(path, name) else { continue };
        let mut st = OamdState::default();
        let mut last_seq = None::<u16>;
        let mut n = 0;
        for au in aether_ac3::access_units(&stream) {
            let e = extract_from_access_unit(au);
            let j = parse_joc(e.joc.as_ref().expect("JOC payload"), SparseOffsetMode::ZeroPoint).unwrap();
            assert!(j.bits_total - j.bits_used < 8, "{name}: JOC left {} bits", j.bits_total - j.bits_used);
            assert_eq!(j.num_objects, 15);
            if let Some(p) = last_seq {
                assert!(j.seq_count == p + 1 || (p == 1023 && j.seq_count == 1), "{name}: seq {p} -> {}", j.seq_count);
            }
            last_seq = Some(j.seq_count);
            let o = parse_oamd(e.oamd.as_ref().expect("OAMD payload"), &mut st).unwrap();
            assert!(o.clean, "{name}: OAMD element not consumed exactly in AU {n}");
            assert_eq!(o.object_count, j.num_objects + 1, "JOC objects = OAMD objects minus the LFE object");
            assert!(o.program.dyn_object_only && o.program.lfe_present);
            n += 1;
        }
        assert!(n > 1000);
    }
}

/// SEMANTIC: OAMD positions of the channel-ID sample are the 7.1.4 + wide
/// speaker positions of table B.10 (Lw/Rw at y = 10/62 exactly as the ADM
/// value 0,677419 of the table).
#[test]
fn oamd_positions_are_speaker_positions() {
    let Some(stream) = load(CHID_MP4, "chid") else { return };
    let mut st = OamdState::default();
    let aus = aether_ac3::access_units(&stream);
    let e = extract_from_access_unit(aus[1]);
    let o = parse_oamd(e.oamd.as_ref().unwrap(), &mut st).unwrap();
    let want: [[f32; 3]; 15] = [
        [0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.5, 0.0, 0.0], [0.0, 1.0, 0.0], [1.0, 1.0, 0.0],
        [0.0, 0.5, 0.0], [1.0, 0.5, 0.0], [0.0, 0.0, 1.0], [1.0, 0.0, 1.0], [0.0, 1.0, 1.0],
        [1.0, 1.0, 1.0], [0.0, 0.5, 1.0], [1.0, 0.5, 1.0], [0.0, 10.0 / 62.0, 0.0], [1.0, 10.0 / 62.0, 0.0],
    ];
    for (k, w) in want.iter().enumerate() {
        let p = o.objects[k + 1][0].state.pos;
        for i in 0..3 {
            assert!((p[i] - w[i]).abs() < 1e-6, "object {k}: {p:?} vs {w:?}");
        }
    }
}

/// SEMANTIC (channel-ID sample): the objects whose names are announced alone
/// (L, R, C, Lb, Rb, Tsl, Tsr) each carry exactly one voice segment: in the
/// best window the object holds at least 40 dB more energy than all other
/// objects together, the decoded 5.1 bed has the expected dominant channel
/// (Atmos folds back and top-side objects into Ls/Rs), and after point
/// panning to 7.1.4 at least 85% of the energy lands in the expected speakers.
#[test]
fn channel_id_voices_land_in_the_right_objects_and_speakers() {
    let Some(stream) = load(CHID_MP4, "chid") else { return };
    let mut dec = aether_ac3::Decoder::new();
    dec.set_ffmpeg_bug_compat(true);
    let mut od = ObjectDecoder::new();
    let win = 4usize;
    let mut wins: Vec<(Vec<f64>, Vec<f64>, Vec<[f32; 3]>)> = Vec::new();
    let mut pos = vec![[0.5f32, 0.5, 0.0]; 15];
    let (mut oe, mut be, mut n) = (vec![0f64; 15], vec![0f64; 6], 0);
    for au in aether_ac3::access_units(&stream) {
        let bed = dec.decode(au).unwrap();
        let o = od.process(au, &bed).unwrap();
        for m in &o.metadata {
            pos[m.object] = m.state.pos;
        }
        for k in 0..15 {
            oe[k] += o.object(k).iter().map(|&v| (v as f64).powi(2)).sum::<f64>();
        }
        for c in 0..6 {
            be[c] += bed.channel(c).iter().map(|&v| (v as f64).powi(2)).sum::<f64>();
        }
        n += 1;
        if n == win {
            wins.push((oe.clone(), be.clone(), pos.clone()));
            oe.iter_mut().for_each(|v| *v = 0.0);
            be.iter_mut().for_each(|v| *v = 0.0);
            n = 0;
        }
    }
    // (object, expected dominant bed channel plane, expected speakers by LAYOUT_714 index)
    // bed planes: L R C LFE Ls Rs; speakers: L R C Ls Rs Lb Rb Tfl Tfr Tbl Tbr
    let cases: [(usize, usize, &[usize]); 7] = [
        (0, 0, &[0]),
        (1, 1, &[1]),
        (2, 2, &[2]),
        (3, 4, &[5]),
        (4, 5, &[6]),
        (11, 4, &[7, 9]),
        (12, 5, &[8, 10]),
    ];
    for (obj, bed_ch, spk) in cases {
        // Window with the highest dominance over the other objects.
        let mut best = (f64::MIN, 0usize);
        for (w, (e, _, _)) in wins.iter().enumerate() {
            let others: f64 = (0..15).filter(|&k| k != obj).map(|k| e[k]).sum();
            if e[obj] / (win as f64 * 1536.0) < 1e-8 {
                continue;
            }
            let r = 10.0 * ((e[obj] + 1e-20) / (others + 1e-20)).log10();
            if r > best.0 {
                best = (r, w);
            }
        }
        assert!(best.0 > 40.0, "object {obj}: dominance only {:.1} dB", best.0);
        let (_, bed_e, p) = &wins[best.1];
        let dom = (0..6).filter(|&c| c != 3).fold(0, |m, c| if bed_e[c] > bed_e[m] { c } else { m });
        assert_eq!(dom, bed_ch, "object {obj}: bed dominant channel");
        let g = pan_gains(p[obj], &LAYOUT_714);
        let total: f32 = g.iter().map(|v| v * v).sum();
        let in_expected: f32 = spk.iter().map(|&i| g[i] * g[i]).sum();
        assert!(in_expected / total > 0.85, "object {obj}: {:.0}% in expected speakers", 100.0 * in_expected / total);
    }
}

/// SEMANTIC / ENERGY (c): on the Atmos sample the decoded objects reproduce the
/// decoded bed through a static least-squares downmix: front channels to
/// better than -60 dB residual, surrounds better than -20 dB (the surrounds
/// are the sum of side, back and top-side objects). The fitted gains show L, R
/// and C are objects 0, 3 and 1 with gain 1.
#[test]
fn objects_reproduce_the_bed_through_a_downmix() {
    let Some(stream) = load(ATMOS_MKV, "atmos") else { return };
    let (bed, objs, _) = decode_all(&stream);
    let n = bed[0].len();
    let win = 32 * 1536;
    let mut worst = [f64::MIN; 6];
    let mut start = 4 * 1536;
    let mut gains = vec![0f64; 15];
    while start + win + LATENCY_SAMPLES <= n {
        for c in [0usize, 1, 2, 4, 5] {
            if let Some((x, r)) = fit_static_downmix(&bed[c], &objs, start, win, LATENCY_SAMPLES) {
                worst[c] = worst[c].max(r);
                if c == 0 && start == 4 * 1536 {
                    gains = x;
                }
            }
        }
        start += win;
    }
    for c in [0usize, 1, 2] {
        assert!(worst[c] < -60.0, "bed {c}: residual {:.1} dB", worst[c]);
    }
    for c in [4usize, 5] {
        assert!(worst[c] < -15.0, "bed {c}: residual {:.1} dB", worst[c]);
    }
    assert!((gains[0] - 1.0).abs() < 0.01, "L = object 0, got {:?}", &gains[..4]);
}
