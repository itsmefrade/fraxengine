//! Synthetic typed mutations of a real metadata-only snippet; no new audio.
use aether_audio::{
    AudioBlock,
    render::{Layout, ObjectRenderer, RenderError, SpatialFeature},
};
use truehd::structs::oamd::{
    BedObjectElement, ExtendedObjectElement, ExtendedPrecisionPositionBlock, OAElementMD,
    ObjectAudioMetadataPayload, ObjectDivergenceBlock, TrimElement,
};
fn metadata() -> ObjectAudioMetadataPayload {
    ObjectAudioMetadataPayload::read(include_bytes!("data/amaze-first-oamd.bin")).unwrap()
}
fn input(metadata: Vec<ObjectAudioMetadataPayload>, offset: u64) -> AudioBlock {
    AudioBlock {
        sample_rate: 48000,
        frames: 40,
        channels: 16,
        presentation: 3,
        pcm: vec![4194304; 640],
        labels: vec![],
        metadata,
        is_duplicate: false,
        access_unit: 0,
        input_offset: 0,
        output_frame_offset: offset,
    }
}
fn requesting(feature: SpatialFeature) -> ObjectAudioMetadataPayload {
    let mut p = metadata();
    let r = &mut p.object_element.as_mut().unwrap().object_data[1][0].object_render_info;
    match feature {
        SpatialFeature::Extent => r.object_size = [0.1, 0., 0.],
        SpatialFeature::Snap => r.b_object_snap = true,
        SpatialFeature::HorizontalZone => r.zone_constraints_idx = 1,
        SpatialFeature::ElevationExclusion => r.b_enable_elevation = false,
        SpatialFeature::ScreenReference => r.b_object_use_screen_ref = true,
        SpatialFeature::Distance => {
            r.b_object_distance_specified = true;
            r.distance_factor = Some(1.);
        }
        SpatialFeature::Divergence => {
            let mut blocks = vec![vec![ObjectDivergenceBlock::default()]; 16];
            blocks[1][0].b_object_divergence = true;
            blocks[1][0].object_divergence = 0.5;
            p.extended_object_element = Some(ExtendedObjectElement {
                b_obj_div_block: true,
                object_div_block: blocks,
                ..Default::default()
            });
        }
        SpatialFeature::Trims => p.trim_element = Some(TrimElement::default()),
        SpatialFeature::BedMetadata => {
            p.bed_object_element = Some(BedObjectElement {
                lfe_gain: Some(1),
                custom_downmix: true,
                ..Default::default()
            })
        }
        SpatialFeature::AlternateObjectData => p.b_alternate_object_data_present = true,
    }
    p
}
#[test]
fn active_unsupported_spatial_controls_are_explicitly_refused() {
    assert_eq!(RenderError::UnsupportedLayout.status_code(), 200);
    assert_eq!(RenderError::InvalidSamples.status_code(), 207);
    assert_eq!(
        RenderError::UnsupportedSpatialFeature(SpatialFeature::Extent).status_code(),
        208
    );
    assert_eq!(
        RenderError::UnsupportedSpatialFeature(SpatialFeature::Trims).status_code(),
        215
    );
    assert_eq!(
        RenderError::UnsupportedSpatialFeature(SpatialFeature::BedMetadata).status_code(),
        216
    );
    assert_eq!(
        RenderError::UnsupportedSpatialFeature(SpatialFeature::AlternateObjectData).status_code(),
        217
    );
    for (index, feature) in [
        SpatialFeature::Extent,
        SpatialFeature::Snap,
        SpatialFeature::HorizontalZone,
        SpatialFeature::ElevationExclusion,
        SpatialFeature::ScreenReference,
        SpatialFeature::Distance,
        SpatialFeature::Divergence,
        SpatialFeature::Trims,
        SpatialFeature::BedMetadata,
        SpatialFeature::AlternateObjectData,
    ]
    .into_iter()
    .enumerate()
    {
        assert_eq!(
            RenderError::UnsupportedSpatialFeature(feature).status_code(),
            208 + index as u32
        );
        for layout in [Layout::FiveOneFour, Layout::SevenOneFour] {
            let mut renderer = ObjectRenderer::new(layout);
            assert!(
                matches!(renderer.render_block(&input(vec![requesting(feature)],0)),Err(RenderError::UnsupportedSpatialFeature(f)) if f==feature),
                "{feature:?} {layout:?}"
            );
        }
    }
}
#[test]
fn bed_and_alternate_controls_are_refused_without_an_object_element_or_flag() {
    for feature in [
        SpatialFeature::BedMetadata,
        SpatialFeature::AlternateObjectData,
    ] {
        let mut p = requesting(feature);
        p.object_element = None;
        p.oa_element_md.clear();
        let mut r = ObjectRenderer::new(Layout::SevenOneFour);
        assert!(
            matches!(r.render_block(&input(vec![p],0)),Err(RenderError::UnsupportedSpatialFeature(f)) if f==feature)
        );
    }
    for (id, alternate, feature) in [
        (6, None, SpatialFeature::BedMetadata),
        (1, Some(0), SpatialFeature::AlternateObjectData),
    ] {
        let mut p = metadata();
        p.object_element = None;
        p.bed_object_element = None;
        p.b_alternate_object_data_present = false;
        p.oa_element_md = vec![OAElementMD {
            oa_element_id_idx: id,
            alternate_object_data_id_idx: alternate,
            ..Default::default()
        }];
        for layout in [Layout::FiveOneFour, Layout::SevenOneFour] {
            let mut r = ObjectRenderer::new(layout);
            assert!(
                matches!(r.render_block(&input(vec![metadata(),p.clone()],0)),Err(RenderError::UnsupportedSpatialFeature(f)) if f==feature)
            );
            assert!(matches!(
                r.render_block(&input(vec![], 0)),
                Err(RenderError::MissingInitialMetadata)
            ));
        }
    }
}
#[test]
fn repeated_object_elements_cannot_bypass_admission_via_parser_overwrite() {
    let mut p = metadata();
    p.oa_element_md = vec![
        OAElementMD {
            oa_element_id_idx: 1,
            ..Default::default()
        };
        2
    ];
    for layout in [Layout::FiveOneFour, Layout::SevenOneFour] {
        let mut r = ObjectRenderer::new(layout);
        assert!(matches!(
            r.render_block(&input(vec![metadata(), p.clone()], 0)),
            Err(RenderError::InvalidMetadata)
        ));
        assert!(matches!(
            r.render_block(&input(vec![], 0)),
            Err(RenderError::MissingInitialMetadata)
        ));
    }
}
#[test]
fn extended_metadata_cannot_be_overwritten_or_precede_its_object_state() {
    for ids in [vec![1, 5, 5], vec![5, 1], vec![5]] {
        let mut p = metadata();
        p.oa_element_md = ids
            .iter()
            .map(|&id| OAElementMD {
                oa_element_id_idx: id,
                ..Default::default()
            })
            .collect();
        if ids == [5] {
            p.object_element = None;
        }
        for layout in [Layout::FiveOneFour, Layout::SevenOneFour] {
            let mut renderer = ObjectRenderer::new(layout);
            assert!(
                matches!(
                    renderer.render_block(&input(vec![metadata(), p.clone()], 0)),
                    Err(RenderError::InvalidMetadata)
                ),
                "IDs {ids:?}"
            );
            assert!(matches!(
                renderer.render_block(&input(vec![], 0)),
                Err(RenderError::MissingInitialMetadata)
            ));
        }
    }
}
#[test]
fn supported_extended_precision_xyz_routes_to_the_exact_ceiling_target() {
    let mut p = metadata();
    p.oa_element_md = [1, 5]
        .map(|id| OAElementMD {
            oa_element_id_idx: id,
            ..Default::default()
        })
        .to_vec();
    let data = &mut p.object_element.as_mut().unwrap().object_data;
    for updates in data.iter_mut() {
        for d in updates {
            d.b_object_not_active = true;
        }
    }
    data[1][0].b_object_not_active = false;
    data[1][0].object_basic_info.object_gain = 0;
    data[1][0].object_render_info.pos3d = [0.25 - 1. / 248., 0.25 + 1. / 248., 1. - 2. / 248.];
    let mut precision = vec![vec![ExtendedPrecisionPositionBlock::default()]; 16];
    precision[1][0] = ExtendedPrecisionPositionBlock {
        ext_prec_pos3d_x: 1. / 248.,
        ext_prec_pos3d_y: -1. / 248.,
        ext_prec_pos3d_z: 2. / 248.,
    };
    p.extended_object_element = Some(ExtendedObjectElement {
        b_ext_prec_pos_block: true,
        ext_prec_pos_block: precision,
        ..Default::default()
    });
    for layout in [Layout::FiveOneFour, Layout::SevenOneFour] {
        let mut b = input(vec![p.clone()], 0);
        b.pcm.fill(0);
        for samples in b.pcm.chunks_mut(16) {
            samples[1] = 4194304;
        }
        let out = ObjectRenderer::new(layout).render_block(&b).unwrap();
        let ceiling = out.labels.iter().position(|&label| label == "Tfl").unwrap();
        for samples in out.pcm.chunks(out.channels) {
            for (i, &v) in samples.iter().enumerate() {
                assert!((v - if i == ceiling { 0.5 } else { 0. }).abs() < 1e-6);
            }
        }
    }
}
#[test]
fn later_rejection_does_not_queue_earlier_payloads_in_the_same_block() {
    let mut renderer = ObjectRenderer::new(Layout::SevenOneFour);
    assert!(matches!(
        renderer.render_block(&input(
            vec![metadata(), requesting(SpatialFeature::Extent)],
            0
        )),
        Err(RenderError::UnsupportedSpatialFeature(
            SpatialFeature::Extent
        ))
    ));
    assert!(matches!(
        renderer.render_block(&input(vec![], 0)),
        Err(RenderError::MissingInitialMetadata)
    ));
    assert_eq!(
        renderer
            .render_block(&input(vec![metadata()], 0))
            .unwrap()
            .applied_updates,
        1
    );
}

