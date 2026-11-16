//! Text formats: an SRT file is converted with ffmpeg to ASS, WebVTT and MKV; all routes must
//! produce the same cues in the shared model. A hand written ASS script covers styles and
//! override tags.

use aether_subs::{Cue, Rgba, TextFormat, TextTrack};
use std::path::{Path, PathBuf};
use std::process::Command;

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)
}

fn ffmpeg(args: &[&str]) -> bool {
    Command::new("ffmpeg").args(["-v", "error", "-y"]).args(args).status().map(|s| s.success()).unwrap_or(false)
}

fn scratch(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("aether-subs-text-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn srt() -> TextTrack {
    TextTrack::parse_file(TextFormat::Srt, &std::fs::read(fixture("sample.srt")).unwrap())
}

fn summary(c: &Cue) -> (i64, i64, String, u8) {
    (c.start_us, c.end_us, c.plain_text(), c.alignment)
}

#[test]
fn srt_cues() {
    let t = srt();
    assert_eq!(t.cues.len(), 4);
    assert_eq!(t.cues[0].plain_text(), "Hello, world!");
    assert!(t.cues[0].spans.iter().any(|s| s.italic && s.text == "world"));
    assert!(t.cues[1].spans.iter().any(|s| s.bold && s.text == "Two"));
    assert!(t.cues[1].spans.iter().any(|s| s.primary == Rgba { r: 255, g: 128, b: 0, a: 255 } && s.text == "colour"));
    assert_eq!(t.cues[2].alignment, 8);
    assert_eq!(t.cues[3].plain_text(), "Café & underline strike");
    assert!(t.cues[3].spans.iter().any(|s| s.underline) && t.cues[3].spans.iter().any(|s| s.strike));
    assert_eq!(t.cues[3].start_us, 9_100_000);
}

#[test]
fn ffmpeg_ass_and_vtt_agree_with_srt() {
    let dir = scratch("conv");
    let (ass, vtt) = (dir.join("s.ass"), dir.join("s.vtt"));
    if !ffmpeg(&["-i", fixture("sample.srt").to_str().unwrap(), ass.to_str().unwrap()])
        || !ffmpeg(&["-i", fixture("sample.srt").to_str().unwrap(), vtt.to_str().unwrap()])
    {
        eprintln!("skip: no ffmpeg");
        return;
    }
    let want: Vec<_> = srt().cues.iter().map(summary).collect();
    let a = TextTrack::parse_file(TextFormat::Ass, &std::fs::read(&ass).unwrap());
    let v = TextTrack::parse_file(TextFormat::WebVtt, &std::fs::read(&vtt).unwrap());
    // ffmpeg's ASS conversion keeps the cue's own timing to centiseconds.
    let round_cs = |x: &(i64, i64, String, u8)| (x.0 / 10_000 * 10_000, x.1 / 10_000 * 10_000, x.2.clone(), x.3);
    // ffmpeg's SRT decoder leaves HTML entities as they are in the ASS text.
    let got_a: Vec<_> = a.cues.iter().map(summary).map(|(s, e, t, an)| (s, e, t.replace("&amp;", "&"), an)).collect();
    assert_eq!(got_a, want.iter().map(round_cs).collect::<Vec<_>>());
    let got_v: Vec<_> = v.cues.iter().map(summary).collect();
    // ffmpeg's WebVTT muxer drops \an, font colour and strike, so only text and timing are compared.
    assert_eq!(got_v.iter().map(|x| (x.0, x.1, x.2.clone())).collect::<Vec<_>>(), want.iter().map(|x| (x.0, x.1, x.2.clone())).collect::<Vec<_>>());
    // Styling survives both conversions.
    for t in [&a, &v] {
        assert!(t.cues[0].spans.iter().any(|s| s.italic && s.text == "world"));
        assert!(t.cues[1].spans.iter().any(|s| s.bold && s.text == "Two"));
    }
    assert!(a.cues[1].spans.iter().any(|s| s.primary.r == 255 && s.primary.g == 128 && s.primary.b == 0));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn styled_ass_script() {
    let t = TextTrack::parse_file(TextFormat::Ass, &std::fs::read(fixture("styled.ass")).unwrap());
    assert_eq!((t.info().play_res_x, t.info().play_res_y), (1280, 720));
    assert_eq!(t.cues.len(), 3, "Comment lines are not cues");
    let c0 = &t.cues[0];
    assert_eq!(c0.plain_text(), "Plain italic and bold, with comma\nsecond line");
    assert_eq!((c0.margin_l, c0.margin_v, c0.alignment), (60, 40, 2));
    assert!(c0.spans[0].font == "Arial" && c0.spans[0].size == 48.0 && (c0.spans[0].border - 2.5).abs() < 1e-6 && c0.spans[0].shadow == 1.0);
    assert_eq!(c0.spans[0].back, Rgba { r: 0, g: 0, b: 0, a: 127 });
    let sign = &t.cues[1];
    assert_eq!(sign.layer, 1);
    assert_eq!(sign.pos, Some((640.0, 100.0)));
    assert_eq!(sign.fade, Some((300, 500)));
    let s = &sign.spans[0];
    assert!(s.bold && s.italic && s.font == "Georgia");
    assert_eq!(s.primary, Rgba { r: 0, g: 255, b: 0, a: 255 });
    assert_eq!(s.outline, Rgba { r: 0, g: 0, b: 255, a: 255 });
    assert_eq!(s.border, 5.0);
    let mv = &t.cues[2];
    assert_eq!((mv.alignment, mv.margin_l, mv.margin_r, mv.margin_v), (9, 100, 200, 80));
    assert_eq!(mv.pos, Some((1200.0, 50.0)));
    assert_eq!(mv.spans[0].primary.a, 127);
    // Draw order: lower layers first.
    let active = t.active_at(2_700_000);
    assert_eq!(active.iter().map(|c| c.layer).collect::<Vec<_>>(), vec![0, 1]);
}

#[test]
fn ffmpeg_ass_dialogue_through_the_packet_api() {
    // Matroska packet shape: ReadOrder,Layer,Style,Name,MarginL,MarginR,MarginV,Effect,Text
    let dir = scratch("pkt");
    let ass = dir.join("s.ass");
    if !ffmpeg(&["-i", fixture("sample.srt").to_str().unwrap(), ass.to_str().unwrap()]) {
        eprintln!("skip: no ffmpeg");
        return;
    }
    let file = TextTrack::parse_file(TextFormat::Ass, &std::fs::read(&ass).unwrap());
    let header: String = std::fs::read_to_string(&ass).unwrap().lines().take_while(|l| !l.starts_with("Dialogue")).collect::<Vec<_>>().join("\n");
    let mut t = TextTrack::new(TextFormat::Ass, header.as_bytes());
    for (n, c) in file.cues.iter().enumerate() {
        let pkt = format!("{n},0,Default,,0,0,0,,{}", "x");
        // Re-create the packet text from the Dialogue line so the Text field keeps its tags.
        let line = std::fs::read_to_string(&ass).unwrap();
        let dlg = line.lines().filter(|l| l.starts_with("Dialogue:")).nth(n).unwrap().to_string();
        let text = dlg.splitn(10, ',').nth(9).unwrap();
        let pkt = pkt.replace(",x", &format!(",{text}"));
        let cue = t.push_packet(c.start_us, c.end_us - c.start_us, pkt.as_bytes()).unwrap().clone();
        assert_eq!(cue.plain_text(), c.plain_text());
        assert_eq!(cue.start_us, c.start_us);
    }
    let _ = std::fs::remove_dir_all(&dir);
}
