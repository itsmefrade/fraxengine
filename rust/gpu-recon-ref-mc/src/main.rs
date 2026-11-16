//! Reference vectors for the WebGPU motion compensation kernel (web/gpu-recon/mc.wgsl.mjs).
//!
//! The expected pictures come from rust_h265's own `inter_pred.rs`, `mc_simd.rs`, `simd.rs` and
//! `pixel.rs`, compiled here unmodified through `#[path]` (read only, never edited). Only the decoder
//! types they touch (`PictureState`, `DecodedPicture`, `PredWeightTable`, `SharedTable`, `with_block`)
//! are stubbed below with the fields MC reads; `with_block` is copied verbatim from cu_tree.rs:771.
//! Every CU goes through the real `motion_compensation_cu` / `motion_compensation_pu`.
//!
//! Build with `--features scalar` for the portable scalar MC (the vendored crate's `scalar` feature),
//! which differs from the simd128 / NEON build only where a 2-D luma intermediate leaves i16 (see
//! FORMAT-mc.md, "Narrowing").
//!
//! Usage: gpu-recon-ref-mc <out-dir>   writes mc-<case>.bin files and prints their statistics.
#![allow(dead_code, unused_imports, unexpected_cfgs, clippy::all)]

#[path = "../../aether-hevc/vendor/rust_h265/src/inter_pred.rs"]
mod inter_pred;
#[path = "../../aether-hevc/vendor/rust_h265/src/mc_simd.rs"]
mod mc_simd;
#[path = "../../aether-hevc/vendor/rust_h265/src/pixel.rs"]
pub(crate) mod pixel;
#[path = "../../aether-hevc/vendor/rust_h265/src/simd.rs"]
pub(crate) mod simd;

mod shared {
    // The real SharedTable is a Vec with interior sharing for wavefront threads; MC only indexes it.
    pub type SharedTable<T> = Vec<T>;
}

mod slice {
    pub const MAX_REFS: usize = 16;
    // Field for field the struct of slice.rs:79.
    #[derive(Clone, Debug, Default)]
    pub struct PredWeightTable {
        pub luma_log2_weight_denom: u8,
        pub chroma_log2_weight_denom: u8,
        pub luma_weight_l0: [i16; MAX_REFS],
        pub luma_offset_l0: [i16; MAX_REFS],
        pub chroma_weight_l0: [[i16; 2]; MAX_REFS],
        pub chroma_offset_l0: [[i16; 2]; MAX_REFS],
        pub luma_weight_l1: [i16; MAX_REFS],
        pub luma_offset_l1: [i16; MAX_REFS],
        pub chroma_weight_l1: [[i16; 2]; MAX_REFS],
        pub chroma_offset_l1: [[i16; 2]; MAX_REFS],
        pub high_precision_offsets: bool,
        pub all_default: bool,
    }
}

mod dpb {
    use crate::shared::SharedTable;
    pub enum PicturePlanes {
        U8([SharedTable<u8>; 3]),
        U16([SharedTable<u16>; 3]),
    }
    pub struct Progress;
    impl Progress {
        // Every reference is complete here.
        pub fn wait_final_rows(&self, _rows: usize) {}
    }
    pub struct DecodedPicture {
        pub planes: PicturePlanes,
        pub progress: Progress,
        pub width: u32,
        pub height: u32,
        pub y_stride: usize,
        pub uv_stride: usize,
        pub chroma_shift_w: u32,
        pub chroma_shift_h: u32,
    }
}

mod cu_tree {
    use crate::pixel::Pixel;
    use crate::shared::SharedTable;

