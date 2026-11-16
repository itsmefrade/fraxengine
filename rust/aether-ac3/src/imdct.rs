//! IMDCT built on a DCT-IV that is evaluated with a half-length complex FFT,
//! plus the Kaiser-Bessel-derived window used by AC-3.

use core::f64::consts::PI;

/// DCT-IV of length `n` (type IV, unnormalised):
/// `D[m] = sum_k X[k] cos(pi/n (m + 1/2)(k + 1/2))`.
pub(crate) struct Dct4 {
    n: usize,
    half: usize,
    /// exp(-i pi m / n) for m in 0..n/2 (pre-twiddle), as (re, im).
    pre: Vec<(f32, f32)>,
    /// exp(-i pi (4k + 1) / (4 n)) for k in 0..n/2 (post-twiddle).
    post: Vec<(f32, f32)>,
    /// exp(-2 pi i k / half) for k in 0..half/2.
    fft_tw: Vec<(f32, f32)>,
    bitrev: Vec<u16>,
}

impl Dct4 {
    pub fn new(n: usize) -> Self {
        assert!(n.is_power_of_two() && n >= 4);
        let half = n / 2;
        let pre = (0..half)
            .map(|m| {
                let a = -PI * m as f64 / n as f64;
                (a.cos() as f32, a.sin() as f32)
            })
            .collect();
        let post = (0..half)
            .map(|k| {
                let a = -PI * (4 * k + 1) as f64 / (4 * n) as f64;
                (a.cos() as f32, a.sin() as f32)
            })
            .collect();
        let fft_tw = (0..half / 2)
            .map(|k| {
                let a = -2.0 * PI * k as f64 / half as f64;
                (a.cos() as f32, a.sin() as f32)
            })
            .collect();
        let bits = half.trailing_zeros();
        let bitrev = (0..half)
            .map(|i| (i.reverse_bits() >> (usize::BITS - bits)) as u16)
            .collect();
        Dct4 {
            n,
            half,
            pre,
            post,
            fft_tw,
            bitrev,
        }
    }

    /// In-place radix-2 decimation-in-time FFT over `re`/`im` already in
    /// bit-reversed order.
    #[inline]
    fn fft(&self, re: &mut [f32], im: &mut [f32]) {
        let n = self.half;
        let mut len = 2;
        while len <= n {
            let step = n / len;
            let h = len / 2;
            let mut start = 0;
            while start < n {
                for j in 0..h {
                    let (wr, wi) = self.fft_tw[j * step];
                    let a = start + j;
                    let b = a + h;
                    let tr = re[b] * wr - im[b] * wi;
                    let ti = re[b] * wi + im[b] * wr;
                    re[b] = re[a] - tr;
                    im[b] = im[a] - ti;
                    re[a] += tr;
                    im[a] += ti;
                }
                start += len;
            }
            len <<= 1;
        }
    }

    /// Computes `out = DCT-IV(x)`; `x` and `out` hold `n` samples each.
    pub fn dct4(&self, x: &[f32], out: &mut [f32]) {
        let n = self.n;
        let half = self.half;
        let mut re = [0f32; 128];
        let mut im = [0f32; 128];
        debug_assert!(half <= 128);
        for m in 0..half {
            let a = x[2 * m];
            let b = x[n - 1 - 2 * m];
            let (wr, wi) = self.pre[m];
            let j = self.bitrev[m] as usize;
            re[j] = a * wr - b * wi;
            im[j] = a * wi + b * wr;
        }
        self.fft(&mut re[..half], &mut im[..half]);
        for k in 0..half {
            let (wr, wi) = self.post[k];
            let yr = re[k] * wr - im[k] * wi;
            let yi = re[k] * wi + im[k] * wr;
            out[2 * k] = yr;
            out[n - 1 - 2 * k] = -yi;
        }
    }
}

/// Modified Bessel function of the first kind, order 0.
fn bessel_i0(x: f64) -> f64 {
    let mut sum = 1.0;
    let mut term = 1.0;
    let q = x * x / 4.0;
    let mut k = 1.0;
    while term > sum * 1e-18 {
        term *= q / (k * k);
        sum += term;
        k += 1.0;
    }
    sum
}

/// Kaiser-Bessel-derived window of length `n` (FFmpeg `ff_kbd_window_init`).
pub(crate) fn kbd_window(alpha: f64, n: usize) -> Vec<f32> {
    let alpha2 = 4.0 * (alpha * PI / n as f64) * (alpha * PI / n as f64);
    let mut temp = vec![0f64; n / 2 + 1];
    let mut scale = 0.0;
    for i in 0..=n / 2 {
        let tmp = (i * (n - i)) as f64 * alpha2;
        temp[i] = bessel_i0(tmp.sqrt());
        scale += temp[i] * (1 + usize::from(i != 0 && i < n / 2)) as f64;
    }
    scale = 1.0 / (scale + 1.0);
    let mut win = vec![0f32; n];
    let mut sum = 0.0;
    for i in 0..=n / 2 {
        sum += temp[i];
        win[i] = (sum * scale).sqrt() as f32;
    }
    for i in n / 2 + 1..n {
        sum += temp[n - i];
        win[i] = (sum * scale).sqrt() as f32;
    }
    win
}

#[cfg(test)]
mod tests {
    use super::*;

    fn naive_dct4(x: &[f32]) -> Vec<f64> {
        let n = x.len();
        (0..n)
            .map(|m| {
                (0..n)
                    .map(|k| {
                        x[k] as f64 * (PI / n as f64 * (m as f64 + 0.5) * (k as f64 + 0.5)).cos()
                    })
                    .sum()
            })
            .collect()
    }

    #[test]
    fn dct4_matches_definition() {
        for &n in &[128usize, 256] {
            let dct = Dct4::new(n);
            let x: Vec<f32> = (0..n)
                .map(|i| ((i * 37 + 11) % 101) as f32 / 50.0 - 1.0)
                .collect();
            let mut out = vec![0f32; n];
            dct.dct4(&x, &mut out);
            let want = naive_dct4(&x);
            let mut max = 0f64;
            for (a, b) in out.iter().zip(&want) {
                max = max.max((*a as f64 - b).abs());
            }
            assert!(max < 1e-3, "n={n}: max error {max}");
        }
    }
}
