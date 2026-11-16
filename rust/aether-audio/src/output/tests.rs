use super::hrtf::{self, direction};
use super::*;

const SQ: f32 = matrix::H;

struct Lcg(u64);
impl Lcg {
    fn next(&mut self) -> f32 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        ((self.0 >> 40) as f32 / (1u64 << 24) as f32) * 2.0 - 1.0
    }
}

fn noise(seed: u64, n: usize) -> Vec<f32> {
    let mut r = Lcg(seed);
    (0..n).map(|_| r.next()).collect()
}

const L714: [&str; 12] = ["FL", "FR", "FC", "LFE", "BL", "BR", "SL", "SR", "TFL", "TFR", "TBL", "TBR"];
const L71: [&str; 8] = ["FL", "FR", "FC", "LFE", "BL", "BR", "SL", "SR"];
const L51: [&str; 6] = ["FL", "FR", "FC", "LFE", "BL", "BR"];

fn coef(plan: &OutputPlan, out: &str, input: &str) -> f32 {
    let o = plan.out_layout.iter().position(|l| *l == out).unwrap();
    let i = plan.source.iter().position(|l| *l == input).unwrap();
    plan.matrix[o].iter().find(|&&(x, _)| x == i).map_or(0.0, |&(_, g)| g)
}

fn near(a: f32, b: f32) -> bool {
    (a - b).abs() < 1e-6
}

#[test]
fn identity_is_bit_exact_when_the_device_carries_the_source() {
    for (src, dev) in [(&L51[..], 6), (&L51[..], 8), (&L71[..], 8), (&L714[..], 12), (&L714[..], 16)] {
        let plan = OutputPlan::new(src, dev, Mode::Speakers).unwrap();
        assert_eq!(plan.kind, PlanKind::Identity, "{src:?} on {dev}");
        assert_eq!(plan.out_layout, plan.source);
        let mut out = plan.build().unwrap();
        // Includes values a float path might disturb: negative zero, denormals, 24 bit integers / 2^23.
        let mut input: Vec<Vec<f32>> = (0..src.len()).map(|c| noise(c as u64 + 1, 1024)).collect();
        input[0][0] = -0.0;
        input[0][1] = f32::MIN_POSITIVE / 4.0;
        input[1][0] = 8_388_607.0 / 8_388_608.0;
        input[1][1] = -8_388_608.0 / 8_388_608.0;
        input[2][0] = 1.5;
        let res = out.process(&input).unwrap();
        for (a, b) in input.iter().zip(&res) {
            assert!(a.iter().zip(b).all(|(x, y)| x.to_bits() == y.to_bits()));
        }
    }
}

#[test]
fn lossless_path_keeps_24_bit_samples_through_single_coefficient_rows() {
    // 7.1.4 -> 7.1 keeps FC, LFE, SL, SR bit-exact (single 1.0 coefficient rows are copies).
    let plan = OutputPlan::new(&L714, 8, Mode::Speakers).unwrap();
    assert_eq!(plan.kind, PlanKind::Mix);
    let mut out = plan.build().unwrap();
    let ints: Vec<i32> = (0..512).map(|i| ((i * 16_001) % 16_777_216) - 8_388_608).collect();
    let plane: Vec<f32> = ints.iter().map(|&v| v as f32 / 8_388_608.0).collect();
    let input: Vec<Vec<f32>> = (0..12).map(|_| plane.clone()).collect();
    let res = out.process(&input).unwrap();
    for label in ["FC", "LFE", "SL", "SR"] {
        let o = plan.out_layout.iter().position(|l| *l == label).unwrap();
        assert!(res[o].iter().zip(&plane).all(|(a, b)| a.to_bits() == b.to_bits()), "{label}");
    }
}

#[test]
fn heights_need_a_device_that_can_carry_them() {
    let l512 = ["FL", "FR", "FC", "LFE", "SL", "SR", "TFL", "TFR"];
    let on8 = OutputPlan::new(&l512, 8, Mode::Speakers).unwrap();
    assert_eq!(on8.kind, PlanKind::Mix);
    assert_eq!(on8.out_layout, ["FL", "FR", "FC", "LFE", "SL", "SR"]);
    let on10 = OutputPlan::new(&l512, 10, Mode::Speakers).unwrap();
    assert_eq!(on10.kind, PlanKind::Identity);
}

