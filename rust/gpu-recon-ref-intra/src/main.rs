//! Reference vectors for the WebGPU intra prediction stage (`web/gpu-recon/intra.*`, layout in
//! `web/gpu-recon/FORMAT-intra.md`).
//!
//! Builds one picture: a random CTU / CU / TU tree (tiles, slices, intra and inter CUs, NxN, the 4:2:0 deferred
//! 4x4 chroma TU), random residuals, and reconstructs every intra TU in decoding order with the rust_h265 code
//! (verbatim copies in `intra_pred.rs` and `avail.rs`). Inter CUs stand for blocks whose MC + residual is already
//! in the picture (wave 0): their samples are the initial picture content.
//!
//! Writes `<out>/<name>.{json,init.bin,tus.bin,res.bin,imap.bin,expect.bin}`.
//!
//! Usage: gpu-recon-ref-intra --out DIR --name N [--w 1920] [--h 1080] [--bd 10] [--ctb 6] [--seed 1]
//!        [--intra 1.0] [--cip] [--slices 1] [--tiles 1x1] [--no-strong] [--smooth-off] [--rdpcm]
//!        [--detail 0.5] [--cbf 0.7] [--smooth 0.5] [--tus FILE]
//!
//! `--tus FILE` replaces the random tree by the intra TUs of a real picture: 4-word records in decoding order as in
//! FORMAT-intra.md (word 0 position, word 1 log2 / plane / mode / availability bits; words 2 and 3 ignored), for
//! example dumped from an instrumented rust_h265. Position, size, mode and availability come from the file; the
//! residuals and the initial picture stay random. Width and height must be given and match.

mod avail;
mod intra_pred;
mod intra_simd;
mod pixel;
mod simd;

use avail::{MvField, PictureState, cip_masks, compute_luma_avail_inner};
use intra_pred::{
    ReferenceAvailability, add_residual, build_reference_samples, filter_reference_samples, predict_angular,
    predict_dc, predict_planar,
};
use std::io::Write;
use std::marker::PhantomData;

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u32 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (self.0 >> 33) as u32
    }
    fn below(&mut self, n: u32) -> u32 {
        self.next() % n
    }
    fn chance(&mut self, p: f64) -> bool {
        (self.next() as f64) < p * (1u64 << 31) as f64
    }
}

struct Opts {
    out: String,
    name: String,
    w: u32,
    h: u32,
    bd: u8,
    log2_ctb: u8,
    seed: u64,
    intra: f64,
    cip: bool,
    slices: u32,
    tiles: (u32, u32),
    strong: bool,
    smooth_off: bool,
    rdpcm: bool,
    detail: f64,
    cbf: f64,
    smooth: f64,
    tus: Option<String>,
}

fn parse() -> Opts {
    let mut o = Opts {
        out: ".".into(),
        name: "intra".into(),
        w: 1920,
        h: 1080,
        bd: 10,
        log2_ctb: 6,
        seed: 1,
        intra: 1.0,
        cip: false,
        slices: 1,
        tiles: (1, 1),
        strong: true,
        smooth_off: false,
        rdpcm: false,
        detail: 0.5,
        cbf: 0.7,
        smooth: 0.5,
        tus: None,
    };
    let a: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i < a.len() {
        let v = a.get(i + 1).cloned().unwrap_or_default();
        let mut used = true;
        match a[i].as_str() {
            "--out" => o.out = v,
            "--name" => o.name = v,
            "--w" => o.w = v.parse().unwrap(),
            "--h" => o.h = v.parse().unwrap(),
            "--bd" => o.bd = v.parse().unwrap(),
            "--ctb" => o.log2_ctb = v.parse().unwrap(),
            "--seed" => o.seed = v.parse().unwrap(),
            "--intra" => o.intra = v.parse().unwrap(),
            "--slices" => o.slices = v.parse().unwrap(),
            "--detail" => o.detail = v.parse().unwrap(),
            "--cbf" => o.cbf = v.parse().unwrap(),
            "--smooth" => o.smooth = v.parse().unwrap(),
            "--tus" => o.tus = Some(v),
            "--tiles" => {
                let (c, r) = v.split_once('x').unwrap();
                o.tiles = (c.parse().unwrap(), r.parse().unwrap());
            }
            "--cip" => {
                o.cip = true;
                used = false;
            }
            "--no-strong" => {
                o.strong = false;
                used = false;
            }
            "--smooth-off" => {
                o.smooth_off = true;
                used = false;
            }
            "--rdpcm" => {
                o.rdpcm = true;
                used = false;
            }
            f => panic!("unknown flag {f}"),
        }
        i += if used { 2 } else { 1 };
    }
    assert!(o.w % 8 == 0 && o.h % 8 == 0, "picture size must be a multiple of the minimum CB (8)");
    o
}

