use crate::{AudioBlock, TrueHdDecoder};
use std::cell::RefCell;
include!("wasm_latch.rs");
thread_local! {
    static DECODER:RefCell<Option<TrueHdDecoder>>=const{RefCell::new(None)};
    static LABELS:RefCell<Vec<u8>>=const{RefCell::new(Vec::new())};
    static METADATA:RefCell<Vec<u8>>=const{RefCell::new(Vec::new())};
    static RENDER_FAILURE:RefCell<RenderFailureLatch>=RefCell::new(RenderFailureLatch::default());
    static RENDERER:RefCell<Option<crate::render::ObjectRenderer>>=const{RefCell::new(None)};
    static RENDER_CHECKPOINT:RefCell<Option<(usize, crate::render::ObjectRenderer)>>=const{RefCell::new(None)};
}
fn valid(ptr: usize, len: usize) -> bool {
    ptr != 0
        && ptr
            .checked_add(len)
            .is_some_and(|end| end <= core::arch::wasm32::memory_size::<0>() * 65536)
}
#[unsafe(no_mangle)]
pub extern "C" fn aether_audio_alloc(len: usize) -> *mut u8 {
    if len == 0 || len > 65536 {
        return core::ptr::null_mut();
    }
    Box::into_raw(vec![0u8; len].into_boxed_slice()) as *mut u8
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn aether_audio_free(ptr: *mut u8, len: usize) {
    if ptr.is_null() || len == 0 || len > 65536 {
        return;
    }
    unsafe {
        drop(Box::from_raw(core::ptr::slice_from_raw_parts_mut(ptr, len)));
    }
}
#[unsafe(no_mangle)]
pub extern "C" fn aether_audio_reset(presentation: u32) -> u32 {
    match TrueHdDecoder::new(presentation as usize) {
        Ok(decoder) => {
            DECODER.with(|d| *d.borrow_mut() = Some(decoder));
            LABELS.with(|v| v.borrow_mut().clear());
            METADATA.with(|v| v.borrow_mut().clear());
            RENDERER.with(|r| *r.borrow_mut() = None);
            RENDER_CHECKPOINT.with(|c| c.borrow_mut().take());
            RENDER_FAILURE.with(|l| l.borrow_mut().reset());
            0
        }
        Err(e) => 100 + e as u32,
    }
}

/// Store one in-memory renderer checkpoint at a drained decoder boundary.
#[unsafe(no_mangle)]
pub extern "C" fn aether_audio_save_render_checkpoint() -> u32 {
    if let Some(code) = RENDER_FAILURE.with(|l| l.borrow().status()) {
        return code;
    }
    let boundary = DECODER.with(|d| {
        d.borrow()
            .as_ref()
            .filter(|decoder| !decoder.poisoned && decoder.pending_bytes() == 0)
            .map(|decoder| (decoder.selection, decoder.output_frames))
    });
    let Some((selection, frame)) = boundary else {
        return 2;
    };
    let checkpoint = RENDERER.with(|r| {
        r.borrow()
            .as_ref()
            .ok_or(crate::render::RenderError::MissingInitialMetadata)?
            .restart_checkpoint(frame)
    });
    match checkpoint {
        Ok(checkpoint) => {
            RENDER_CHECKPOINT.with(|c| *c.borrow_mut() = Some((selection, checkpoint)));
            0
        }
        Err(error) => error.status_code(),
    }
}

/// The caller must feed the same track from the major-sync packet immediately
/// following the saved boundary.
#[unsafe(no_mangle)]
pub extern "C" fn aether_audio_restart_from_render_checkpoint() -> u32 {
    let Some((selection, checkpoint)) = RENDER_CHECKPOINT.with(|c| c.borrow().clone()) else {
        return 2;
    };
    let decoder = match TrueHdDecoder::new(selection) {
        Ok(decoder) => decoder,
        Err(error) => return 100 + error as u32,
    };
    DECODER.with(|d| *d.borrow_mut() = Some(decoder));
    RENDERER.with(|r| *r.borrow_mut() = Some(checkpoint));
    LABELS.with(|v| v.borrow_mut().clear());
    METADATA.with(|v| v.borrow_mut().clear());
    RENDER_FAILURE.with(|l| l.borrow_mut().reset());
    0
}

/// Counts in the saved renderer state, without exporting or changing it.
#[unsafe(no_mangle)]
pub extern "C" fn aether_audio_render_checkpoint_activity() -> u32 {
    RENDER_CHECKPOINT.with(|slot| {
        slot.borrow().as_ref().map_or(u32::MAX, |(_, renderer)| {
            let (active, pending) = renderer.checkpoint_activity();
            active | (pending << 8)
        })
    })
}

/// Returns the number of bytes written, or zero if the checkpoint is unavailable.
/// The caller owns the output allocation.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn aether_audio_export_render_checkpoint(
    ptr: usize,
    capacity: usize,
) -> usize {
    if capacity > 65536 || !valid(ptr, capacity) {
        return 0;
    }
    RENDER_CHECKPOINT.with(|slot| {
        let slot = slot.borrow();
        let Some((selection, renderer)) = slot.as_ref() else {
            return 0;
        };
        let Ok(bytes) = crate::render::encode_checkpoint(*selection, renderer) else {
            return 0;
        };
        if bytes.len() > capacity {
            return 0;
        }
        unsafe { core::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr as *mut u8, bytes.len()) };
        bytes.len()
    })
}