#[test]
fn mono_is_duplicated_not_attenuated() {
    let plan = OutputPlan::new(&["FC"], 2, Mode::Speakers).unwrap();
    assert_eq!(plan.out_layout, ["FL", "FR"]);
    assert_eq!(plan.matrix_dense(), [1.0, 1.0]);
    let mono = OutputPlan::new(&["FL", "FR"], 1, Mode::Speakers).unwrap();
    assert_eq!(mono.out_layout, ["FC"]);
    assert!(near(coef(&mono, "FC", "FL"), SQ) && near(coef(&mono, "FC", "FR"), SQ));
}

#[test]
fn five_one_to_stereo_is_bs775() {
    let plan = OutputPlan::new(&L51, 2, Mode::Speakers).unwrap();
    assert_eq!(plan.out_layout, ["FL", "FR"]);
    // Lo = L + 0.7071 C + 0.7071 Ls ; LFE dropped.
    assert!(near(coef(&plan, "FL", "FL"), 1.0));
    assert!(near(coef(&plan, "FL", "FC"), SQ));
    assert!(near(coef(&plan, "FL", "BL"), SQ));
    assert_eq!(coef(&plan, "FL", "LFE"), 0.0);
    assert_eq!(coef(&plan, "FL", "FR"), 0.0);
    assert_eq!(coef(&plan, "FL", "BR"), 0.0);
    assert!(near(coef(&plan, "FR", "BR"), SQ));
    assert!(plan.description.contains("LFE dropped"), "{}", plan.description);
    // Worst case 1 + 0.7071 + 0.7071 = 2.414 (+7.7 dB), and the description says so.
    assert!(near(plan.peak_gain(), 1.0 + 2.0 * SQ));
    assert!(plan.description.contains("limiter off"));
}

#[test]
fn lfe_can_be_mixed_into_stereo_on_request() {
    let opt = Options { lfe_gain: Some(0.5), ..Options::default() };
    let plan = OutputPlan::with_options(&L51, 2, Mode::Speakers, opt).unwrap();
    assert!(near(coef(&plan, "FL", "LFE"), 0.5) && near(coef(&plan, "FR", "LFE"), 0.5));
}

#[test]
fn seven_one_to_five_one_merges_side_and_rear_at_minus_3_db() {
    let plan = OutputPlan::new(&L71, 6, Mode::Speakers).unwrap();
    assert_eq!(plan.out_layout, ["FL", "FR", "FC", "LFE", "SL", "SR"]);
    assert!(near(coef(&plan, "SL", "SL"), SQ) && near(coef(&plan, "SL", "BL"), SQ));
    assert!(near(coef(&plan, "SR", "SR"), SQ) && near(coef(&plan, "SR", "BR"), SQ));
    assert!(near(coef(&plan, "FL", "FL"), 1.0) && near(coef(&plan, "FC", "FC"), 1.0) && near(coef(&plan, "LFE", "LFE"), 1.0));
    assert_eq!(coef(&plan, "SL", "SR"), 0.0);
}

#[test]
fn seven_one_four_to_seven_one_folds_heights_into_the_floor_layer() {
    let plan = OutputPlan::new(&L714, 8, Mode::Speakers).unwrap();
    assert_eq!(plan.out_layout, ["FL", "FR", "FC", "LFE", "SL", "SR", "BL", "BR"]);
    assert!(near(coef(&plan, "FL", "FL"), 1.0) && near(coef(&plan, "FL", "TFL"), SQ));
    assert!(near(coef(&plan, "FR", "TFR"), SQ));
    assert!(near(coef(&plan, "BL", "BL"), 1.0) && near(coef(&plan, "BL", "TBL"), SQ));
    assert!(near(coef(&plan, "BR", "TBR"), SQ));
    assert_eq!(coef(&plan, "SL", "TBL"), 0.0);
    assert_eq!(coef(&plan, "FL", "TFR"), 0.0);
    assert!(near(coef(&plan, "SL", "SL"), 1.0), "a full 7.1 target keeps sides at unity");
}

