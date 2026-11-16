//! Metadata-only audit of unsupported spatial controls in user-supplied TrueHD.
//! Emits aggregate counts, never compressed frames, element PCM or object paths.
use aether_audio::TrueHdDecoder;
use std::collections::BTreeMap;
use std::io::Read;

fn main() {
    let path = std::env::args()
        .nth(1)
        .expect("usage: audit-spatial-local clip.thd | -");
    let mut input: Box<dyn Read> = if path == "-" {
        Box::new(std::io::stdin().lock())
    } else {
        Box::new(std::fs::File::open(path).expect("open local clip"))
    };
    let mut decoder = TrueHdDecoder::new(3).unwrap();
    let (mut frames, mut payloads, mut dynamic_updates, mut active_updates) =
        (0u64, 0u64, 0u64, 0u64);
    let (mut extent, mut snap, mut elevation_disabled, mut screen, mut distance) =
        (0u64, 0u64, 0u64, 0u64, 0u64);
    let mut zones = BTreeMap::<u8, u64>::new();
    let mut extents = BTreeMap::<String, u64>::new();
    let (mut divergence, mut trim_payloads) = (0u64, 0u64);
    let (
        mut bed_payloads,
        mut bed_element_ids,
        mut alternate_payloads,
        mut alternate_ids,
        mut duplicate_object_payloads,
    ) = (0u64, 0u64, 0u64, 0u64, 0u64);
    let (
        mut duplicate_extension_payloads,
        mut extension_order_payloads,
        mut precision_records,
        mut nonzero_precision_records,
        mut nonfinite_precision_records,
    ) = (0u64, 0u64, 0u64, 0u64, 0u64);
    let mut bytes_read = 0;
    let mut buffer = [0; 4096];
    loop {
        let len = input.read(&mut buffer).expect("read TrueHD input");
        if len == 0 {
            break;
        }
        bytes_read += len;
        assert!(bytes_read <= 64 * 1024 * 1024, "bounded local audit");
        decoder.push(&buffer[..len]).unwrap();
        while let Some(block) = decoder.pull().unwrap() {
            frames += block.frames as u64;
            for payload in &block.metadata {
                payloads += 1;
                assert_eq!(payload.program_assignment.num_isf_objects, 0);
                trim_payloads += u64::from(payload.trim_element.is_some());
                bed_payloads += u64::from(payload.bed_object_element.is_some());
                bed_element_ids += payload
                    .oa_element_md
                    .iter()
                    .filter(|m| m.oa_element_id_idx == 6)
                    .count() as u64;
                alternate_payloads += u64::from(payload.b_alternate_object_data_present);
                alternate_ids += payload
                    .oa_element_md
                    .iter()
                    .filter(|m| m.alternate_object_data_id_idx.is_some())
                    .count() as u64;
                duplicate_object_payloads += u64::from(
                    payload
                        .oa_element_md
                        .iter()
                        .filter(|m| m.oa_element_id_idx == 1)
                        .count()
                        > 1,
                );
                duplicate_extension_payloads += u64::from(
                    payload
                        .oa_element_md
                        .iter()
                        .filter(|m| m.oa_element_id_idx == 5)
                        .count()
                        > 1,
                );
                let mut object_seen = false;
                let mut bad_extension_order = false;
                for md in &payload.oa_element_md {
                    if md.oa_element_id_idx == 1 {
                        object_seen = true;
                    } else if md.oa_element_id_idx == 5 && !object_seen {
                        bad_extension_order = true;
                    }
                }
                extension_order_payloads += u64::from(bad_extension_order);
                if let Some(ext) = &payload.extended_object_element {
                    for p in ext.ext_prec_pos_block.iter().flatten() {
                        let xyz = [p.ext_prec_pos3d_x, p.ext_prec_pos3d_y, p.ext_prec_pos3d_z];
                        precision_records += 1;
                        nonzero_precision_records += u64::from(xyz.iter().any(|&v| v != 0.));
                        nonfinite_precision_records +=
                            u64::from(xyz.iter().any(|v| !v.is_finite()));
                    }
                    divergence += ext
                        .object_div_block
                        .iter()
                        .flatten()
                        .filter(|v| v.b_object_divergence && v.object_divergence != 0.)
                        .count() as u64;
                }
                if let Some(element) = &payload.object_element {
                    for object in element
                        .object_data
                        .iter()
                        .skip(payload.program_assignment.num_bed_objects)
                    {
                        for update in object {
                            dynamic_updates += 1;
                            if update.b_object_not_active {
                                continue;
                            }
                            active_updates += 1;
                            let r = &update.object_render_info;
                            extent += u64::from(r.object_size.iter().any(|&v| v != 0.));
                            snap += u64::from(r.b_object_snap);
                            elevation_disabled += u64::from(!r.b_enable_elevation);
                            screen += u64::from(r.b_object_use_screen_ref);
                            distance += u64::from(r.b_object_distance_specified);
                            *zones.entry(r.zone_constraints_idx).or_default() += 1;
                            *extents
                                .entry(format!(
                                    "{:.9},{:.9},{:.9}",
                                    r.object_size[0], r.object_size[1], r.object_size[2]
                                ))
                                .or_default() += 1;
                        }
                    }
                }
            }
        }
    }
    assert_eq!(decoder.pending_bytes(), 0, "no partial access-unit tail");
    let histogram = |entries: Vec<(String, u64)>| {
        entries
            .into_iter()
            .map(|(k, v)| format!("\"{k}\":{v}"))
            .collect::<Vec<_>>()
            .join(",")
    };
    let extension_summary = format!(
        "\"duplicateExtensionElementPayloads\":{duplicate_extension_payloads},\"extensionBeforeOrMissingObjectPayloads\":{extension_order_payloads},\"precisionPositionRecords\":{precision_records},\"nonzeroPrecisionPositionRecords\":{nonzero_precision_records},\"nonfinitePrecisionPositionRecords\":{nonfinite_precision_records}"
    );
    println!(
        "{{\"frames\":{frames},\"payloads\":{payloads},\"dynamicUpdateRecords\":{dynamic_updates},\"activeDynamicUpdateRecords\":{active_updates},\"nonzeroExtent\":{extent},\"snap\":{snap},\"elevationDisabled\":{elevation_disabled},\"screenReference\":{screen},\"distanceSpecified\":{distance},\"nonzeroDivergenceRecords\":{divergence},\"trimPayloads\":{trim_payloads},\"bedMetadataPayloads\":{bed_payloads},\"bedElementIds\":{bed_element_ids},\"alternateObjectDataPayloads\":{alternate_payloads},\"alternateObjectDataIds\":{alternate_ids},\"duplicateObjectElementPayloads\":{duplicate_object_payloads},{extension_summary},\"zoneHistogram\":{{{}}},\"extentHistogram\":{{{}}}}}",
        histogram(zones.into_iter().map(|(k, v)| (k.to_string(), v)).collect()),
        histogram(extents.into_iter().collect())
    );
}
