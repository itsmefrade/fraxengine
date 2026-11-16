//! QMF bank checks (spec-derived).
use aether_joc::qmf::*;

fn lcg(s: &mut u64) -> f64 {
    *s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
    ((*s >> 33) as f64 / (1u64 << 31) as f64) * 2.0 - 1.0
}

/// Float64 direct-form analysis + synthesis straight from pseudocode 8..17,
/// with the synthesis phase offset of the printed equation (j - 2n + 1/2).
fn reference_roundtrip(x: &[f64]) -> Vec<f64> {
    let q: Vec<f64> = aether_joc::qmf::qwin().iter().map(|&v| v as f64).collect();
    let n = 64usize;
    let pi = std::f64::consts::PI;
    let mut ana = vec![0f64; 640];
    let mut syn = vec![0f64; 1280];
    let mut y = vec![0f64; x.len()];
    for b in 0..x.len() / n {
        ana.copy_within(0..640 - n, n);
        for j in 0..n {
            ana[j] = x[b * n + n - 1 - j];
        }
        let mut u = vec![0f64; 128];
        for j in 0..128 {
            for k in 0..5 {
                u[j] += ana[j + 128 * k] * q[j + 128 * k];
            }
        }
        let mut qr = vec![0f64; n];
        let mut qi = vec![0f64; n];
        for sb in 0..n {
            for j in 0..128 {
                let a = pi * (sb as f64 + 0.5) * (j as f64 - 0.5) / n as f64;
                qr[sb] += u[j] * a.cos();
                qi[sb] += u[j] * a.sin();
            }
        }
        syn.copy_within(0..1280 - 2 * n, 2 * n);
        for j in 0..128 {
            let mut s = 0.0;
            for sb in 0..n {
                let a = pi * (sb as f64 + 0.5) * (j as f64 - 2.0 * n as f64 + 0.5) / n as f64;
                s += (qr[sb] * a.cos() - qi[sb] * a.sin()) / n as f64;
            }
            syn[j] = s;
        }
        let mut g = vec![0f64; 640];
        for j in 0..5 {
            for sb in 0..n {
                g[2 * n * j + sb] = syn[4 * n * j + sb];
                g[2 * n * j + n + sb] = syn[4 * n * j + 3 * n + sb];
            }
        }
        for ts in 0..n {
            let mut p = 0.0;
            for j in 0..10 {
                p += g[n * j + ts] * q[n * j + ts];
            }
            y[b * n + ts] = p;
        }
    }
    y
}

fn align_error(x: &[f64], y: &[f64], delay: usize, skip: usize) -> (f64, f64, f64) {
    // returns (max abs error, rms error, gain) over [skip, len - delay)
    let (mut num, mut den) = (0f64, 0f64);
    for i in skip..x.len() - delay {
        num += x[i] * y[i + delay];
        den += x[i] * x[i];
    }
    let g = num / den;
    let (mut mx, mut e2, mut c) = (0f64, 0f64, 0f64);
    for i in skip..x.len() - delay {
        let r = y[i + delay] - x[i];
        mx = mx.max(r.abs());
        e2 += r * r;
        c += 1.0;
    }
    (mx, (e2 / c).sqrt(), g)
}

#[test]
fn qmf_fft_matches_direct_form_and_reconstructs() {
    let mut s = 7u64;
    let x: Vec<f64> = (0..64 * 300).map(|_| lcg(&mut s)).collect();
    // Reference, float64 direct form.
    let yr = reference_roundtrip(&x);
    let (mx_r, rms_r, g_r) = align_error(&x, &yr, 577, 1500);
    println!("float64 direct form: delay 577 gain {g_r:.9} max abs err {mx_r:.3e} rms {rms_r:.3e}");
    // Optimised f32 implementation.
    let t = QmfTables::new();
    let mut ana = QmfAnalysis::default();
    let mut syn = QmfSynthesis::default();
    let xf: Vec<f32> = x.iter().map(|&v| v as f32).collect();
    let mut y = vec![0f32; x.len()];
    let (mut re, mut im) = ([0f32; 64], [0f32; 64]);
    for b in 0..x.len() / 64 {
        ana.process(&t, &xf[b * 64..], &mut re, &mut im);
        syn.process(&t, &re, &im, &mut y[b * 64..b * 64 + 64]);
    }
    let yd: Vec<f64> = y.iter().map(|&v| v as f64).collect();
    let (mx, rms, g) = align_error(&x, &yd, 577, 1500);
    println!("f32 fast path:       delay 577 gain {g:.9} max abs err {mx:.3e} rms {rms:.3e}");
    let mut worst = 0f64;
    for i in 0..x.len() {
        worst = worst.max((yd[i] - yr[i]).abs());
    }
    println!("f32 vs f64 direct form max abs diff {worst:.3e}");
    assert!(worst < 1e-5);
    // The normative prototype is a near-perfect-reconstruction design: the
    // delay-aligned error of the specified bank itself (float64, direct form)
    // is about 7e-5 rms / 4e-4 peak on full-scale white noise, so a 1e-5
    // bound cannot be met by any implementation of the specified bank.
    assert!((g_r - 1.0).abs() < 1e-5);
    assert!(rms_r < 1.5e-4 && mx_r < 1e-3);
    assert!(rms < 1.5e-4 && mx < 1e-3);
}
