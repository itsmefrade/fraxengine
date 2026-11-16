//! Dev tool: decode an Annex B file with the session configuration (display order, one thread), report
//! throughput and optionally write per-frame SHA-256 of the Y/U/V planes (little-endian u16 bytes).
//! Usage: hevcbench <file.hevc> [--frames N] [--hash out.txt] [--repeat R] [--threads T]
use rust_h265::{DecodeLimits, Decoder, Frame, PixelData, parse_nal};
use std::io::Write;
use std::time::Instant;

const K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];
struct Sha256 {
    h: [u32; 8],
    buf: [u8; 64],
    n: usize,
    len: u64,
}
impl Sha256 {
    fn new() -> Self {
        Self {
            h: [
                0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
                0x5be0cd19,
            ],
            buf: [0; 64],
            n: 0,
            len: 0,
        }
    }
    fn block(h: &mut [u32; 8], b: &[u8]) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([b[4 * i], b[4 * i + 1], b[4 * i + 2], b[4 * i + 3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
        }
        let [mut a, mut b_, mut c, mut d, mut e, mut f, mut g, mut hh] = *h;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ (!e & g);
            let t1 = hh.wrapping_add(s1).wrapping_add(ch).wrapping_add(K[i]).wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let mj = (a & b_) ^ (a & c) ^ (b_ & c);
            let t2 = s0.wrapping_add(mj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b_;
            b_ = a;
            a = t1.wrapping_add(t2);
        }
        for (x, y) in h.iter_mut().zip([a, b_, c, d, e, f, g, hh]) {
            *x = x.wrapping_add(y);
        }
    }
    fn update(&mut self, mut d: &[u8]) {
        self.len += d.len() as u64;
        if self.n > 0 {
            let take = (64 - self.n).min(d.len());
            self.buf[self.n..self.n + take].copy_from_slice(&d[..take]);
            self.n += take;
            d = &d[take..];
            if self.n == 64 {
                let b = self.buf;
                Self::block(&mut self.h, &b);
                self.n = 0;
            }
        }
        while d.len() >= 64 {
            Self::block(&mut self.h, &d[..64]);
            d = &d[64..];
        }
        if !d.is_empty() {
            self.buf[..d.len()].copy_from_slice(d);
            self.n = d.len();
        }
    }
    fn finish(mut self) -> String {
        let bits = self.len * 8;
        let mut pad = vec![0x80u8];
        while (self.len as usize + pad.len()) % 64 != 56 {
            pad.push(0);
        }
        pad.extend(bits.to_be_bytes());
        let l = self.len;
        self.update(&pad);
        self.len = l;
        self.h.iter().map(|x| format!("{x:08x}")).collect()
    }
}
fn plane_bytes(p: &PixelData) -> Vec<u8> {
    match p {
        PixelData::U8(v) => v.iter().flat_map(|&x| (x as u16).to_le_bytes()).collect(),
        PixelData::U16(v) => v.iter().flat_map(|x| x.to_le_bytes()).collect(),
    }
}
fn hash_frame(f: &Frame) -> String {
    let mut out = Vec::new();
    for p in [&f.y, &f.u, &f.v] {
        let mut s = Sha256::new();
        s.update(&plane_bytes(p));
        out.push(s.finish());
    }
    out.join(" ")
}

fn split_annexb(d: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= d.len() {
        if d[i] == 0 && d[i + 1] == 0 && d[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut out = Vec::new();
    for (k, &s) in starts.iter().enumerate() {
        let mut e = if k + 1 < starts.len() { starts[k + 1] - 3 } else { d.len() };
        while e > s && d[e - 1] == 0 {
            e -= 1;
        }
        if e - s >= 2 {
            out.push(&d[s..e]);
        }
    }
    out
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let (mut path, mut frames, mut hash, mut repeat, mut threads) = (None, usize::MAX, None, 1, 1);
    let mut use_plan = false;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--frames" => { frames = args[i + 1].parse().unwrap(); i += 2 }
            "--hash" => { hash = Some(args[i + 1].clone()); i += 2 }
            "--repeat" => { repeat = args[i + 1].parse().unwrap(); i += 2 }
            "--plan" => { use_plan = true; i += 1 }
            "--threads" => { threads = args[i + 1].parse().unwrap(); i += 2 }
            s => { path = Some(s.to_string()); i += 1 }
        }
    }
    let data = std::fs::read(path.expect("file")).unwrap();
    let nals = split_annexb(&data);
    // --plan: prune the predecessor-GOP prefix as the WASM worker does and drop its output pictures.
    let (keep, skip_total) = if use_plan {
        let heads: Vec<&[u8]> = nals.iter().map(|n| {
            let t = (n[0] >> 1) & 63;
            if t < 32 { &n[..n.len().min(aether_hevc::plan::SLICE_HEAD)] } else if matches!(t, 32 | 33 | 34 | 36 | 37) { n } else { &n[..2] }
        }).collect();
        match aether_hevc::plan::plan(&heads) {
            Some(p) => { eprintln!("plan: keep {}/{} NALs, drop first {} output pictures", p.keep.iter().filter(|k| **k).count(), p.keep.len(), p.skip); (Some(p.keep), p.skip as usize) }
            None => { eprintln!("plan: none"); (None, 0) }
        }
    } else { (None, 0) };
    let mut best = f64::MAX;
    for run in 0..repeat {
        let mut dec = Decoder::with_limits(DecodeLimits {
            max_picture_pixels: 3840 * 2160,
            max_picture_memory_bytes: 256 * 1024 * 1024,
        })
        .with_display_order()
        .with_threads(threads);
        let mut out = hash.as_ref().filter(|_| run == 0).map(|h| std::io::BufWriter::new(std::fs::File::create(h).unwrap()));
        let mut n = 0usize;
        let mut hash_time = 0.0;
        let t = Instant::now();
        let mut skip_left = skip_total;
        let mut emit = |f: Frame, n: &mut usize, hash_time: &mut f64| {
            if skip_left > 0 { skip_left -= 1; return; }
            if let Some(o) = out.as_mut() {
                let t = Instant::now();
                writeln!(o, "{}", hash_frame(&f)).unwrap();
                *hash_time += t.elapsed().as_secs_f64();
            }
            *n += 1;
        };
        'outer: for (ni, raw) in nals.iter().enumerate() {
            if keep.as_ref().is_some_and(|k| !k[ni]) { continue; }
            let ty = (raw[0] >> 1) & 63;
            let layer = ((raw[0] & 1) << 5) | (raw[1] >> 3);
            if ty >= 62 || layer != 0 { continue; }
            let nal = parse_nal(raw).expect("nal");
            dec.decode_nal(&nal).expect("decode");
            while let Some(f) = dec.next_output() {
                emit(f, &mut n, &mut hash_time);
                if n >= frames { break 'outer; }
            }
        }
        while n < frames {
            let Some(f) = dec.flush() else { break };
            emit(f, &mut n, &mut hash_time);
        }
        let el = t.elapsed().as_secs_f64() - hash_time;
        best = best.min(el);
        eprintln!("run {run}: {n} frames in {el:.3}s = {:.2} fps", n as f64 / el);
        if run + 1 == repeat { println!("BEST {:.2} fps ({n} frames, {best:.3}s)", n as f64 / best); }
    }
}
