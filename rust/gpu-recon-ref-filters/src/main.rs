//! Test-vector generator for the WGSL deblocking and SAO kernels (web/gpu-recon).
//!
//! Builds pictures with boundary-strength maps, QP, no-filter blocks, slices / tiles and SAO parameters, runs the
//! rust_h265 loop filters copied verbatim (deblock_ref.rs, sao_ref.rs), and writes inputs plus expected outputs in
//! the GPU layouts (pack.rs). Usage:
//!
//!   gpu-recon-ref-filters [out_dir] [real_frame.yuv WxH]
//!
//! out_dir defaults to ../../web/results/gpu-recon-filters (served by the dev server, git ignored). The optional
//! real frame (yuv420p10le, e.g. `ffmpeg -i in.mp4 -frames:v 1 -pix_fmt yuv420p10le -f rawvideo f.yuv`) gives the
//! "real" content cases natural image statistics; without it they fall back to synthetic smooth content.

mod deblock_ref;
mod pack;
mod sao_ref;
mod state;

use sao_ref::{SaoParams, SaoType};
use state::{PictureState, Pixel, Pps, Sps};
use std::io::Write;

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
    fn range(&mut self, lo: i64, hi: i64) -> i64 {
        lo + self.below((hi - lo + 1) as u64) as i64
    }
    fn chance(&mut self, p: f64) -> bool {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64 <= p
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Content {
    Random,
    Smooth,
    Real,
}

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    /// Every table entry independently random (strengths 0..2 everywhere, also off the 8x8 grid).
    Random,
    /// CU quadtree: strengths on CU / TU edges, QP per CU, no-filter per CU, coding-error steps per CU.
    Realistic,
}

struct Cfg {
    name: String,
    w: usize,
    h: usize,
    bd_y: u8,
    bd_c: u8,
    log2_ctb: u8,
    log2_min_cb: u8,
    content: Content,
    mode: Mode,
    nf_density: f64,
    tiles: (usize, usize),
    slices: usize,
    random_offsets: bool,
    intra_share: f64,
    seed: u64,
}

struct Real {
    w: usize,
    h: usize,
    planes: [Vec<u16>; 3],
}

fn load_real(path: &str, dims: &str) -> Option<Real> {
    let (w, h) = dims.split_once('x')?;
    let (w, h): (usize, usize) = (w.parse().ok()?, h.parse().ok()?);
    let bytes = std::fs::read(path).ok()?;
    let n = w * h;
    let nc = (w / 2) * (h / 2);
    if bytes.len() < 2 * (n + 2 * nc) {
        return None;
    }
    let rd = |off: usize, len: usize| -> Vec<u16> {
        (0..len)
            .map(|i| u16::from_le_bytes([bytes[2 * (off + i)], bytes[2 * (off + i) + 1]]))
            .collect()
    };
    Some(Real {
        w,
        h,
        planes: [rd(0, n), rd(n, nc), rd(n + nc, nc)],
    })
}

struct Cu {
    x: usize,
    y: usize,
    size: usize,
}

/// CU quadtree over the picture (implicit split where a CU crosses the picture border).
fn cu_tree(rng: &mut Rng, cfg: &Cfg) -> Vec<Cu> {
    let ctb = 1usize << cfg.log2_ctb;
    let min_cb = 1usize << cfg.log2_min_cb;
    let mut out = Vec::new();
    fn rec(rng: &mut Rng, out: &mut Vec<Cu>, x: usize, y: usize, s: usize, min: usize, w: usize, h: usize) {
        if x >= w || y >= h {
            return;
        }
        let crosses = x + s > w || y + s > h;
        let split = s > min && (crosses || rng.chance(if s >= 32 { 0.6 } else { 0.4 }));
        if split {
            let hs = s / 2;
            for (dx, dy) in [(0, 0), (hs, 0), (0, hs), (hs, hs)] {
                rec(rng, out, x + dx, y + dy, hs, min, w, h);
            }
        } else {
            out.push(Cu { x, y, size: s });
        }
    }
    for cy in (0..cfg.h).step_by(ctb) {
        for cx in (0..cfg.w).step_by(ctb) {
            rec(rng, &mut out, cx, cy, ctb, min_cb, cfg.w, cfg.h);
        }
    }
    out
}

