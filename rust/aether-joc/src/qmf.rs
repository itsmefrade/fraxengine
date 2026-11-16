//! 64-band complex QMF bank of ETSI TS 103 420 clause 7.
//!
//! The matrix products of the specification (pseudocode 12 and 14) are
//! evaluated through a 128-point complex FFT; the result is the same up to
//! float rounding. The reference direct form is kept in the tests.

use crate::tables_gen::QWIN;

pub const N: usize = 64;
pub const LEN: usize = 640;

/// Phase offset `d` of the synthesis kernel
/// `exp(i*pi/(4n) * (2*sb+1) * (2*j + d))`, pseudocode 14 uses `-2n-1`.
pub const SYN_D: i32 = 1 - 4 * N as i32;

struct Fft128 {
    tw_re: [f32; 64],
    tw_im: [f32; 64],
    rev: [u8; 128],
}

impl Fft128 {
    fn new() -> Self {
        let mut tw_re = [0f32; 64];
        let mut tw_im = [0f32; 64];
        for k in 0..64 {
            let a = 2.0 * std::f64::consts::PI * k as f64 / 128.0;
            tw_re[k] = a.cos() as f32;
            tw_im[k] = a.sin() as f32; // positive exponent kernel
        }
        let mut rev = [0u8; 128];
        for i in 0..128usize {
            rev[i] = ((i as u8).reverse_bits() >> 1) as u8;
        }
        Self { tw_re, tw_im, rev }
    }

