//! Calls the exported functions natively. The host-read import is only live in the
//! browser, so demux goes through `aether_demux_probe` here.
use aether_wasm::*;

fn result() -> Vec<u8> {
    unsafe { std::slice::from_raw_parts(aether_result_ptr(), aether_result_len()).to_vec() }
}

#[test]
fn demux_probe_counts_tracks() {
    let path = format!("{}/../aether-demux/tests/testdata/tiny-hevc-flac.mkv", env!("CARGO_MANIFEST_DIR"));
    let data = std::fs::read(path).unwrap();
    assert_eq!(unsafe { demux::aether_demux_probe(data.as_ptr(), data.len()) }, 2);
    assert_eq!(unsafe { demux::aether_demux_probe(data.as_ptr(), 16) }, -1);
}

#[test]
fn alloc_round_trip_and_limits() {
    let p = aether_alloc(64);
    assert!(!p.is_null());
    unsafe { aether_free(p, 64) };
    assert!(aether_alloc(0).is_null());
    assert!(aether_alloc(MAX_ALLOC + 1).is_null());
}

#[test]
fn relabel_rejects_non_sps_and_reports_error_index() {
    let nal = [0x40u8, 0x01, 0, 0];
    assert!(unsafe { dovi::aether_dovi_relabel_sps(nal.as_ptr(), nal.len(), 1, 13, -1, 0) } < 0);
}

#[test]
fn fel_tables_need_a_matching_epoch() {
    dovi::aether_fel_reset(1);
    let nal = [0x7cu8, 0x01, 0, 0, 0, 0, 0, 0, 0];
    assert!(unsafe { dovi::aether_fel_gpu_tables(2, nal.as_ptr(), nal.len()) } < 0);
}

#[test]
fn audio_decoder_lifecycle() {
    assert_eq!(audio::aether_audio_pull(), -1, "no decoder open");
    assert_eq!(audio::aether_audio_open(9, 0), -1);
    assert_eq!(audio::aether_audio_open(0, 0), 0);
    assert_eq!(audio::aether_audio_pull(), 1, "needs input");
    audio::aether_audio_close();
}

#[test]
fn player_session_starts_once() {
    assert_eq!(player::aether_player_create(), 0);
    assert_eq!(player::aether_player_start(), 0);
    let generation = player::aether_player_result() as u64;
    assert!(generation > 0);
    assert_eq!(player::aether_player_phase(), 1);
    let _ = result();
}

fn last_error() -> String {
    let n = unsafe { aether_last_error(std::ptr::null_mut(), 0) };
    let mut buf = vec![0u8; n];
    assert_eq!(unsafe { aether_last_error(buf.as_mut_ptr(), n) }, n);
    String::from_utf8(buf).unwrap()
}

#[test]
fn errors_are_retrievable_as_messages() {
    audio::aether_audio_close();
    let data = [1u8, 2, 3];
    assert_eq!(unsafe { audio::aether_audio_push(data.as_ptr(), data.len()) }, -1);
    assert_eq!(last_error(), "no audio decoder open");
    assert_eq!(audio::aether_audio_open(1, 0), 0);
    let junk = [0xffu8; 16];
    assert_eq!(unsafe { audio::aether_audio_push(junk.as_ptr(), junk.len()) }, -200);
    assert!(last_error().starts_with("DTS core header"), "{}", last_error());
    audio::aether_audio_close();
    assert_eq!(audio::aether_audio_open_pcm(9, 16, 2, 48000, 0), -1);
    assert_eq!(audio::aether_audio_open_pcm(0, 20, 2, 48000, 0), -2);
    assert!(last_error().contains("unsupported sample size"));
    assert_eq!(demux::aether_demux_info(), -1);
    assert_eq!(last_error(), "no demuxer open");
    aether_clear_error();
    assert_eq!(last_error(), "");
}

#[test]
fn pcm_pull_returns_planar_f32_with_labels() {
    assert_eq!(audio::aether_audio_open_pcm(0, 16, 2, 44100, 0), 0);
    let pcm = [0x00u8, 0x80, 0x00, 0x40, 0xff, 0x7f, 0x00, 0x00];
    assert_eq!(unsafe { audio::aether_audio_push(pcm.as_ptr(), pcm.len()) }, 0);
    assert_eq!(audio::aether_audio_pull(), 0);
    let r = result();
    let w = |i: usize| u32::from_le_bytes(r[i * 4..i * 4 + 4].try_into().unwrap());
    assert_eq!((w(0), w(1), w(2), w(3), w(4)), (44100, 2, 2, 5, 1));
    assert_eq!(&r[20..25], b"FL,FR");
    assert_eq!(audio::aether_audio_block_flags(), 1);
    let f = |at: usize| f32::from_le_bytes(r[at..at + 4].try_into().unwrap());
    assert_eq!([f(28), f(32), f(36), f(40)], [-1.0, 32767.0 / 32768.0, 0.5, 0.0]);
    assert_eq!(audio::aether_audio_pull(), 1);
    audio::aether_audio_close();
}

#[test]
fn ac3_codecs_open_and_reject_garbage() {
    for codec in [3, 4] {
        assert_eq!(audio::aether_audio_open(codec, 0), 0);
        let junk = [0x11u8; 64];
        assert_eq!(unsafe { audio::aether_audio_push(junk.as_ptr(), junk.len()) }, -400);
        assert!(last_error().starts_with("AC-3 "));
    }
    audio::aether_audio_close();
}

#[test]
fn layout_table_is_json() {
    assert_eq!(audio::aether_audio_layout_table(), 0);
    let t = String::from_utf8(result()).unwrap();
    assert!(t.contains("\"wave\":[\"FL\",\"FR\",\"FC\",\"LFE\",\"BL\""));
}

#[test]
fn output_stage_round_trip() {
    use aether_wasm::output::*;
    let labels = b"FL,FR,FC,LFE,BL,BR";
    let h = unsafe { aether_output_create(labels.as_ptr(), labels.len(), 2, 0, 48000, 0, -1.0, 1024) };
    assert!(h >= 1);
    assert_eq!(aether_output_info(h), 0);
    let info = String::from_utf8(result()).unwrap();
    assert!(info.contains("\"kind\":\"mix\"") && info.contains("\"outLayout\":[\"FL\",\"FR\"]"), "{info}");
    let inp = aether_output_input_ptr(h);
    let out = aether_output_output_ptr(h);
    unsafe {
        let input = std::slice::from_raw_parts_mut(inp, 6 * 1024);
        input.fill(0.0);
        input[0] = 1.0; // FL
        input[2 * 1024] = 1.0; // FC
    }
    assert_eq!(aether_output_process(h, 1024), 0);
    let left = unsafe { *out };
    assert!((left - (1.0 + std::f32::consts::FRAC_1_SQRT_2)).abs() < 1e-6);
    assert_eq!(aether_output_process(h, 4096), -3);
    assert_eq!(aether_output_reset(h), 0);
    assert_eq!(aether_output_limiter_stats(h), 0);
    aether_output_close(h);
    assert_eq!(aether_output_process(h, 16), -1);
    let bad = b"FL,XX";
    assert_eq!(unsafe { aether_output_create(bad.as_ptr(), bad.len(), 2, 0, 48000, 0, -1.0, 0) }, -2);
    assert!(last_error().contains("XX"));
}
