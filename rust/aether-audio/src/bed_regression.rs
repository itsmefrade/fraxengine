//! Experimental port of AetherEngine BedRegression at 74db82a2b02d3e282d8f04a415578e34e936c346.
//! Inputs are already decoded, aligned planar PCM; placement/panning is external.
use std::f64::consts::PI;
const N: usize = 2048;
const H: usize = 512;
const BINS: usize = N / 2 + 1;
#[derive(Clone, Copy, Default, Debug)]
struct C {
    r: f64,
    i: f64,
}
impl C {
    fn conj(self) -> Self {
        Self {
            r: self.r,
            i: -self.i,
        }
    }
    fn mul(self, b: Self) -> Self {
        Self {
            r: self.r * b.r - self.i * b.i,
            i: self.r * b.i + self.i * b.r,
        }
    }
    fn add(self, b: Self) -> Self {
        Self {
            r: self.r + b.r,
            i: self.i + b.i,
        }
    }
    fn sub(self, b: Self) -> Self {
        Self {
            r: self.r - b.r,
            i: self.i - b.i,
        }
    }
    fn scale(self, x: f64) -> Self {
        Self {
            r: self.r * x,
            i: self.i * x,
        }
    }
}
fn fft(a: &mut [C], inverse: bool) {
    let n = a.len();
    let mut j = 0;
    for i in 1..n {
        let mut bit = n >> 1;
        while j & bit != 0 {
            j ^= bit;
            bit >>= 1
        }
        j ^= bit;
        if i < j {
            a.swap(i, j)
        }
    }
    let mut len = 2;
    while len <= n {
        let ang = (if inverse { 2.0 } else { -2.0 }) * PI / len as f64;
        let wlen = C {
            r: ang.cos(),
            i: ang.sin(),
        };
        for start in (0..n).step_by(len) {
            let mut w = C { r: 1., i: 0. };
            for k in 0..len / 2 {
                let u = a[start + k];
                let v = a[start + k + len / 2].mul(w);
                a[start + k] = u.add(v);
                a[start + k + len / 2] = u.sub(v);
                w = w.mul(wlen)
            }
        }
        len *= 2
    }
    if inverse {
        for x in a {
            *x = x.scale(1. / n as f64)
        }
    }
}
#[derive(Clone, Debug)]
pub struct Fold {
    pub element: usize,
    pub bed_plane: usize,
    pub gain: f32,
}
#[derive(Clone, Debug)]
pub struct Config {
    pub bed_channels: usize,
    pub bed_lfe: Option<usize>,
    pub outputs: usize,
    pub output_lfe: usize,
    /// output-major D matrix excluding the input LFE
    pub fallback: Vec<f32>,
    pub elements: usize,
    pub folds: Vec<Fold>,
}
#[derive(Clone, Debug)]
pub struct Snapshot {
    pub placed: Vec<bool>,
    /// element-major output gains
    pub gains: Vec<f32>,
}
#[derive(Debug, PartialEq)]
pub enum Error {
    InvalidConfig,
    InvalidInput,
    InvalidSnapshot,
    Arithmetic,
}
pub struct BedRegression {
    cfg: Config,
    inputs: Vec<usize>,
    bands: Vec<usize>,
    centers: Vec<f32>,
    window: Vec<f32>,
    fold: Vec<f32>,
    bh: Vec<Vec<f32>>,
    eh: Vec<Vec<f32>>,
    hopb: Vec<Vec<f32>>,
    hope: Vec<Vec<f32>>,
    accum: Vec<Vec<f32>>,
    ready: Vec<Vec<f32>>,
    filled: usize,
    consumed: usize,
    written: usize,
    cov: Vec<C>,
    cross: Vec<C>,
    snapshot: Option<Snapshot>,
    ring: Vec<Vec<f32>>,
}
impl BedRegression {
    pub fn new(cfg: Config) -> Result<Self, Error> {
        if cfg.bed_channels == 0
            || cfg.bed_channels > 16
            || cfg.outputs == 0
            || cfg.outputs > 16
            || cfg.elements > 64
            || cfg.folds.len() > 1024
            || cfg.output_lfe >= cfg.outputs
            || cfg.bed_lfe.is_some_and(|i| i >= cfg.bed_channels)
        {
            return Err(Error::InvalidConfig);
        }
        let inputs: Vec<_> = (0..cfg.bed_channels)
            .filter(|i| Some(*i) != cfg.bed_lfe)
            .collect();
        let nb = inputs.len();
        if nb == 0
            || cfg.fallback.len() != cfg.outputs * nb
            || cfg.fallback.iter().any(|x| !x.is_finite() || x.abs() > 16.)
            || cfg.folds.iter().any(|f| {
                f.element >= cfg.elements
                    || !inputs.contains(&f.bed_plane)
                    || !f.gain.is_finite()
                    || f.gain.abs() > 16.
            })
        {
            return Err(Error::InvalidConfig);
        }
        let mut fold = vec![0.; cfg.elements * cfg.outputs];
        for f in &cfg.folds {
            let j = inputs.iter().position(|x| *x == f.bed_plane).unwrap();
            for o in 0..cfg.outputs {
                fold[f.element * cfg.outputs + o] -= f.gain * cfg.fallback[o * nb + j]
            }
        }
        let mut edges = vec![0];
        for i in 0..64 {
            let e = (2. * ((BINS as f64) / 2.).powf(i as f64 / 63.)).round() as usize;
            if e > *edges.last().unwrap() {
                edges.push(e.min(BINS))
            }
        }
        if *edges.last().unwrap() < BINS {
            edges.push(BINS)
        }
        let mut bands = vec![0; BINS];
        let mut centers = vec![];
        for b in 0..edges.len() - 1 {
            for x in &mut bands[edges[b]..edges[b + 1]] {
                *x = b
            }
            centers.push((edges[b] + edges[b + 1] - 1) as f32 / 2.)
        }
        let bc = centers.len();
        let window = (0..N)
            .map(|i| {
                (0.5f32 - 0.5 * (2. * std::f32::consts::PI * i as f32 / N as f32).cos()).sqrt()
            })
            .collect();
        let planes = |c, n| vec![vec![0.; n]; c];
        Ok(Self {
            bh: planes(cfg.bed_channels, N),
            eh: planes(cfg.elements, N),
            hopb: planes(cfg.bed_channels, H),
            hope: planes(cfg.elements, H),
            accum: planes(cfg.outputs, N),
            ready: planes(cfg.outputs, H),
            cov: vec![C::default(); bc * nb * nb],
            cross: vec![C::default(); bc * cfg.elements * nb],
            cfg,
            inputs,
            bands,
            centers,
            window,
            fold,
            filled: 0,
            consumed: 0,
            written: 0,
            snapshot: None,
            ring: vec![],
        })
    }
    pub fn set_gains(&mut self, s: Snapshot) -> Result<(), Error> {
        if s.placed.len() != self.cfg.elements
            || s.gains.len() != self.cfg.elements * self.cfg.outputs
            || s.gains.iter().any(|x| !x.is_finite() || x.abs() > 16.)
        {
            return Err(Error::InvalidSnapshot);
        }
        self.snapshot = Some(s);
        Ok(())
    }
    pub fn latency(&self) -> usize {
        N
    }
    pub fn backlog(&self) -> usize {
        self.consumed - self.written
    }
    pub fn reset(&mut self) {
        for p in self
            .bh
            .iter_mut()
            .chain(self.eh.iter_mut())
            .chain(self.hopb.iter_mut())
            .chain(self.hope.iter_mut())
            .chain(self.accum.iter_mut())
            .chain(self.ready.iter_mut())
        {
            p.fill(0.)
        }
        self.cov.fill(C::default());
        self.cross.fill(C::default());
        self.filled = 0;
        self.consumed = 0;
        self.written = 0;
        self.ring.clear();
        self.snapshot = None
    }
    /// Empty input is permitted. Gain changes are sampled at each completed hop, as upstream.
    pub fn process(&mut self, bed: &[&[f32]], elements: &[&[f32]]) -> Result<Vec<Vec<f32>>, Error> {
        if bed.len() != self.cfg.bed_channels || elements.len() != self.cfg.elements {
            return Err(Error::InvalidInput);
        }
        let n = bed[0].len();
        if n > 1_048_576
            || bed
                .iter()
                .chain(elements.iter())
                .any(|p| p.len() != n || p.iter().any(|x| !x.is_finite() || x.abs() > 1e6))
        {
            return Err(Error::InvalidInput);
        }
        let mut out = vec![Vec::new(); self.cfg.outputs];
        let mut pos = 0;
        while pos < n {
            let take = (H - self.filled).min(n - pos);
            for (c, p) in bed.iter().enumerate() {
                self.hopb[c][self.filled..self.filled + take].copy_from_slice(&p[pos..pos + take])
            }
            for (c, p) in elements.iter().enumerate() {
                self.hope[c][self.filled..self.filled + take].copy_from_slice(&p[pos..pos + take])
            }
            let skip = take.min(N.saturating_sub(self.consumed));
            for (o, p) in out.iter_mut().enumerate() {
                p.extend_from_slice(&self.ready[o][self.filled + skip..self.filled + take])
            }
            self.consumed += take;
            self.filled += take;
            pos += take;
            if self.filled == H {
                self.filled = 0;
                self.hop()
            }
        }
        self.written += out[0].len();
        Ok(out)
    }
    pub fn drain(&mut self) -> Result<Vec<Vec<f32>>, Error> {
        let pending = self.backlog();
        let zero = vec![0.; N];
        let b = vec![zero.as_slice(); self.cfg.bed_channels];
        let e = vec![zero.as_slice(); self.cfg.elements];
        let mut out = self.process(&b, &e)?;
        for p in &mut out {
            p.truncate(pending)
        }
        self.reset();
        Ok(out)
    }
    fn hop(&mut self) {
        for (h, p) in self.bh.iter_mut().zip(&self.hopb) {
            h.copy_within(H.., 0);
            h[N - H..].copy_from_slice(p)
        }
        for (h, p) in self.eh.iter_mut().zip(&self.hope) {
            h.copy_within(H.., 0);
            h[N - H..].copy_from_slice(p)
        }
        if let Some(s) = &self.snapshot {
            let mut g = s.gains.clone();
            for e in 0..self.cfg.elements {
                for o in 0..self.cfg.outputs {
                    if !s.placed[e] || o == self.cfg.output_lfe {
                        g[e * self.cfg.outputs + o] = 0.
                    }
                }
            }
            if self.ring.len() == 3 {
                self.ring.remove(0);
            }
            self.ring.push(g)
        }
        let nb = self.inputs.len();
        let k = self.cfg.elements;
        let o = self.cfg.outputs;
        let bc = self.centers.len();
        let spec = |p: &[f32]| {
            let mut a: Vec<C> = p
                .iter()
                .zip(&self.window)
                .map(|(x, w)| C {
                    r: (x * w) as f64,
                    i: 0.,
                })
                .collect();
            fft(&mut a, false);
            a.truncate(BINS);
            a
        };
        let bed: Vec<_> = self.inputs.iter().map(|i| spec(&self.bh[*i])).collect();
        let lfe = self.cfg.bed_lfe.map(|i| spec(&self.bh[i]));
        let gains = self.ring.first().cloned();
        let active: Vec<bool> = (0..k)
            .map(|e| {
                gains
                    .as_ref()
                    .is_some_and(|g| g[e * o..(e + 1) * o].iter().any(|x| *x != 0.))
            })
            .collect();
        let elems: Vec<_> = (0..k)
            .map(|e| {
                if active[e] {
                    spec(&self.eh[e])
                } else {
                    vec![C::default(); BINS]
                }
            })
            .collect();
        for x in self.cov.iter_mut().chain(self.cross.iter_mut()) {
            *x = x.scale(0.5)
        }
        for f in 0..BINS {
            let b = self.bands[f];
            for i in 0..nb {
                for j in i..nb {
                    let p = b * nb * nb + i * nb + j;
                    self.cov[p] = self.cov[p].add(bed[i][f].mul(bed[j][f].conj()).scale(0.5))
                }
            }
            for e in 0..k {
                if active[e] {
                    for j in 0..nb {
                        let p = b * k * nb + e * nb + j;
                        self.cross[p] =
                            self.cross[p].add(elems[e][f].mul(bed[j][f].conj()).scale(0.5))
                    }
                }
            }
        }
        let mut weights = vec![C::default(); bc * o * nb];
        for band in 0..bc {
            let base = band * nb * nb;
            if gains.is_none() || !active.iter().any(|x| *x) {
                for q in 0..o * nb {
                    weights[band * o * nb + q].r = self.cfg.fallback[q] as f64
                }
                continue;
            }
            let trace: f64 = (0..nb).map(|i| self.cov[base + i * nb + i].r).sum();
            if trace <= 1e-12 {
                continue;
            }
            let mut l = vec![C::default(); nb * nb];
            for i in 0..nb {
                for j in 0..nb {
                    l[i * nb + j] = if j >= i {
                        self.cov[base + i * nb + j]
                    } else {
                        self.cov[base + j * nb + i].conj()
                    }
                }
                l[i * nb + i].r += 0.001 * trace / nb as f64 + 1e-15
            }
            let mut ok = true;
            for j in 0..nb {
                let mut d = l[j * nb + j].r;
                for q in 0..j {
                    let x = l[j * nb + q];
                    d -= x.r * x.r + x.i * x.i
                }
                if d <= 0. {
                    ok = false;
                    break;
                }
                let diag = d.sqrt();
                l[j * nb + j] = C { r: diag, i: 0. };
                for i in j + 1..nb {
                    let mut x = l[i * nb + j];
                    for q in 0..j {
                        x = x.sub(l[i * nb + q].mul(l[j * nb + q].conj()))
                    }
                    l[i * nb + j] = x.scale(1. / diag)
                }
            }
            if !ok {
                continue;
            }
            let mut a = vec![C::default(); k * nb];
            for e in 0..k {
                if !active[e] {
                    continue;
                }
                let mut y = vec![C::default(); nb];
                for i in 0..nb {
                    let mut x = self.cross[band * k * nb + e * nb + i].conj();
                    for q in 0..i {
                        x = x.sub(l[i * nb + q].mul(y[q]))
                    }
                    y[i] = x.scale(1. / l[i * nb + i].r)
                }
                for i in (0..nb).rev() {
                    let mut x = y[i];
                    for q in i + 1..nb {
                        x = x.sub(l[q * nb + i].conj().mul(a[e * nb + q].conj()))
                    }
                    a[e * nb + i] = x.scale(1. / l[i * nb + i].r).conj()
                }
            }
            let mut w = vec![C::default(); o * nb];
            for c in 0..o {
                for j in 0..nb {
                    if !self.cfg.folds.is_empty() {
                        w[c * nb + j].r = self.cfg.fallback[c * nb + j] as f64
                    }
                }
                if c == self.cfg.output_lfe {
                    continue;
                }
                for e in 0..k {
                    if active[e] {
                        let mut g = gains.as_ref().unwrap()[e * o + c] as f64;
                        if !self.cfg.folds.is_empty() {
                            g += self.fold[e * o + c] as f64
                        }
                        for j in 0..nb {
                            w[c * nb + j] = w[c * nb + j].add(a[e * nb + j].scale(g))
                        }
                    }
                }
            }
            let mut eo = 0.;
            for c in 0..o {
                if c == self.cfg.output_lfe {
                    continue;
                }
                for i in 0..nb {
                    let mut t = C::default();
                    for j in 0..nb {
                        let cv = if j <= i {
                            self.cov[base + j * nb + i]
                        } else {
                            self.cov[base + i * nb + j].conj()
                        };
                        t = t.add(w[c * nb + j].mul(cv))
                    }
                    eo += t.mul(w[c * nb + i].conj()).r
                }
            }
            let bounds = if self.cfg.folds.is_empty() {
                (0.9, 1.6)
            } else {
                (0.5, 2.5)
            };
            let r = eo / trace;
            let scale = if eo > 1e-15 && r > bounds.1 {
                (bounds.1 / r).sqrt()
            } else if eo > 1e-15 && r < bounds.0 {
                (bounds.0 / r).sqrt()
            } else {
                1.
            };
            for q in 0..o * nb {
                let x = w[q].scale(scale);
                weights[band * o * nb + q] = C {
                    r: (x.r as f32) as f64,
                    i: (x.i as f32) as f64,
                }
            }
        }
        let mut lower = 0;
        let mut spectra = vec![vec![C::default(); N]; o];
        for f in 0..BINS {
            while lower + 1 < bc && self.centers[lower + 1] <= f as f32 {
                lower += 1
            }
            let upper = (lower + 1).min(bc - 1);
            let t = if upper != lower && (f as f32) > self.centers[lower] {
                ((f as f32 - self.centers[lower]) / (self.centers[upper] - self.centers[lower]))
                    .min(1.)
            } else {
                0.
            };
            for c in 0..o {
                if c == self.cfg.output_lfe {
                    if let Some(l) = &lfe {
                        spectra[c][f] = l[f]
                    }
                    continue;
                }
                let mut sum = C::default();
                for j in 0..nb {
                    let a = weights[lower * o * nb + c * nb + j];
                    let b = weights[upper * o * nb + c * nb + j];
                    sum = sum.add(a.add(b.sub(a).scale(t as f64)).mul(bed[j][f]))
                }
                spectra[c][f] = sum
            }
        }
        for c in 0..o {
            for f in 1..N / 2 {
                spectra[c][N - f] = spectra[c][f].conj()
            }
            fft(&mut spectra[c], true);
            for i in 0..N {
                self.accum[c][i] += (spectra[c][i].r as f32) * self.window[i] * 0.5
            }
            self.ready[c].copy_from_slice(&self.accum[c][..H]);
            self.accum[c].copy_within(H.., 0);
            self.accum[c][N - H..].fill(0.)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn config() -> Config {
        Config {
            bed_channels: 2,
            bed_lfe: Some(1),
            outputs: 2,
            output_lfe: 1,
            fallback: vec![1., 0.],
            elements: 1,
            folds: vec![],
        }
    }
    #[test]
    fn streaming_partition_reset_and_validation() {
        let n = 4097;
        let x: Vec<f32> = (0..n).map(|i| ((i as f32) * 0.04).sin() * 0.2).collect();
        let l: Vec<f32> = (0..n).map(|i| ((i as f32) * 0.01).cos() * 0.1).collect();
        let mut a = BedRegression::new(config()).unwrap();
        let mut b = BedRegression::new(config()).unwrap();
        let one = a.process(&[&x, &l], &[&x]).unwrap();
        assert_eq!(one[0].len(), n - N);
        let mut chunks = vec![vec![], vec![]];
        let mut p = 0;
        while p < n {
            let end = (p + 137).min(n);
            let got = b.process(&[&x[p..end], &l[p..end]], &[&x[p..end]]).unwrap();
            for c in 0..2 {
                chunks[c].extend_from_slice(&got[c])
            }
            p = end
        }
        assert_eq!(one, chunks);
        let before = b.backlog();
        assert_eq!(
            b.process(&[&[f32::NAN], &[0.]], &[&[0.]]),
            Err(Error::InvalidInput)
        );
        assert_eq!(b.backlog(), before);
        let taila = a.drain().unwrap();
        let tailb = b.drain().unwrap();
        assert_eq!(taila, tailb);
        assert_eq!(taila[0].len(), N);
        assert_eq!(a.backlog(), 0);
        for (i, v) in one[0].iter().chain(taila[0].iter()).enumerate().skip(4 * H) {
            assert!((v - x[i]).abs() < 2e-5)
        }
    }
    #[test]
    fn rejects_bad_config_and_snapshots() {
        let mut c = config();
        c.folds.push(Fold {
            element: 1,
            bed_plane: 0,
            gain: 1.,
        });
        assert!(matches!(BedRegression::new(c), Err(Error::InvalidConfig)));
        let mut excessive = config();
        excessive.folds = vec![
            Fold {
                element: 0,
                bed_plane: 0,
                gain: 0.5
            };
            1025
        ];
        assert!(matches!(
            BedRegression::new(excessive),
            Err(Error::InvalidConfig)
        ));
        let mut r = BedRegression::new(config()).unwrap();
        assert_eq!(
            r.set_gains(Snapshot {
                placed: vec![true],
                gains: vec![f32::NAN, 0.]
            }),
            Err(Error::InvalidSnapshot)
        );
    }
}
