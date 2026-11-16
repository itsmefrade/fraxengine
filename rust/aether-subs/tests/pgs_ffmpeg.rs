//! PGS decoding against the real POCAWE_Sample.mkv subtitle track: display set timings against
//! `ffprobe -show_packets`, bitmaps against FFmpeg's own rendering (overlay on a grey frame).
//! `fixtures/pocawe.sup` (git-ignored) is `ffmpeg -map 0:s:0 -c copy -f sup` of that file, so
//! the bitmap checks need it and ffmpeg; the MKV timing check also needs the lab media.

use aether_subs::pgs::{self, Display, PgsDecoder};
use std::path::{Path, PathBuf};
use std::process::Command;

const LAB: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../test-media/POCAWE_Sample.mkv");

fn have_ffmpeg() -> bool {
    Command::new("ffmpeg").arg("-version").output().map(|o| o.status.success()).unwrap_or(false)
}

fn sup_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/pocawe.sup")
}

/// The .sup is derived from lab media and git-ignored; without it these tests skip.
fn have_sup() -> bool {
    let ok = sup_path().is_file();
    if !ok {
        eprintln!("skip: {} absent (git-ignored fixture)", sup_path().display());
    }
    ok
}

fn scratch(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("aether-subs-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn displays() -> Vec<Display> {
    pgs::parse_sup(&std::fs::read(sup_path()).unwrap()).unwrap()
}

/// pts in ms of every subtitle packet, from ffprobe.
fn probe_pts_ms(file: &Path) -> Vec<i64> {
    let out = Command::new("ffprobe")
        .args(["-v", "error", "-select_streams", "s:0", "-show_packets", "-of", "csv=p=0", "-show_entries", "packet=pts_time"])
        .arg(file)
        .output()
        .expect("ffprobe");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.trim().parse::<f64>().ok())
        .map(|t| (t * 1000.0).round() as i64)
        .collect::<Vec<i64>>()
        .into_iter()
        .fold(Vec::new(), |mut v, t| {
            // The sup muxer writes one packet per segment: keep the first of each pts.
            if v.last() != Some(&t) {
                v.push(t);
            }
            v
        })
}

#[test]
fn display_sets_have_expected_shape() {
    if !have_sup() {
        return;
    }
    let d = displays();
    assert_eq!(d.len(), 21, "21 display sets in the sample");
    assert!(d.iter().all(|x| x.video_width == 1920 && x.video_height == 1080));
    // Alternating content and clear display sets, starts strictly increasing.
    assert!(d.windows(2).all(|w| w[0].start_us < w[1].start_us));
    let content = d.iter().filter(|x| !x.clear).count();
    assert!(content >= 10, "{content} content display sets");
    for x in d.iter().filter(|x| !x.clear) {
        assert!(!x.bitmaps.is_empty());
        for b in &x.bitmaps {
            assert_eq!(b.rgba.len(), (b.width * b.height * 4) as usize);
            assert!(b.x >= 0 && b.y >= 0 && b.x + b.width as i32 <= 1920 && b.y + b.height as i32 <= 1080);
            assert!(b.rgba.chunks_exact(4).any(|p| p[3] > 0), "bitmap is not empty");
        }
    }
}

#[test]
fn timings_match_ffprobe_packets() {
    if !have_sup() {
        return;
    }
    if !have_ffmpeg() {
        eprintln!("skip: no ffmpeg");
        return;
    }
    let d = displays();
    // The sup written by ffmpeg carries the packet pts; the MKV is the original source.
    let from_sup = probe_pts_ms(&sup_path());
    assert_eq!(from_sup.len(), d.len());
    for (x, ms) in d.iter().zip(&from_sup) {
        assert!((x.start_us / 1000 - ms).abs() <= 1, "{} vs {ms}", x.start_us / 1000);
    }
    if Path::new(LAB).exists() {
        let from_mkv = probe_pts_ms(Path::new(LAB));
        assert_eq!(from_mkv.len(), d.len());
        // ffmpeg rebased the sup to its first packet: a constant offset (the stream start time).
        let offset = from_mkv[0] - d[0].start_us / 1000;
        assert!(offset >= 0);
        for (x, ms) in d.iter().zip(&from_mkv) {
            assert!((x.start_us / 1000 + offset - ms).abs() <= 1, "mkv {} + {offset} vs {ms}", x.start_us / 1000);
        }
    }
}

