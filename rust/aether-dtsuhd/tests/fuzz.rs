//! Deterministic mutation tests: parsing and decoding must never panic.
mod fixtures;
use aether_dtsuhd::Decoder;

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

fn mutate(rng: &mut Rng, src: &[u8]) -> Vec<u8> {
    let mut v = src.to_vec();
    match rng.below(6) {
        0 => {
            // flip a few bits
            for _ in 0..1 + rng.below(8) {
                let i = rng.below(v.len());
                v[i] ^= 1 << rng.below(8);
            }
        }
        1 => {
            // truncate
            let n = rng.below(v.len());
            v.truncate(n);
        }
        2 => {
            // overwrite a run with random bytes
            let i = rng.below(v.len());
            let n = rng.below(32);
            for k in i..(i + n).min(v.len()) {
                v[k] = rng.next() as u8;
            }
        }
        3 => {
            // append garbage
            for _ in 0..rng.below(64) {
                v.push(rng.next() as u8);
            }
        }
        4 => {
            // zero a run
            let i = rng.below(v.len());
            let n = rng.below(64);
            for k in i..(i + n).min(v.len()) {
                v[k] = 0;
            }
        }
        _ => {
            // random bytes after a valid sync word
            let n = 4 + rng.below(300);
            v = vec![0; n];
            for b in v.iter_mut() {
                *b = rng.next() as u8;
            }
            v[..4].copy_from_slice(&[0x40, 0x41, 0x1B, 0xF2]);
        }
    }
    v
}

#[test]
fn parse_never_panics_on_mutated_frames() {
    let frames = fixtures::frames();
    let mut rng = Rng(0x1234_5678_9abc_def1);
    let mut ok = 0;
    for it in 0..30_000 {
        let mut dec = Decoder::new();
        let _ = dec.parse_frame(&frames[0]);
        let src = &frames[it % frames.len()];
        let m = mutate(&mut rng, src);
        if dec.parse_frame(&m).is_ok() {
            ok += 1;
        }
    }
    eprintln!("mutated frames that still parsed: {ok}");
}

#[test]
fn decode_never_panics_on_mutated_frames() {
    let frames = fixtures::frames();
    let mut rng = Rng(0xdead_beef_cafe_f00d);
    for it in 0..1_500 {
        let mut dec = Decoder::new();
        let _ = dec.decode_frame(&frames[0]);
        let _ = dec.decode_frame(&frames[1]);
        let src = &frames[(it % (frames.len() - 1)) + 1];
        let m = mutate(&mut rng, src);
        let _ = dec.decode_frame(&m);
        // and one more frame to exercise state carried over from a damaged frame
        let _ = dec.decode_frame(&frames[2 % frames.len()]);
    }
}

#[test]
fn every_prefix_and_single_byte_corruption_of_each_fixture_is_survivable() {
    let frames = fixtures::frames();
    for f in &frames {
        let mut dec = Decoder::new();
        let _ = dec.decode_frame(&frames[0]);
        for n in 0..f.len() {
            let _ = dec.parse_frame(&f[..n]);
        }
        for i in 0..f.len().min(400) {
            let mut m = f.clone();
            m[i] ^= 0xA5;
            let mut d = Decoder::new();
            let _ = d.decode_frame(&frames[0]);
            let _ = d.decode_frame(&m);
        }
    }
}

#[test]
fn ace_streams_never_panic_on_random_payloads() {
    use aether_dtsuhd::ace::{AceStream, FrameCtx};
    let mut rng = Rng(0x0bad_5eed_1234_4321);
    for round in 0..400 {
        let stereo = round % 2 == 0;
        let mut s = AceStream::new(stereo);
        for frame in 0..6 {
            let len = 1 + rng.below(if round % 7 == 0 { 3000 } else { 400 });
            let mut payload: Vec<u8> = (0..len).map(|_| rng.next() as u8).collect();
            // bias towards coded streams: coding mode "full" is the first bit set
            if rng.below(4) != 0 {
                payload[0] |= 0x80;
            }
            let ctx = FrameCtx {
                sampling_rate: if rng.below(2) == 0 { 48000 } else { 44100 },
                is_predictive: frame > 0 && rng.below(3) != 0,
                enable_deemphasis: rng.below(2) == 0,
                bandwidth_mode: rng.below(4),
            };
            let _ = s.decode_frame(&payload, &ctx);
            for ch in &s.out {
                // output is always finite
                assert!(ch.iter().all(|v| v.is_finite()));
            }
        }
    }
}

#[test]
fn lfe_and_metadata_never_panic_on_random_bytes() {
    use aether_dtsuhd::bitreader::BitReader;
    use aether_dtsuhd::lfe::LfeChannel;
    use aether_dtsuhd::metadata::parse_md_chunk;
    let mut rng = Rng(0x5eed_5eed_5eed_5eed);
    let mut lfe = LfeChannel::new();
    for _ in 0..2000 {
        let len = 1 + rng.below(200);
        let data: Vec<u8> = (0..len).map(|_| rng.next() as u8).collect();
        let mut br = BitReader::new(&data);
        let cur = lfe.read_decode(&mut br, 1024);
        let out = lfe.push_and_upsample(&cur, 1024);
        assert!(out.iter().all(|v| v.is_finite()));
        let mut chunk = data.clone();
        chunk[0] = 0x01;
        for &(full, sync) in &[(true, true), (false, true), (true, false), (false, false)] {
            let _ = parse_md_chunk(
                &chunk,
                chunk.len(),
                rng.below(2) == 0,
                sync,
                full,
                1024,
                true,
                rng.below(2) == 0,
            );
        }
    }
}
