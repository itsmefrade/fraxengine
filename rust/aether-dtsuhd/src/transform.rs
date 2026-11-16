//! Frequency transforms of the ACE decoder (ETSI TS 103 491 clause 9.10.5):
//! DCT-IV based inverse MDCT with window butterflies, and the sequency ordered Haar
//! transform used for time-frequency reshaping.

use std::f64::consts::PI;
use std::sync::OnceLock;

/// Orthonormal DCT-IV plan using an N/2-point complex FFT.
pub struct Dct4 {
    n: usize,
    tw: Vec<(f64, f64)>,     // exp(-i pi (k + 1/8) / N)
    fft_tw: Vec<(f64, f64)>, // exp(-2 pi i k / (N/2))
    rev: Vec<usize>,
}

impl Dct4 {
    pub fn new(n: usize) -> Self {
        assert!(n >= 4 && n.is_power_of_two());
        let h = n / 2;
        let tw = (0..h)
            .map(|k| {
                let a = -PI * (k as f64 + 0.125) / n as f64;
                (a.cos(), a.sin())
            })
            .collect();
        let fft_tw = (0..h / 2)
            .map(|k| {
                let a = -2.0 * PI * k as f64 / h as f64;
                (a.cos(), a.sin())
            })
            .collect();
        let bits = h.trailing_zeros();
        let rev = (0..h)
            .map(|i| {
                if bits == 0 {
                    0
                } else {
                    (i.reverse_bits() >> (usize::BITS - bits)) as usize
                }
            })
            .collect();
        Dct4 { n, tw, fft_tw, rev }
    }

    fn fft(&self, re: &mut [f64], im: &mut [f64]) {
        let h = self.n / 2;
        for i in 0..h {
            let j = self.rev[i];
            if j > i {
                re.swap(i, j);
                im.swap(i, j);
            }
        }
        let mut len = 2;
        while len <= h {
            let step = h / len;
            for start in (0..h).step_by(len) {
                for k in 0..len / 2 {
                    let (wr, wi) = self.fft_tw[k * step];
                    let (a, b) = (start + k, start + k + len / 2);
                    let tr = re[b] * wr - im[b] * wi;
                    let ti = re[b] * wi + im[b] * wr;
                    re[b] = re[a] - tr;
                    im[b] = im[a] - ti;
                    re[a] += tr;
                    im[a] += ti;
                }
            }
            len <<= 1;
        }
    }

    /// Plain orthonormal DCT-IV: y[m] = sqrt(2/N) sum_n x[n] cos(pi/(4N) (2m+1)(2n+1)).
    pub fn forward(&self, x: &[f32], y: &mut [f32]) {
        let n = self.n;
        let h = n / 2;
        let mut re = vec![0f64; h];
        let mut im = vec![0f64; h];
        for k in 0..h {
            let a = x[2 * k] as f64;
            let b = x[n - 1 - 2 * k] as f64;
            let (c, s) = self.tw[k];
            re[k] = a * c - b * s;
            im[k] = a * s + b * c;
        }
        self.fft(&mut re, &mut im);
        let scale = (2.0 / n as f64).sqrt();
        for k in 0..h {
            let (c, s) = self.tw[k];
            let r = re[k] * c - im[k] * s;
            let i = re[k] * s + im[k] * c;
            y[2 * k] = (r * scale) as f32;
            y[n - 1 - 2 * k] = (-i * scale) as f32;
        }
    }
}

static DCT4_128: OnceLock<Dct4> = OnceLock::new();
static DCT4_1024: OnceLock<Dct4> = OnceLock::new();

pub fn dct4_plan(n: usize) -> &'static Dct4 {
    match n {
        128 => DCT4_128.get_or_init(|| Dct4::new(128)),
        _ => DCT4_1024.get_or_init(|| Dct4::new(1024)),
    }
}

/// Block transform of the inverse MDCT: orthonormal DCT-IV of one block, in place.
///
/// Clause 9.10.5.3.3 prints the kernel with a reversed input vector and a negated sum
/// (`-cos(..) * vector[size-1-n]`). Taken literally that mirrors the spectrum (coefficient 0
/// would land at the Nyquist frequency) which contradicts the band layout of the same
/// specification (band 0 is the low frequency band), so the plain DCT-IV is used.
pub fn idct4_block(x: &mut [f32]) {
    let n = x.len();
    let mut y = vec![0f32; n];
    dct4_plan(n).forward(x, &mut y);
    x.copy_from_slice(&y);
}

/// Vorbis power-complementary slope of length `k`.
pub fn slope(k: usize) -> Vec<f32> {
    (0..k)
        .map(|i| {
            let w = (PI / 2.0 * (i as f64 + 0.5) / k as f64).sin();
            (PI / 2.0 * w * w).sin() as f32
        })
        .collect()
}

