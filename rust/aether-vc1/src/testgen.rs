//! Random-payload test stream generator (differential and robustness tests).
//!
//! Valid sequence header, entry point and the leading part of each picture header (random flags)
//! followed by random payload bits.  The payload is repaired with this crate's own decoder until
//! every picture parses without error, so the result is a deterministic error free stream that
//! other decoders (FFmpeg) can decode for comparison.  Not part of the decoding API.
#![allow(clippy::all)]
use crate::decoder::Decoder;

pub(crate) struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545F4914F6CDD1D)
    }
    fn bits(&mut self, n: u32) -> u32 {
        (self.next() >> 20) as u32 & ((1u64 << n) - 1) as u32
    }
    fn range(&mut self, lo: u32, hi: u32) -> u32 {
        lo + (self.next() >> 24) as u32 % (hi - lo + 1)
    }
    fn pick<T: Copy>(&mut self, v: &[T]) -> T {
        v[(self.next() >> 24) as usize % v.len()]
    }
}

#[derive(Default, Clone)]
struct Bw {
    bits: Vec<u8>,
}
impl Bw {
    fn put(&mut self, v: u32, n: u32) {
        for i in (0..n).rev() {
            self.bits.push(((v >> i) & 1) as u8);
        }
    }
    fn unary(&mut self, n: u32, maxlen: u32) {
        for _ in 0..n {
            self.bits.push(1);
        }
        if n < maxlen {
            self.bits.push(0);
        }
    }
    fn bytes(&self) -> Vec<u8> {
        let mut b = self.bits.clone();
        while b.len() % 8 != 0 {
            b.push(0);
        }
        b.chunks(8).map(|c| c.iter().fold(0u8, |a, &x| (a << 1) | x)).collect()
    }
}

fn escape(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + 8);
    let mut z = 0;
    for &b in data {
        if z >= 2 && b < 4 {
            out.push(3);
            z = 0;
        }
        out.push(b);
        z = if b == 0 { z + 1 } else { 0 };
    }
    out
}

struct Cfg {
    slices: bool,
    fcm_mode: i64,
    interlace: bool,
    broadcast: bool,
    tfcntr: bool,
    finterp: bool,
    refdist_flag: bool,
}

struct Extra {
    code: u8, // 0x0b slice, 0x0c second field
    data: Vec<u8>,
}

struct FrameGen {
    unit0: Vec<u8>,
    extra: Vec<Extra>,
}

impl FrameGen {
    fn unit_mut(&mut self, unit: usize) -> &mut Vec<u8> {
        if unit == 0 {
            &mut self.unit0
        } else {
            &mut self.extra[unit - 1].data
        }
    }
}

fn bfraction(r: &mut Rng, b: &mut Bw) {
    let idx = r.pick(&[0u32, 1, 2, 3, 4, 5, 6, 7, 7, 13, 20]);
    if idx < 7 {
        b.put(idx, 3);
    } else {
        b.put(7, 3);
        b.put(idx - 7, 4);
    }
}