fn align64(v: u32) -> u32 {
    (v + 63) & !63
}

/// One plane in the GPU layout's strides (`strideY = align64(w)`, `strideC = align64(w / 2)`).
struct Plane {
    w: usize,
    h: usize,
    stride: usize,
    s: Vec<u16>,
}

/// TU record words (FORMAT-intra.md).
#[derive(Clone, Copy)]
struct Tu {
    w: [u32; 4],
}

struct Gen {
    o: Opts,
    rng: Rng,
    st: PictureState<u16>,
    planes: [Plane; 3],
    tab_ipm: Vec<u8>,
    tus: Vec<Tu>,
    res: Vec<i32>,
    /// Global wave per 4x4 block of each plane (0 = inter or not yet decoded intra).
    wave: [Vec<u32>; 3],
    stats: Stats,
}

#[derive(Default)]
struct Stats {
    by_size: [[u64; 4]; 3],
    by_mode: Vec<u64>,
    strong: u64,
    filtered: u64,
    no_res: u64,
    intra_samples: u64,
    max_wave: u32,
    intra_cus: u64,
    inter_cus: u64,
    edge_off: u64,
}

const INTRA_PLANAR: u8 = 0;
const INTRA_DC: u8 = 1;
const INTRA_ANGULAR_10: u8 = 10;
const INTRA_ANGULAR_26: u8 = 26;
const INTRA_ANGULAR_34: u8 = 34;

impl Gen {
    fn fill_initial(&mut self) {
        // Mixture of flat, smooth gradient and noisy regions: smooth reference rows make the strong 32x32
        // smoothing and the DC / planar paths meaningful, noise exercises clipping and substitution.
        let maxv = (1i32 << self.o.bd) - 1;
        for c in 0..3 {
            let (w, h, stride) = (self.planes[c].w, self.planes[c].h, self.planes[c].stride);
            let blk = if c == 0 { 64 } else { 32 };
            for by in (0..h).step_by(blk) {
                for bx in (0..w).step_by(blk) {
                    let kind = if self.rng.chance(self.o.smooth) { self.rng.below(2) } else { 2 };
                    let base = self.rng.below(maxv as u32 + 1) as i32;
                    let gx = self.rng.below(9) as i32 - 4;
                    let gy = self.rng.below(9) as i32 - 4;
                    for y in by..(by + blk).min(h) {
                        for x in bx..(bx + blk).min(w) {
                            let v = match kind {
                                0 => base,
                                1 => base + ((gx * (x - bx) as i32 + gy * (y - by) as i32) >> 2),
                                _ => self.rng.below(maxv as u32 + 1) as i32,
                            };
                            self.planes[c].s[y * stride + x] = v.clamp(0, maxv) as u16;
                        }
                    }
                }
            }
        }
    }

    fn set_mvf(&mut self, x0: u32, y0: u32, size: u32, pred_flag: u8) {
        let n = (size >> 2) as usize;
        for j in 0..n {
            for i in 0..n {
                let idx = ((y0 >> 2) as usize + j) * self.st.min_pu_width + (x0 >> 2) as usize + i;
                self.st.tab_mvf[idx] = MvField { pred_flag };
            }
        }
    }

    fn coding_quadtree(&mut self, x0: u32, y0: u32, log2: u8) {
        let size = 1u32 << log2;
        let inside = x0 + size <= self.o.w && y0 + size <= self.o.h;
        let split = if log2 == 3 {
            false
        } else if !inside {
            true
        } else {
            // Deeper trees for higher `detail`; 64 -> 32 splits almost always in real 4K intra content.
            let p = match log2 {
                6 => 0.6 + 0.4 * self.o.detail,
                5 => 0.2 + 0.6 * self.o.detail,
                _ => 0.1 + 0.6 * self.o.detail,
            };
            self.rng.chance(p)
        };
        if split {
            let h = size / 2;
            for (dx, dy) in [(0, 0), (h, 0), (0, h), (h, h)] {
                if x0 + dx < self.o.w && y0 + dy < self.o.h {
                    self.coding_quadtree(x0 + dx, y0 + dy, log2 - 1);
                }
            }
            return;
        }
        self.coding_unit(x0, y0, log2);
    }

