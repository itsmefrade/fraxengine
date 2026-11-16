use aether_dtsuhd::transform::*;
use aether_dtsuhd::vq::*;

struct Rng(u64);
impl Rng {
    fn f(&mut self) -> f32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        ((self.0 >> 11) as f64 / (1u64 << 53) as f64 * 2.0 - 1.0) as f32
    }
}

#[test]
fn dct4_matches_the_definition() {
    for &n in &[8usize, 128, 1024] {
        let plan = Dct4::new(n);
        let mut rng = Rng(7);
        let x: Vec<f32> = (0..n).map(|_| rng.f()).collect();
        let mut y = vec![0f32; n];
        plan.forward(&x, &mut y);
        let mut worst = 0f64;
        for m in 0..n {
            let mut acc = 0f64;
            for k in 0..n {
                acc += x[k] as f64
                    * (std::f64::consts::PI / (4.0 * n as f64)
                        * (2 * m + 1) as f64
                        * (2 * k + 1) as f64)
                        .cos();
            }
            acc *= (2.0 / n as f64).sqrt();
            worst = worst.max((acc - y[m] as f64).abs());
        }
        assert!(worst < 1e-4, "n={n} worst={worst}");
    }
}

/// Synthesises `frames` frames through the windowed inverse MDCT chain; returns the completed output.
fn synth(spectra: &[Vec<f32>], shorts: &[bool]) -> Vec<f32> {
    let mut prev = vec![0f32; 1024];
    let mut prev_short = false;
    let mut out = vec![];
    for (k, s) in spectra.iter().enumerate() {
        let mut cur = s.clone();
        let n_curr = if shorts[k] { 128 } else { 1024 };
        let n_prev = if prev_short { 128 } else { 1024 };
        if k == 0 {
            for b in cur.chunks_mut(n_curr) {
                idct4_block(b);
            }
        } else {
            windowed_imdct(&mut prev, &mut cur, n_prev, n_curr);
            out.extend_from_slice(&prev);
        }
        prev = cur;
        prev_short = shorts[k];
    }
    out
}

fn energy(v: &[f32]) -> f64 {
    v.iter().map(|x| (*x as f64).powi(2)).sum()
}

#[test]
fn lapped_transform_preserves_energy_and_inner_products() {
    // The windowed inverse MDCT with power complementary slopes is an orthogonal transform,
    // so the analysis transform is its transpose and reconstruction is perfect.
    let mut rng = Rng(99);
    for pattern in [
        [false; 8],
        [false, true, true, false, true, false, false, false],
        [true; 8],
    ] {
        let mk = |rng: &mut Rng| -> Vec<Vec<f32>> {
            let mut v: Vec<Vec<f32>> = (0..8)
                .map(|_| (0..1024).map(|_| rng.f()).collect())
                .collect();
            // zero the first and the last two frames so that nothing is left pending at the edges
            for i in [0usize, 6, 7] {
                v[i].iter_mut().for_each(|x| *x = 0.0);
            }
            v
        };
        let a = mk(&mut rng);
        let b = mk(&mut rng);
        let ya = synth(&a, &pattern);
        let yb = synth(&b, &pattern);
        let ea: f64 = a.iter().map(|f| energy(f)).sum();
        let eb: f64 = b.iter().map(|f| energy(f)).sum();
        assert!(
            (energy(&ya) / ea - 1.0).abs() < 1e-5,
            "energy {} {}",
            energy(&ya),
            ea
        );
        let dot_s: f64 = a
            .iter()
            .zip(b.iter())
            .map(|(x, y)| {
                x.iter()
                    .zip(y)
                    .map(|(p, q)| *p as f64 * *q as f64)
                    .sum::<f64>()
            })
            .sum();
        let dot_t: f64 = ya
            .iter()
            .zip(yb.iter())
            .map(|(p, q)| *p as f64 * *q as f64)
            .sum();
        assert!(
            (dot_s - dot_t).abs() < 1e-3 * (ea * eb).sqrt(),
            "inner product {dot_s} vs {dot_t}"
        );
    }
}

#[test]
fn a_single_coefficient_gives_a_windowed_cosine_with_unit_energy() {
    let mut s = vec![vec![0f32; 1024]; 5];
    s[1][37] = 1.0;
    let y = synth(&s, &[false; 5]);
    assert!((energy(&y) - 1.0).abs() < 1e-5);
}

/// Fraction of the energy of the basis function of coefficient `k` within +-2.5 bins of k + 1/2.
fn localisation(k: usize, short: bool) -> f64 {
    let n = if short { 128 } else { 1024 };
    let mut s = vec![vec![0f32; 1024]; 5];
    s[1][k] = 1.0;
    let y = synth(&s, &[short; 5]);
    // DFT magnitude via a direct evaluation around the expected frequency
    let len = y.len();
    let mut total = 0f64;
    let mut near = 0f64;
    let bins = 4 * n;
    for b in 0..bins {
        // frequency in units of MDCT bins (n bins cover 0..Nyquist), oversampled by 2
        let f = b as f64 / 2.0;
        if f >= n as f64 {
            break;
        }
        let w = std::f64::consts::PI * f / n as f64;
        let (mut re, mut im) = (0f64, 0f64);
        for (i, v) in y.iter().enumerate() {
            re += *v as f64 * (w * i as f64).cos();
            im -= *v as f64 * (w * i as f64).sin();
        }
        let p = re * re + im * im;
        total += p;
        if (f - (k as f64 + 0.5)).abs() <= 2.5 {
            near += p;
        }
    }
    let _ = len;
    near / total
}

#[test]
fn basis_functions_are_alias_free_and_not_mirrored() {
    // Coefficient k must appear at frequency (k + 1/2) pi / N: band 0 is the low frequency band.
    for &k in &[3usize, 17, 40, 100] {
        let l = localisation(k, true);
        assert!(l > 0.99, "short k={k} localisation {l}");
    }
}