    /// In-place `X[k] = sum x[j] exp(+2 pi i j k / 128)`.
    fn run(&self, re: &mut [f32; 128], im: &mut [f32; 128]) {
        for i in 0..128 {
            let j = self.rev[i] as usize;
            if j > i {
                re.swap(i, j);
                im.swap(i, j);
            }
        }
        let mut len = 2;
        while len <= 128 {
            let half = len / 2;
            let step = 128 / len;
            let mut start = 0;
            while start < 128 {
                for k in 0..half {
                    let wr = self.tw_re[k * step];
                    let wi = self.tw_im[k * step];
                    let a = start + k;
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
            len *= 2;
        }
    }
}

/// Shared constant tables (twiddles) for analysis and synthesis.
pub struct QmfTables {
    fft: Fft128,
    ana_pre: [(f32, f32); 128],
    ana_post: [(f32, f32); 64],
    syn_pre: [(f32, f32); 64],
    syn_post: [(f32, f32); 128],
}

impl QmfTables {
    pub fn new() -> Self {
        Self::with_syn_offset(SYN_D)
    }

    pub fn with_syn_offset(d: i32) -> Self {
        let n = N as f64;
        let pi = std::f64::consts::PI;
        let mut ana_pre = [(0f32, 0f32); 128];
        for j in 0..128 {
            let a = pi * 0.5 * j as f64 / n;
            ana_pre[j] = (a.cos() as f32, a.sin() as f32);
        }
        let mut ana_post = [(0f32, 0f32); 64];
        for sb in 0..64 {
            let a = -pi * (0.5 * sb as f64 + 0.25) / n;
            ana_post[sb] = (a.cos() as f32, a.sin() as f32);
        }
        let mut syn_pre = [(0f32, 0f32); 64];
        for sb in 0..64 {
            let a = pi * sb as f64 * d as f64 / (2.0 * n);
            let s = 1.0 / n;
            syn_pre[sb] = ((a.cos() * s) as f32, (a.sin() * s) as f32);
        }
        let mut syn_post = [(0f32, 0f32); 128];
        for j in 0..128 {
            let a = pi * (2.0 * j as f64 + d as f64) / (4.0 * n);
            syn_post[j] = (a.cos() as f32, a.sin() as f32);
        }
        Self { fft: Fft128::new(), ana_pre, ana_post, syn_pre, syn_post }
    }
}

impl Default for QmfTables {
    fn default() -> Self {
        Self::new()
    }
}

/// QMF analysis state for one channel (clause 7.2).
#[derive(Clone)]
pub struct QmfAnalysis {
    filt: [f32; LEN],
}

impl Default for QmfAnalysis {
    fn default() -> Self {
        Self { filt: [0.0; LEN] }
    }
}

impl QmfAnalysis {
    pub fn reset(&mut self) {
        self.filt = [0.0; LEN];
    }

    /// Consumes 64 PCM samples and produces 64 complex subband samples.
    pub fn process(&mut self, t: &QmfTables, pcm: &[f32], out_re: &mut [f32], out_im: &mut [f32]) {
        debug_assert!(pcm.len() >= N && out_re.len() >= N && out_im.len() >= N);
        self.filt.copy_within(0..LEN - N, N);
        for j in 0..N {
            self.filt[j] = pcm[N - 1 - j];
        }
        let mut re = [0f32; 128];
        let mut im = [0f32; 128];
        for j in 0..128 {
            let mut u = 0f32;
            let mut k = 0;
            while k < LEN {
                u += self.filt[j + k] * QWIN[j + k];
                k += 128;
            }
            let (c, s) = t.ana_pre[j];
            re[j] = u * c;
            im[j] = u * s;
        }
        t.fft.run(&mut re, &mut im);
        for sb in 0..N {
            let (c, s) = t.ana_post[sb];
            out_re[sb] = re[sb] * c - im[sb] * s;
            out_im[sb] = re[sb] * s + im[sb] * c;
        }
    }
}

/// QMF synthesis state for one channel (clause 7.3).
#[derive(Clone)]
pub struct QmfSynthesis {
    filt: Vec<f32>, // 2 * LEN
}

impl Default for QmfSynthesis {
    fn default() -> Self {
        Self { filt: vec![0.0; 2 * LEN] }
    }
}

impl QmfSynthesis {
    pub fn reset(&mut self) {
        self.filt.iter_mut().for_each(|v| *v = 0.0);
    }

    /// Consumes 64 complex subband samples and produces 64 PCM samples.
    pub fn process(&mut self, t: &QmfTables, in_re: &[f32], in_im: &[f32], pcm: &mut [f32]) {
        self.filt.copy_within(0..2 * LEN - 2 * N, 2 * N);
        let mut re = [0f32; 128];
        let mut im = [0f32; 128];
        for sb in 0..N {
            let (c, s) = t.syn_pre[sb];
            re[sb] = in_re[sb] * c - in_im[sb] * s;
            im[sb] = in_re[sb] * s + in_im[sb] * c;
        }
        t.fft.run(&mut re, &mut im);
        for j in 0..128 {
            let (c, s) = t.syn_post[j];
            self.filt[j] = re[j] * c - im[j] * s;
        }
        // g and w (pseudocode 15, 16) and the final summation (17) in one go.
        for ts in 0..N {
            let mut acc = 0f32;
            for j in 0..LEN / N {
                // w[n*j + ts]; the index into g picks from qsyn_filt:
                let gi = N * j + ts;
                let blk = gi / (2 * N); // j of pseudocode 15
                let rem = gi % (2 * N);
                let src = if rem < N { 4 * N * blk + rem } else { 4 * N * blk + 3 * N + (rem - N) };
                acc += self.filt[src] * QWIN[gi];
            }
            pcm[ts] = acc;
        }
    }
}

/// Reference direct-form analysis (pseudocode 12), used by tests.
#[doc(hidden)]
pub fn analysis_direct(filt: &[f32; LEN], out: &mut [(f64, f64); N]) {
    let n = N as f64;
    let mut u = [0f64; 128];
    for j in 0..128 {
        let mut s = 0f64;
        let mut k = 0;
        while k < LEN {
            s += filt[j + k] as f64 * QWIN[j + k] as f64;
            k += 128;
        }
        u[j] = s;
    }
    for sb in 0..N {
        let mut re = 0f64;
        let mut im = 0f64;
        for j in 0..128 {
            let a = std::f64::consts::PI * (sb as f64 + 0.5) * (j as f64 - 0.5) / n;
            re += u[j] * a.cos();
            im += u[j] * a.sin();
        }
        out[sb] = (re, im);
    }
}

/// The QWIN prototype filter (for tests and tools).
pub fn qwin() -> &'static [f32; 640] {
    &QWIN
}
