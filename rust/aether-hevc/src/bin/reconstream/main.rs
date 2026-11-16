//! Dev tool for the reconstruction command stream (rust_h265 feature `recon-stream`, format in
//! `web/gpu-recon/FORMAT-STREAM.md`).
//!
//! reconstream verify <file.hevc> [--frames N] [--hash out.txt] [--entropy-only] [--threads N]
//!     Decodes with the recorder, packs every event to the u32 layout and parses it back, rebuilds every picture
//!     from the parsed stream alone (`rust_h265::recon::Replayer`) and compares each output frame with the
//!     decoder's. `--hash` writes the replay's per-frame SHA-256 (same lines as hevcbench / tools/verify.sh).
//!     `--entropy-only` records with reconstruction off in the decoder (the GPU configuration). Without it the
//!     boundary strengths resolved from each picture's edge codes are also compared with the decoder's own.
//! reconstream dump <file.hevc> <out-dir> [--pictures N] [--no-stages]
//!     Writes the stream of the first N pictures (decode order) to <out-dir>/stream.rcs and their stage pictures
//!     (prediction, residual, pre-deblock, post-deblock, post-SAO) next to it, plus index.json, and the decoder's
//!     boundary strengths as a block map with strengths in bits 0..7 (`picNNN.bs.u32`, FORMAT-deblock.md).
//!     `--no-stages` leaves the stage pictures out (25 MB each at 4K).
//! reconstream bench <file.hevc> [--frames N] [--repeat R] [--threads N] --mode full|record|entropy|entropy-nopack
//!     full: plain decode; record: decode + recording + packing; entropy: parse + recording + packing, no
//!     reconstruction (what stays on the CPU with GPU reconstruction). Prints an FNV-1a hash of the packed stream
//!     (record and entropy modes): equal hashes before and after a change mean the stream did not change.
mod sha;

use rust_h265::recon::{Event, Replayer, STREAM_MAGIC, STREAM_VERSION, Stages, decode_event, encode_event};
use rust_h265::{DecodeLimits, Decoder, Frame, parse_nal};
use std::io::Write;
use std::time::Instant;

fn decoder() -> Decoder {
    Decoder::with_limits(DecodeLimits {
        max_picture_pixels: 3840 * 2160,
        max_picture_memory_bytes: 256 * 1024 * 1024,
    })
    .with_display_order()
}

/// Base-layer NAL units of an Annex B file, as hevcbench feeds them.
fn nals(data: &[u8]) -> Vec<&[u8]> {
    sha::split_annexb(data)
        .into_iter()
        .filter(|raw| {
            let ty = (raw[0] >> 1) & 63;
            let layer = ((raw[0] & 1) << 5) | (raw[1] >> 3);
            ty < 62 && layer == 0
        })
        .collect()
}

fn arg(args: &[String], k: &str) -> Option<String> {
    args.iter().position(|a| a == k).map(|i| args[i + 1].clone())
}

/// `--threads N` (default 1): with recording, pictures stay sequential and the CTB rows of WPP slices decode on N
/// threads; the stream must not change (same `bench` hash).
fn threads(args: &[String]) -> usize {
    arg(args, "--threads").map_or(1, |v| v.parse().unwrap())
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("verify") => verify(&args),
        Some("dump") => dump(&args),
        Some("bench") => bench(&args),
        _ => eprintln!("usage: reconstream verify|dump|bench <file.hevc> ..."),
    }
}