    #[derive(Clone, Copy, Default, Debug)]
    pub struct Mv {
        pub x: i16,
        pub y: i16,
    }
    #[derive(Clone, Copy, Default, Debug)]
    pub struct MvField {
        pub mv: [Mv; 2],
        pub ref_idx: [i8; 2],
        pub pred_flag: u8,
    }
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum PartMode {
        Part2Nx2N,
        PartNxN,
        Part2NxN,
        PartNx2N,
        Part2NxnU,
        Part2NxnD,
        PartnLx2N,
        PartnRx2N,
    }
    pub struct PictureState<P: Pixel> {
        pub tab_mvf: Vec<MvField>,
        pub log2_min_pu_size: u8,
        pub min_pu_width: usize,
        pub y_stride: usize,
        pub uv_stride: usize,
        pub chroma_shift_w: u32,
        pub chroma_shift_h: u32,
        pub bit_depth: u8,
        pub bit_depth_c: u8,
        pub y_plane: SharedTable<P>,
        pub u_plane: SharedTable<P>,
        pub v_plane: SharedTable<P>,
        pub row_exact: bool,
        pub block_buf: Vec<P>,
        pub mc_scratch: crate::inter_pred::McScratch<P>,
    }

    // Verbatim from rust_h265 cu_tree.rs:771.
    pub(crate) fn with_block<P: Pixel, R>(
        plane: &mut SharedTable<P>,
        buf: &mut Vec<P>,
        row_exact: bool,
        off: usize,
        stride: usize,
        (w, h): (usize, usize),
        f: impl FnOnce(&mut [P], usize) -> R,
    ) -> R {
        if h == 0 || w == 0 {
            return f(&mut [], stride);
        }
        if !row_exact {
            return f(&mut plane[off..off + (h - 1) * stride + w], stride);
        }
        buf.clear();
        for j in 0..h {
            buf.extend_from_slice(&plane[off + j * stride..off + j * stride + w]);
        }
        let r = f(&mut buf[..], w);
        for j in 0..h {
            plane[off + j * stride..off + j * stride + w].copy_from_slice(&buf[j * w..(j + 1) * w]);
        }
        r
    }
}

use cu_tree::{Mv, MvField, PartMode, PictureState};
use dpb::{DecodedPicture, PicturePlanes, Progress};
use pixel::Pixel;
use slice::PredWeightTable;
use std::sync::Arc;

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u32 {
        // splitmix64
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        ((z ^ (z >> 31)) >> 32) as u32
    }
    /// Uniform in [lo, hi].
    fn range(&mut self, lo: i32, hi: i32) -> i32 {
        lo + (self.next() as u64 % (hi as i64 - lo as i64 + 1) as u64) as i32
    }
    fn pct(&mut self, p: u32) -> bool {
        self.next() % 100 < p
    }
}

fn align64(v: u32) -> u32 {
    (v + 63) & !63
}

/// The README picture layout.
#[derive(Clone, Copy)]
struct Layout {
    w: u32,
    h: u32,
    stride_y: u32,
    stride_c: u32,
    off_cb: u32,
    off_cr: u32,
    words: u32,
}
impl Layout {
    fn new(w: u32, h: u32) -> Self {
        let stride_y = align64(w);
        let stride_c = align64(w / 2);
        let off_cb = stride_y * h / 2;
        let off_cr = off_cb + stride_c * (h / 2) / 2;
        let words = off_cr + stride_c * (h / 2) / 2;
        Layout { w, h, stride_y, stride_c, off_cb, off_cr, words }
    }
}

/// Planes of one picture as sample vectors (strides of `Layout`).
struct Planes {
    p: [Vec<u16>; 3],
}

fn pack(l: &Layout, pl: &Planes, out: &mut [u32]) {
    let geo = [(0, l.stride_y, l.h), (l.off_cb, l.stride_c, l.h / 2), (l.off_cr, l.stride_c, l.h / 2)];
    for (c, &(off, stride, rows)) in geo.iter().enumerate() {
        for i in 0..(stride * rows) as usize / 2 {
            out[off as usize + i] = pl.p[c][2 * i] as u32 | (pl.p[c][2 * i + 1] as u32) << 16;
        }
    }
}

fn to_p<P: Pixel>(v: &[u16], bd: u8) -> Vec<P> {
    v.iter().map(|&s| P::from_i32_clamped(s as i32, bd)).collect()
}
fn from_p<P: Pixel>(v: &[P]) -> Vec<u16> {
    v.iter().map(|s| s.to_i32() as u16).collect()
}

