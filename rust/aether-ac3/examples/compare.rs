//! Developer tool: per-frame comparison of the decoder against a reference
//! dump produced by FFmpeg (`-f f32le`, interleaved).
//! Usage: compare <stream> <reference.f32le> [max frames to list]

use aether_ac3::{access_units, Decoder};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let stream = std::fs::read(&args[1]).unwrap();
    let reference: Vec<f32> = std::fs::read(&args[2])
        .unwrap()
        .chunks_exact(4)
        .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .collect();
    let list: usize = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(10);
    let mut dec = Decoder::new();
    dec.set_ffmpeg_bug_compat(true);
    let mut off = 0usize;
    let mut bad = 0;
    let (mut se, mut sr) = (0f64, 0f64);
    for (n, au) in access_units(&stream).into_iter().enumerate() {
        let f = dec.decode(au).unwrap();
        let ch = f.channels();
        let mut buf = vec![0f32; f.frames * ch];
        f.interleave_into(&mut buf);
        let r = &reference[off..off + buf.len()];
        let mut max = 0f32;
        let mut which = 0;
        for (i, (a, b)) in buf.iter().zip(r).enumerate() {
            se += ((a - b) as f64).powi(2);
            sr += (*b as f64).powi(2);
            let e = (a - b).abs();
            if e > max {
                max = e;
                which = i;
            }
        }
        if max > 1e-5 && bad < list {
            let mut per_block = vec![0f32; f.frames / 256];
            for (i, (a, b)) in buf.iter().zip(r).enumerate() {
                let blk = (i / ch) / 256;
                per_block[blk] = per_block[blk].max((a - b).abs());
            }
            println!("  per-block max err: {:?}", per_block);
            println!(
                "frame {n}: max err {max:e} at sample {} ch {}",
                which / ch,
                which % ch
            );
            bad += 1;
        }
        off += buf.len();
    }
    println!("{:?}", dec.stats());
    println!(
        "rms err {:e} rms ref {:e}",
        (se / off as f64).sqrt(),
        (sr / off as f64).sqrt()
    );
}