    fn coding_unit(&mut self, x0: u32, y0: u32, log2: u8) {
        let size = 1u32 << log2;
        let intra = self.o.intra >= 1.0 || self.rng.chance(self.o.intra);
        if !intra {
            self.stats.inter_cus += 1;
            self.set_mvf(x0, y0, size, 1);
            return;
        }
        self.stats.intra_cus += 1;
        self.set_mvf(x0, y0, size, 0);
        let edge_off = self.o.rdpcm && self.rng.chance(0.2);
        let nxn = log2 == 3 && self.rng.chance(0.5);
        let pb = if nxn { size / 2 } else { size };
        let side = if nxn { 2 } else { 1 };
        let mut luma = [0u8; 4];
        for k in 0..side * side {
            // Planar and DC are the most frequent modes in real content; keep every angular mode covered.
            let m = match self.rng.below(4) {
                0 => INTRA_PLANAR,
                1 => INTRA_DC,
                _ => self.rng.below(35) as u8,
            };
            luma[k as usize] = m;
            let (px, py) = (x0 + pb * (k % side), y0 + pb * (k / side));
            let n = (pb >> 2) as usize;
            for j in 0..n {
                for i in 0..n {
                    let idx = ((py >> 2) as usize + j) * self.st.min_pu_width + (px >> 2) as usize + i;
                    self.tab_ipm[idx] = m;
                }
            }
        }
        // Spec table 8-3 / rust_h265 decode_intra_mode_signaling (4:2:0: one chroma mode from PU 0).
        let idx = self.rng.below(5);
        let chroma = if idx == 4 {
            luma[0]
        } else {
            const TABLE: [u8; 4] = [INTRA_PLANAR, INTRA_ANGULAR_26, INTRA_ANGULAR_10, INTRA_DC];
            let mapped = TABLE[idx as usize];
            if luma[0] == mapped { INTRA_ANGULAR_34 } else { mapped }
        };
        self.transform_tree(x0, y0, x0, y0, log2, 0, 0, nxn, chroma, edge_off);
    }

    #[allow(clippy::too_many_arguments)]
    fn transform_tree(
        &mut self,
        x0: u32,
        y0: u32,
        xb: u32,
        yb: u32,
        log2: u8,
        depth: u32,
        blk: u32,
        nxn: bool,
        chroma: u8,
        edge_off: bool,
    ) {
        let split = log2 > 5 || (nxn && depth == 0) || (log2 > 2 && self.rng.chance(0.15 + 0.5 * self.o.detail));
        if split {
            let h = 1u32 << (log2 - 1);
            for (k, (dx, dy)) in [(0, 0), (h, 0), (0, h), (h, h)].into_iter().enumerate() {
                self.transform_tree(x0 + dx, y0 + dy, x0, y0, log2 - 1, depth + 1, k as u32, nxn, chroma, edge_off);
            }
            return;
        }
        let mode = self.tab_ipm[(y0 as usize >> 2) * self.st.min_pu_width + (x0 as usize >> 2)];
        self.intra_tu(0, x0, y0, x0, y0, log2, mode, edge_off);
        if log2 > 2 {
            for c in 1..3 {
                self.intra_tu(c, x0 / 2, y0 / 2, x0, y0, log2 - 1, chroma, edge_off);
            }
        } else if blk == 3 {
            for c in 1..3 {
                self.intra_tu(c, xb / 2, yb / 2, xb, yb, 2, chroma, edge_off);
            }
        }
    }

    /// One intra TU, mirroring rust_h265 `predict_intra_luma` / `predict_intra_chroma` + `add_residual`
    /// (cu_tree.rs).
    #[allow(clippy::too_many_arguments)]
    fn intra_tu(&mut self, c: usize, x: u32, y: u32, xl: u32, yl: u32, log2: u8, mode: u8, edge_off: bool) {
        let luma_size = if c == 0 { 1u32 << log2 } else { 2u32 << log2 };
        let avail = compute_luma_avail_inner(&self.st, xl, yl, luma_size, self.o.cip);
        self.intra_tu_avail(c, x, y, log2, mode, edge_off, avail);
    }