/// Decode, round trip every event through the packed layout, replay, compare.
fn verify(args: &[String]) {
    let data = std::fs::read(&args[2]).unwrap();
    let frames: usize = arg(args, "--frames").map_or(usize::MAX, |v| v.parse().unwrap());
    let mut hash_out = arg(args, "--hash").map(|h| std::io::BufWriter::new(std::fs::File::create(h).unwrap()));
    // --entropy-only: the decoder only parses (as with GPU reconstruction), so its frames carry no samples and
    // the replay is checked against the --hash reference list alone.
    let entropy = args.iter().any(|a| a == "--entropy-only");
    let mut dec = decoder().with_threads(threads(args)).with_recon_stream(entropy);
    dec.recon_recorder().unwrap().check_bs = !entropy;
    let mut rep = Replayer::new();
    let (mut n, mut mismatches, mut pictures) = (0usize, 0usize, 0usize);
    let (mut words_total, mut tus, mut coefs, mut pus, mut intra, mut max_slot, mut max_waves) = (0usize, 0, 0, 0, 0, 0u32, 0u32);
    let (mut weighted, mut pcm) = (0usize, 0usize);
    let mut dec_frames: std::collections::VecDeque<Frame> = Default::default();
    let t = Instant::now();
    let mut pump = |dec: &mut Decoder, dec_frames: &mut std::collections::VecDeque<Frame>, n: &mut usize| -> bool {
        let mut buf = Vec::new();
        for ev in dec.take_recon_events() {
            buf.clear();
            encode_event(&ev, &mut buf);
            let (back, used) = decode_event(&buf).expect("parse event");
            assert_eq!(used, buf.len());
            assert!(back == ev || matches!(&ev, Event::Picture(p) if p.unsupported.is_some()), "event changed in the packed round trip");
            words_total += buf.len();
            if let Event::Picture(p) = &back {
                pictures += 1;
                tus += p.tus.len();
                coefs += p.coeffs.len();
                pus += p.pus.len();
                intra += p.intra.len();
                weighted += p.pus.iter().filter(|u| u.weighted).count();
                pcm += p.pcm.len();
                max_slot = max_slot.max(p.header.slot);
                max_waves = max_waves.max(p.header.waves);
            }
            match rep.event(&back) {
                Ok(Some(f)) => {
                    let h = sha::hash_frame(&f);
                    let d = dec_frames.pop_front().expect("decoder frame for each replay output");
                    if !entropy && sha::hash_frame(&d) != h {
                        mismatches += 1;
                        eprintln!("frame {n} (poc {}): replay differs from decoder", f.pic_order_cnt);
                    }
                    if let Some(o) = hash_out.as_mut() {
                        writeln!(o, "{h}").unwrap();
                    }
                    *n += 1;
                    if *n >= frames {
                        return true;
                    }
                }
                Ok(None) => {}
                Err(e) => panic!("replay: {e}"),
            }
        }
        false
    };
    'outer: for raw in nals(&data) {
        dec.decode_nal(&parse_nal(raw).expect("nal")).expect("decode");
        while let Some(f) = dec.next_output() {
            dec_frames.push_back(f);
        }
        if pump(&mut dec, &mut dec_frames, &mut n) {
            break 'outer;
        }
    }
    if n < frames {
        while let Some(f) = dec.flush() {
            dec_frames.push_back(f);
        }
        pump(&mut dec, &mut dec_frames, &mut n);
    }
    println!(
        "{n} frames, {pictures} pictures replayed from the packed stream, {mismatches} mismatches vs decoder ({:.1}s)",
        t.elapsed().as_secs_f64()
    );
    let r = dec.recon_recorder().unwrap();
    let bs_bad = r.bs_mismatches;
    if !entropy {
        println!("boundary strengths from edge codes: {} pictures checked against the decoder, {bs_bad} block map words differ", r.bs_checked);
    }
    println!(
        "stream: {:.2} MB/picture avg, {:.0} TUs, {:.0} coefficient entries, {:.0} PUs, {:.0} intra blocks per picture; slots 0..={max_slot}; max waves {max_waves}; {weighted} weighted PUs, {pcm} PCM CUs in all",
        words_total as f64 * 4.0 / pictures.max(1) as f64 / 1e6,
        tus as f64 / pictures.max(1) as f64,
        coefs as f64 / pictures.max(1) as f64,
        pus as f64 / pictures.max(1) as f64,
        intra as f64 / pictures.max(1) as f64,
    );
    if mismatches > 0 || bs_bad > 0 {
        std::process::exit(1);
    }
}