fn build<P: Pixel>(cfg: &Cfg, real: Option<&Real>) -> (PictureState<P>, Sps, Pps) {
    let mut rng = Rng(cfg.seed | 1);
    let (w, h) = (cfg.w, cfg.h);
    let ctb = 1usize << cfg.log2_ctb;
    let w_al = w.div_ceil(ctb) * ctb;
    let h_al = h.div_ceil(ctb) * ctb;
    let (wc, hc) = (w.div_ceil(ctb), h.div_ceil(ctb));
    let min_cb_width = w_al >> cfg.log2_min_cb;
    let min_cb_height = h_al >> cfg.log2_min_cb;
    let (mpw, mph) = (w_al >> 2, h_al >> 2);
    let stride_y = pack::align64(w);
    let stride_c = pack::align64(w / 2);
    let max_y = (1i64 << cfg.bd_y) - 1;
    let max_c = (1i64 << cfg.bd_c) - 1;
    let qp_min = -6 * (cfg.bd_y as i64 - 8);

    let mut st = PictureState::<P> {
        width: w as u32,
        height: h as u32,
        y_stride: stride_y,
        uv_stride: stride_c,
        chroma_array_type: 1,
        chroma_shift_w: 1,
        chroma_shift_h: 1,
        log2_ctb_size: cfg.log2_ctb,
        width_in_ctbs: wc,
        log2_min_cb_size: cfg.log2_min_cb,
        min_cb_width,
        min_pu_width: mpw,
        tab_qp_y: vec![0; min_cb_width * min_cb_height],
        bs_vertical: vec![0; mpw * mph],
        bs_horizontal: vec![0; mpw * mph],
        sao_params: vec![SaoParams::default(); wc * hc],
        tab_slice_addr_rs: vec![-1; wc * hc],
        filter_slice_edges: vec![true; wc * hc],
        tab_tile_id: vec![0; wc * hc],
        tab_ctb_ts: vec![0; wc * hc],
        tab_deblock_offsets: vec![(0, 0); wc * hc],
        tab_no_filter: vec![0; mpw * mph],
        any_no_filter: false,
        filter_across_tiles: true,
        sao_snapshot: [Vec::new(), Vec::new(), Vec::new()],
        y_plane: vec![P::zero(); stride_y * h],
        u_plane: vec![P::zero(); stride_c * (h / 2)],
        v_plane: vec![P::zero(); stride_c * (h / 2)],
    };

    // ---- Tiles, tile scan, slices ----
    let cuts = |rng: &mut Rng, n: usize, k: usize| -> Vec<usize> {
        let mut c: Vec<usize> = (1..n).collect();
        let mut out = vec![0];
        for _ in 0..k.saturating_sub(1).min(c.len()) {
            let i = rng.below(c.len() as u64) as usize;
            out.push(c.remove(i));
        }
        out.sort();
        out.push(n);
        out
    };
    let col_bd = cuts(&mut rng, wc, cfg.tiles.0);
    let row_bd = cuts(&mut rng, hc, cfg.tiles.1);
    let mut ts_to_rs = Vec::with_capacity(wc * hc);
    let mut tile = 0u32;
    for tr in 0..row_bd.len() - 1 {
        for tcol in 0..col_bd.len() - 1 {
            for y in row_bd[tr]..row_bd[tr + 1] {
                for x in col_bd[tcol]..col_bd[tcol + 1] {
                    let rs = y * wc + x;
                    st.tab_tile_id[rs] = tile;
                    st.tab_ctb_ts[rs] = ts_to_rs.len() as u32;
                    ts_to_rs.push(rs);
                }
            }
            tile += 1;
        }
    }
    st.filter_across_tiles = tile == 1 || rng.chance(0.5);
    let slice_cuts = cuts(&mut rng, wc * hc, cfg.slices);
    for s in 0..slice_cuts.len() - 1 {
        let first_rs = ts_to_rs[slice_cuts[s]];
        let across = slice_cuts.len() == 2 || rng.chance(0.5);
        let offs = if cfg.random_offsets {
            ((rng.range(-6, 6) * 2) as i8, (rng.range(-6, 6) * 2) as i8)
        } else {
            (0, 0)
        };
        for &rs in &ts_to_rs[slice_cuts[s]..slice_cuts[s + 1]] {
            st.tab_slice_addr_rs[rs] = first_rs as i32;
            st.filter_slice_edges[rs] = across;
            st.tab_deblock_offsets[rs] = offs;
        }
    }

    // ---- Base content (i64 planes, picture sized) ----
    let dims = [(w, h, max_y, cfg.bd_y), (w / 2, h / 2, max_c, cfg.bd_c), (w / 2, h / 2, max_c, cfg.bd_c)];
    let mut planes: Vec<Vec<i64>> = Vec::new();
    let real_origin = real.map(|r| {
        (
            rng.below(r.w.saturating_sub(w).max(1) as u64) as usize & !1,
            rng.below(r.h.saturating_sub(h).max(1) as u64) as usize & !1,
        )
    });
    let phase: Vec<f64> = (0..12).map(|_| rng.below(10_000) as f64 / 1000.0).collect();
    for (c, &(pw, ph, max, bd)) in dims.iter().enumerate() {
        let mut p = vec![0i64; pw * ph];
        for y in 0..ph {
            for x in 0..pw {
                p[y * pw + x] = match (cfg.content, real) {
                    (Content::Random, _) => rng.range(0, max),
                    (Content::Real, Some(r)) => {
                        let (ox, oy) = real_origin.unwrap();
                        let (rw, rh) = if c == 0 { (r.w, r.h) } else { (r.w / 2, r.h / 2) };
                        let (o_x, o_y) = if c == 0 { (ox, oy) } else { (ox / 2, oy / 2) };
                        // Mirror past the frame's edge (the 4K case is taller than the source frame).
                        let mut sx = (o_x + x) % (2 * rw);
                        if sx >= rw {
                            sx = 2 * rw - 1 - sx;
                        }
                        let mut sy = (o_y + y) % (2 * rh);
                        if sy >= rh {
                            sy = 2 * rh - 1 - sy;
                        }
                        let v = r.planes[c][sy * rw + sx] as i64;
                        if bd >= 10 { v << (bd - 10) } else { v >> (10 - bd) }
                    }
                    _ => {
                        let s = (pw.max(ph)) as f64;
                        let (fx, fy) = (x as f64 / s, y as f64 / s);
                        let k = c * 4;
                        let v = 0.5
                            + 0.25 * (fx * 7.0 + phase[k]).sin()
                            + 0.15 * (fy * 11.0 + phase[k + 1]).sin()
                            + 0.07 * ((fx + fy) * 31.0 + phase[k + 2]).sin();
                        (v.clamp(0.0, 1.0) * max as f64) as i64 + rng.range(-1, 1)
                    }
                };
            }
        }
        planes.push(p);
    }

    match cfg.mode {
        Mode::Random => {
            let bs_rand = |rng: &mut Rng| match rng.below(10) {
                0..=3 => 0u8,
                4..=6 => 1,
                _ => 2,
            };
            for y4 in 0..h / 4 {
                for x4 in 0..w / 4 {
                    st.bs_vertical[y4 * (w >> 2) + x4] = bs_rand(&mut rng);
                    st.bs_horizontal[y4 * (w >> 2) + x4] = bs_rand(&mut rng);
                    if rng.chance(cfg.nf_density) {
                        st.tab_no_filter[y4 * mpw + x4] = 1;
                        st.any_no_filter = true;
                    }
                }
            }
            for v in st.tab_qp_y.iter_mut() {
                *v = rng.range(qp_min, 51) as i8;
            }
            // Smooth content gets block steps so that the luma decisions pick strong, normal and no filtering.
            if cfg.content != Content::Random {
                let amp = 1i64 << (cfg.bd_y as i64 - 6);
                for (c, &(pw, ph, max, _)) in dims.iter().enumerate() {
                    let b = if c == 0 { 4 } else { 2 };
                    for by in 0..ph / b {
                        for bx in 0..pw / b {
                            let d = rng.range(-amp, amp);
                            for y in by * b..by * b + b {
                                for x in bx * b..bx * b + b {
                                    let v = &mut planes[c][y * pw + x];
                                    *v = (*v + d).clamp(0, max);
                                }
                            }
                        }
                    }
                }
            }
        }
        Mode::Realistic => {
            let cus = cu_tree(&mut rng, cfg);
            let mut cu_id = vec![u32::MAX; (w / 4) * (h / 4)];
            let mut intra = Vec::with_capacity(cus.len());
            let base_qp = rng.range(22, 37);
            for (i, cu) in cus.iter().enumerate() {
                let is_intra = rng.chance(cfg.intra_share);
                intra.push(is_intra);
                let qp = (base_qp + rng.range(-3, 3)).clamp(qp_min, 51) as i8;
                let nf = rng.chance(cfg.nf_density);
                for y in (cu.y..cu.y + cu.size).step_by(4) {
                    for x in (cu.x..cu.x + cu.size).step_by(4) {
                        cu_id[(y / 4) * (w / 4) + x / 4] = i as u32;
                        if nf {
                            st.tab_no_filter[(y >> 2) * mpw + (x >> 2)] = 1;
                            st.any_no_filter = true;
                        }
                    }
                }
                let l = cfg.log2_min_cb;
                for y in (cu.y..cu.y + cu.size).step_by(1 << l) {
                    for x in (cu.x..cu.x + cu.size).step_by(1 << l) {
                        st.tab_qp_y[(y >> l) * min_cb_width + (x >> l)] = qp;
                    }
                }
                // Coding error: a DC step per CU (stronger at higher QP), per plane.
                let amp = ((qp as i64 - qp_min).max(1) / 6 + 1) << (cfg.bd_y as i64 - 8);
                for (c, &(pw, _, max, _)) in dims.iter().enumerate() {
                    let sh = if c == 0 { 0 } else { 1 };
                    let d = rng.range(-amp, amp);
                    for y in (cu.y >> sh)..((cu.y + cu.size) >> sh) {
                        for x in (cu.x >> sh)..((cu.x + cu.size) >> sh) {
                            let v = &mut planes[c][y * pw + x];
                            *v = (*v + d).clamp(0, max);
                        }
                    }
                }
            }
            let bs_w = w >> 2;
            // Transform edges inside a CU (one quadtree level): intra 2, inter 1 or 0.
            let mut tu_split = vec![false; cus.len()];
            for (i, cu) in cus.iter().enumerate() {
                tu_split[i] = cu.size >= 16 && rng.chance(0.4);
            }
            for y in (0..h).step_by(4) {
                for x in (0..w).step_by(8) {
                    let q = cu_id[(y / 4) * (w / 4) + x / 4] as usize;
                    let edge_of = |p: usize| p != q;
                    // Vertical edge at x (left of 4x4 at (x, y)).
                    if x > 0 {
                        let p = cu_id[(y / 4) * (w / 4) + (x - 1) / 4] as usize;
                        let cu = &cus[q];
                        let tu_edge = tu_split[q] && x == cu.x + cu.size / 2;
                        if edge_of(p) || tu_edge {
                            st.bs_vertical[(y >> 2) * bs_w + (x >> 2)] = if intra[p] || intra[q] {
                                2
                            } else if rng.chance(0.5) {
                                1
                            } else {
                                0
                            };
                        }
                    }
                }
            }
            for y in (8..h).step_by(8) {
                for x in (0..w).step_by(4) {
                    let q = cu_id[(y / 4) * (w / 4) + x / 4] as usize;
                    let p = cu_id[((y - 1) / 4) * (w / 4) + x / 4] as usize;
                    let cu = &cus[q];
                    let tu_edge = tu_split[q] && y == cu.y + cu.size / 2;
                    if p != q || tu_edge {
                        st.bs_horizontal[(y >> 2) * bs_w + (x >> 2)] = if intra[p] || intra[q] {
                            2
                        } else if rng.chance(0.5) {
                            1
                        } else {
                            0
                        };
                    }
                }
            }
        }
    }

    // ---- Planes into the state ----
    for (c, &(pw, ph, _, _)) in dims.iter().enumerate() {
        let (dst, stride) = match c {
            0 => (&mut st.y_plane, stride_y),
            1 => (&mut st.u_plane, stride_c),
            _ => (&mut st.v_plane, stride_c),
        };
        for y in 0..ph {
            for x in 0..pw {
                dst[y * stride + x] = P::from_i32_clamped(planes[c][y * pw + x] as i32, dims[c].3);
            }
        }
    }

    // ---- SAO parameters per CTB (as decode_sao_param leaves them) ----
    for rs in 0..wc * hc {
        let mut s = SaoParams::default();
        for c in 0..3 {
            let bd = if c == 0 { cfg.bd_y } else { cfg.bd_c };
            if c == 2 {
                s.type_idx[2] = s.type_idx[1];
                s.eo_class[2] = s.eo_class[1];
            } else {
                s.type_idx[c] = match rng.below(10) {
                    0..=2 => SaoType::NotApplied,
                    3..=5 => SaoType::Band,
                    _ => SaoType::Edge,
                };
                s.eo_class[c] = rng.below(4) as u8;
            }
            let cmax = (1i64 << (bd.min(10) - 5)) - 1;
            let scale = rng.below((bd as u64).saturating_sub(10) + 1) as u32;
            s.band_position[c] = rng.below(32) as u8;
            for k in 0..4 {
                let a = rng.range(0, cmax) as i16;
                let v = match s.type_idx[c] {
                    SaoType::Band => {
                        if rng.chance(0.5) {
                            -a
                        } else {
                            a
                        }
                    }
                    _ => {
                        if k > 1 {
                            -a
                        } else {
                            a
                        }
                    }
                };
                s.offset_val[c][k + 1] = v << scale;
            }
            if s.type_idx[c] == SaoType::NotApplied {
                s.offset_val[c] = [0; 5];
            }
        }
        st.sao_params[rs] = s;
    }

    let sps = Sps {
        bit_depth_luma: cfg.bd_y,
        bit_depth_chroma: cfg.bd_c,
        ctb_log2_size_y: cfg.log2_ctb,
        sample_adaptive_offset_enabled_flag: true,
    };
    let pps = Pps {
        pps_cb_qp_offset: rng.range(-12, 12) as i32,
        pps_cr_qp_offset: rng.range(-12, 12) as i32,
    };
    (st, sps, pps)
}