    #[allow(clippy::too_many_arguments)]
    fn intra_tu_avail(&mut self, c: usize, x: u32, y: u32, log2: u8, mode: u8, edge_off: bool, avail: ReferenceAvailability) {
        let size = 1usize << log2;
        let bd = self.o.bd;
        let shift = if c == 0 { 0 } else { 1 };
        let cip = self.o.cip.then(|| cip_masks(&self.st, x as usize, y as usize, size, (shift, shift)));
        let (pw, ph, stride) = (self.planes[c].w, self.planes[c].h, self.planes[c].stride);
        let (mut top, mut left) = build_reference_samples(
            &self.planes[c].s[..],
            stride,
            pw,
            ph,
            x as usize,
            y as usize,
            log2,
            bd,
            avail,
            cip.as_ref().map(|(t, l)| (t.as_slice(), l.as_slice())),
        );
        let (t0, l0) = (top, left);
        if mode != 1 && !self.o.smooth_off {
            filter_reference_samples(&mut top, &mut left, log2, mode, self.o.strong, c as u8, 1, bd);
        }
        if top[..2 * size + 1] != t0[..2 * size + 1] || left[..2 * size + 1] != l0[..2 * size + 1] {
            self.stats.filtered += 1;
            // Strong smoothing leaves both far ends and makes the rows linear; detect by its precondition.
            if self.o.strong && c == 0 && log2 == 5 && !self.o.smooth_off {
                let thr = 1i32 << (bd - 5);
                let ts = (t0[0] as i32 + t0[2 * size] as i32 - 2 * t0[size] as i32).abs() < thr;
                let ls = (l0[0] as i32 + l0[2 * size] as i32 - 2 * l0[size] as i32).abs() < thr;
                let min_dist = ((mode as i32) - 26).abs().min(((mode as i32) - 10).abs());
                if ts && ls && min_dist > 0 {
                    self.stats.strong += 1;
                }
            }
        }
        let mut blk = vec![0u16; size * size];
        let edge = !edge_off;
        match mode {
            0 => predict_planar(&mut blk, size, &top, &left, log2, bd),
            1 => predict_dc(&mut blk, size, &top, &left, log2, c == 0 && edge, bd),
            _ => predict_angular(&mut blk, size, &top, &left, log2, mode, if c == 0 && edge { 0 } else { 1 }, bd),
        }
        let has_res = self.rng.chance(self.o.cbf);
        let res_off = if has_res {
            let off = self.res.len() as u32;
            let mut r = vec![0i16; size * size];
            let big = self.rng.chance(0.15);
            let amp = if self.rng.chance(0.5) { 8 } else { 96 };
            for v in r.iter_mut() {
                *v = if big && self.rng.chance(0.3) {
                    // Beyond the sample range in both directions: exercises the clip.
                    let m = (1i32 << bd) + 64;
                    (self.rng.below(2 * m as u32) as i32 - m) as i16
                } else {
                    (self.rng.below(2 * amp + 1) as i32 - amp as i32) as i16
                };
            }
            add_residual(&mut blk, size, &r, log2, bd);
            self.res.extend(r.iter().map(|&v| v as i32));
            off
        } else {
            self.stats.no_res += 1;
            u32::MAX
        };
        for yy in 0..size {
            let row = (y as usize + yy) * stride + x as usize;
            self.planes[c].s[row..row + size].copy_from_slice(&blk[yy * size..yy * size + size]);
        }

        // Global wave: 1 + the largest wave among the intra TUs whose samples this TU reads.
        let wv = self.read_wave(c, x as usize, y as usize, size, avail) + 1;
        let gw = pw.div_ceil(4);
        for by in (y as usize >> 2)..((y as usize + size) >> 2) {
            for bx in (x as usize >> 2)..((x as usize + size) >> 2) {
                self.wave[c][by * gw + bx] = wv;
            }
        }
        self.stats.max_wave = self.stats.max_wave.max(wv);
        self.stats.by_size[c][log2 as usize - 2] += 1;
        self.stats.by_mode[mode as usize] += 1;
        self.stats.intra_samples += (size * size) as u64;
        if edge_off {
            self.stats.edge_off += 1;
        }
        let av = (avail.bottom_left as u32)
            | (avail.left as u32) << 1
            | (avail.up_left as u32) << 2
            | (avail.up as u32) << 3
            | (avail.up_right as u32) << 4;
        let w1 = log2 as u32
            | (c as u32) << 3
            | (mode as u32) << 5
            | av << 11
            | (edge_off as u32) << 16
            | (self.o.cip as u32) << 17;
        self.tus.push(Tu { w: [x | y << 16, w1, res_off, wv] });
    }