fn write_planes_u16(path: &std::path::Path, planes: &[Vec<u16>; 3]) {
    let mut b = Vec::new();
    for p in planes {
        for s in p {
            b.extend_from_slice(&s.to_le_bytes());
        }
    }
    std::fs::write(path, b).unwrap();
}

fn write_planes_i16(path: &std::path::Path, planes: &[Vec<i16>; 3]) {
    let mut b = Vec::new();
    for p in planes {
        for s in p {
            b.extend_from_slice(&s.to_le_bytes());
        }
    }
    std::fs::write(path, b).unwrap();
}

/// Stream file and stage pictures of the first N pictures.
fn dump(args: &[String]) {
    let data = std::fs::read(&args[2]).unwrap();
    let dir = std::path::PathBuf::from(&args[3]);
    std::fs::create_dir_all(&dir).unwrap();
    let max_pics: usize = arg(args, "--pictures").map_or(4, |v| v.parse().unwrap());
    let stages = !args.iter().any(|a| a == "--no-stages");
    let mut dec = decoder().with_recon_stream(false);
    let r = dec.recon_recorder().unwrap();
    r.check_bs = true;
    r.keep_decoder_bs = true;
    let mut rep = Replayer::new();
    rep.capture_stages = stages;
    let mut words = vec![STREAM_MAGIC, STREAM_VERSION];
    let mut index = Vec::new();
    let mut pics = 0usize;
    let mut handle = |ev: Event, words: &mut Vec<u32>, index: &mut Vec<String>, pics: &mut usize| {
        let at = words.len();
        encode_event(&ev, words);
        let (back, _) = decode_event(&words[at..]).unwrap();
        rep.event(&back).unwrap();
        let entry = match &back {
            Event::Picture(p) => {
                let h = &p.header;
                if stages {
                    let s: &Stages = rep.stages.as_ref().unwrap();
                    let base = dir.join(format!("pic{:03}", h.pic_id));
                    write_planes_u16(&base.with_extension("pred.u16"), &s.pred);
                    write_planes_i16(&base.with_extension("residual.i16"), &s.residual);
                    write_planes_u16(&base.with_extension("pre_deblock.u16"), &s.pre_deblock);
                    write_planes_u16(&base.with_extension("post_deblock.u16"), &s.post_deblock);
                    write_planes_u16(&base.with_extension("post_sao.u16"), &s.post_sao);
                }
                let refs: Vec<u32> = p
                    .pus
                    .iter()
                    .flat_map(|u| u.slot)
                    .filter(|&s| s != rust_h265::recon::NONE)
                    .collect::<std::collections::BTreeSet<u32>>()
                    .into_iter()
                    .collect();
                *pics += 1;
                format!(
                    "{{\"event\":\"picture\",\"word\":{at},\"pic_id\":{},\"slot\":{},\"poc\":{},\"width\":{},\"height\":{},\"bit_depth\":[{},{}],\"tus\":{},\"coefs\":{},\"pus\":{},\"intra\":{},\"pcm\":{},\"waves\":{},\"ref_slots\":{:?},\"stages\":\"pic{:03}\"}}",
                    h.pic_id, h.slot, h.poc, h.width, h.height, h.bit_depth_y, h.bit_depth_c, p.tus.len(), p.coeffs.len(),
                    p.pus.len(), p.intra.len(), p.pcm.len(), h.waves, refs, h.pic_id
                )
            }
            Event::Generated(g) => format!("{{\"event\":\"generated\",\"word\":{at},\"pic_id\":{},\"slot\":{},\"poc\":{}}}", g.pic_id, g.slot, g.poc),
            Event::Output { pic_id, slot } => format!("{{\"event\":\"output\",\"word\":{at},\"pic_id\":{pic_id},\"slot\":{slot}}}"),
            Event::Discard { pic_id, slot } => format!("{{\"event\":\"discard\",\"word\":{at},\"pic_id\":{pic_id},\"slot\":{slot}}}"),
        };
        index.push(entry);
    };
    for raw in nals(&data) {
        dec.decode_nal(&parse_nal(raw).expect("nal")).expect("decode");
        while dec.next_output().is_some() {}
        for (id, map) in std::mem::take(&mut dec.recon_recorder().unwrap().decoder_bs) {
            if (id as usize) < max_pics + 16 {
                let b: Vec<u8> = map.iter().flat_map(|w| w.to_le_bytes()).collect();
                std::fs::write(dir.join(format!("pic{id:03}.bs.u32")), b).unwrap();
            }
        }
        for ev in dec.take_recon_events() {
            if pics < max_pics || !matches!(ev, Event::Picture(_)) {
                handle(ev, &mut words, &mut index, &mut pics);
            }
        }
        if pics >= max_pics {
            break;
        }
    }
    let bytes: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
    std::fs::write(dir.join("stream.rcs"), bytes).unwrap();
    std::fs::write(dir.join("index.json"), format!("[\n{}\n]\n", index.join(",\n"))).unwrap();
    let r = dec.recon_recorder().unwrap();
    println!(
        "{pics} pictures, {} stream words -> {}; boundary strengths from edge codes: {} pictures checked, {} block map words differ",
        words.len(),
        dir.display(),
        r.bs_checked,
        r.bs_mismatches
    );
}