#[test]
fn experimental_elevation_exclusion_moves_a_ceiling_object_to_the_ear_level_plane() {
    for layout in [Layout::FiveOneFour, Layout::SevenOneFour] {
        let mut payload = metadata();
        let element = payload.object_element.as_mut().unwrap();
        for object in &mut element.object_data {
            object[0].b_object_not_active = true;
        }
        let object = &mut element.object_data[1][0];
        object.b_object_not_active = false;
        object.object_basic_info.object_gain = 0;
        object.object_render_info.pos3d = [0., 0., 1.];
        object.object_render_info.b_enable_elevation = true;
        element.md_update_info.block_update_info[0].ramp_duration = 2;

        let mut first = input(vec![payload.clone()], 0);
        first.pcm.fill(0);
        for row in first.pcm.chunks_mut(16) {
            row[1] = 4194304;
        }
        let mut renderer = ObjectRenderer::with_elevation_exclusion(layout);
        let ceiling = renderer.render_block(&first).unwrap();
        let tfl = ceiling
            .labels
            .iter()
            .position(|&label| label == "Tfl")
            .unwrap();
        assert_eq!(ceiling.pcm[tfl], 0.5);
        assert_eq!(ceiling.pcm[0], 0.);

        payload.object_element.as_mut().unwrap().object_data[1][0]
            .object_render_info
            .b_enable_elevation = false;
        let mut second = first;
        second.output_frame_offset = 40;
        second.metadata = vec![payload];
        let floor = renderer.render_block(&second).unwrap();
        assert_eq!(floor.pcm[0], 0.25);
        assert_eq!(floor.pcm[tfl], 0.25);
        for row in floor.pcm.chunks(floor.channels).skip(1) {
            assert_eq!(row[0], 0.5);
            assert!(row[1..].iter().all(|&sample| sample == 0.));
        }
    }
}
#[test]
fn inactive_spatial_state_is_refused_on_activation_instead_of_silently_losing_extent() {
    let mut inactive = requesting(SpatialFeature::Extent);
    inactive.object_element.as_mut().unwrap().object_data[1][0].b_object_not_active = true;
    let mut renderer = ObjectRenderer::new(Layout::SevenOneFour);
    assert!(renderer.render_block(&input(vec![inactive], 0)).is_ok());
    assert!(matches!(
        renderer.render_block(&input(vec![requesting(SpatialFeature::Extent)], 40)),
        Err(RenderError::UnsupportedSpatialFeature(
            SpatialFeature::Extent
        ))
    ));
}