fn gen_frame(r: &mut Rng, cfg: &Cfg, ptype: &str, payload: usize, first: bool, mb_rows: usize) -> FrameGen {
    let mut fcm = 0;
    if cfg.interlace {
        fcm = if cfg.fcm_mode >= 0 { cfg.fcm_mode as u32 } else { r.pick(&[0u32, 1, 2, 2]) };
    }
    let mut b = Bw::default();
    let mut fptype = 0u32;
    if cfg.interlace {
        match fcm {
            0 => b.put(0, 1),
            1 => b.put(2, 2),
            _ => b.put(3, 2),
        }
    }
    if fcm == 2 {
        fptype = match ptype {
            "SKIP" => r.pick(&[1u32, 2, 3]),
            "I" if first => 0,
            "I" => r.pick(&[0u32, 1, 2]),
            "P" => r.pick(&[1u32, 2, 3]),
            "B" => r.pick(&[4u32, 5, 6, 7]),
            _ => r.pick(&[5u32, 6, 7]),
        };
        b.put(fptype, 3);
    } else {
        let n = match ptype {
            "P" => 0,
            "B" => 1,
            "I" => 2,
            "SKIP" => 4,
            _ => 3,
        };
        b.unary(n, 4);
    }
    if cfg.tfcntr {
        b.put(r.bits(8), 8);
    }
    if cfg.broadcast {
        if !cfg.interlace {
            b.put(r.bits(2), 2);
        } else {
            b.put(r.bits(1), 1);
            b.put(r.bits(1), 1);
        }
    }
    if ptype == "SKIP" && fcm != 2 {
        for _ in 0..64 {
            b.bits.push(r.bits(1) as u8);
        }
        return FrameGen { unit0: b.bytes(), extra: Vec::new() };
    }
    b.put(r.bits(1), 1); // rnd
    if cfg.interlace {
        b.put(r.bits(1), 1); // uvsamp
    }
    if fcm == 2 {
        if cfg.refdist_flag && fptype & 4 == 0 {
            let rd = r.range(0, 3);
            b.put(rd, 2);
            if rd == 3 {
                let n = r.pick(&[0u32, 0, 1, 3]);
                b.unary(n, 14);
            }
        }
        if fptype & 4 != 0 {
            bfraction(r, &mut b);
        }
    } else if fcm == 0 {
        if cfg.finterp {
            b.put(r.bits(1), 1);
        }
        if ptype == "B" {
            bfraction(r, &mut b);
        }
    }
    b.put(r.range(1, 31), 5); // pqindex
    for _ in 0..payload * 8 {
        b.bits.push(r.bits(1) as u8);
    }
    let unit0 = b.bytes();
    let mut extra: Vec<Extra> = Vec::new();
    let rows_total = (mb_rows + 1) & !1;
    let rows_f = if fcm == 2 { rows_total / 2 } else { mb_rows };
    let add_slices = |r: &mut Rng, extra: &mut Vec<Extra>, base: usize| {
        if cfg.slices && rows_f > 1 {
            let n = r.range(0, 2.min(rows_f as u32 - 1)) as usize;
            let mut starts: Vec<usize> = (0..n).map(|_| r.range(1, rows_f as u32 - 1) as usize).collect();
            starts.sort();
            starts.dedup();
            for s in starts {
                let mut bs = Bw::default();
                bs.put((base + s) as u32, 9);
                bs.put(0, 1); // pic_header_flag
                for _ in 0..payload * 8 / 2 {
                    bs.bits.push(r.bits(1) as u8);
                }
                extra.push(Extra { code: 0x0b, data: bs.bytes() });
            }
        }
    };
    add_slices(r, &mut extra, 0);
    if fcm == 2 {
        let mut b2 = Bw::default();
        b2.put(r.range(1, 31), 5);
        for _ in 0..payload * 8 {
            b2.bits.push(r.bits(1) as u8);
        }
        extra.push(Extra { code: 0x0c, data: b2.bytes() });
        add_slices(r, &mut extra, rows_f);
    }
    FrameGen { unit0, extra }
}

fn assemble_au(f: &FrameGen) -> Vec<u8> {
    let mut v = vec![0, 0, 1, 0x0d];
    v.extend(escape(&f.unit0));
    for e in &f.extra {
        v.extend_from_slice(&[0, 0, 1, e.code]);
        v.extend(escape(&e.data));
    }
    v
}

fn rerandom(r: &mut Rng, data: &mut [u8], bitpos: usize, from_back: usize, len: usize) {
    let start = bitpos.saturating_sub(from_back);
    let end = (start + len).min(data.len() * 8);
    for p in start..end {
        let bit = r.bits(1) as u8;
        let m = 0x80u8 >> (p & 7);
        if bit != 0 {
            data[p >> 3] |= m;
        } else {
            data[p >> 3] &= !m;
        }
    }
}


#[derive(Clone, Debug)]
pub struct GenConfig {
    pub width: usize,
    pub height: usize,
    pub frames: usize,
    /// -1 random, 0 progressive sequence, 1 interlaced sequence
    pub interlace: i32,
    pub payload_bytes: usize,
    /// -1 mixed, 0 progressive, 1 interlaced frame, 2 interlaced field pictures
    pub fcm: i32,
    /// Add slice units to pictures.
    pub slices: bool,
}

impl Default for GenConfig {
    fn default() -> Self {
        GenConfig { width: 96, height: 80, frames: 8, interlace: -1, payload_bytes: 8000, fcm: -1, slices: false }
    }
}

