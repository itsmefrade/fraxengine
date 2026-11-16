//! Binaural rendering by uniform-partitioned FFT convolution (overlap-save).
//!
//! Every input channel has one response per ear. Per hop of `hop` frames: the channels are
//! transformed two at a time (one complex FFT carries two real signals), their spectra go into a
//! ring of `parts` past spectra, the left and right ear spectra are the sums over channels and
//! partitions of spectrum x filter, and both ears come back through one inverse FFT (left in the
//! real part, right in the imaginary part). Everything is allocated in `new`; `process` does no
//! allocation.
//!
//! Latency: zero when the caller passes whole hops (`hop` divides the frame count). Any other
//! frame count works, the first call is then padded with silence in front (a fixed delay of up to
//! `hop - 1` frames, the stream stays continuous).

use super::fft::Fft;

pub struct Binaural {
    hop: usize,
    nfft: usize,
    bins: usize,
    parts: usize,
    nch: usize,
    fft: Fft,
    /// Filter spectra, index `((c * 2 + ear) * parts + p) * bins + k`.
    filt_re: Vec<f32>,
    filt_im: Vec<f32>,
    /// Input spectra history, index `(c * parts + slot) * bins + k`.
    hist_re: Vec<f32>,
    hist_im: Vec<f32>,
    pos: usize,
    /// Previous hop of every channel, `c * hop`.
    prev: Vec<f32>,
    cur: Vec<f32>,
    fill: usize,
    z_re: Vec<f32>,
    z_im: Vec<f32>,
    l_re: Vec<f32>,
    l_im: Vec<f32>,
    r_re: Vec<f32>,
    r_im: Vec<f32>,
    ring: [Vec<f32>; 2],
    ring_head: usize,
    ring_len: usize,
    max_frames: usize,
}

impl Binaural {
    /// `irs[c]` is the (left ear, right ear) response of input channel `c`. `hop` must be a power
    /// of two; `max_frames` bounds the frame count of one `process` call.
    pub fn new(irs: &[(Vec<f32>, Vec<f32>)], hop: usize, max_frames: usize) -> Self {
        assert!(hop.is_power_of_two() && hop >= 16);
        let nch = irs.len();
        let nfft = 2 * hop;
        let bins = hop + 1;
        let longest = irs.iter().map(|(l, r)| l.len().max(r.len())).max().unwrap_or(1).max(1);
        let parts = longest.div_ceil(hop);
        let fft = Fft::new(nfft);
        let mut filt_re = vec![0.0; nch * 2 * parts * bins];
        let mut filt_im = vec![0.0; nch * 2 * parts * bins];
        let mut zr = vec![0.0f32; nfft];
        let mut zi = vec![0.0f32; nfft];
        for (c, (l, r)) in irs.iter().enumerate() {
            for (ear, ir) in [l, r].into_iter().enumerate() {
                for p in 0..parts {
                    zr.fill(0.0);
                    zi.fill(0.0);
                    let lo = (p * hop).min(ir.len());
                    let hi = ((p + 1) * hop).min(ir.len());
                    zr[..hi - lo].copy_from_slice(&ir[lo..hi]);
                    fft.forward(&mut zr, &mut zi);
                    let base = ((c * 2 + ear) * parts + p) * bins;
                    filt_re[base..base + bins].copy_from_slice(&zr[..bins]);
                    filt_im[base..base + bins].copy_from_slice(&zi[..bins]);
                }
            }
        }
        let cap = max_frames + 2 * hop;
        Self {
            hop,
            nfft,
            bins,
            parts,
            nch,
            fft,
            filt_re,
            filt_im,
            hist_re: vec![0.0; nch * parts * bins],
            hist_im: vec![0.0; nch * parts * bins],
            pos: 0,
            prev: vec![0.0; nch * hop],
            cur: vec![0.0; nch * hop],
            fill: 0,
            z_re: vec![0.0; nfft],
            z_im: vec![0.0; nfft],
            l_re: vec![0.0; bins],
            l_im: vec![0.0; bins],
            r_re: vec![0.0; bins],
            r_im: vec![0.0; bins],
            ring: [vec![0.0; cap], vec![0.0; cap]],
            ring_head: 0,
            ring_len: 0,
            max_frames,
        }
    }

    pub fn hop(&self) -> usize {
        self.hop
    }

    pub fn partitions(&self) -> usize {
        self.parts
    }

    pub fn reset(&mut self) {
        self.hist_re.fill(0.0);
        self.hist_im.fill(0.0);
        self.prev.fill(0.0);
        self.cur.fill(0.0);
        self.fill = 0;
        self.pos = 0;
        self.ring_head = 0;
        self.ring_len = 0;
    }