    /// Largest wave among the 4x4 blocks covering the reference samples of a TU (the regions
    /// `build_reference_samples` reads under `avail`).
    fn read_wave(&self, c: usize, x0: usize, y0: usize, size: usize, a: ReferenceAvailability) -> u32 {
        let (pw, ph) = (self.planes[c].w, self.planes[c].h);
        let gw = pw.div_ceil(4);
        let g = &self.wave[c];
        let at = |x: usize, y: usize| g[(y >> 2) * gw + (x >> 2)];
        let mut m = 0;
        if x0 > 0 {
            let mut rows = 0;
            if a.left {
                rows = size;
            }
            if a.bottom_left {
                rows = size + ph.saturating_sub(y0 + size).min(size);
            }
            let first = if a.left { 0 } else { size };
            for r in (first..rows).step_by(4) {
                m = m.max(at(x0 - 1, y0 + r));
            }
            if a.up_left && y0 > 0 {
                m = m.max(at(x0 - 1, y0 - 1));
            }
        }
        if y0 > 0 {
            if a.up {
                for cc in (0..size).step_by(4) {
                    m = m.max(at(x0 + cc, y0 - 1));
                }
            }
            if a.up_right {
                let n = pw.saturating_sub(x0 + size).min(size);
                for cc in (0..n).step_by(4) {
                    m = m.max(at(x0 + size + cc, y0 - 1));
                }
            }
        }
        m
    }
}

