//! Iterative radix-2 complex FFT on split real / imaginary `f32` arrays. Tables are built once in
//! `new`; `forward` and `inverse` never allocate.

pub struct Fft {
    n: usize,
    rev: Vec<u32>,
    cos: Vec<f32>,
    sin: Vec<f32>,
}

impl Fft {
    /// `n` must be a power of two, at least 2.
    pub fn new(n: usize) -> Self {
        assert!(n >= 2 && n.is_power_of_two(), "fft size must be a power of two");
        let bits = n.trailing_zeros();
        let rev = (0..n as u32).map(|i| i.reverse_bits() >> (32 - bits)).collect();
        let half = n / 2;
        let mut cos = Vec::with_capacity(half);
        let mut sin = Vec::with_capacity(half);
        for k in 0..half {
            let a = -2.0 * std::f64::consts::PI * k as f64 / n as f64;
            cos.push(a.cos() as f32);
            sin.push(a.sin() as f32);
        }
        Self { n, rev, cos, sin }
    }

    /// In-place forward transform, `X[k] = sum x[j] exp(-2 pi i j k / n)`, no scaling.
    pub fn forward(&self, re: &mut [f32], im: &mut [f32]) {
        let n = self.n;
        debug_assert!(re.len() >= n && im.len() >= n);
        for i in 0..n {
            let j = self.rev[i] as usize;
            if j > i {
                re.swap(i, j);
                im.swap(i, j);
            }
        }
        let mut len = 2;
        while len <= n {
            let half = len / 2;
            let step = n / len;
            let mut start = 0;
            while start < n {
                for j in 0..half {
                    let wr = self.cos[j * step];
                    let wi = self.sin[j * step];
                    let a = start + j;
                    let b = a + half;
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

    /// In-place inverse transform including the `1/n` factor.
    pub fn inverse(&self, re: &mut [f32], im: &mut [f32]) {
        // conj(FFT(conj(X))) / n, done by swapping the two arrays.
        self.forward(im, re);
        let s = 1.0 / self.n as f32;
        for i in 0..self.n {
            re[i] *= s;
            im[i] *= s;
        }
    }
}
