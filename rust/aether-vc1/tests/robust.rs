//! Robustness: malformed and mutated input must never panic, hang or grow memory without bound.
use aether_vc1::decoder::{split_access_units, MAX_OUTPUT_QUEUE};
use aether_vc1::testgen::{generate, GenConfig};
use aether_vc1::Decoder;

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            0
        } else {
            (self.next() >> 11) as usize % n
        }
    }
}

fn drain(dec: &mut Decoder) -> usize {
    let mut n = 0;
    while let Some(f) = dec.take_frame() {
        // touch every plane so that out-of-range plane geometry would panic here
        for p in 0..3 {
            let (d, s) = f.plane(p);
            let (w, h) = (f.plane_width(p), f.plane_height(p));
            assert!(s >= w && d.len() >= (h - 1) * s + w, "bad plane geometry");
        }
        let mut v = Vec::new();
        f.write_i420(&mut v);
        assert_eq!(v.len(), f.i420_size());
        n += 1;
    }
    n
}

fn decode_whole(data: &[u8]) {
    let mut dec = Decoder::new();
    dec.decode_annexg(data, |i| i as i64);
    assert!(dec.pending() <= MAX_OUTPUT_QUEUE);
    dec.flush();
    drain(&mut dec);
}

fn decode_units(data: &[u8]) {
    let mut dec = Decoder::new();
    for (i, (a, b)) in split_access_units(data).into_iter().enumerate() {
        let _ = dec.decode(&data[a..b], i as i64);
        assert!(dec.pending() <= MAX_OUTPUT_QUEUE);
        drain(&mut dec);
    }
    dec.flush();
    drain(&mut dec);
}

/// Packets as a Matroska demuxer would deliver them: start code of the frame unit removed,
/// sequence/entry point headers only in CodecPrivate.
fn decode_container_style(data: &[u8]) {
    let aus = split_access_units(data);
    if aus.is_empty() {
        return;
    }
    let mut dec = Decoder::new();
    let first = &data[aus[0].0..aus[0].1];
    // extradata: everything before the first frame start code
    let fpos = first.windows(4).position(|w| w == [0, 0, 1, 0x0d]).unwrap_or(0);
    let _ = dec.set_extradata(&first[..fpos]);
    for (i, (a, b)) in aus.iter().enumerate() {
        let au = &data[*a..*b];
        let p = au.windows(4).position(|w| w == [0, 0, 1, 0x0d]).map(|p| p + 4).unwrap_or(0);
        let _ = dec.decode(&au[p..], i as i64);
        drain(&mut dec);
    }
    dec.flush();
    drain(&mut dec);
}

fn mutate(data: &mut Vec<u8>, r: &mut Rng) {
    let n = data.len();
    if n == 0 {
        return;
    }
    match r.below(10) {
        0 => {
            for _ in 0..1 + r.below(16) {
                let p = r.below(n);
                data[p] ^= 1 << r.below(8);
            }
        }
        1 => {
            let p = r.below(n);
            let l = 1 + r.below(64.min(n - p));
            for b in &mut data[p..p + l] {
                *b = r.next() as u8;
            }
        }
        2 => {
            let p = r.below(n);
            let l = 1 + r.below(256.min(n - p));
            for b in &mut data[p..p + l] {
                *b = 0;
            }
        }
        3 => {
            let p = r.below(n);
            let l = 1 + r.below(64.min(n - p));
            data.drain(p..p + l);
        }
        4 => {
            let p = r.below(n);
            let l = 1 + r.below(64);
            let ins: Vec<u8> = (0..l).map(|_| r.next() as u8).collect();
            data.splice(p..p, ins);
        }
        5 => {
            let p = r.below(n);
            data.truncate(p);
        }
        6 => {
            // corrupt a start code type
            let mut pos = Vec::new();
            for i in 0..n.saturating_sub(3) {
                if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
                    pos.push(i + 3);
                }
            }
            if !pos.is_empty() {
                let p = pos[r.below(pos.len())];
                data[p] = [0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f, 0x00, 0x7f, 0xff][r.below(9)];
            }
        }
        7 => {
            // mutate the first bytes (sequence / entry point headers)
            let l = 40.min(n);
            let p = r.below(l);
            data[p] = r.next() as u8;
        }
        8 => {
            let a = r.below(n);
            let l = 1 + r.below(512.min(n - a));
            let chunk: Vec<u8> = data[a..a + l].to_vec();
            let b = r.below(n);
            data.splice(b..b, chunk);
        }
        _ => {
            // inject an extra field / slice marker
            let p = r.below(n);
            let code = [0x0b, 0x0c, 0x0d, 0x0e, 0x0f][r.below(5)];
            data.splice(p..p, [0, 0, 1, code]);
        }
    }
}

#[test]
fn mutated_generated_streams_do_not_panic() {
    // VC1_FUZZ_SCALE=n multiplies the amount of work (soak runs)
    let scale: u64 = std::env::var("VC1_FUZZ_SCALE").ok().and_then(|v| v.parse().ok()).unwrap_or(1);
    let mut r = Rng(0x1234_5678_9abc_def1);
    let mut total_frames = 0;
    for seed in 0..24u64 * scale {
        let cfg = GenConfig {
            width: 64 + 16 * (seed as usize % 5),
            height: 48 + 16 * (seed as usize % 4),
            frames: 5,
            interlace: (seed % 3) as i32 - 1,
            payload_bytes: 3000,
            fcm: -1,
            slices: seed % 2 == 0,
        };
        let base = generate(seed, &cfg);
        for _ in 0..24 {
            let mut d = base.clone();
            for _ in 0..1 + r.below(4) {
                mutate(&mut d, &mut r);
            }
            decode_whole(&d);
            decode_units(&d);
            decode_container_style(&d);
            total_frames += 1;
        }
    }
    assert!(total_frames > 0);
}