/// Time domain aliasing cancellation rotation across a block boundary. Clause 9.10.5.3.5
/// prints `v0 = w[N-n-1] a - w[n] b; v1 = w[n] a + w[N-n-1] b`. With that sign layout the
/// resulting basis functions are only ~81 % concentrated around their frequency (aliasing is
/// not cancelled), whereas the transposed rotation below gives a proper alias free MDCT
/// (99.9 % of the energy within +-2 bins, see tests/transform.rs).
fn butterfly(a: &mut f32, b: &mut f32, w_lo: f32, w_hi: f32) {
    let (va, vb) = (*a, *b);
    *a = w_hi * va + w_lo * vb;
    *b = -w_lo * va + w_hi * vb;
}

/// Inverse of `butterfly` (used for analysis in tests).
pub fn butterfly_inverse(a: &mut f32, b: &mut f32, w_lo: f32, w_hi: f32) {
    let (va, vb) = (*a, *b);
    *a = w_hi * va - w_lo * vb;
    *b = w_lo * va + w_hi * vb;
}

/// Inverse windowed MDCT step (do_windowed_imdct). `curr` holds the spectrum of the
/// current frame in block-major layout and is replaced by its DCT-IV output; `prev`
/// (the previous frame's DCT-IV output) is completed at its tail.
pub fn windowed_imdct(prev: &mut [f32], curr: &mut [f32], n_prev: usize, n_curr: usize) {
    let length = curr.len();
    let k = n_prev.min(n_curr);
    for blk in curr.chunks_mut(n_curr) {
        idct4_block(blk);
    }
    let w = slope(k);
    // boundary between the previous frame and this one
    for j in 0..k / 2 {
        let n = k / 2 - 1 - j;
        let (w_lo, w_hi) = (w[n], w[k - n - 1]);
        let pi = prev.len() - 1 - j;
        let (mut a, mut b) = (prev[pi], curr[j]);
        butterfly(&mut a, &mut b, w_lo, w_hi);
        prev[pi] = a;
        curr[j] = b;
    }
    // block boundaries inside the current frame
    if n_curr < length {
        let w = slope(n_curr);
        for blk in 1..length / n_curr {
            let base = blk * n_curr;
            for j in 0..n_curr / 2 {
                let n = n_curr / 2 - 1 - j;
                let (w_lo, w_hi) = (w[n], w[n_curr - n - 1]);
                let (mut a, mut b) = (curr[base - 1 - j], curr[base + j]);
                butterfly(&mut a, &mut b, w_lo, w_hi);
                curr[base - 1 - j] = a;
                curr[base + j] = b;
            }
        }
    }
}

/// Sequency ordered Haar/Walsh transform of the specification (haar_coefficient):
/// H[m][n] = (-1)^popcount(gray(m) & rev(n)); y = H x / sqrt(N). Symmetric and involutory.
pub fn haar_block(x: &mut [f32]) {
    let n = x.len();
    if n <= 1 {
        return;
    }
    let bits = n.trailing_zeros();
    let mut t = vec![0f32; n];
    for i in 0..n {
        let r = (i.reverse_bits() >> (usize::BITS - bits)) as usize;
        t[r] = x[i];
    }
    // natural-order fast Walsh-Hadamard
    let mut len = 1;
    while len < n {
        for s in (0..n).step_by(len * 2) {
            for i in s..s + len {
                let (a, b) = (t[i], t[i + len]);
                t[i] = a + b;
                t[i + len] = a - b;
            }
        }
        len <<= 1;
    }
    let sc = 1.0 / (n as f32).sqrt();
    for m in 0..n {
        x[m] = t[m ^ (m >> 1)] * sc;
    }
}

/// reshape_band (clause 9.10.5.4): convert a band between time-frequency resolutions.
pub fn reshape_band(
    frame_duration: usize,
    band_start: usize,
    band_end: usize,
    old_blocks: usize,
    new_blocks: usize,
    vector: &mut [f32],
) {
    if old_blocks == new_blocks || old_blocks == 0 || new_blocks == 0 {
        return;
    }
    let mut frame = vec![0f32; frame_duration];
    {
        let abs = band_start / old_blocks;
        let abe = band_end / old_blocks;
        let per = abe - abs;
        let blk_len = frame_duration / old_blocks;
        for blk in 0..old_blocks {
            for n in 0..per {
                frame[blk * blk_len + abs + n] = vector[blk * per + n];
            }
        }
        for b in frame.chunks_mut(blk_len) {
            haar_block(b);
        }
    }
    let abs = band_start / new_blocks;
    let abe = band_end / new_blocks;
    let per = abe - abs;
    let blk_len = frame_duration / new_blocks;
    for b in frame.chunks_mut(blk_len) {
        haar_block(b);
    }
    for blk in 0..new_blocks {
        for n in 0..per {
            vector[blk * per + n] = frame[blk * blk_len + abs + n];
        }
    }
}
