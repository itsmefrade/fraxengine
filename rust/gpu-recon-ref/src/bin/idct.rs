//! Test vectors for web/gpu-recon/idct.wgsl.mjs: dequantisation + inverse transform of HEVC TUs.
//!
//! Inputs are written in exactly the layout of web/gpu-recon/FORMAT-idct.md (TU records, coefficient entries, scaling
//! list table); expected outputs come from the rust_h265 functions:
//!   - dequantisation: the formula of `decode_residual_simple` / `decode_residual_coding_generic`
//!     (residual_coding.rs:1269-1286 and 1452-1472, 1026-1055), copied below as `dequant_one`;
//!   - transform skip shift: residual_coding.rs:1478-1487; transquant bypass: `level as i16` (residual_coding.rs:1450);
//!   - inverse transform: `apply_inverse_transform` (inverse_transform.rs:466) through the marked copy in
//!     `ref_inverse_transform`, cross-checked here against the butterfly kernels.
//! Usage: idct <out_dir> [scale]   (scale multiplies the TU counts, default 1)

#[path = "../ref_inverse_transform.rs"]
mod ref_inverse_transform;

use ref_inverse_transform::{apply_inverse_transform, reference_idct, transform_4x4_luma, TRANSFORM};
use std::fs;
use std::io::Write;

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    fn range(&mut self, lo: i64, hi: i64) -> i64 {
        lo + self.below((hi - lo + 1) as u64) as i64
    }
    fn chance(&mut self, num: u64, den: u64) -> bool {
        self.below(den) < num
    }
}

const LEVEL_SCALE: [u32; 6] = [40, 45, 51, 57, 64, 72];

/// residual_coding.rs:462 `compute_dequant_scale` (shift and add), with `qp` = qP + QpBdOffset.
fn dequant_params(qp: i32, log2: u32, bd: u32) -> (u32, u32) {
    let shift = (bd + log2).saturating_sub(5);
    let qp = qp.max(0) as usize;
    let scale = LEVEL_SCALE[qp % 6] << (qp / 6).min(31);
    (shift, scale)
}

/// residual_coding.rs:1464-1465 (the i64 branch of the streamlined decoder, same as 1048-1049 of the generic one).
fn dequant_one(level: i64, scale: u32, scale_m: u32, shift: u32) -> i32 {
    let add = 1i64 << (shift - 1);
    let dq = (level * scale as i64 * scale_m as i64 + add) >> shift;
    dq.clamp(-32768, 32767) as i32
}

/// Scaling list table: 24 lists (sizeId * 6 + matrixId) of 17 u32 words (64 bytes packed little endian, then the DC).
struct Lists {
    sl: [[[u8; 64]; 6]; 4],
    dc: [[u8; 6]; 4],
}

const DEF_INTRA: [u8; 64] = [
    16, 16, 16, 16, 17, 18, 21, 24, 16, 16, 16, 16, 17, 19, 22, 25, 16, 16, 17, 18, 20, 22, 25, 29, 16, 16, 18, 21, 24, 27,
    31, 36, 17, 17, 20, 24, 30, 35, 41, 47, 18, 19, 22, 27, 35, 44, 54, 65, 21, 22, 25, 31, 41, 54, 70, 88, 24, 25, 29, 36,
    47, 65, 88, 115,
];
const DEF_INTER: [u8; 64] = [
    16, 16, 16, 16, 17, 18, 20, 24, 16, 16, 16, 17, 18, 20, 24, 25, 16, 16, 17, 18, 20, 24, 25, 28, 16, 17, 18, 20, 24, 25,
    28, 33, 17, 18, 20, 24, 25, 28, 33, 41, 18, 20, 24, 25, 28, 33, 41, 54, 20, 24, 25, 28, 33, 41, 54, 71, 24, 25, 28, 33,
    41, 54, 71, 91,
];