#[test]
fn matroska_blocks_decode_like_sup() {
    if !have_sup() {
        return;
    }
    let sup = std::fs::read(sup_path()).unwrap();
    let want = pgs::parse_sup(&sup).unwrap();
    let mut dec = PgsDecoder::new();
    let mut got = Vec::new();
    for (pts, block) in pgs::sup_to_blocks(&sup).unwrap() {
        got.extend(dec.push(pts, &block));
    }
    pgs::finalize_ends(&mut got);
    assert_eq!(got, want);
}

fn composite(d: &Display, bg: u8) -> Vec<u8> {
    let (w, h) = (d.video_width as usize, d.video_height as usize);
    let mut img = vec![bg; w * h * 3];
    for b in &d.bitmaps {
        for row in 0..b.height as usize {
            for col in 0..b.width as usize {
                let p = &b.rgba[(row * b.width as usize + col) * 4..][..4];
                let (x, y) = (b.x as usize + col, b.y as usize + row);
                let o = (y * w + x) * 3;
                let a = p[3] as u32;
                for k in 0..3 {
                    img[o + k] = ((p[k] as u32 * a + img[o + k] as u32 * (255 - a) + 127) / 255) as u8;
                }
            }
        }
    }
    img
}

#[test]
fn bitmaps_match_ffmpeg_rendering() {
    if !have_sup() {
        return;
    }
    if !have_ffmpeg() {
        eprintln!("skip: no ffmpeg");
        return;
    }
    let d = displays();
    // Content display sets with a known end, at most 12, sampled in the middle of their span.
    let picks: Vec<(&Display, i64)> = d.iter().filter(|x| !x.clear && x.end_us.is_some()).map(|x| (x, x.start_us + (x.end_us.unwrap() - x.start_us) / 2)).take(12).collect();
    assert!(picks.len() >= 8);
    let fps = 10i64;
    // ffmpeg rebases the input to its first packet, so the overlay clock starts at that pts.
    let shift = d[0].start_us;
    let frames: Vec<i64> = picks.iter().map(|(_, t)| ((t - shift) * fps + 500_000) / 1_000_000).collect();
    let select = frames.iter().map(|n| format!("eq(n\\,{n})")).collect::<Vec<_>>().join("+");
    let dir = scratch("pgs");
    let raw = dir.join("out.rgb");
    let last = *frames.iter().max().unwrap();
    let status = Command::new("ffmpeg")
        .args(["-v", "error", "-y", "-f", "lavfi", "-i"])
        .arg(format!("color=c=0x808080:s=1920x1080:r={fps}:d={}", (last / fps) + 2))
        .arg("-i")
        .arg(sup_path())
        .arg("-filter_complex")
        .arg(format!("[0:v][1:s]overlay=format=auto,select='{select}'[o]"))
        .args(["-map", "[o]", "-fps_mode", "passthrough", "-pix_fmt", "rgb24", "-f", "rawvideo"])
        .arg(&raw)
        .status()
        .expect("ffmpeg");
    assert!(status.success());
    let data = std::fs::read(&raw).unwrap();
    let frame_len = 1920 * 1080 * 3;
    assert_eq!(data.len(), frame_len * picks.len(), "one frame per pick");
    let mut worst = 0i32;
    let mut over2 = 0usize;
    let mut total_changed = 0usize;
    for (k, (disp, _)) in picks.iter().enumerate() {
        let want = &data[k * frame_len..(k + 1) * frame_len];
        let ours = composite(disp, 0x80);
        let changed = ours.iter().filter(|&&v| v != 0x80).count();
        total_changed += changed;
        assert!(changed > 0, "display {k} draws something");
        for (a, b) in ours.iter().zip(want) {
            let diff = (*a as i32 - *b as i32).abs();
            worst = worst.max(diff);
            if diff > 2 {
                over2 += 1;
            }
        }
    }
    eprintln!("pgs pixel diff: {} frames, {total_changed} non-background samples, max channel diff {worst}, samples over 2: {over2}", picks.len());
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(over2, 0);
    assert!(worst <= 2);
}