#[test]
fn seven_one_four_to_seven_one_two_folds_the_rear_heights_forward() {
    let plan = OutputPlan::new(&L714, 10, Mode::Speakers).unwrap();
    assert_eq!(plan.out_layout, ["FL", "FR", "FC", "LFE", "SL", "SR", "BL", "BR", "TFL", "TFR"]);
    assert!(near(coef(&plan, "TFL", "TFL"), 1.0) && near(coef(&plan, "TFL", "TBL"), SQ));
    assert!(near(coef(&plan, "TFR", "TBR"), SQ) && near(coef(&plan, "TFL", "TBR"), 0.0));
}

#[test]
fn seven_one_four_to_five_one_composes_the_stages() {
    let plan = OutputPlan::new(&L714, 6, Mode::Speakers).unwrap();
    assert_eq!(plan.out_layout, ["FL", "FR", "FC", "LFE", "SL", "SR"]);
    // 7.1.4 -> 7.1 -> 5.1: TBL goes to BL at 0.7071 and from there to Ls at 0.7071.
    assert!(near(coef(&plan, "SL", "TBL"), 0.5));
    assert!(near(coef(&plan, "SL", "SL"), SQ) && near(coef(&plan, "SL", "BL"), SQ));
    assert!(near(coef(&plan, "FL", "TFL"), SQ));
}

#[test]
fn seven_one_four_to_stereo_composes_the_stages() {
    let plan = OutputPlan::new(&L714, 2, Mode::Speakers).unwrap();
    let want = [("FL", 1.0), ("FC", SQ), ("SL", SQ), ("BL", SQ), ("TFL", SQ), ("TBL", 0.5), ("FR", 0.0), ("TFR", 0.0), ("LFE", 0.0)];
    for (input, g) in want {
        assert!(near(coef(&plan, "FL", input), g), "FL <- {input}: {} vs {g}", coef(&plan, "FL", input));
    }
    assert!(near(coef(&plan, "FR", "TBR"), 0.5));
}

#[test]
fn seven_one_four_to_five_one_four_merges_the_surround_pairs() {
    let plan = OutputPlan::with_target(&L714, &matrix::T514, Options::default(), 0.0).unwrap();
    assert!(near(coef(&plan, "SL", "SL"), SQ) && near(coef(&plan, "SL", "BL"), SQ));
    assert!(near(coef(&plan, "TBL", "TBL"), 1.0) && near(coef(&plan, "TFL", "TFL"), 1.0));
}

#[test]
fn lone_surround_pair_relabels_at_unity_and_target_selection_never_upmixes() {
    let plan = OutputPlan::with_target(&L51, &matrix::T71, Options::default(), 0.0).unwrap();
    assert!(near(coef(&plan, "BL", "BL"), 1.0));
    assert_eq!(coef(&plan, "SL", "BL"), 0.0, "an upmix leaves the missing pair silent");
    let side = OutputPlan::with_target(&["FL", "FR", "FC", "LFE", "SL", "SR"], &["FL", "FR", "FC", "LFE", "BL", "BR"], Options::default(), 0.0).unwrap();
    assert!(near(coef(&side, "BL", "SL"), 1.0));
    // A 5.1 source on an 8 channel device is identity (no upmix needed), on a stereo one it folds.
    assert_eq!(OutputPlan::new(&L51, 8, Mode::Speakers).unwrap().kind, PlanKind::Identity);
}

#[test]
fn target_selection_follows_the_device() {
    let cases: [(&[&str], usize, usize); 9] = [
        (&L714, 12, 12),
        (&L714, 10, 10),
        (&L714, 8, 8),
        (&L714, 7, 6),
        (&L714, 6, 6),
        (&L714, 5, 2),
        (&L714, 2, 2),
        (&L714, 1, 1),
        (&L71, 6, 6),
    ];
    for (src, dev, want) in cases {
        let plan = OutputPlan::new(src, dev, Mode::Speakers).unwrap();
        assert_eq!(plan.out_layout.len(), want, "{} on {dev}: {}", layout_name(src), plan.description);
    }
    // 9.1.6 source on a 12 channel device renders 7.1.4.
    let l916: Vec<&str> = L714.iter().copied().chain(["WL", "WR", "TSL", "TSR"]).collect();
    let plan = OutputPlan::new(&l916, 12, Mode::Speakers).unwrap();
    assert_eq!(plan.out_layout.len(), 12);
    assert!(near(coef(&plan, "TFL", "TSL"), SQ) && near(coef(&plan, "TBL", "TSL"), SQ));
    assert!(near(coef(&plan, "FL", "WL"), SQ) && near(coef(&plan, "SL", "WL"), SQ));
}