    /// Renders `frames` frames. Input plane `c` is `input[c * in_stride..][..frames]`, the output
    /// is two planes `out[0..frames]` (left) and `out[out_stride..][..frames]` (right).
    pub fn process(&mut self, input: &[f32], in_stride: usize, frames: usize, out: &mut [f32], out_stride: usize) {
        assert!(frames <= self.max_frames);
        let hop = self.hop;
        let mut done = 0;
        while done < frames {
            let take = (hop - self.fill).min(frames - done);
            for c in 0..self.nch {
                let src = &input[c * in_stride + done..c * in_stride + done + take];
                self.cur[c * hop + self.fill..c * hop + self.fill + take].copy_from_slice(src);
            }
            self.fill += take;
            done += take;
            if self.fill == hop {
                self.run_hop();
                self.fill = 0;
            }
        }
        let cap = self.ring[0].len();
        let pad = frames.saturating_sub(self.ring_len);
        for ear in 0..2 {
            let dst = &mut out[ear * out_stride..ear * out_stride + frames];
            dst[..pad].fill(0.0);
            for (i, d) in dst[pad..].iter_mut().enumerate() {
                *d = self.ring[ear][(self.ring_head + i) % cap];
            }
        }
        let used = frames - pad;
        self.ring_head = (self.ring_head + used) % cap;
        self.ring_len -= used;
    }

    fn run_hop(&mut self) {
        let hop = self.hop;
        let n = self.nfft;
        let bins = self.bins;
        let parts = self.parts;
        let slot = self.pos;
        let mut c = 0;
        while c < self.nch {
            let two = c + 1 < self.nch;
            self.z_re[..hop].copy_from_slice(&self.prev[c * hop..(c + 1) * hop]);
            self.z_re[hop..].copy_from_slice(&self.cur[c * hop..(c + 1) * hop]);
            if two {
                self.z_im[..hop].copy_from_slice(&self.prev[(c + 1) * hop..(c + 2) * hop]);
                self.z_im[hop..].copy_from_slice(&self.cur[(c + 1) * hop..(c + 2) * hop]);
            } else {
                self.z_im.fill(0.0);
            }
            self.fft.forward(&mut self.z_re, &mut self.z_im);
            let a = (c * parts + slot) * bins;
            let b = ((c + 1) * parts + slot) * bins;
            for k in 0..bins {
                let kn = (n - k) % n;
                let (zr, zi, zrn, zin) = (self.z_re[k], self.z_im[k], self.z_re[kn], self.z_im[kn]);
                self.hist_re[a + k] = 0.5 * (zr + zrn);
                self.hist_im[a + k] = 0.5 * (zi - zin);
                if two {
                    self.hist_re[b + k] = 0.5 * (zi + zin);
                    self.hist_im[b + k] = -0.5 * (zr - zrn);
                }
            }
            c += 2;
        }
        self.l_re.fill(0.0);
        self.l_im.fill(0.0);
        self.r_re.fill(0.0);
        self.r_im.fill(0.0);
        for c in 0..self.nch {
            for p in 0..parts {
                let s = (slot + parts - p) % parts;
                let h = (c * parts + s) * bins;
                let fl = ((c * 2) * parts + p) * bins;
                let fr = ((c * 2 + 1) * parts + p) * bins;
                let (xr, xi) = (&self.hist_re[h..h + bins], &self.hist_im[h..h + bins]);
                let (flr, fli) = (&self.filt_re[fl..fl + bins], &self.filt_im[fl..fl + bins]);
                let (frr, fri) = (&self.filt_re[fr..fr + bins], &self.filt_im[fr..fr + bins]);
                for k in 0..bins {
                    self.l_re[k] += xr[k] * flr[k] - xi[k] * fli[k];
                    self.l_im[k] += xr[k] * fli[k] + xi[k] * flr[k];
                    self.r_re[k] += xr[k] * frr[k] - xi[k] * fri[k];
                    self.r_im[k] += xr[k] * fri[k] + xi[k] * frr[k];
                }
            }
        }
        // Z = L + iR with both spectra Hermitian.
        for k in 0..bins {
            self.z_re[k] = self.l_re[k] - self.r_im[k];
            self.z_im[k] = self.l_im[k] + self.r_re[k];
        }
        for k in bins..n {
            let m = n - k;
            self.z_re[k] = self.l_re[m] + self.r_im[m];
            self.z_im[k] = -self.l_im[m] + self.r_re[m];
        }
        self.fft.inverse(&mut self.z_re, &mut self.z_im);
        let cap = self.ring[0].len();
        for i in 0..hop {
            let at = (self.ring_head + self.ring_len + i) % cap;
            self.ring[0][at] = self.z_re[hop + i];
            self.ring[1][at] = self.z_im[hop + i];
        }
        self.ring_len += hop;
        self.prev.copy_from_slice(&self.cur);
        self.pos = (self.pos + 1) % parts;
    }
}