#[derive(Clone, Copy)]
enum Fill {
    Uniform,
    Extremes,
    Adversarial,
}

/// Half-sample luma filter signs (-,+,-,+,+,-,+,-) along both axes: with max where the signs agree and 0
/// elsewhere, a half/half 2-D phase makes the second filter pass reach about 33,250 (> i16::MAX) for
/// every bit depth (FORMAT-mc.md, "Narrowing").
const ADV: [u8; 8] = [0, 1, 0, 1, 1, 0, 1, 0];

fn make_planes(l: &Layout, bd: [u8; 2], fill: Fill, rng: &mut Rng) -> Planes {
    let geo = [(l.stride_y, l.w, l.h), (l.stride_c, l.w / 2, l.h / 2), (l.stride_c, l.w / 2, l.h / 2)];
    let mut p: [Vec<u16>; 3] = Default::default();
    for c in 0..3 {
        let (stride, w, h) = geo[c];
        let max = (1i32 << bd[(c > 0) as usize]) - 1;
        let mut v = vec![0u16; (stride * h) as usize];
        for y in 0..h {
            for x in 0..stride {
                // Samples past the picture width must never be read: fill them with a marker that would
                // show up as a mismatch.
                let s = if x >= w {
                    (max as u32 ^ 0x5) as i32 & max
                } else {
                    match fill {
                        Fill::Uniform => rng.range(0, max),
                        Fill::Extremes => match rng.next() % 10 {
                            0..=1 => 0,
                            2..=3 => max,
                            _ => rng.range(0, max),
                        },
                        Fill::Adversarial => {
                            if ADV[(x & 7) as usize] == ADV[(y & 7) as usize] {
                                max
                            } else {
                                0
                            }
                        }
                    }
                };
                v[(y * stride + x) as usize] = s as u16;
            }
        }
        p[c] = v;
    }
    Planes { p }
}

struct Case {
    name: &'static str,
    w: u32,
    h: u32,
    bd_y: u8,
    bd_c: u8,
    high_precision: bool,
    seed: u64,
}

/// Reference lists as DPB slots. Slot 3 is the adversarial picture; L0 index 3 is slot 0 again with
/// other weights (two ref indices on one picture is a common weighted-prediction trick).
const L0_SLOTS: [u32; 4] = [0, 1, 3, 0];
const L1_SLOTS: [u32; 3] = [2, 1, 3];
const CUR_SLOT: u32 = 4;
const NUM_SLOTS: u32 = 5;
const WT_PER_LIST: u32 = 16;

#[derive(Default)]
struct Stats {
    pus: u64,
    holes: u64,
    bi: u64,
    uni: u64,
    weighted: u64,
    adversarial: u64,
    samples: u64,
    sizes: std::collections::BTreeMap<(u32, u32), u64>,
    luma_phases: [u64; 16],
    chroma_phases: Vec<u64>,
    outside: u64,
    int_chroma: u64,
}

fn gen_mv(rng: &mut Rng, x0: u32, y0: u32, w: u32, h: u32, l: &Layout, adversarial: bool) -> Mv {
    let clamp16 = |v: i64| v.clamp(i16::MIN as i64, i16::MAX as i64) as i16;
    if adversarial {
        // Half-sample on both axes (the only phase whose 2-D pass can leave i16).
        return Mv { x: (rng.range(-40, 40) * 4 + 2) as i16, y: (rng.range(-40, 40) * 4 + 2) as i16 };
    }
    let mut comp = |p0: u32, size: u32, dim: u32| -> i16 {
        match rng.next() % 100 {
            0..=39 => rng.range(-64, 64) as i16,
            40..=64 => rng.range(-1024, 1024) as i16,
            65..=89 => {
                // Near or past a picture edge.
                let t = rng.range(-(size as i32) - 80, dim as i32 + 80);
                clamp16((t as i64 - p0 as i64) * 4 + rng.range(0, 3) as i64)
            }
            90..=94 => [i16::MIN, i16::MAX, i16::MIN + 1, i16::MAX - 2][rng.next() as usize % 4],
            _ => rng.range(i16::MIN as i32, i16::MAX as i32) as i16,
        }
    };
    let mut x = comp(x0, w, l.w);
    let mut y = comp(y0, h, l.h);
    // Whole-sample MVs (the copy / average shortcuts of copy_block and avg_block): luma and chroma integer, or
    // luma integer with a half chroma sample.
    match rng.next() % 100 {
        0..=11 => {
            x &= !7;
            y &= !7;
        }
        12..=19 => {
            x &= !3;
            y &= !3;
        }
        _ => {}
    }
    Mv { x, y }
}

