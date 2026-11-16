mod fixtures;
use aether_dtsuhd::bitreader::*;
use aether_dtsuhd::ftoc::{crc16, parse_ftoc, FtocState, ParseError};
use aether_dtsuhd::{ChannelStatus, Decoder};

#[test]
fn crc16_matches_the_frame_crc_of_the_first_sync_frame() {
    let f = &fixtures::frames()[0];
    // FTOC is 11 bytes long and ends with the CRC.
    assert_eq!(crc16(&f[..9]), u16::from_be_bytes([f[9], f[10]]));
    // standard CRC-16/CCITT-FALSE check value
    assert_eq!(crc16(b"123456789"), 0x29B1);
}

#[test]
fn ftoc_of_the_lab_sample_sync_frame() {
    let f = &fixtures::frames()[0];
    let mut st = FtocState::new();
    let ft = parse_ftoc(f, &mut st).unwrap();
    assert!(ft.is_sync);
    assert!(ft.full_channel_based_mix);
    assert_eq!(ft.ftoc_bytes, 11);
    assert_eq!((ft.major_version, ft.minor_revision), (2, 0));
    assert_eq!(ft.frame_duration, 1024);
    assert_eq!(ft.sample_rate, 48000);
    assert_eq!(ft.md_chunks.len(), 1);
    assert_eq!(ft.md_chunks[0].size, 7);
    assert_eq!(ft.audio_chunks.len(), 1);
    assert_eq!((ft.audio_chunks[0].id, ft.audio_chunks[0].size), (1, 162));
    assert_eq!(ft.total_bytes, f.len());
}

#[test]
fn stream_info_of_the_lab_sample() {
    let frames = fixtures::frames();
    let mut dec = Decoder::new();
    let info = dec.parse_frame(&frames[0]).unwrap();
    assert_eq!(info.sample_rate, 48000);
    assert_eq!(info.frame_duration_samples, 1024);
    assert_eq!(info.channel_mask, Some(0x802F));
    assert_eq!(info.layout_name.as_deref(), Some("5.1.4"));
    assert_eq!(
        info.channel_labels,
        ["C", "L", "R", "Ls", "Rs", "LFE1", "Lh", "Rh", "Lhr", "Rhr"]
    );
    assert_eq!(info.object_count, 1);
    let md = &info.metadata[0];
    assert!(md.objects_complete);
    assert_eq!(md.loudness.as_ref().unwrap().loudness_db, -19.0);
    let ace = info.ace.as_ref().unwrap();
    assert_eq!(
        (
            ace.num_lfe_streams,
            ace.num_mono_streams,
            ace.num_stereo_streams
        ),
        (1, 1, 4)
    );
    assert!(ace.enable_deemphasis);
    assert!(ace.padding_consistent);
    // the non-sync frame inherits the sample rate and channel layout
    let info1 = dec.parse_frame(&frames[1]).unwrap();
    assert!(!info1.is_sync);
    assert_eq!(info1.sample_rate, 48000);
    assert_eq!(info1.channel_mask, Some(0x802F));
    assert!(info1.ace.unwrap().padding_consistent);
}

#[test]
fn every_fixture_frame_parses_and_accounts_for_all_bytes() {
    let frames = fixtures::frames();
    let mut dec = Decoder::new();
    // frames 1, 1000, 2000 are non-sync and need the state of the sync frame 0
    dec.parse_frame(&frames[0]).unwrap();
    for f in &frames[1..] {
        let info = dec.parse_frame(f).unwrap();
        assert_eq!(info.frame_bytes, f.len());
        assert!(info.ace.unwrap().padding_consistent);
    }
}

#[test]
fn non_sync_frame_without_state_is_rejected() {
    let frames = fixtures::frames();
    let mut dec = Decoder::new();
    assert!(dec.parse_frame(&frames[1]).is_err());
}

#[test]
fn corrupted_ftoc_crc_is_detected() {
    let mut f = fixtures::frames()[0].clone();
    f[6] ^= 0x10;
    let mut st = FtocState::new();
    assert_eq!(parse_ftoc(&f, &mut st), Err(ParseError::BadCrc));
}

#[test]
fn bit_reader_primitives() {
    let mut br = BitReader::new(&[0b1010_0000, 0xFF]);
    assert_eq!(br.read_bit(), 1);
    assert_eq!(br.read_uint(3), 0b010);
    // unary: zeros terminated by one
    let mut br = BitReader::new(&[0b0001_0000]);
    assert_eq!(br.read_unary(8), 3);
    // uniform code over 5 symbols: 2 bit and 3 bit codewords
    let mut br = BitReader::new(&[0b0100_0000]);
    assert_eq!(br.read_uniform(5), 1);
    // reading beyond the end yields zeros and records the overrun
    let mut br = BitReader::new(&[0xFF]);
    assert_eq!(br.read_uint(16), 0xFF00);
    assert!(br.overrun());
    assert_eq!(negative_rice_map_decode(0), 0);
    assert_eq!(negative_rice_map_decode(1), -1);
    assert_eq!(negative_rice_map_decode(2), 1);
    assert_eq!(positive_rice_map_decode(1), 1);
    assert_eq!(modular(5, 0, 3), 1);
    assert_eq!(modular(-1, 0, 3), 3);
    assert_eq!(decode_center_map(10, 3, 0), 3);
    assert_eq!(decode_center_map(10, 3, 1), 4);
    assert_eq!(decode_center_map(10, 3, 2), 2);
}

#[test]
fn decoding_the_first_frames_reports_lfe_and_unsupported_streams_honestly() {
    let frames = fixtures::frames();
    let mut dec = Decoder::new();
    let d0 = dec.decode_frame(&frames[0]).unwrap();
    assert_eq!(d0.pcm.len(), 10);
    assert!(d0
        .pcm
        .iter()
        .all(|c| c.len() == 1024 && c.iter().all(|v| v.is_finite())));
    assert_eq!(d0.status[5], ChannelStatus::Decoded); // LFE1
    let d1 = dec.decode_frame(&frames[1]).unwrap();
    // the first 1000 or so frames of the lab sample are digital silence
    assert!(d1.pcm.iter().all(|c| c.iter().all(|v| v.abs() < 1e-3)));
}
