use aether_demux::{Container, Demuxer, FileSource, TrackKind};

fn fixture(name: &str) -> String {
    format!("{}/tests/testdata/{name}", env!("CARGO_MANIFEST_DIR"))
}

#[test]
fn matroska_lists_tracks_and_packets() {
    let src = FileSource::open(fixture("tiny-hevc-flac.mkv")).unwrap();
    let d = Demuxer::open(&src).unwrap();
    assert_eq!(d.container(), Container::Matroska);
    let [video, audio] = d.tracks() else { panic!("two tracks") };
    assert_eq!(video.kind, TrackKind::Video);
    assert_eq!(video.codec, "V_MPEGH/ISO/HEVC");
    assert_eq!((video.width, video.height), (Some(128), Some(72)));
    assert!(video.codec_private.len() > 23, "hvcC present");
    assert_eq!(audio.kind, TrackKind::Audio);
    assert_eq!(audio.codec, "A_FLAC");
    assert_eq!(audio.language.as_deref(), Some("fra"));
    assert_eq!((audio.channels, audio.sample_rate), (Some(1), Some(48000)));
    // Counts and the first keyframe size come from ffprobe on the same file.
    assert_eq!(d.track_packets(video.id).count(), 48);
    assert_eq!(d.track_packets(audio.id).count(), 94);
    assert_eq!(d.track_packets(video.id).filter(|p| p.keyframe).count(), 4);
    assert_eq!(d.track_packets(video.id).next().unwrap().size, 929);
    assert!(d.duration_ns().unwrap() >= 1_900_000_000);
}

#[test]
fn matroska_payload_range_is_the_nal_data_and_seek_lands_on_verified_keyframe() {
    let src = FileSource::open(fixture("tiny-hevc-flac.mkv")).unwrap();
    let d = Demuxer::open(&src).unwrap();
    let video = &d.tracks()[0];
    let mut buf = Vec::new();
    let first = d.track_packets(video.id).next().unwrap();
    d.read_packet(&src, first, &mut buf).unwrap();
    assert!(aether_demux::verify_keyframe(video, &buf));
    // Target 1.2 s: the keyframe at 1.0 s.
    let i = d.seek(&src, video.id, 1_200_000_000).unwrap();
    assert_eq!(d.packets()[i].pts, 1_000_000_000);
    assert!(d.packets()[i].keyframe);
    // A non keyframe payload fails the bitstream check even if a muxer flagged it.
    let delta = d.track_packets(video.id).find(|p| !p.keyframe).unwrap();
    d.read_packet(&src, delta, &mut buf).unwrap();
    assert!(!aether_demux::verify_keyframe(video, &buf));
}

#[test]
fn mp4_lists_video_track_and_samples() {
    let src = FileSource::open(fixture("tiny-hevc.mp4")).unwrap();
    let d = Demuxer::open(&src).unwrap();
    assert_eq!(d.container(), Container::Mp4);
    let t = &d.tracks()[0];
    assert_eq!(t.codec, "hvc1");
    assert_eq!((t.width, t.height), (Some(128), Some(72)));
    assert_eq!(d.track_packets(t.id).count(), 26);
    assert_eq!(d.track_packets(t.id).filter(|p| p.keyframe).count(), 3);
    let i = d.seek(&src, t.id, i64::MAX).unwrap();
    assert!(d.packets()[i].keyframe);
}

#[test]
fn rejects_unknown_container() {
    let src = aether_demux::SliceSource(b"RIFF....WAVEfmt ");
    assert!(Demuxer::open(&src).is_err());
}

/// Uses test-media/p8-flat-dvh1.mp4 when it is present.
#[test]
fn mp4_reports_dolby_vision_record() {
    let path = format!("{}/../../test-media/p8-flat-dvh1.mp4", env!("CARGO_MANIFEST_DIR"));
    let Ok(src) = FileSource::open(path) else { return };
    let d = Demuxer::open(&src).unwrap();
    let t = &d.tracks()[0];
    assert_eq!(t.codec, "dvh1");
    let dv = t.dolby_vision.as_ref().expect("dvvC for profile 8");
    assert_eq!(&dv.box_type, b"dvvC");
    assert_eq!(dv.bytes.len(), 24);
    assert_eq!(d.track_packets(t.id).count(), 62);
}