/// Generate a stream; panics only on internal logic errors.
pub fn generate(seed: u64, c: &GenConfig) -> Vec<u8> {
    let (w, h, nframes) = (c.width, c.height, c.frames);
    let il = c.interlace;
    let payload = c.payload_bytes;
    let mut r = Rng(seed.wrapping_mul(0x9E3779B97F4A7C15) | 1);
    for _ in 0..8 {
        r.next();
    }
    let interlace = if il < 0 { r.bits(1) == 1 } else { il == 1 };
    let cfg = Cfg { fcm_mode: c.fcm as i64, interlace, broadcast: r.bits(1) == 1, tfcntr: r.bits(1) == 1, finterp: r.bits(1) == 1, refdist_flag: r.bits(1) == 1, slices: c.slices };
    let postproc = r.bits(1);
    // sequence header
    let mut b = Bw::default();
    b.put(3, 2);
    b.put(3, 3);
    b.put(1, 2);
    b.put(r.bits(3), 3);
    b.put(r.bits(5), 5);
    b.put(postproc, 1);
    b.put((w / 2 - 1) as u32, 12);
    b.put((h / 2 - 1) as u32, 12);
    b.put(cfg.broadcast as u32, 1);
    b.put(cfg.interlace as u32, 1);
    b.put(cfg.tfcntr as u32, 1);
    b.put(cfg.finterp as u32, 1);
    b.put(1, 1);
    b.put(0, 1);
    b.put(0, 1);
    b.put(0, 1);
    let seq = b.bytes();
    let mut b = Bw::default();
    b.put(0, 1);
    b.put(r.bits(1), 1);
    b.put(0, 1);
    b.put(cfg.refdist_flag as u32, 1);
    b.put(r.bits(1), 1); // loop filter
    b.put(r.bits(1), 1); // fastuvmc
    let ext = r.bits(1);
    b.put(ext, 1);
    b.put(r.pick(&[0u32, 1, 2, 2]), 2);
    b.put(r.bits(1), 1); // vstransform
    b.put(r.bits(1), 1); // overlap
    b.put(r.bits(2), 2); // quantizer mode
    b.put(0, 1);
    if ext == 1 {
        b.put(r.bits(1), 1);
    }
    b.put(0, 1);
    b.put(0, 1);
    let ep = b.bytes();
    let mut head = vec![0, 0, 1, 0x0f];
    head.extend(escape(&seq));
    head.extend_from_slice(&[0, 0, 1, 0x0e]);
    head.extend(escape(&ep));

    let mb_rows = (h + 15) / 16;
    let mut types: Vec<&str> = vec!["I"];
    let mut anchor_skipped = false;
    for _ in 1..nframes {
        let mut t = r.pick(&["I", "P", "P", "P", "B", "B", "BI", "SKIP"]);
        // direct mode reads motion vectors of the next anchor, which FFmpeg leaves undefined for
        // skipped pictures (stale pool memory); keep B pictures away from those
        if anchor_skipped && (t == "B" || t == "BI") {
            t = "P";
        }
        if t != "B" && t != "BI" {
            anchor_skipped = t == "SKIP";
        }
        types.push(t);
    }
    let mut frames: Vec<FrameGen> = Vec::new();
    for (fi, ty) in types.iter().enumerate() {
        let mut fr = gen_frame(&mut r, &cfg, ty, payload, fi == 0, mb_rows);
        let mut tries = 0u32;
        loop {
            // decode everything so far
            let mut dec = Decoder::new();
            let mut err: Option<(usize, usize)> = None;
            let mut res_err = false;
            for k in 0..=fi {
                let f = if k == fi { &fr } else { &frames[k] };
                let mut au = Vec::new();
                if k == 0 {
                    au.extend_from_slice(&head);
                }
                au.extend(assemble_au(f));
                let res = dec.decode(&au, k as i64);
                if k == fi {
                    err = dec.last_error;
                    res_err = res.is_err();
                }
            }
            if err.is_none() && !res_err {
                break;
            }
            tries += 1;
            if tries > 3000 || err.is_none() {
                fr = gen_frame(&mut r, &cfg, ty, payload, fi == 0, mb_rows);
                tries = 0;
                continue;
            }
            let (unit, pos) = err.unwrap();
            let data: &mut Vec<u8> = fr.unit_mut(unit);
            let (back, len) = if tries < 40 { (0, 12) } else if tries < 400 { (8, 40) } else { (64, 200) };
            rerandom(&mut r, data, pos, back, len);
        }
        frames.push(fr);
    }
    let mut stream = head.clone();
    for f in &frames {
        stream.extend(assemble_au(f));
    }
    stream
}