fn snapshot<P: Pixel>(st: &PictureState<P>) -> Vec<u32> {
    pack::pack_picture(
        [&st.y_plane, &st.u_plane, &st.v_plane],
        [st.y_stride, st.uv_stride, st.uv_stride],
        st.width as usize,
        st.height as usize,
    )
}

fn words_bytes(v: &[u32]) -> Vec<u8> {
    v.iter().flat_map(|w| w.to_le_bytes()).collect()
}

/// Runs one case and writes `<name>.bin`; returns the index entry (JSON) and changed-sample counts.
fn run_case<P: Pixel>(cfg: &Cfg, real: Option<&Real>, out_dir: &str) -> String {
    let (mut st, sps, pps) = build::<P>(cfg, real);
    let params = pack::params_words(&st, &sps, &pps);
    let input = snapshot(&st);
    let map = pack::pack_block8_map(&st);
    let dbctb = pack::pack_deblock_ctbs(&st);
    let saoctb = pack::pack_sao_ctbs(&st);
    deblock_ref::deblock_picture(&mut st, &sps, &pps);
    let deblocked = snapshot(&st);
    sao_ref::apply_sao_picture(&mut st, &sps, 1);
    let fin = snapshot(&st);

    let count_diff = |a: &[u32], b: &[u32]| -> u64 {
        a.iter()
            .zip(b)
            .map(|(x, y)| ((x & 0xffff) != (y & 0xffff)) as u64 + ((x >> 16) != (y >> 16)) as u64)
            .sum()
    };
    let deblock_changed = count_diff(&input, &deblocked);
    let sao_changed = count_diff(&deblocked, &fin);

    let sections: [(&str, &[u32]); 7] = [
        ("params", &params),
        ("input", &input),
        ("map", &map),
        ("deblockCtb", &dbctb),
        ("saoCtb", &saoctb),
        ("deblocked", &deblocked),
        ("final", &fin),
    ];
    let mut body = Vec::new();
    let mut sec_json = Vec::new();
    for (name, data) in sections {
        sec_json.push(format!("\"{name}\":[{},{}]", body.len(), data.len() * 4));
        body.extend(words_bytes(data));
    }
    let samples = (cfg.w * cfg.h * 3 / 2) as u64;
    let header = format!(
        "{{\"name\":\"{}\",\"width\":{},\"height\":{},\"bitDepthY\":{},\"bitDepthC\":{},\"log2Ctb\":{},\"anyNoFilter\":{},\"samples\":{},\"deblockChanged\":{},\"saoChanged\":{},\"sections\":{{{}}}}}",
        cfg.name,
        cfg.w,
        cfg.h,
        cfg.bd_y,
        cfg.bd_c,
        cfg.log2_ctb,
        st.any_no_filter,
        samples,
        deblock_changed,
        sao_changed,
        sec_json.join(",")
    );
    let mut hb = header.clone().into_bytes();
    while hb.len() % 4 != 0 {
        hb.push(b' ');
    }
    let path = format!("{out_dir}/{}.bin", cfg.name);
    let mut f = std::fs::File::create(&path).expect("create case file");
    f.write_all(b"GRF1").unwrap();
    f.write_all(&(hb.len() as u32).to_le_bytes()).unwrap();
    f.write_all(&hb).unwrap();
    f.write_all(&body).unwrap();
    eprintln!(
        "{:<28} {}x{} bd {}/{} ctb {} samples {} deblock-changed {} sao-changed {} nf {}",
        cfg.name, cfg.w, cfg.h, cfg.bd_y, cfg.bd_c, 1 << cfg.log2_ctb, samples, deblock_changed, sao_changed, st.any_no_filter
    );
    format!("{{\"file\":\"{}.bin\",\"name\":\"{}\",\"samples\":{}}}", cfg.name, cfg.name, samples)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let out_dir = args.get(1).cloned().unwrap_or_else(|| "../../web/results/gpu-recon-filters".into());
    std::fs::create_dir_all(&out_dir).expect("out dir");
    let real = match (args.get(2), args.get(3)) {
        (Some(p), Some(d)) => load_real(p, d),
        _ => None,
    };
    if real.is_none() {
        eprintln!("no real frame given: 'real' cases use synthetic smooth content");
    }

    let mut cfgs = Vec::new();
    let mut rng = Rng(0x5eed_1234_abcd_0001);
    // Random geometry / tables / content, all bit depths, slices and tiles.
    for i in 0..48 {
        let bd_y = [8u8, 10, 10, 12][i % 4];
        let bd_c = if rng.chance(0.2) { [8u8, 10, 12][rng.below(3) as usize] } else { bd_y };
        let log2_ctb = 4 + rng.below(3) as u8;
        let content = [Content::Random, Content::Smooth, Content::Real][i % 3];
        let mode = if i % 2 == 0 { Mode::Random } else { Mode::Realistic };
        // Picture sizes are multiples of MinCbSizeY (spec 7.4.3.2.1).
        let log2_min_cb = 3 + rng.below((log2_ctb - 3) as u64) as u8;
        let mcb = 1usize << log2_min_cb;
        cfgs.push(Cfg {
            name: format!("rand-{i:02}"),
            w: (8 * rng.range(4, 90) as usize).div_ceil(mcb) * mcb,
            h: (8 * rng.range(4, 60) as usize).div_ceil(mcb) * mcb,
            bd_y,
            bd_c,
            log2_ctb,
            log2_min_cb,
            content,
            mode,
            nf_density: [0.0, 0.05, 0.3][rng.below(3) as usize],
            tiles: (1 + rng.below(4) as usize, 1 + rng.below(3) as usize),
            slices: 1 + rng.below(6) as usize,
            random_offsets: rng.chance(0.7),
            intra_share: rng.below(100) as f64 / 100.0,
            seed: rng.next(),
        });
    }
    // 1080p, 8-bit, many slices and tiles, real content.
    cfgs.push(Cfg {
        name: "hd-8bit-slices-tiles".into(),
        w: 1920,
        h: 1080,
        bd_y: 8,
        bd_c: 8,
        log2_ctb: 5,
        log2_min_cb: 3,
        content: Content::Real,
        mode: Mode::Realistic,
        nf_density: 0.02,
        tiles: (4, 3),
        slices: 9,
        random_offsets: true,
        intra_share: 0.3,
        seed: 0x1080,
    });
    // 4K Main 10, CTB 64, one slice: the timing case.
    cfgs.push(Cfg {
        name: "uhd-10bit".into(),
        w: 3840,
        h: 2160,
        bd_y: 10,
        bd_c: 10,
        log2_ctb: 6,
        log2_min_cb: 3,
        content: Content::Real,
        mode: Mode::Realistic,
        nf_density: 0.0,
        tiles: (1, 1),
        slices: 1,
        random_offsets: false,
        intra_share: 0.25,
        seed: 0x4000,
    });

    let mut index = Vec::new();
    for cfg in &cfgs {
        let entry = if cfg.bd_y == 8 && cfg.bd_c == 8 {
            run_case::<u8>(cfg, real.as_ref(), &out_dir)
        } else {
            run_case::<u16>(cfg, real.as_ref(), &out_dir)
        };
        index.push(entry);
    }
    std::fs::write(format!("{out_dir}/index.json"), format!("[\n{}\n]\n", index.join(",\n"))).expect("index");
}