#[test]
fn aliases_and_bad_input() {
    let plan = OutputPlan::new(&["L", "R", "C", "LFE", "Ls", "Rs"], 2, Mode::Speakers).unwrap();
    assert_eq!(plan.source, ["FL", "FR", "FC", "LFE", "SL", "SR"]);
    assert_eq!(OutputPlan::new::<&str>(&[], 2, Mode::Speakers).unwrap_err(), OutputError::EmptySource);
    assert!(matches!(OutputPlan::new(&["FL", "XX"], 2, Mode::Speakers), Err(OutputError::UnknownLabel(_))));
    assert!(matches!(OutputPlan::new(&["FL", "L"], 2, Mode::Speakers), Err(OutputError::DuplicateLabel(_))));
    assert_eq!(OutputPlan::new(&["FL"], 0, Mode::Speakers).unwrap_err(), OutputError::BadDeviceChannels);
    let bad = Options { sample_rate: 100, ..Options::default() };
    assert!(matches!(OutputPlan::with_options(&["FL"], 2, Mode::Speakers, bad), Err(OutputError::BadSampleRate(100))));
}

#[test]
fn mix_matches_the_plan_matrix_on_signals() {
    let plan = OutputPlan::new(&L714, 2, Mode::Speakers).unwrap();
    let mut out = plan.build().unwrap();
    let input: Vec<Vec<f32>> = (0..12).map(|c| noise(40 + c as u64, 777)).collect();
    let res = out.process(&input).unwrap();
    let m = plan.matrix_dense();
    for o in 0..2 {
        for i in 0..777 {
            let want: f64 = (0..12).map(|c| f64::from(m[o * 12 + c]) * f64::from(input[c][i])).sum();
            assert!((f64::from(res[o][i]) - want).abs() < 1e-5);
        }
    }
}

#[test]
fn limiter_is_off_by_default_and_transparent_below_threshold() {
    let plan = OutputPlan::new(&L51, 2, Mode::Speakers).unwrap();
    assert!(!plan.options.limiter);
    let mut loud = plan.build().unwrap();
    let input: Vec<Vec<f32>> = (0..6).map(|_| vec![1.0; 256]).collect();
    let res = loud.process(&input).unwrap();
    assert!(res[0][0] > 2.0, "no implicit limiting: {}", res[0][0]);
    assert_eq!(loud.limiter_stats(), (0, 0.0));

    let opt = Options { limiter: true, ..Options::default() };
    let plan = OutputPlan::with_options(&L51, 2, Mode::Speakers, opt).unwrap();
    assert!(plan.description.contains("limiter on"));
    let mut lim = plan.build().unwrap();
    let res = lim.process(&input).unwrap();
    assert!(res.iter().flatten().all(|v| v.abs() <= limiter::THRESHOLD + 1e-6));
    let (frames, deepest) = lim.limiter_stats();
    assert_eq!(frames, 256);
    assert!(deepest < -6.0);
    // A quiet signal passes untouched (bit-exact against the unlimited mix).
    let quiet: Vec<Vec<f32>> = (0..6).map(|c| noise(c as u64, 512).iter().map(|v| v * 0.1).collect()).collect();
    let mut plain = OutputPlan::new(&L51, 2, Mode::Speakers).unwrap().build().unwrap();
    lim.reset();
    let a = lim.process(&quiet).unwrap();
    let b = plain.process(&quiet).unwrap();
    assert_eq!(a, b);
}