/// Install renderer state without altering active playback. The caller binds
/// these bytes to the same track and major-sync packet as the original state.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn aether_audio_import_render_checkpoint(ptr: usize, length: usize) -> u32 {
    if length > 65536 || !valid(ptr, length) {
        return 2;
    }
    let bytes = unsafe { core::slice::from_raw_parts(ptr as *const u8, length) };
    match crate::render::decode_checkpoint(bytes) {
        Ok(state) => {
            RENDER_CHECKPOINT.with(|slot| *slot.borrow_mut() = Some(state));
            0
        }
        Err(error) => error.status_code(),
    }
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn aether_audio_push(ptr: usize, len: usize) -> u32 {
    if let Some(code) = RENDER_FAILURE.with(|l| l.borrow().status()) {
        return code;
    }
    if len > crate::MAX_PUSH_BYTES || !valid(ptr, len) {
        return 2;
    }
    let bytes = unsafe { core::slice::from_raw_parts(ptr as *const u8, len) };
    DECODER.with(|d| match d.borrow_mut().as_mut() {
        Some(decoder) => decoder.push(bytes).map_or_else(|e| 100 + e as u32, |_| 0),
        None => 3,
    })
}
fn number(v: f64) -> String {
    if v.is_finite() {
        v.to_string()
    } else {
        "null".to_owned()
    }
}
fn metadata_json(block: &AudioBlock) -> String {
    let mut payloads = Vec::new();
    for payload in block.metadata.iter().take(8) {
        let mut objects = Vec::new();
        let mut count = 0;
        let mut truncated = false;
        if let Some(element) = &payload.object_element {
            for (object_index, updates) in element.object_data.iter().enumerate() {
                for (update_index, info) in updates.iter().enumerate() {
                    if count >= 256 {
                        truncated = true;
                        break;
                    }
                    let r = &info.object_render_info;
                    let timing = element.md_update_info.block_update_info.get(update_index);
                    objects.push(format!("{{\"objectIndex\":{object_index},\"updateIndex\":{update_index},\"active\":{},\"gainDb\":{},\"position\":[{},{},{}],\"size\":[{},{},{}],\"elevationEnabled\":{},\"screenReference\":{},\"snap\":{},\"blockOffsetFactor\":{},\"rampFrames\":{}}}",!info.b_object_not_active,info.object_basic_info.object_gain,number(r.pos3d[0]),number(r.pos3d[1]),number(r.pos3d[2]),number(r.object_size[0]),number(r.object_size[1]),number(r.object_size[2]),r.b_enable_elevation,r.b_object_use_screen_ref,r.b_object_snap,timing.map_or(0,|t|t.block_offset_factor_bits),timing.map_or(0,|t|t.ramp_duration)));
                    count += 1;
                }
                if truncated {
                    break;
                }
            }
        }
        let program = &payload.program_assignment;
        let bed_indices: Vec<usize> = program
            .bed_assignment
            .iter()
            .flat_map(|bed| bed.to_index_vec())
            .collect();
        let bed_names = [
            "L", "R", "C", "LFE", "Ls", "Rs", "Lb", "Rb", "Tfl", "Tfr", "Tml", "Tmr", "Tbl", "Tbr",
            "Lw", "Rw", "LFE2",
        ];
        let roles = (0..payload.object_count)
            .map(|i| {
                if i < program.num_bed_objects {
                    format!(
                        "\"bed:{}\"",
                        bed_indices.get(i).map_or("unknown", |&n| bed_names
                            .get(n)
                            .copied()
                            .unwrap_or("unknown"))
                    )
                } else if i < program.num_bed_objects + program.num_isf_objects {
                    format!("\"isf:{}\"", i - program.num_bed_objects)
                } else {
                    format!(
                        "\"object:{}\"",
                        i - program.num_bed_objects - program.num_isf_objects
                    )
                }
            })
            .collect::<Vec<_>>()
            .join(",");
        payloads.push(format!("{{\"version\":{},\"objectCount\":{},\"evoSampleOffset\":{},\"elementSampleOffset\":{},\"programAssignment\":{{\"bedChannelIndices\":{:?},\"bedObjects\":{},\"isfObjects\":{},\"dynamicObjects\":{},\"bedChannelDistribute\":{}}},\"elementRoles\":[{}],\"updates\":[{}],\"truncated\":{}}}",payload.oamd_version,payload.object_count,payload.evo_sample_offset,payload.object_element.as_ref().map_or(0,|e|e.md_update_info.sample_offset),bed_indices,program.num_bed_objects,program.num_isf_objects,program.num_dynamic_objects,program.b_bed_chan_distribute,roles,objects.join(","),truncated));
    }
    format!("[{}]", payloads.join(","))
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn aether_audio_pull(pcm_output: usize, info_output: usize) -> u32 {
    pull(pcm_output, info_output, true)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn aether_audio_pull_pcm(pcm_output: usize, info_output: usize) -> u32 {
    pull(pcm_output, info_output, false)
}

fn pull(pcm_output: usize, info_output: usize, include_metadata: bool) -> u32 {
    if let Some(code) = RENDER_FAILURE.with(|l| l.borrow().status()) {
        return code;
    }
    if !valid(pcm_output, 160 * 16 * 4) || !valid(info_output, 64) {
        return 2;
    }
    let result = DECODER.with(|d| match d.borrow_mut().as_mut() {
        Some(decoder) => decoder.pull().map_err(|e| 100 + e as u32),
        None => Err(3),
    });
    let block = match result {
        Ok(Some(block)) => block,
        Ok(None) => return 1,
        Err(code) => return code,
    };
    let metadata_count = block.metadata.len() as u32;
    let object_count = block
        .metadata
        .iter()
        .map(|m| m.object_count as u32)
        .max()
        .unwrap_or(0);
    let rendered = RENDERER.with(|r| match r.borrow_mut().as_mut() {
        Some(renderer) => renderer
            .render_block(&block)
            .map(Some)
            .map_err(|e| e.status_code()),
        None => Ok(None),
    });
    let rendered = match rendered {
        Ok(rendered) => rendered,
        Err(code) => {
            RENDER_FAILURE.with(|l| l.borrow_mut().record(code));
            LABELS.with(|v| v.borrow_mut().clear());
            METADATA.with(|v| v.borrow_mut().clear());
            return code;
        }
    };
    let output_channels = rendered.as_ref().map_or(block.channels, |r| r.channels);
    let applied_updates = rendered.as_ref().map_or(0, |r| r.applied_updates as u32);
    LABELS.with(|v| {
        let mut labels = v.borrow_mut();
        labels.clear();
        labels.push(b'[');
        let mut append = |label: &str| {
            if labels.len() > 1 {
                labels.push(b',');
            }
            labels.push(b'"');
            labels.extend_from_slice(label.as_bytes());
            labels.push(b'"');
        };
        if let Some(output) = &rendered {
            for label in &output.labels {
                append(label);
            }
        } else {
            for label in &block.labels {
                append(label);
            }
        }
        labels.push(b']');
    });
    METADATA.with(|v| {
        let mut metadata = v.borrow_mut();
        if include_metadata {
            *metadata = metadata_json(&block).into_bytes();
        } else {
            metadata.clear();
        }
    });
    let (samples, bytes) = match &rendered {
        Some(output) => (
            output.pcm.as_ptr().cast::<u8>(),
            size_of_val(output.pcm.as_slice()),
        ),
        None => (
            block.pcm.as_ptr().cast::<u8>(),
            size_of_val(block.pcm.as_slice()),
        ),
    };
    // WASM is little-endian. The output allocation supplied by the caller
    // is separate from the decoder and renderer's owned PCM.
    unsafe { core::ptr::copy_nonoverlapping(samples, pcm_output as *mut u8, bytes) };
    let info = [
        block.sample_rate,
        block.frames as u32,
        output_channels as u32,
        block.presentation as u32,
        metadata_count,
        object_count,
        u32::from(block.is_duplicate),
        block.access_unit as u32,
        (block.access_unit >> 32) as u32,
        block.input_offset as u32,
        (block.input_offset >> 32) as u32,
        block.output_frame_offset as u32,
        (block.output_frame_offset >> 32) as u32,
        u32::from(rendered.is_some()),
        block.channels as u32,
        applied_updates,
    ];
    for (i, word) in info.iter().enumerate() {
        unsafe {
            core::ptr::copy_nonoverlapping(
                word.to_le_bytes().as_ptr(),
                (info_output + i * 4) as *mut u8,
                4,
            );
        }
    }
    0
}
#[unsafe(no_mangle)]
pub extern "C" fn aether_audio_pending() -> u32 {
    DECODER.with(|d| {
        d.borrow()
            .as_ref()
            .map_or(0, |decoder| decoder.pending_bytes() as u32)
    })
}
#[unsafe(no_mangle)]
pub extern "C" fn aether_audio_labels_ptr() -> *const u8 {
    LABELS.with(|v| v.borrow().as_ptr())
}
#[unsafe(no_mangle)]
pub extern "C" fn aether_audio_labels_len() -> usize {
    LABELS.with(|v| v.borrow().len())
}
#[unsafe(no_mangle)]
pub extern "C" fn aether_audio_metadata_ptr() -> *const u8 {
    METADATA.with(|v| v.borrow().as_ptr())
}
#[unsafe(no_mangle)]
pub extern "C" fn aether_audio_metadata_len() -> usize {
    METADATA.with(|v| v.borrow().len())
}

#[unsafe(no_mangle)]
pub extern "C" fn aether_audio_set_render(mode: u32) -> u32 {
    if let Some(code) = RENDER_FAILURE.with(|l| l.borrow().status()) {
        return code;
    }
    let layout = match mode {
        0 => None,
        1 | 3 => Some(crate::render::Layout::FiveOneFour),
        2 | 4 => Some(crate::render::Layout::SevenOneFour),
        _ => return 2,
    };
    RENDERER.with(|r| {
        *r.borrow_mut() = layout.map(if mode >= 3 {
            crate::render::ObjectRenderer::with_elevation_exclusion
        } else {
            crate::render::ObjectRenderer::new
        });
    });
    0
}
