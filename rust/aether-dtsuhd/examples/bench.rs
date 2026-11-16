//! Native speed measurement. Usage: bench [file.mp4]
#[path = "support/mp4lite.rs"]
mod mp4lite;
use aether_dtsuhd::bench::synthetic_stream_load;
use aether_dtsuhd::Decoder;
use std::time::Instant;

fn main() {
    let frames = 2000;
    let t = Instant::now();
    let (ok, sum) = synthetic_stream_load(frames, 1);
    let dt = t.elapsed().as_secs_f64();
    let audio = frames as f64 * 1024.0 / 48000.0;
    println!("synthetic 5.1.4 full-load: {ok} stream frames in {dt:.3}s for {audio:.1}s of audio -> {:.0}x real time (checksum {sum:.3})", audio / dt);
    if let Some(path) = std::env::args().nth(1) {
        let (_, samples) = mp4lite::read_audio_samples(&path);
        let t = Instant::now();
        let mut dec = Decoder::new();
        let mut n = 0;
        for s in &samples {
            if dec.decode_frame(s).is_ok() {
                n += 1;
            }
        }
        let dt = t.elapsed().as_secs_f64();
        let audio = samples.len() as f64 * 1024.0 / 48000.0;
        println!(
            "lab sample: {n}/{} frames, {dt:.3}s for {audio:.1}s of audio -> {:.0}x real time",
            samples.len(),
            audio / dt
        );
    }
}