#[test]
fn random_garbage_and_degenerate_inputs() {
    let mut r = Rng(42);
    for len in [0usize, 1, 2, 3, 4, 5, 16, 33, 200, 5000] {
        let d: Vec<u8> = (0..len).map(|_| r.next() as u8).collect();
        decode_whole(&d);
        decode_units(&d);
        decode_container_style(&d);
    }
    // start codes only
    for code in 0u8..=255 {
        decode_whole(&[0, 0, 1, code]);
        decode_whole(&[0, 0, 1, code, 0xff, 0xff, 0xff, 0xff, 0xff, 0, 0, 1, 0x0d, 0x80]);
    }
    // all-ones / all-zero bodies after valid-looking markers
    for fill in [0x00u8, 0xff, 0xaa, 0x55] {
        for code in [0x0f, 0x0e, 0x0d, 0x0c, 0x0b] {
            let mut d = vec![0, 0, 1, 0x0f];
            d.extend(std::iter::repeat(fill).take(40));
            d.extend([0, 0, 1, 0x0e]);
            d.extend(std::iter::repeat(fill).take(40));
            d.extend([0, 0, 1, code]);
            d.extend(std::iter::repeat(fill).take(3000));
            decode_whole(&d);
            decode_units(&d);
        }
    }
}

#[test]
fn hostile_dimensions_are_rejected_or_bounded() {
    // sequence header claiming 8192x8192 with a plausible entry point
    let mut b = BitW::default();
    b.put(3, 2);
    b.put(3, 3);
    b.put(1, 2);
    b.put(0, 3);
    b.put(0, 5);
    b.put(0, 1);
    b.put(4095, 12);
    b.put(4095, 12);
    b.put(0, 1);
    b.put(1, 1);
    b.put(0, 1);
    b.put(0, 1);
    b.put(1, 1);
    b.put(0, 1);
    b.put(0, 1);
    b.put(0, 1);
    let seq = b.bytes();
    let mut e = BitW::default();
    for _ in 0..13 {
        e.put(0, 1);
    }
    e.put(0, 1);
    e.put(0, 1);
    e.put(0, 1);
    e.put(0, 1);
    let ep = e.bytes();
    let mut d = vec![0, 0, 1, 0x0f];
    d.extend(seq);
    d.extend([0, 0, 1, 0x0e]);
    d.extend(ep);
    for k in 0..8 {
        d.extend([0, 0, 1, 0x0d, 0xc0 | k, 0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc, 0xde, 0xf0]);
    }
    decode_whole(&d);
    decode_units(&d);
}

#[test]
fn dimension_switches_mid_stream() {
    let a = generate(1, &GenConfig { width: 64, height: 48, frames: 3, interlace: 0, payload_bytes: 2000, fcm: -1, slices: false });
    let b = generate(2, &GenConfig { width: 112, height: 80, frames: 3, interlace: 1, payload_bytes: 2000, fcm: -1, slices: false });
    let c = generate(3, &GenConfig { width: 64, height: 48, frames: 3, interlace: 1, payload_bytes: 2000, fcm: 2, slices: true });
    let mut cat = Vec::new();
    for _ in 0..3 {
        cat.extend_from_slice(&a);
        cat.extend_from_slice(&b);
        cat.extend_from_slice(&c);
    }
    decode_whole(&cat);
    decode_units(&cat);
    // and interleaved at random cut points
    let mut r = Rng(7);
    for _ in 0..40 {
        let mut d = Vec::new();
        for src in [&a, &b, &c, &a, &c] {
            let cut = r.below(src.len());
            d.extend_from_slice(&src[..cut]);
        }
        decode_whole(&d);
    }
}

#[test]
fn output_queue_is_bounded_when_host_never_drains() {
    let s = generate(5, &GenConfig { width: 64, height: 48, frames: 40, interlace: 0, payload_bytes: 1500, fcm: -1, slices: false });
    let mut dec = Decoder::new();
    for (i, (a, b)) in split_access_units(&s).into_iter().enumerate() {
        let _ = dec.decode(&s[a..b], i as i64);
        assert!(dec.pending() <= MAX_OUTPUT_QUEUE);
    }
    assert!(dec.pending() <= MAX_OUTPUT_QUEUE);
}

#[derive(Default)]
struct BitW {
    bits: Vec<u8>,
}
impl BitW {
    fn put(&mut self, v: u32, n: u32) {
        for i in (0..n).rev() {
            self.bits.push(((v >> i) & 1) as u8);
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

/// Mutations of a real 1080p Blu-ray stream prefix (only when the conformance fixtures exist).
#[test]
fn mutated_real_stream_does_not_panic() {
    let dir = std::env::var_os("VC1_FIXTURES").map(std::path::PathBuf::from).unwrap_or_else(|| std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures"));
    let Ok(full) = std::fs::read(dir.join("dtshra51.vc1")) else {
        eprintln!("SKIP: fixtures missing (run tools/regen-fixtures.sh)");
        return;
    };
    let scale: usize = std::env::var("VC1_FUZZ_SCALE").ok().and_then(|v| v.parse().ok()).unwrap_or(1);
    let aus = split_access_units(&full);
    let end = aus[7.min(aus.len() - 1)].1;
    let base = &full[..end];
    let mut r = Rng(99);
    for _ in 0..6 * scale {
        let mut d = base.to_vec();
        for _ in 0..1 + r.below(6) {
            mutate(&mut d, &mut r);
        }
        decode_whole(&d);
    }
}
