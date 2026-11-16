use aether_audio::{AudioError, TrueHdDecoder};
fn decoded(input: &[u8], chunk_size: usize) -> (Vec<i32>, Vec<String>, usize, u32, usize) {
    let mut decoder = TrueHdDecoder::new(3).unwrap();
    let (mut pcm, mut labels, mut frames, mut rate, mut metadata) =
        (Vec::new(), Vec::new(), 0, 0, 0);
    for chunk in input.chunks(chunk_size) {
        decoder.push(chunk).unwrap();
        while let Some(block) = decoder.pull().unwrap() {
            assert_eq!(block.output_frame_offset, frames as u64);
            frames += block.frames;
            rate = block.sample_rate;
            labels = block.labels;
            metadata += block.metadata.len();
            pcm.extend(block.pcm);
        }
    }
    assert_eq!(decoder.pending_bytes(), 0);
    (pcm, labels, frames, rate, metadata)
}
#[test]
fn truehd_stereo_is_lossless_across_arbitrary_chunk_boundaries() {
    let input = include_bytes!("data/audio-truehd-stereo-s16.thd");
    let reference = include_bytes!("data/audio-truehd-stereo-s16.s16le");
    let expected: Vec<i32> = reference
        .chunks_exact(2)
        .map(|b| i32::from(i16::from_le_bytes([b[0], b[1]])) << 8)
        .collect();
    for chunk_size in [1, 7, 4096, 65536] {
        let (pcm, labels, frames, rate, metadata) = decoded(input, chunk_size);
        assert_eq!(frames, 25600);
        assert_eq!(rate, 48000);
        assert_eq!(metadata, 0);
        assert_eq!(labels, ["L", "R"]);
        assert_eq!(pcm, expected);
    }
}
#[test]
fn truehd_surround_is_lossless_and_preserves_channel_order() {
    let reference = include_bytes!("data/audio-truehd-surround51-s24.s24le");
    let expected: Vec<i32> = reference
        .chunks_exact(3)
        .map(|b| i32::from_le_bytes([0, b[0], b[1], b[2]]) >> 8)
        .collect();
    let (pcm, labels, frames, rate, metadata) =
        decoded(include_bytes!("data/audio-truehd-surround51-s24.thd"), 127);
    assert_eq!(frames, 25600);
    assert_eq!(rate, 48000);
    assert_eq!(metadata, 0);
    assert_eq!(labels, ["L", "R", "C", "LFE", "Ls", "Rs"]);
    assert_eq!(pcm, expected);
}
#[test]
fn bounds_refuse_excess_input_and_invalid_presentation() {
    assert!(matches!(
        TrueHdDecoder::new(4),
        Err(AudioError::InvalidPresentation)
    ));
    let mut decoder = TrueHdDecoder::new(3).unwrap();
    assert_eq!(decoder.push(&vec![0; 65537]), Err(AudioError::PushTooLarge));
    for _ in 0..16 {
        decoder.push(&vec![0; 65536]).unwrap();
    }
    assert_eq!(decoder.push(&[0]), Err(AudioError::PendingLimit));
}
#[test]
fn malformed_major_sync_poisoning_requires_a_fresh_decoder() {
    let mut bytes = include_bytes!("data/audio-truehd-stereo-s16.thd").to_vec();
    bytes[12] ^= 0x20;
    let mut decoder = TrueHdDecoder::new(3).unwrap();
    decoder.push(&bytes).unwrap();
    assert!(decoder.pull().is_err());
    assert_eq!(decoder.push(&[0]), Err(AudioError::Poisoned));
}