fn make_lists(rng: &mut Rng) -> Lists {
    let mut l = Lists { sl: [[[0; 64]; 6]; 4], dc: [[16; 6]; 4] };
    for s in 0..4 {
        for m in 0..6 {
            let n = if s == 0 { 16 } else { 64 };
            for i in 0..n {
                l.sl[s][m][i] = match (s, m % 3, m) {
                    // Lists 0..1 of every size are the defaults (flat 16 for 4x4), the rest random incl. extremes.
                    (0, _, 0) | (0, _, 3) => 16,
                    (_, _, 0) => DEF_INTRA[i],
                    (_, _, 3) => DEF_INTER[i],
                    _ => match rng.below(10) {
                        0 => 1,
                        1 => 255,
                        _ => rng.range(1, 255) as u8,
                    },
                };
            }
            l.dc[s][m] = if s >= 2 { if m == 0 || m == 3 { 16 } else { rng.range(1, 255) as u8 } } else { l.sl[s][m][0] };
        }
    }
    l
}

fn list_words(l: &Lists) -> Vec<u32> {
    let mut w = Vec::new();
    for s in 0..4 {
        for m in 0..6 {
            for j in 0..16 {
                let b = &l.sl[s][m];
                w.push(b[4 * j] as u32 | (b[4 * j + 1] as u32) << 8 | (b[4 * j + 2] as u32) << 16 | (b[4 * j + 3] as u32) << 24);
            }
            w.push(l.dc[s][m] as u32);
        }
    }
    w
}

