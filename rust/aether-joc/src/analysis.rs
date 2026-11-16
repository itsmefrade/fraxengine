//! Diagnostics used by the validation tools and tests.

/// Solves `A x = b` by Gaussian elimination with partial pivoting.
fn solve(a: &mut [Vec<f64>], b: &mut [f64]) -> Vec<f64> {
    let n = b.len();
    for i in 0..n {
        a[i][i] += 1e-9 * (1.0 + a[i][i]);
    }
    for c in 0..n {
        let mut p = c;
        for r in c + 1..n {
            if a[r][c].abs() > a[p][c].abs() {
                p = r;
            }
        }
        a.swap(c, p);
        b.swap(c, p);
        let d = a[c][c];
        for r in c + 1..n {
            let f = a[r][c] / d;
            if f == 0.0 {
                continue;
            }
            for k in c..n {
                let v = a[c][k];
                a[r][k] -= f * v;
            }
            let v = b[c];
            b[r] -= f * v;
        }
    }
    let mut x = vec![0f64; n];
    for i in (0..n).rev() {
        let mut s = b[i];
        for k in i + 1..n {
            s -= a[i][k] * x[k];
        }
        x[i] = s / a[i][i];
    }
    x
}

/// Fits one bed channel as a static linear combination of the decoded
/// objects over `objects[k][start + latency .. start + len + latency]` (the
/// objects lag the bed by `latency` samples). Returns the gains and the
/// residual energy relative to the bed energy in dB (`None` if the bed is
/// silent).
pub fn fit_static_downmix(
    bed: &[f32],
    objects: &[Vec<f32>],
    start: usize,
    len: usize,
    latency: usize,
) -> Option<(Vec<f64>, f64)> {
    let nobj = objects.len();
    let bedw = &bed[start..start + len];
    let be: f64 = bedw.iter().map(|&v| (v as f64).powi(2)).sum();
    if be / (len as f64) < 1e-9 {
        return None;
    }
    let r = start + latency..start + len + latency;
    let mut ata = vec![vec![0f64; nobj]; nobj];
    for i in 0..nobj {
        for j in i..nobj {
            let s: f64 = r.clone().map(|t| objects[i][t] as f64 * objects[j][t] as f64).sum();
            ata[i][j] = s;
            ata[j][i] = s;
        }
    }
    let mut atb: Vec<f64> = (0..nobj)
        .map(|i| r.clone().zip(bedw).map(|(t, &b)| objects[i][t] as f64 * b as f64).sum())
        .collect();
    let x = solve(&mut ata, &mut atb);
    let mut res = 0f64;
    for (idx, t) in r.enumerate() {
        let est: f64 = (0..nobj).map(|k| x[k] * objects[k][t] as f64).sum();
        res += (bedw[idx] as f64 - est).powi(2);
    }
    Some((x, 10.0 * (res / be).log10()))
}