#[test]
fn fft_matches_a_naive_dft() {
    let n = 64;
    let fft = fft::Fft::new(n);
    let mut re = noise(5, n);
    let mut im = noise(6, n);
    let (r0, i0) = (re.clone(), im.clone());
    fft.forward(&mut re, &mut im);
    for k in 0..n {
        let (mut sr, mut si) = (0.0f64, 0.0f64);
        for j in 0..n {
            let a = -2.0 * std::f64::consts::PI * (j * k) as f64 / n as f64;
            sr += f64::from(r0[j]) * a.cos() - f64::from(i0[j]) * a.sin();
            si += f64::from(r0[j]) * a.sin() + f64::from(i0[j]) * a.cos();
        }
        assert!((f64::from(re[k]) - sr).abs() < 1e-4 && (f64::from(im[k]) - si).abs() < 1e-4);
    }
    fft.inverse(&mut re, &mut im);
    assert!(re.iter().zip(&r0).all(|(a, b)| (a - b).abs() < 1e-5));
}

// ---- binaural -------------------------------------------------------------------------------

fn binaural_plan(labels: &[&str], rate: u32) -> OutputPlan {
    let opt = Options { sample_rate: rate, ..Options::default() };
    OutputPlan::with_options(labels, 2, Mode::Headphones, opt).unwrap()
}

/// 1 / sqrt(total energy of the front left speaker pair) from the committed table at the native rate.
fn native_scale() -> f32 {
    let (l, r) = hrtf::native_pair(direction("FL", true, true).unwrap());
    1.0 / l.iter().chain(r.iter()).map(|x| x * x).sum::<f32>().sqrt()
}

#[test]
fn binaural_impulse_responses_match_the_dataset_for_every_speaker() {
    let plan = binaural_plan(&L714, 44100);
    assert_eq!(plan.kind, PlanKind::Binaural);
    assert_eq!(plan.out_layout, ["FL", "FR"]);
    let scale = native_scale();
    let blocks = 1024;
    for (c, label) in L714.iter().enumerate() {
        let mut out = plan.build().unwrap();
        let mut input = vec![vec![0.0f32; blocks]; 12];
        input[c][0] = 1.0;
        let res = out.process(&input).unwrap();
        let (el, er): (Vec<f32>, Vec<f32>) = match direction(label, true, true) {
            Some(d) => {
                let (l, r) = hrtf::native_pair(d);
                (l.iter().map(|x| x * scale).collect(), r.iter().map(|x| x * scale).collect())
            }
            None => {
                let mut d = vec![0.0; 23];
                d[22] = 0.5;
                (d.clone(), d)
            }
        };
        // 0.5 ms at 44.1 kHz is 22 samples.
        for (ear, want) in [(0, &el), (1, &er)] {
            for i in 0..blocks {
                let w = want.get(i).copied().unwrap_or(0.0);
                assert!((res[ear][i] - w).abs() < 2e-6, "{label} ear {ear} tap {i}: {} vs {w}", res[ear][i]);
            }
        }
    }
}

#[test]
fn binaural_channels_are_mirror_images() {
    let plan = binaural_plan(&L714, 48000);
    let r = plan.binaural_responses();
    let idx = |l: &str| plan.source.iter().position(|x| *x == l).unwrap();
    for (a, b) in [("FL", "FR"), ("SL", "SR"), ("BL", "BR"), ("TFL", "TFR"), ("TBL", "TBR")] {
        assert_eq!(r[idx(a)].0, r[idx(b)].1, "{a}/{b} near ear");
        assert_eq!(r[idx(a)].1, r[idx(b)].0, "{a}/{b} far ear");
    }
    // The near ear is the louder one and arrives first.
    let energy = |v: &[f32]| v.iter().map(|x| x * x).sum::<f32>();
    assert!(energy(&r[idx("FL")].0) > 2.0 * energy(&r[idx("FL")].1));
    assert_eq!(r[idx("FC")].0, r[idx("FC")].1);
}

#[test]
fn resampled_responses_keep_the_frequency_response() {
    // Compare |H(f)| of the 44.1 kHz table and the 48 kHz and 96 kHz versions of FL's near ear.
    let d = direction("FL", true, true).unwrap();
    let (native, _) = hrtf::native_pair(d);
    let mag = |h: &[f32], rate: f64, f: f64| {
        let (mut re, mut im) = (0.0f64, 0.0f64);
        for (n, &x) in h.iter().enumerate() {
            let a = -2.0 * std::f64::consts::PI * f * n as f64 / rate;
            re += f64::from(x) * a.cos();
            im += f64::from(x) * a.sin();
        }
        (re * re + im * im).sqrt()
    };
    for rate in [48000u32, 88200, 96000, 192000] {
        let (r, _) = hrtf::pair_at(d, rate);
        assert!(r.len() as f64 >= 128.0 * f64::from(rate) / 44100.0 - 1.0);
        for f in [100.0, 500.0, 1000.0, 2000.0, 4000.0, 8000.0, 12000.0, 16000.0] {
            let a = mag(&native, 44100.0, f);
            let b = mag(&r, f64::from(rate), f);
            let diff_db = 20.0 * (b / a).log10();
            assert!(diff_db.abs() < 0.3, "{rate} Hz at {f} Hz: {diff_db:.3} dB");
        }
    }
    assert_eq!(hrtf::resample(&native, 44100, 44100), native);
}