/// resolve_scaling_matrix (residual_coding.rs:1528) reduced to the table lookup: (matrix, dc) for list index `li`.
fn scale_m(l: &Lists, li: usize, log2: u32, x: usize, y: usize) -> u32 {
    let (s, m) = (li / 6, li % 6);
    if x != 0 || y != 0 || log2 < 4 {
        let pos = match log2 {
            3 => (y << 3) + x,
            4 => ((y >> 1) << 3) + (x >> 1),
            5 => ((y >> 2) << 3) + (x >> 2),
            _ => (y << 2) + x,
        };
        l.sl[s][m][pos] as u32
    } else {
        l.dc[s][m] as u32
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Kind {
    Dct,
    Dst,
    Skip,
    Bypass,
}

struct Tu {
    x: u32,
    y: u32,
    log2: u32,
    plane: u32,
    kind: Kind,
    bd: u32,
    li: u32, // 31 = flat
    qp: u32,
    coefs: Vec<(u32, i32)>, // (pos, level)
}

fn expected(l: &Lists, tu: &Tu) -> Vec<i16> {
    let n = 1usize << tu.log2;
    let mut c = vec![0i16; n * n];
    match tu.kind {
        Kind::Bypass => {
            for &(pos, lv) in &tu.coefs {
                c[pos as usize] = lv as i16;
            }
            c
        }
        _ => {
            let (shift, scale) = dequant_params(tu.qp as i32, tu.log2, tu.bd);
            // Transform skip blocks larger than 4x4 use the flat factor (residual_coding.rs:1276).
            let flat = tu.li == 31 || (tu.kind == Kind::Skip && tu.log2 > 2);
            for &(pos, lv) in &tu.coefs {
                let (x, y) = (pos as usize & (n - 1), pos as usize >> tu.log2);
                let sm = if flat { 16 } else { scale_m(l, tu.li as usize, tu.log2, x, y) };
                c[pos as usize] = dequant_one(lv as i64, scale, sm, shift) as i16;
            }
            if tu.kind == Kind::Skip {
                let ts = 15 - tu.bd as i32 - tu.log2 as i32;
                for v in c.iter_mut() {
                    let v32 = *v as i32;
                    *v = if ts > 0 { (v32 + (1 << (ts - 1))) >> ts } else { v32 << -ts } as i16;
                }
                return c;
            }
            // Zero bounds as the decoder derives them: coded coefficient groups (multiples of 4).
            let (mut mx, mut my) = (0usize, 0usize);
            for &(pos, _) in &tu.coefs {
                mx = mx.max(pos as usize & (n - 1));
                my = my.max(pos as usize >> tu.log2);
            }
            let nz = (((mx >> 2) + 1) * 4, ((my >> 2) + 1) * 4);
            let only_dc = tu.coefs.len() == 1 && tu.coefs[0].0 == 0;
            let (lx, ly) = if only_dc { (0, 0) } else { (nz.0 as u32 - 1, nz.1 as u32 - 1) };
            let saved = c.clone();
            apply_inverse_transform(&mut c, tu.log2 as u8, lx, ly, (nz.0 as u8, nz.1 as u8), tu.bd, tu.kind == Kind::Dst);
            // Cross-checks of the copy: butterflies (8..32 and DST) must agree with the dispatch above.
            if tu.kind == Kind::Dst {
                let mut b = saved.clone();
                transform_4x4_luma(&mut b, tu.bd);
                assert_eq!(b, c, "dst");
            } else if tu.log2 >= 3 {
                let mut b = saved.clone();
                reference_idct(&mut b, n, tu.bd);
                assert_eq!(b, c, "idct butterfly vs skip kernel, n={n}");
            }
            c
        }
    }
}

fn level(rng: &mut Rng) -> i32 {
    let l = match rng.below(20) {
        0..=9 => rng.range(-3, 3),
        10..=14 => rng.range(-40, 40),
        15..=17 => rng.range(-700, 700),
        18 => rng.range(-32768, 32767),
        _ => *[32767i64, -32768, 32767, 1, -1, 255, -256][rng.below(7) as usize..].first().unwrap(),
    };
    if l == 0 { 1 } else { l as i32 }
}

fn gen_tu(rng: &mut Rng, log2: u32, forced: Option<Kind>) -> Tu {
    let n = 1u32 << log2;
    let kind = forced.unwrap_or_else(|| match rng.below(20) {
        0 => Kind::Bypass,
        1 | 2 => Kind::Skip,
        3..=7 if log2 == 2 => Kind::Dst,
        _ => Kind::Dct,
    });
    let kind = if kind == Kind::Dst && log2 != 2 { Kind::Dct } else { kind };
    let bd = *[8u32, 8, 10, 10, 10, 9, 11, 12][rng.below(8) as usize..].first().unwrap();
    let plane = if kind == Kind::Dst { 0 } else { rng.below(3) as u32 };
    let qp_max = 51 + 6 * (bd - 8);
    let qp = match rng.below(10) {
        0 => 0,
        1 => qp_max,
        2..=4 => rng.range(0, qp_max as i64) as u32,
        _ => rng.range(18, 40) as u32 + 6 * (bd - 8) / 2,
    }
    .min(qp_max);
    let li = if rng.chance(4, 10) {
        31
    } else {
        let inter = rng.below(2) as usize;
        let s = if log2 == 5 && plane != 0 { 2 } else { (log2 - 2) as usize };
        (s * 6 + 3 * inter + plane as usize) as u32
    };
    let total = (n * n) as u64;
    let mut cells: Vec<u32> = Vec::new();
    match rng.below(10) {
        0 => cells.push(0),
        1 => cells.push(rng.below(total) as u32),
        2 => {
            // single row or column
            let row = rng.chance(1, 2);
            let k = rng.below(n as u64) as u32;
            for i in 0..n {
                if rng.chance(2, 3) {
                    cells.push(if row { k * n + i } else { i * n + k });
                }
            }
        }
        3 => {
            // dense, full range levels
            for p in 0..total as u32 {
                cells.push(p);
            }
        }
        4 | 5 => {
            // low frequency box
            let bx = ((rng.below(n as u64) as u32 / 4) + 1) * 4;
            let by = ((rng.below(n as u64) as u32 / 4) + 1) * 4;
            let dens = rng.range(1, 4) as u64;
            for y in 0..by.min(n) {
                for x in 0..bx.min(n) {
                    if rng.chance(dens, 5) {
                        cells.push(y * n + x);
                    }
                }
            }
        }
        _ => {
            let k = 1 + rng.below((total / 6).max(2)) as usize;
            for _ in 0..k {
                cells.push(rng.below(total) as u32);
            }
        }
    }
    cells.sort();
    cells.dedup();
    if cells.is_empty() {
        cells.push(0);
    }
    let extreme = rng.chance(1, 12);
    let dense_full = cells.len() as u64 == total && rng.chance(1, 2);
    let coefs = cells
        .iter()
        .map(|&p| {
            let lv = if extreme || dense_full {
                if rng.chance(1, 2) { 32767 } else { -32768 }
            } else {
                level(rng)
            };
            (p, lv)
        })
        .collect();
    Tu { x: rng.below(240) as u32 * 16, y: rng.below(135) as u32 * 16, log2, plane, kind, bd, li, qp, coefs }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let out = args.get(1).expect("usage: idct <out_dir> [scale]");
    let scale: f64 = args.get(2).map(|s| s.parse().unwrap()).unwrap_or(1.0);
    fs::create_dir_all(out).unwrap();
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let lists = make_lists(&mut rng);

    let mut tus: Vec<Tu> = Vec::new();
    // Random mix (counts per size), then exhaustive single-coefficient edge cases per position.
    for (log2, count) in [(2u32, 30000usize), (3, 12000), (4, 4000), (5, 1000)] {
        for _ in 0..((count as f64 * scale) as usize) {
            tus.push(gen_tu(&mut rng, log2, None));
        }
    }
    for log2 in 2..=5u32 {
        let n = 1u32 << log2;
        for kind in [Kind::Dct, Kind::Dst, Kind::Skip, Kind::Bypass] {
            if kind == Kind::Dst && log2 != 2 {
                continue;
            }
            // Transform skip and bypass are position independent per sample, so the large sizes sample positions.
            let step = if matches!(kind, Kind::Skip | Kind::Bypass) && log2 >= 4 { 7 } else { 1 };
            for p in (0..n * n).step_by(step) {
                for lv in [32767i32, -32768] {
                    let mut tu = gen_tu(&mut rng, log2, Some(kind));
                    tu.coefs = vec![(p, lv)];
                    tus.push(tu);
                }
            }
        }
    }

    // Serialise.
    let mut recs: Vec<u32> = Vec::new();
    let mut coefs: Vec<u32> = Vec::new();
    let mut exp: Vec<i16> = Vec::new();
    let mut samples = 0usize;
    for tu in &tus {
        let n = 1usize << tu.log2;
        let flags = (tu.log2 - 2)
            | tu.plane << 2
            | u32::from(tu.kind == Kind::Dst) << 4
            | u32::from(tu.kind == Kind::Skip) << 5
            | u32::from(tu.kind == Kind::Bypass) << 6
            | (tu.bd - 8) << 8
            | tu.li << 11
            | tu.qp << 16;
        recs.extend_from_slice(&[tu.x | tu.y << 16, flags, coefs.len() as u32, tu.coefs.len() as u32, exp.len() as u32, 0]);
        for &(pos, lv) in &tu.coefs {
            coefs.push(pos | ((lv as i16 as u16 as u32) << 16));
        }
        exp.extend(expected(&lists, tu));
        samples += n * n;
    }

    let write_u32 = |name: &str, v: &[u32]| {
        let mut f = fs::File::create(format!("{out}/{name}")).unwrap();
        let bytes: Vec<u8> = v.iter().flat_map(|x| x.to_le_bytes()).collect();
        f.write_all(&bytes).unwrap();
    };
    write_u32("idct-tus.bin", &recs);
    write_u32("idct-coefs.bin", &coefs);
    write_u32("idct-lists.bin", &list_words(&lists));
    let mut f = fs::File::create(format!("{out}/idct-expected.bin")).unwrap();
    f.write_all(&exp.iter().flat_map(|x| x.to_le_bytes()).collect::<Vec<u8>>()).unwrap();
    // The transform matrix as signed bytes, for the JS side to cross-check its embedded copy.
    let m: Vec<u8> = TRANSFORM.iter().flat_map(|r| r.iter().map(|&v| v as u8)).collect();
    fs::write(format!("{out}/idct-matrix.bin"), m).unwrap();
    fs::write(
        format!("{out}/idct-meta.json"),
        format!("{{\"tus\":{},\"coefs\":{},\"samples\":{}}}\n", tus.len(), coefs.len(), samples),
    )
    .unwrap();
    eprintln!("idct vectors: {} TUs, {} coefficient entries, {} residual samples", tus.len(), coefs.len(), samples);
}