fn main() {
    let o = parse();
    let (w, h) = (o.w as usize, o.h as usize);
    let ctb = 1u32 << o.log2_ctb;
    let (wc, hc) = (o.w.div_ceil(ctb), o.h.div_ceil(ctb));
    let min_pu_width = w / 4;
    let st = PictureState {
        width: o.w,
        height: o.h,
        log2_ctb_size: o.log2_ctb,
        log2_min_pu_size: 2,
        min_pu_width,
        tab_slice_addr_rs: vec![-1; (wc * hc) as usize],
        tab_tile_id: vec![0; (wc * hc) as usize],
        tab_mvf: vec![MvField { pred_flag: 1 }; min_pu_width * (h / 4)],
        _p: PhantomData,
    };
    let mk = |pw: usize, ph: usize, stride: usize| Plane { w: pw, h: ph, stride, s: vec![0; stride * ph] };
    let (sy, sc) = (align64(o.w) as usize, align64(o.w / 2) as usize);
    let planes = [mk(w, h, sy), mk(w / 2, h / 2, sc), mk(w / 2, h / 2, sc)];
    let wave = [
        vec![0u32; w.div_ceil(4) * h.div_ceil(4)],
        vec![0u32; (w / 2).div_ceil(4) * (h / 2).div_ceil(4)],
        vec![0u32; (w / 2).div_ceil(4) * (h / 2).div_ceil(4)],
    ];
    let mut g = Gen {
        rng: Rng(o.seed.wrapping_mul(0x9E3779B97F4A7C15) | 1),
        o,
        st,
        planes,
        tab_ipm: vec![INTRA_DC; min_pu_width * (h / 4)],
        tus: Vec::new(),
        res: Vec::new(),
        wave,
        stats: Stats { by_mode: vec![0; 35], ..Stats::default() },
    };
    g.fill_initial();
    let init: Vec<u32> = pack(&g.planes);

    // Tile scan: uniform tile grid, raster CTUs inside each tile (spec 6.5.1).
    let (tcols, trows) = g.o.tiles;
    let col_bd: Vec<u32> = (0..=tcols).map(|i| i * wc / tcols).collect();
    let row_bd: Vec<u32> = (0..=trows).map(|i| i * hc / trows).collect();
    let mut order = Vec::new();
    for tr in 0..trows as usize {
        for tc in 0..tcols as usize {
            for cy in row_bd[tr]..row_bd[tr + 1] {
                for cx in col_bd[tc]..col_bd[tc + 1] {
                    order.push((cy * wc + cx, (tr * tcols as usize + tc) as u32));
                }
            }
        }
    }
    // Slices: contiguous runs of the decoding order with random cut points.
    let n = order.len();
    let mut cuts: Vec<usize> = (1..g.o.slices).map(|_| 1 + g.rng.below(n.max(2) as u32 - 1) as usize).collect();
    cuts.sort();
    if let Some(path) = g.o.tus.clone() {
        let raw = std::fs::read(&path).unwrap();
        let w: Vec<u32> = raw.chunks(4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]])).collect();
        // Intra map: every 4x4 luma block a luma TU covers (the rest counts as inter).
        for r in w.chunks(4) {
            let (x, y, info) = (r[0] & 0xffff, r[0] >> 16, r[1]);
            if (info >> 3) & 3 == 0 {
                g.set_mvf(x, y, 1 << (info & 7), 0);
            }
        }
        for r in w.chunks(4) {
            let (x, y, info) = (r[0] & 0xffff, r[0] >> 16, r[1]);
            let av = (info >> 11) & 31;
            let avail = ReferenceAvailability {
                bottom_left: av & 1 != 0,
                left: av & 2 != 0,
                up_left: av & 4 != 0,
                up: av & 8 != 0,
                up_right: av & 16 != 0,
            };
            g.intra_tu_avail(((info >> 3) & 3) as usize, x, y, (info & 7) as u8, ((info >> 5) & 63) as u8, false, avail);
        }
        order.clear();
    }
    let mut slice_addr = order.first().map_or(0, |o| o.0 as i32);
    for (i, &(rs, tile)) in order.iter().enumerate() {
        if cuts.contains(&i) {
            slice_addr = rs as i32;
        }
        g.st.tab_slice_addr_rs[rs as usize] = slice_addr;
        g.st.tab_tile_id[rs as usize] = tile;
        let (cx, cy) = (rs % wc, rs / wc);
        g.coding_quadtree(cx * ctb, cy * ctb, g.o.log2_ctb);
    }

    let expect = pack(&g.planes);
    let mut imap = vec![0u32; (min_pu_width * (h / 4)).div_ceil(32)];
    for (i, m) in g.st.tab_mvf.iter().enumerate() {
        if m.pred_flag == 0 {
            imap[i >> 5] |= 1 << (i & 31);
        }
    }
    let tus: Vec<u32> = g.tus.iter().flat_map(|t| t.w).collect();
    let out = format!("{}/{}", g.o.out, g.o.name);
    write_u32(&format!("{out}.init.bin"), &init);
    write_u32(&format!("{out}.expect.bin"), &expect);
    write_u32(&format!("{out}.tus.bin"), &tus);
    write_u32(&format!("{out}.imap.bin"), &imap);
    let res: Vec<u32> = g.res.iter().map(|&v| v as u32).collect();
    write_u32(&format!("{out}.res.bin"), &res);
    let s = &g.stats;
    let json = format!(
        concat!(
            "{{\"width\":{},\"height\":{},\"bitDepth\":{},\"log2Ctb\":{},\"cip\":{},\"strongIntraSmoothing\":{},",
            "\"intraSmoothingDisabled\":{},\"slices\":{},\"tiles\":[{},{}],\"seed\":{},\"intraFraction\":{},",
            "\"tuCount\":{},\"residualWords\":{},\"intraSamples\":{},\"maxWave\":{},\"intraCus\":{},\"interCus\":{},",
            "\"bySize\":{:?},\"byMode\":{:?},\"filtered\":{},\"strong\":{},\"noResidual\":{},\"edgeFiltersOff\":{},",
            "\"imapStride\":{}}}\n"
        ),
        g.o.w,
        g.o.h,
        g.o.bd,
        g.o.log2_ctb,
        g.o.cip,
        g.o.strong,
        g.o.smooth_off,
        g.o.slices,
        tcols,
        trows,
        g.o.seed,
        g.o.intra,
        g.tus.len(),
        g.res.len(),
        s.intra_samples,
        s.max_wave,
        s.intra_cus,
        s.inter_cus,
        s.by_size,
        s.by_mode,
        s.filtered,
        s.strong,
        s.no_res,
        s.edge_off,
        min_pu_width,
    );
    std::fs::write(format!("{out}.json"), &json).unwrap();
    print!("{json}");
}

/// GPU picture layout (web/gpu-recon/README.md): Y, Cb, Cr back to back, two 16-bit samples per u32 word,
/// the sample at even x in the low half.
fn pack(p: &[Plane; 3]) -> Vec<u32> {
    let mut v = Vec::new();
    for pl in p {
        for pair in pl.s.chunks(2) {
            v.push(pair[0] as u32 | (pair[1] as u32) << 16);
        }
    }
    v
}

fn write_u32(path: &str, d: &[u32]) {
    let mut f = std::io::BufWriter::new(std::fs::File::create(path).unwrap());
    for &x in d {
        f.write_all(&x.to_le_bytes()).unwrap();
    }
}
