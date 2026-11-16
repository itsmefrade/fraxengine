//! DTS-HD High Resolution Audio (core, XXCH, X96) against FFmpeg's float decode.
//! Needs a raw `.dts` elementary stream (`ffmpeg -map 0:a:0 -c copy -f dts`) and the
//! reference `ffmpeg -t 20 -f f32le` of the same stream, so programme media is never compiled in.
use aether_audio::dts::DtsSession;
use dca::{exss_substream_size, parse_header};

#[test]
#[ignore = "requires AETHER_DTS_HRA (raw dts), AETHER_DTS_HRA_REF (ffmpeg f32le) and AETHER_DTS_HRA_CHANNELS"]
fn hra_x96_xxch_decode_matches_ffmpeg_float_within_1e_4() {
    let data = std::fs::read(std::env::var("AETHER_DTS_HRA").expect("AETHER_DTS_HRA")).unwrap();
    let reference = std::fs::read(std::env::var("AETHER_DTS_HRA_REF").expect("AETHER_DTS_HRA_REF"))
        .unwrap()
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect::<Vec<_>>();
    let channels: usize = std::env::var("AETHER_DTS_HRA_CHANNELS")
        .expect("AETHER_DTS_HRA_CHANNELS")
        .parse()
        .unwrap();
    let mut session = DtsSession::new();
    let (mut pos, mut frames_out, mut worst) = (0usize, 0usize, 0f32);
    while pos + 16 < data.len() && (frames_out + 1) * channels <= reference.len() {
        let info = parse_header(&data[pos..]).expect("core header");
        let mut end = pos + info.frame_size;
        let mut exss = end;
        while exss + 4 <= data.len() && data[exss] == 0 {
            exss += 1;
        }
        if data.get(exss..exss + 4) == Some(&[0x64, 0x58, 0x20, 0x25]) {
            end = exss + exss_substream_size(&data[exss..]).expect("exss size");
        }
        let packet = &data[pos..end.min(data.len())];
        pos = end;
        let mut framed = (packet.len() as u32).to_le_bytes().to_vec();
        framed.extend_from_slice(packet);
        let out = session.push(&framed, 48_000).expect("packet decodes");
        assert_eq!(out.sample_rate, 96_000);
        assert_eq!(out.channel_labels.len(), channels);
        for (i, &v) in out.pcm.iter().enumerate() {
            let Some(&r) = reference.get(frames_out * channels + i) else {
                break;
            };
            worst = worst.max((v - r).abs());
        }
        frames_out += out.pcm.len() / channels;
    }
    assert!(frames_out >= 96_000 * 10, "only {frames_out} sample frames");
    assert!(worst <= 1e-4, "max abs difference {worst}");
}
