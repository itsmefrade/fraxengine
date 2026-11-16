//! Requires the ignored local user clip; programme media is never compiled in.
use aether_audio::{
    TrueHdDecoder,
    render::{Layout, ObjectRenderer},
};
#[test]
#[ignore = "requires AETHER_LOCAL_ATMOS pointing to the user-authorized five-second Amaze clip"]
fn genuine_local_atmos_decodes_objects_metadata_and_two_render_layouts() {
    let input = std::fs::read(
        std::env::var("AETHER_LOCAL_ATMOS").expect("AETHER_LOCAL_ATMOS path required"),
    )
    .unwrap();
    assert_eq!(input.len(), 1_684_964);
    let mut decoder = TrueHdDecoder::new(3).unwrap();
    let (mut r714, mut r514) = (
        ObjectRenderer::new(Layout::SevenOneFour),
        ObjectRenderer::new(Layout::FiveOneFour),
    );
    let (mut frames, mut units, mut payloads, mut updates) = (0u64, 0usize, 0usize, 0usize);
    let mut heights = [0f64; 4];
    let mut last_applied = 0;
    for chunk in input.chunks(4096) {
        decoder.push(chunk).unwrap();
        while let Some(block) = decoder.pull().unwrap() {
            assert_eq!(block.output_frame_offset, frames);
            assert_eq!(block.channels, 16);
            assert_eq!(block.sample_rate, 48000);
            assert_eq!(block.labels, ["LFE"]);
            frames += block.frames as u64;
            units += 1;
            for payload in &block.metadata {
                payloads += 1;
                assert_eq!(payload.object_count, 16);
                assert_eq!(payload.program_assignment.num_bed_objects, 1);
                assert_eq!(payload.program_assignment.num_dynamic_objects, 15);
                updates += payload
                    .object_element
                    .as_ref()
                    .unwrap()
                    .object_data
                    .iter()
                    .map(|v| v.len())
                    .sum::<usize>();
            }
            let bed714 = r714.render_block(&block).unwrap();
            let bed514 = r514.render_block(&block).unwrap();
            assert_eq!(bed714.channels, 12);
            assert_eq!(bed514.channels, 10);
            last_applied = bed714.applied_updates;
            assert!(bed714.pcm.iter().chain(&bed514.pcm).all(|v| v.is_finite()));
            for row in bed714.pcm.chunks(12) {
                for (c, energy) in heights.iter_mut().enumerate() {
                    *energy += (row[8 + c] as f64).powi(2);
                }
            }
        }
    }
    assert_eq!(
        (frames, units, payloads, updates, last_applied),
        (240000, 6000, 157, 2512, 157)
    );
    assert_eq!(decoder.pending_bytes(), 0);
    assert!(heights.iter().all(|energy| *energy > 0.));
}
#[test]
#[ignore = "requires AETHER_LOCAL_ATMOS and AETHER_LOCAL_ATMOS_BED_PCM24 local FFmpeg oracle"]
fn genuine_local_channel_bed_matches_ffmpeg_after_explicit_channel_reorder() {
    let input = std::fs::read(std::env::var("AETHER_LOCAL_ATMOS").unwrap()).unwrap();
    let reference = std::fs::read(std::env::var("AETHER_LOCAL_ATMOS_BED_PCM24").unwrap()).unwrap();
    let mut decoder = TrueHdDecoder::new(2).unwrap();
    let mut actual = Vec::new();
    for chunk in input.chunks(4096) {
        decoder.push(chunk).unwrap();
        while let Some(block) = decoder.pull().unwrap() {
            assert_eq!(block.channels, 8);
            assert_eq!(block.labels, ["L", "R", "C", "LFE", "Ls", "Rs", "Lb", "Rb"]);
            for row in block.pcm.chunks(8) {
                for c in [0, 1, 2, 3, 6, 7, 4, 5] {
                    actual.extend_from_slice(&row[c].to_le_bytes()[..3]);
                }
            }
        }
    }
    assert_eq!(actual.len(), 5_760_000);
    assert_eq!(actual, reference);
}
