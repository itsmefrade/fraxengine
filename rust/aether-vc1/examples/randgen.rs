//! Differential-test stream generator (see `aether_vc1::testgen`).
//!
//! usage: randgen SEED OUT.vc1 [W H FRAMES INTERLACE(-1 random|0|1) PAYLOAD_BYTES FCM(-1 mixed|0|1|2) SLICES(0|1)]
//!
//! Mixing frame coding modes can make direct-mode B macroblocks read motion vectors that FFmpeg
//! never wrote (stale pool memory), so strict comparisons should use a fixed FCM.  Avoid widths
//! below 64: FFmpeg's chroma edge buffers alias for very small strides.
use aether_vc1::testgen::{generate, GenConfig};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let seed: u64 = args[1].parse().unwrap();
    let g = |i: usize, d: i64| -> i64 { args.get(i).and_then(|s| s.parse().ok()).unwrap_or(d) };
    let cfg = GenConfig {
        width: g(3, 96) as usize,
        height: g(4, 80) as usize,
        frames: g(5, 8) as usize,
        interlace: g(6, -1) as i32,
        payload_bytes: g(7, 8000) as usize,
        fcm: g(8, -1) as i32,
        slices: g(9, 0) != 0,
    };
    let s = generate(seed, &cfg);
    std::fs::write(&args[2], &s).unwrap();
    eprintln!("seed {} {:?} bytes {}", seed, cfg, s.len());
}