/// CPU time of full decode vs recording vs entropy-only.
fn bench(args: &[String]) {
    let data = std::fs::read(&args[2]).unwrap();
    let frames: usize = arg(args, "--frames").map_or(usize::MAX, |v| v.parse().unwrap());
    let repeat: usize = arg(args, "--repeat").map_or(1, |v| v.parse().unwrap());
    let mode = arg(args, "--mode").unwrap_or_else(|| "full".into());
    let nals = nals(&data);
    let mut best = f64::MAX;
    for run in 0..repeat {
        // Reference output as the player's session: no output copy in any mode.
        let mut dec = match mode.as_str() {
            "full" => decoder().with_threads(threads(args)).with_ref_output(),
            "record" => decoder().with_threads(threads(args)).with_ref_output().with_recon_stream(false),
            "entropy" | "entropy-nopack" => decoder().with_threads(threads(args)).with_ref_output().with_recon_stream(true),
            m => panic!("unknown mode {m}"),
        };
        let pack = mode == "record" || mode == "entropy";
        let (mut n, mut bytes) = (0usize, 0usize);
        let mut buf = Vec::new();
        let mut hash = 0xcbf29ce484222325u64;
        let t = Instant::now();
        'outer: for raw in &nals {
            dec.decode_nal(&parse_nal(raw).expect("nal")).expect("decode");
            while let Some(f) = dec.next_output() {
                dec.recycle_frame(f);
                n += 1;
                if n >= frames {
                    break 'outer;
                }
            }
            let events = dec.take_recon_events();
            if pack {
                for ev in events {
                    buf.clear();
                    encode_event(&ev, &mut buf);
                    bytes += buf.len() * 4;
                    for w in &buf {
                        hash = (hash ^ *w as u64).wrapping_mul(0x100000001b3);
                    }
                }
            }
        }
        while n < frames && dec.flush().is_some() {
            n += 1;
        }
        let el = t.elapsed().as_secs_f64();
        best = best.min(el);
        eprintln!("run {run}: {mode}: {n} frames in {el:.3}s = {:.2} fps, stream {:.2} MB/frame", n as f64 / el, bytes as f64 / n.max(1) as f64 / 1e6);
        if run + 1 == repeat {
            println!("BEST {mode} {:.2} fps ({n} frames, {best:.3}s) stream {hash:016x}", n as f64 / best);
        }
    }
}