#[test]
fn binaural_matches_direct_convolution_at_48k() {
    let plan = binaural_plan(&L714, 48000);
    let resp = plan.binaural_responses();
    let n = 2048;
    let input: Vec<Vec<f32>> = (0..12).map(|c| noise(100 + c as u64, n)).collect();
    let mut out = plan.build().unwrap();
    let res = out.process(&input).unwrap();
    for ear in 0..2 {
        for i in (0..n).step_by(7) {
            let mut acc = 0.0f64;
            for c in 0..12 {
                let ir = if ear == 0 { &resp[c].0 } else { &resp[c].1 };
                for (k, &h) in ir.iter().enumerate() {
                    if k <= i {
                        acc += f64::from(h) * f64::from(input[c][i - k]);
                    }
                }
            }
            assert!((f64::from(res[ear][i]) - acc).abs() < 3e-5, "ear {ear} frame {i}: {} vs {acc}", res[ear][i]);
        }
    }
}

#[test]
fn binaural_stream_does_not_depend_on_the_chunking() {
    let plan = binaural_plan(&L714, 48000);
    let total = 4096;
    let input: Vec<Vec<f32>> = (0..12).map(|c| noise(300 + c as u64, total)).collect();
    let whole = plan.build().unwrap().process(&input).unwrap();
    // Whole hops: identical, zero latency.
    let mut out = plan.build().unwrap();
    let mut got = vec![Vec::new(), Vec::new()];
    for start in (0..total).step_by(1024) {
        let chunk: Vec<Vec<f32>> = input.iter().map(|p| p[start..start + 1024].to_vec()).collect();
        let r = out.process(&chunk).unwrap();
        for e in 0..2 {
            got[e].extend(&r[e]);
        }
    }
    for e in 0..2 {
        assert!(got[e].iter().zip(&whole[e]).all(|(a, b)| (a - b).abs() < 1e-6));
    }
    // Render quantum sized chunks (128) are also whole-hop-free of latency only when 256 divides
    // the count; with 128 the first call pads, and the stream is delayed by exactly 128 frames.
    let mut out = plan.build().unwrap();
    let mut got = vec![Vec::new(), Vec::new()];
    for start in (0..total).step_by(128) {
        let chunk: Vec<Vec<f32>> = input.iter().map(|p| p[start..start + 128].to_vec()).collect();
        let r = out.process(&chunk).unwrap();
        for e in 0..2 {
            got[e].extend(&r[e]);
        }
    }
    for e in 0..2 {
        assert!(got[e][..128].iter().all(|v| *v == 0.0));
        assert!(got[e][128..].iter().zip(&whole[e]).all(|(a, b)| (a - b).abs() < 1e-6));
    }
}

#[test]
fn binaural_energy_is_sane_and_front_left_is_calibrated() {
    let plan = binaural_plan(&L714, 48000);
    let n = 48000;
    let mut report = Vec::new();
    for (c, label) in L714.iter().enumerate() {
        let mut out = plan.build().unwrap();
        let mut input = vec![vec![0.0f32; n]; 12];
        input[c] = noise(900 + c as u64, n);
        let e_in: f64 = input[c].iter().map(|x| f64::from(*x).powi(2)).sum();
        let mut res: Vec<Vec<f32>> = vec![Vec::new(), Vec::new()];
        for start in (0..n).step_by(4096) {
            let end = (start + 4096).min(n);
            let chunk: Vec<Vec<f32>> = input.iter().map(|p| p[start..end].to_vec()).collect();
            let r = out.process(&chunk).unwrap();
            for e in 0..2 {
                res[e].extend(&r[e]);
            }
        }
        let e_out: f64 = res.iter().flatten().map(|x| f64::from(*x).powi(2)).sum();
        let ratio = e_out / e_in;
        report.push(format!("{label}={ratio:.2}"));
        assert!(res.iter().flatten().all(|x| x.is_finite()));
        if *label == "FL" || *label == "FR" {
            assert!((ratio - 1.0).abs() < 0.08, "{label} energy ratio {ratio}");
        }
        if *label == "LFE" {
            assert!((ratio - 2.0 * 0.25).abs() < 0.02, "LFE at 0.5 into both ears: {ratio}");
        } else {
            assert!((0.35..3.0).contains(&ratio), "{label} energy ratio {ratio}");
        }
    }
    eprintln!("binaural energy ratios (output L+R / input): {}", report.join(" "));
}

