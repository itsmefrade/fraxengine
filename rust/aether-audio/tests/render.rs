use aether_audio::{
    AudioBlock,
    render::{ElementState, Layout, ObjectRenderer, Role, TimedUpdate, point_gains},
};
fn block(frames: usize, offset: u64, channels: usize) -> AudioBlock {
    AudioBlock {
        sample_rate: 48000,
        frames,
        channels,
        presentation: 3,
        pcm: vec![4194304; frames * channels],
        labels: vec![],
        metadata: vec![],
        is_duplicate: false,
        access_unit: 0,
        input_offset: 0,
        output_frame_offset: offset,
    }
}
#[test]
fn panner_preserves_unit_power_and_excludes_lfe() {
    for layout in [Layout::FiveOneFour, Layout::SevenOneFour] {
        for x in 0..=10 {
            for y in 0..=10 {
                for z in 0..=10 {
                    let gains =
                        point_gains(layout, [x as f32 / 10., y as f32 / 10., z as f32 / 10.])
                            .unwrap();
                    assert_eq!(gains[3], 0.);
                    assert!((gains.iter().map(|g| g * g).sum::<f32>() - 1.).abs() < 1e-5);
                }
            }
        }
    }
}
#[test]
fn speaker_targets_and_ceiling_positions_are_discrete() {
    let gains = point_gains(Layout::SevenOneFour, [0.25, 0.25, 1.]).unwrap();
    assert_eq!(gains[8], 1.);
    assert!(
        gains
            .iter()
            .enumerate()
            .all(|(i, &g)| i == 8 || g.abs() < 1e-6)
    );
    assert_eq!(
        point_gains(Layout::SevenOneFour, [-1., -1., -1.]).unwrap()[0],
        1.
    );
}
#[test]
fn ramp_starts_at_exact_sample_and_finishes_on_target() {
    let roles = vec![Role::Object];
    let mut r = ObjectRenderer::with_roles(Layout::SevenOneFour, roles.clone()).unwrap();
    r.schedule(TimedUpdate {
        frame: 0,
        ramp: 100,
        roles: roles.clone(),
        states: vec![ElementState {
            position: [0., 0., 0.],
            gain: 1.,
        }],
    })
    .unwrap();
    r.schedule(TimedUpdate {
        frame: 2,
        ramp: 2,
        roles,
        states: vec![ElementState {
            position: [1., 0., 0.],
            gain: 1.,
        }],
    })
    .unwrap();
    let out = r.render_block(&block(4, 0, 1)).unwrap();
    assert_eq!(out.pcm[0], 0.5);
    assert_eq!(out.pcm[12], 0.5);
    assert!((out.pcm[24] - 0.25).abs() < 1e-6);
    assert!((out.pcm[25] - 0.25).abs() < 1e-6);
    assert!(out.pcm[36].abs() < 1e-6);
    assert_eq!(out.pcm[37], 0.5);
}
#[test]
fn unchanged_metadata_does_not_restart_a_ramp_across_blocks() {
    let roles = vec![Role::Object];
    let left = ElementState {
        position: [0., 0., 0.],
        gain: 1.,
    };
    let right = ElementState {
        position: [1., 0., 0.],
        gain: 1.,
    };
    let mut r = ObjectRenderer::with_roles(Layout::SevenOneFour, roles.clone()).unwrap();
    for (frame, ramp, state) in [(0, 0, left), (1, 4, right), (3, 4, right)] {
        r.schedule(TimedUpdate {
            frame,
            ramp,
            roles: roles.clone(),
            states: vec![state],
        })
        .unwrap();
    }
    let first = r.render_block(&block(3, 0, 1)).unwrap();
    let second = r.render_block(&block(2, 3, 1)).unwrap();
    assert!((first.pcm[24 + 1] - 0.25).abs() < 1e-6);
    assert!((second.pcm[1] - 0.375).abs() < 1e-6);
    assert_eq!(second.pcm[12 + 1], 0.5);
}
#[test]
fn synthetic_lfe_input_routes_only_to_lfe() {
    let mut r = ObjectRenderer::with_roles(Layout::SevenOneFour, vec![Role::Bed("LFE")]).unwrap();
    let out = r.render_block(&block(2, 0, 1)).unwrap();
    for (frame, samples) in out.pcm.chunks(12).enumerate() {
        assert_eq!(samples[3], 0.5, "frame{frame}");
        assert!(samples.iter().enumerate().all(|(i, &g)| i == 3 || g == 0.));
    }
}
#[test]
fn extracted_metadata_fixture_preserves_real_bed_and_object_assignment() {
    use truehd::structs::oamd::ObjectAudioMetadataPayload;
    let payload =
        ObjectAudioMetadataPayload::read(include_bytes!("data/amaze-first-oamd.bin")).unwrap();
    assert_eq!(payload.object_count, 16);
    assert_eq!(payload.program_assignment.num_bed_objects, 1);
    assert_eq!(payload.program_assignment.num_dynamic_objects, 15);
    assert_eq!(
        payload.program_assignment.bed_assignment[0].to_index_vec(),
        [3]
    );
    let mut input = block(40, 0, 16);
    input.metadata.push(payload);
    let mut r = ObjectRenderer::new(Layout::SevenOneFour);
    let out = r.render_block(&input).unwrap();
    assert_eq!(out.channels, 12);
    assert_eq!(out.pcm.len(), 480);
    assert_eq!(out.applied_updates, 1);
    assert!(out.pcm.iter().all(|x| x.is_finite()));
}
#[test]
fn scheduled_role_count_change_is_an_error_not_an_index_panic() {
    let mut r =
        ObjectRenderer::with_roles(Layout::SevenOneFour, vec![Role::Object, Role::Object]).unwrap();
    r.schedule(TimedUpdate {
        frame: 0,
        ramp: 0,
        roles: vec![Role::Object],
        states: vec![ElementState {
            position: [0.; 3],
            gain: 1.,
        }],
    })
    .unwrap();
    assert!(r.render_block(&block(1, 0, 2)).is_err());
}