fn run_case<P: Pixel>(case: &Case, out_dir: &str) {
    let l = Layout::new(case.w, case.h);
    let bd = [case.bd_y, case.bd_c];
    let mut rng = Rng(case.seed);

    // Reference pictures and the current picture's initial content (areas without a PU keep it).
    let fills = [Fill::Uniform, Fill::Extremes, Fill::Uniform, Fill::Adversarial, Fill::Uniform];
    let planes: Vec<Planes> = fills.iter().map(|&f| make_planes(&l, bd, f, &mut rng)).collect();
    let mut arena = vec![0u32; (NUM_SLOTS * l.words) as usize];
    for (s, pl) in planes.iter().enumerate() {
        pack(&l, pl, &mut arena[s * l.words as usize..(s + 1) * l.words as usize]);
    }
    let dpic = |pl: &Planes| -> Arc<DecodedPicture> {
        let conv = |c: usize| to_p::<P>(&pl.p[c], bd[(c > 0) as usize]);
        let planes = [conv(0), conv(1), conv(2)];
        // `wrap_planes` is the vendored pixel.rs's own constructor.
        Arc::new(DecodedPicture {
            planes: P::wrap_planes(planes),
            progress: Progress,
            width: l.w,
            height: l.h,
            y_stride: l.stride_y as usize,
            uv_stride: l.stride_c as usize,
            chroma_shift_w: 1,
            chroma_shift_h: 1,
        })
    };
    let refs: Vec<Arc<DecodedPicture>> = planes[..4].iter().map(dpic).collect();
    let l0: Vec<Arc<DecodedPicture>> = L0_SLOTS.iter().map(|&s| refs[s as usize].clone()).collect();
    let l1: Vec<Arc<DecodedPicture>> = L1_SLOTS.iter().map(|&s| refs[s as usize].clone()).collect();

    // Explicit weights: random within the coded ranges, a few entries left at the default.
    let mut wt = PredWeightTable::default();
    wt.luma_log2_weight_denom = rng.range(0, 7) as u8;
    wt.chroma_log2_weight_denom = rng.range(0, 7) as u8;
    wt.high_precision_offsets = case.high_precision;
    let off_range = |bd: u8| if case.high_precision { 1i32 << (bd - 1) } else { 128 };
    for i in 0..slice::MAX_REFS {
        let (dl, dc) = (wt.luma_log2_weight_denom, wt.chroma_log2_weight_denom);
        let def = rng.pct(15);
        let mut lw = |rng: &mut Rng, d: u8| if def { 1i16 << d } else { ((1i32 << d) + rng.range(-128, 127)) as i16 };
        let lo = |rng: &mut Rng, bd: u8| if def { 0 } else { rng.range(-off_range(bd), off_range(bd) - 1) as i16 };
        wt.luma_weight_l0[i] = lw(&mut rng, dl);
        wt.luma_offset_l0[i] = lo(&mut rng, case.bd_y);
        wt.luma_weight_l1[i] = lw(&mut rng, dl);
        wt.luma_offset_l1[i] = lo(&mut rng, case.bd_y);
        for k in 0..2 {
            wt.chroma_weight_l0[i][k] = lw(&mut rng, dc);
            wt.chroma_offset_l0[i][k] = lo(&mut rng, case.bd_c);
            wt.chroma_weight_l1[i][k] = lw(&mut rng, dc);
            wt.chroma_offset_l1[i][k] = lo(&mut rng, case.bd_c);
        }
    }
    // Weight entries in the FORMAT-mc.md layout: entry list * 16 + refIdx, offsets already scaled.
    let mut wts = vec![0u32; (2 * WT_PER_LIST * 4) as usize];
    let scale = |b: u8| if wt.high_precision_offsets { 1i32 } else { 1i32 << (b - 8) };
    let pack16 = |a: i32, b: i32| (a as i16 as u16 as u32) | ((b as i16 as u16 as u32) << 16);
    for list in 0..2usize {
        for i in 0..WT_PER_LIST as usize {
            let e = (list * WT_PER_LIST as usize + i) * 4;
            let (lw, lo, cw, co) = if list == 0 {
                (wt.luma_weight_l0[i], wt.luma_offset_l0[i], wt.chroma_weight_l0[i], wt.chroma_offset_l0[i])
            } else {
                (wt.luma_weight_l1[i], wt.luma_offset_l1[i], wt.chroma_weight_l1[i], wt.chroma_offset_l1[i])
            };
            wts[e] = pack16(lw as i32, lo as i32 * scale(case.bd_y));
            wts[e + 1] = pack16(cw[0] as i32, co[0] as i32 * scale(case.bd_c));
            wts[e + 2] = pack16(cw[1] as i32, co[1] as i32 * scale(case.bd_c));
            wts[e + 3] = wt.luma_log2_weight_denom as u32 | (wt.chroma_log2_weight_denom as u32) << 8;
        }
    }

    let cur = &planes[CUR_SLOT as usize];
    let min_pu_width = (l.w >> 2) as usize;
    let mut state = PictureState::<P> {
        tab_mvf: vec![MvField::default(); min_pu_width * (l.h >> 2) as usize],
        log2_min_pu_size: 2,
        min_pu_width,
        y_stride: l.stride_y as usize,
        uv_stride: l.stride_c as usize,
        chroma_shift_w: 1,
        chroma_shift_h: 1,
        bit_depth: case.bd_y,
        bit_depth_c: case.bd_c,
        y_plane: to_p::<P>(&cur.p[0], case.bd_y),
        u_plane: to_p::<P>(&cur.p[1], case.bd_c),
        v_plane: to_p::<P>(&cur.p[2], case.bd_c),
        row_exact: false,
        block_buf: Vec::new(),
        mc_scratch: inter_pred::McScratch::new(),
    };

    let mut pus: Vec<u32> = Vec::new();
    let mut st = Stats { chroma_phases: vec![0; 64], ..Default::default() };

    // Coding quadtree over 64x64 CTBs; CUs that cross the picture edge are split, as in the spec.
    let mut stack: Vec<(u32, u32, u32)> = Vec::new();
    for cy in (0..l.h).step_by(64) {
        for cx in (0..l.w).step_by(64) {
            stack.push((cx, cy, 64));
        }
    }
    stack.reverse();
    while let Some((x0, y0, s)) = stack.pop() {
        if x0 >= l.w || y0 >= l.h {
            continue;
        }
        let crosses = x0 + s > l.w || y0 + s > l.h;
        if s > 8 && (crosses || rng.pct(match s { 64 => 45, 32 => 50, _ => 40 })) {
            let h = s / 2;
            for (dx, dy) in [(h, h), (0, h), (h, 0), (0, 0)] {
                stack.push((x0 + dx, y0 + dy, h));
            }
            continue;
        }
        let part = if s == 8 {
            [PartMode::Part2Nx2N, PartMode::Part2NxN, PartMode::PartNx2N][rng.next() as usize % 3]
        } else {
            match rng.next() % 12 {
                0..=3 => PartMode::Part2Nx2N,
                4 => PartMode::Part2NxN,
                5 => PartMode::PartNx2N,
                6 => PartMode::Part2NxnU,
                7 => PartMode::Part2NxnD,
                8 => PartMode::PartnLx2N,
                9 => PartMode::PartnRx2N,
                10 if s == 16 => PartMode::PartNxN,
                _ => PartMode::Part2Nx2N,
            }
        };
        let pu_rects: Vec<(u32, u32, u32, u32)> = match part {
            PartMode::Part2Nx2N => vec![(x0, y0, s, s)],
            PartMode::Part2NxN => vec![(x0, y0, s, s / 2), (x0, y0 + s / 2, s, s / 2)],
            PartMode::PartNx2N => vec![(x0, y0, s / 2, s), (x0 + s / 2, y0, s / 2, s)],
            PartMode::Part2NxnU => vec![(x0, y0, s, s / 4), (x0, y0 + s / 4, s, s * 3 / 4)],
            PartMode::Part2NxnD => vec![(x0, y0, s, s * 3 / 4), (x0, y0 + s * 3 / 4, s, s / 4)],
            PartMode::PartnLx2N => vec![(x0, y0, s / 4, s), (x0 + s / 4, y0, s * 3 / 4, s)],
            PartMode::PartnRx2N => vec![(x0, y0, s * 3 / 4, s), (x0 + s * 3 / 4, y0, s / 4, s)],
            PartMode::PartNxN => {
                let h = s / 2;
                vec![(x0, y0, h, h), (x0 + h, y0, h, h), (x0, y0 + h, h, h), (x0 + h, y0 + h, h, h)]
            }
        };
        // The weighted flag is per slice in a real stream; per CU here (the function takes it per call).
        let weighted = rng.pct(30);
        for &(px, py, pw, ph) in &pu_rects {
            let mut mvf = MvField::default();
            let hole = rng.pct(6);
            if !hole {
                let r = rng.next() % 100;
                mvf.pred_flag = if r < 35 { 1 } else if r < 60 { 2 } else { 3 };
                for list in 0..2 {
                    if mvf.pred_flag & (1 << list) == 0 {
                        continue;
                    }
                    let n = if list == 0 { L0_SLOTS.len() } else { L1_SLOTS.len() };
                    let mut idx = rng.next() as usize % n;
                    if idx == 2 && !rng.pct(30) {
                        idx = 0; // keep the adversarial picture to about 10% of the predictions
                    }
                    mvf.ref_idx[list] = idx as i8;
                    mvf.mv[list] = gen_mv(&mut rng, px, py, pw, ph, &l, idx == 2);
                }
            }
            state.tab_mvf[(py >> 2) as usize * min_pu_width + (px >> 2) as usize] = mvf;
            if hole {
                st.holes += 1;
                continue;
            }
            // Record (FORMAT-mc.md).
            let flags = mvf.pred_flag as u32 | (weighted as u32) << 2;
            let slot = |list: usize| -> u32 {
                if mvf.pred_flag & (1 << list) == 0 {
                    return 0;
                }
                let i = mvf.ref_idx[list] as usize;
                if list == 0 { L0_SLOTS[i] } else { L1_SLOTS[i] }
            };
            let wti = |list: usize| -> u32 {
                if mvf.pred_flag & (1 << list) == 0 { 0 } else { list as u32 * WT_PER_LIST + mvf.ref_idx[list] as u32 }
            };
            let mvw = |m: Mv| (m.x as u16 as u32) | (m.y as u16 as u32) << 16;
            pus.extend_from_slice(&[
                px | py << 16,
                pw | ph << 8 | flags << 16,
                slot(0) | slot(1) << 16,
                mvw(mvf.mv[0]),
                mvw(mvf.mv[1]),
                wti(0) | wti(1) << 16,
            ]);
            st.pus += 1;
            if mvf.pred_flag == 3 { st.bi += 1 } else { st.uni += 1 }
            st.weighted += weighted as u64;
            st.adversarial += (0..2).any(|li| mvf.pred_flag & (1 << li) != 0 && mvf.ref_idx[li] == 2) as u64;
            st.samples += (pw * ph + 2 * (pw / 2) * (ph / 2)) as u64;
            *st.sizes.entry((pw, ph)).or_default() += 1;
            for li in 0..2 {
                if mvf.pred_flag & (1 << li) == 0 {
                    continue;
                }
                let m = mvf.mv[li];
                st.luma_phases[((m.x & 3) * 4 + (m.y & 3)) as usize] += 1;
                st.int_chroma += ((m.x | m.y) & 7 == 0) as u64;
                st.chroma_phases[((m.x & 7) * 8 + (m.y & 7)) as usize] += 1;
                let (rx, ry) = (px as i32 + (m.x as i32 >> 2), py as i32 + (m.y as i32 >> 2));
                if rx - 3 < 0 || ry - 3 < 0 || rx + pw as i32 + 4 > l.w as i32 || ry + ph as i32 + 4 > l.h as i32 {
                    st.outside += 1;
                }
            }
        }
        let wt_ref = &wt;
        inter_pred::motion_compensation_cu::<P>(&mut state, &l0, &l1, x0, y0, s, part, weighted, wt_ref);
    }

    let expected = Planes { p: [from_p(&state.y_plane), from_p(&state.u_plane), from_p(&state.v_plane)] };
    let mut exp_words = vec![0u32; l.words as usize];
    pack(&l, &expected, &mut exp_words);

    let scalar = cfg!(feature = "scalar");
    let u16_samples = case.bd_y.max(case.bd_c) > 8;
    let flags = u16_samples as u32 | (scalar as u32) << 1;
    let header: [u32; 16] = [
        0x3156_434D, // "MCV1"
        l.w,
        l.h,
        l.stride_y,
        l.stride_c,
        case.bd_y as u32,
        case.bd_c as u32,
        flags,
        NUM_SLOTS,
        CUR_SLOT,
        l.words,
        (pus.len() / 6) as u32,
        (wts.len() / 4) as u32,
        l.off_cb,
        l.off_cr,
        st.samples as u32,
    ];
    let mut bytes: Vec<u8> = Vec::new();
    for v in header.iter().chain(&arena).chain(&pus).chain(&wts).chain(&exp_words) {
        bytes.extend_from_slice(&v.to_le_bytes());
    }
    let name = format!("{}/mc-{}{}.bin", out_dir, case.name, if scalar { "-scalar" } else { "" });
    std::fs::write(&name, &bytes).unwrap();
    let lp = st.luma_phases.iter().filter(|&&n| n > 0).count();
    let cp = st.chroma_phases.iter().filter(|&&n| n > 0).count();
    println!(
        "{name}: {}x{} bd {}/{} pus {} (bi {}, uni {}, weighted {}, adversarial {}, mv windows past an edge {}, \
         whole-sample luma+chroma MVs {}) holes {} \
         predicted samples {} luma phases {lp}/16 chroma phases {cp}/64 sizes {}",
        l.w,
        l.h,
        case.bd_y,
        case.bd_c,
        st.pus,
        st.bi,
        st.uni,
        st.weighted,
        st.adversarial,
        st.outside,
        st.int_chroma,
        st.holes,
        st.samples,
        st.sizes.iter().map(|((w, h), n)| format!("{w}x{h}:{n}")).collect::<Vec<_>>().join(" ")
    );
}

fn main() {
    let out_dir = std::env::args().nth(1).expect("usage: gpu-recon-ref-mc <out-dir>");
    std::fs::create_dir_all(&out_dir).unwrap();
    let cases = [
        Case { name: "bd8", w: 1288, h: 728, bd_y: 8, bd_c: 8, high_precision: false, seed: 1 },
        Case { name: "bd10", w: 1288, h: 728, bd_y: 10, bd_c: 10, high_precision: false, seed: 2 },
        Case { name: "bd10hp", w: 1288, h: 728, bd_y: 10, bd_c: 10, high_precision: true, seed: 5 },
        Case { name: "bd12", w: 1288, h: 728, bd_y: 12, bd_c: 12, high_precision: true, seed: 3 },
        Case { name: "bd8c10", w: 648, h: 360, bd_y: 8, bd_c: 10, high_precision: false, seed: 4 },
    ];
    for c in &cases {
        if c.bd_y.max(c.bd_c) > 8 {
            run_case::<u16>(c, &out_dir);
        } else {
            run_case::<u8>(c, &out_dir);
        }
    }
}