#[test]
fn headphone_modes_on_small_sources() {
    let stereo = OutputPlan::new(&["FL", "FR"], 2, Mode::Headphones).unwrap();
    assert_eq!(stereo.kind, PlanKind::Identity);
    let opt = Options { virtualize_stereo: true, ..Options::default() };
    let v = OutputPlan::with_options(&["FL", "FR"], 2, Mode::Headphones, opt).unwrap();
    assert_eq!(v.kind, PlanKind::Binaural);
    let mono = OutputPlan::new(&["FC"], 2, Mode::Headphones).unwrap();
    assert_eq!(mono.matrix_dense(), [1.0, 1.0]);
    // 5.1 with a lone back pair places the surrounds at 110 degrees.
    let d = direction("BL", false, true).unwrap();
    assert_eq!((d.azimuth, d.elevation, d.mirror), (-110, 0, true));
    let d = direction("BL", true, true).unwrap();
    assert_eq!(d.azimuth, -135);
    assert!(direction("LFE", true, true).is_none());
    let hp = OutputPlan::new(&L51, 6, Mode::Headphones).unwrap();
    assert_eq!(hp.kind, PlanKind::Binaural);
    assert_eq!(hp.out_layout.len(), 2);
}

#[test]
fn binaural_reset_forgets_the_tail() {
    let plan = binaural_plan(&L71, 48000);
    let mut out = plan.build().unwrap();
    let mut input = vec![vec![0.0f32; 256]; 8];
    input[0] = noise(7, 256);
    out.process(&input).unwrap();
    out.reset();
    let silent = vec![vec![0.0f32; 256]; 8];
    let r = out.process(&silent).unwrap();
    assert!(r.iter().flatten().all(|v| *v == 0.0));
}

#[test]
fn frame_limit_and_channel_count_are_checked() {
    let plan = binaural_plan(&L71, 48000);
    let mut out = plan.build().unwrap();
    let too_many = vec![vec![0.0f32; 8193]; 8];
    assert!(matches!(out.process(&too_many), Err(OutputError::FrameCount { .. })));
    assert!(matches!(out.process(&[vec![0.0; 16]]), Err(OutputError::ChannelCount { expected: 8, got: 1 })));
}

/// Cost of one 1024 frame block, 7.1.4 -> binaural. Run: `cargo test -p aether-audio --release
/// output::tests::bench -- --ignored --nocapture`.
#[test]
#[ignore]
fn bench_binaural_block() {
    for rate in [48000u32, 96000] {
        let plan = binaural_plan(&L714, rate);
        let mut out = plan.build().unwrap();
        let input: Vec<f32> = (0..12 * 1024).map(|i| ((i * 31 % 2000) as f32 - 1000.0) / 1000.0).collect();
        let mut dst = vec![0.0f32; 2 * 1024];
        for _ in 0..50 {
            out.process_planar(&input, 1024, 1024, &mut dst, 1024).unwrap();
        }
        let t = std::time::Instant::now();
        let iters = 2000;
        for _ in 0..iters {
            out.process_planar(&input, 1024, 1024, &mut dst, 1024).unwrap();
        }
        let per = t.elapsed().as_secs_f64() * 1e3 / f64::from(iters);
        eprintln!("7.1.4 -> binaural at {rate} Hz: {per:.3} ms per 1024 frame block, {} partitions", plan.binaural_responses()[0].0.len().div_ceil(256));
    }
}