#[test]
fn failed_mid_block_update_does_not_commit_gains_roles_or_timeline() {
    use aether_audio::render::RenderError;
    fn primed() -> ObjectRenderer {
        let mut renderer =
            ObjectRenderer::with_roles(Layout::SevenOneFour, vec![Role::Object]).unwrap();
        for (frame, role, position) in [
            (0, Role::Object, [0., 0., 0.]),
            (4, Role::Object, [1., 0., 0.]),
            (6, Role::Bed("unknown"), [0., 0., 0.]),
        ] {
            renderer
                .schedule(TimedUpdate {
                    frame,
                    ramp: 4,
                    roles: vec![role],
                    states: vec![ElementState { position, gain: 1. }],
                })
                .unwrap();
        }
        renderer.render_block(&block(4, 0, 1)).unwrap();
        renderer
    }
    let mut actual = primed();
    let mut reference = primed();
    assert!(matches!(
        actual.render_block(&block(4, 4, 1)),
        Err(RenderError::UnsupportedRole)
    ));
    let actual_block = actual.render_block(&block(2, 4, 1)).unwrap();
    let reference_block = reference.render_block(&block(2, 4, 1)).unwrap();
    assert_eq!(actual_block.pcm, reference_block.pcm);
    assert_eq!(actual_block.labels, reference_block.labels);
    assert_eq!(
        actual_block.applied_updates,
        reference_block.applied_updates
    );
    assert!(matches!(
        actual.render_block(&block(1, 6, 1)),
        Err(RenderError::UnsupportedRole)
    ));
    assert!(matches!(
        reference.render_block(&block(1, 6, 1)),
        Err(RenderError::UnsupportedRole)
    ));
}
