//! E-AC-3 JOC through the decoder trait: 12 channel 7.1.4 output, bed fallback. Skips without the fixture.
use aether_audio::decoder::{Codec, open};

const CHID: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../test-media/audio-official-joc-channel-id.mp4");

/// Raw E-AC-3 of the fixture's audio (ffmpeg stream copy), `None` when the media or ffmpeg is missing.
fn units() -> Option<Vec<u8>> {
    if !std::path::Path::new(CHID).exists() {
        return None;
    }
    let out = std::env::temp_dir().join("aether-audio-joc-render-chid.eac3");
    let ok = std::process::Command::new("ffmpeg")
        .args(["-v", "error", "-y", "-i", CHID, "-map", "0:a:0", "-c", "copy", "-f", "eac3"])
        .arg(&out)
        .status()
        .ok()?
        .success();
    if !ok {
        return None;
    }
    std::fs::read(out).ok()
}

#[test]
fn objects_render_to_714_and_bed_comes_back_when_off() {
    let Some(stream) = units() else { return };
    let run = |joc: bool| {
        let mut d = open(Codec::Eac3).unwrap();
        d.set_joc(joc);
        let mut out = Vec::new();
        for au in aether_ac3::access_units(&stream).into_iter().take(20) {
            d.push_packet(au).unwrap();
            while let Some(b) = d.pull().unwrap() {
                out.push(b);
            }
        }
        out
    };
    let on = run(true);
    assert_eq!(on[0].frames, 1536 - 577, "the object delay is dropped once at the start");
    assert!(on.iter().all(|b| b.channels() == 12 && b.joc_rendered && b.joc_objects == 15));
    assert_eq!(on[0].labels, ["FL", "FR", "FC", "LFE", "SL", "SR", "BL", "BR", "TFL", "TFR", "TBL", "TBR"]);
    assert!(on.iter().flat_map(|b| b.to_f32()).flatten().all(|v| v.is_finite() && v.abs() <= 1.0));
    let off = run(false);
    assert!(off.iter().all(|b| b.channels() == 6 && !b.joc_rendered && b.joc));
}