#[test]
fn haar_transform_is_orthonormal_and_involutory() {
    let mut rng = Rng(5);
    for &n in &[2usize, 4, 8, 64, 1024] {
        let x: Vec<f32> = (0..n).map(|_| rng.f()).collect();
        let mut y = x.clone();
        haar_block(&mut y);
        assert!((energy(&x) - energy(&y)).abs() < 1e-4 * energy(&x));
        haar_block(&mut y);
        for i in 0..n {
            assert!((x[i] - y[i]).abs() < 1e-5);
        }
    }
}

#[test]
fn reshape_to_the_same_resolution_is_the_identity_and_conserves_energy_otherwise() {
    let mut rng = Rng(11);
    let v: Vec<f32> = (0..48).map(|_| rng.f()).collect();
    let mut w = v.clone();
    reshape_band(1024, 240, 288, 1, 1, &mut w);
    assert_eq!(v, w);
    let mut w = v.clone();
    reshape_band(1024, 240, 288, 1, 8, &mut w);
    // the reshaped band is a cut of an orthogonal transform: it cannot gain energy
    assert!(energy(&w) <= energy(&v) * 1.0001);
}

#[test]
fn pyramid_counts_and_enumeration_are_bijective() {
    assert_eq!(pyramid(2, 1), 2);
    assert_eq!(pyramid(3, 2), 6);
    assert_eq!(pyramid(4, 3), 20);
    assert_eq!(pyramid(8, 0), 1);
    for &(n, k) in &[(2usize, 5usize), (3, 4), (4, 4), (5, 3), (6, 3)] {
        let total = pyramid(n, k) as usize;
        let mut seen = std::collections::HashSet::new();
        for idx in 0..total {
            let mut v = vec![0i32; n];
            make_unsigned_pyramid_vector(idx as u64, n, k, &mut v);
            assert!(v.iter().all(|&x| x >= 0));
            assert_eq!(v.iter().sum::<i32>() as usize, k);
            assert!(seen.insert(v));
        }
        assert_eq!(seen.len(), total);
    }
}

#[test]
fn best_k_grows_with_the_bit_budget_and_respects_limits() {
    let mut last = 0;
    for bits in 2..60 {
        let k = get_best_k_for_n(bits, 10_000, 8, true);
        assert!(k >= last, "bits={bits}");
        assert!(k as i32 <= max_k_for_n(8));
        last = k;
    }
    assert_eq!(get_best_k_for_n(1, 100, 8, false), 0);
    assert_eq!(get_best_k_for_n(40, 1, 8, false), 0);
}

#[test]
fn normalize_handles_zero_vectors() {
    let mut v = vec![0f32; 8];
    normalize(&mut v, 3.0);
    assert!(v.iter().all(|&x| x == 0.0));
    let mut v = vec![3.0f32, 4.0];
    normalize(&mut v, 10.0);
    assert!((v[0] - 6.0).abs() < 1e-5 && (v[1] - 8.0).abs() < 1e-5);
}

#[test]
fn analysis_by_transposition_reconstructs_the_spectra() {
    // Perfect reconstruction: the analysis filterbank is the transpose of the synthesis chain.
    let mut rng = Rng(2024);
    let shorts = [false, true, true, false, false, true, false, false];
    let mut spectra: Vec<Vec<f32>> = (0..8)
        .map(|_| (0..1024).map(|_| rng.f()).collect())
        .collect();
    for i in [0usize, 6, 7] {
        spectra[i].iter_mut().for_each(|x| *x = 0.0);
    }
    let y = synth(&spectra, &shorts);
    assert_eq!(y.len(), 7 * 1024);
    let mut frames: Vec<Vec<f32>> = y.chunks(1024).map(|c| c.to_vec()).collect();
    for i in 0..frames.len() - 1 {
        let n_prev = if shorts[i] { 128 } else { 1024 };
        let n_curr = if shorts[i + 1] { 128 } else { 1024 };
        let k = n_prev.min(n_curr);
        let w = slope(k);
        let (head, tail) = frames.split_at_mut(i + 1);
        let (prev, cur) = (&mut head[i], &mut tail[0]);
        for j in 0..k / 2 {
            let n = k / 2 - 1 - j;
            let (mut a, mut b) = (prev[1023 - j], cur[j]);
            butterfly_inverse(&mut a, &mut b, w[n], w[k - n - 1]);
            prev[1023 - j] = a;
            cur[j] = b;
        }
        if n_curr < 1024 {
            let w = slope(n_curr);
            for blk in 1..1024 / n_curr {
                let base = blk * n_curr;
                for j in 0..n_curr / 2 {
                    let n = n_curr / 2 - 1 - j;
                    let (mut a, mut b) = (cur[base - 1 - j], cur[base + j]);
                    butterfly_inverse(&mut a, &mut b, w[n], w[n_curr - n - 1]);
                    cur[base - 1 - j] = a;
                    cur[base + j] = b;
                }
            }
        }
    }
    // frame 0 (long) has no interior boundaries; short frames have theirs handled above when they are `cur`.
    for (i, f) in frames.iter_mut().enumerate() {
        let n = if shorts[i] { 128 } else { 1024 };
        for blk in f.chunks_mut(n) {
            idct4_block(blk);
        }
    }
    for i in 1..6 {
        for (a, b) in frames[i].iter().zip(spectra[i].iter()) {
            assert!((a - b).abs() < 2e-4, "frame {i}: {a} vs {b}");
        }
    }
}

#[test]
fn long_basis_functions_are_alias_free() {
    let l = localisation(37, false);
    assert!(l > 0.99, "long k=37 localisation {l}");
}
