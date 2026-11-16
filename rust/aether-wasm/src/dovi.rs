//! Dolby Vision bitstream exports: framing inspection, BL/EL/RPU split, RPU
//! summary, FEL GPU tables and SPS colour relabelling.

use crate::{input, set_result};
use aether_dovi::{
    fel::{self, MappingHistory},
    hevc::{self, Framing, Kind, Limits},
    semantic,
    vui::{self, ColourPatch},
};
use std::cell::RefCell;

fn framing(code: u32) -> Option<Framing> {
    match code {
        0 => Some(Framing::AnnexB),
        1..=4 => Some(Framing::LengthPrefixed(code as u8)),
        _ => None,
    }
}

/// Result: u32 LE words `[total, base, enhancement, rpu, crc_valid_rpu]`. Status 0 ok,
/// -1 bad framing argument, `100 + code` for a framing error.
///
/// # Safety
/// `ptr` must reference `len` readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn aether_dovi_inspect(ptr: *const u8, len: usize, framing_code: u32) -> i32 {
    let Some(f) = framing(framing_code) else { return -1 };
    match hevc::inspect(unsafe { input(ptr, len) }, f, Limits::default()) {
        Ok(r) => {
            let words = [r.total, r.base, r.enhancement, r.rpu, r.crc_valid_rpu];
            set_result(words.iter().flat_map(|w| w.to_le_bytes()).collect());
            0
        }
        Err(e) => 100 + e as i32,
    }
}

/// Result: repeated `[offset u32, length u32, kind u32]` (kind 0 base, 1 enhancement, 2 RPU),
/// offsets relative to `ptr`. Returns the NAL count or a negative status.
///
/// # Safety
/// `ptr` must reference `len` readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn aether_dovi_split(ptr: *const u8, len: usize, framing_code: u32) -> i32 {
    let Some(f) = framing(framing_code) else { return -1 };
    let data = unsafe { input(ptr, len) };
    match hevc::split(data, f, Limits::default()) {
        Ok(nals) => {
            let mut out = Vec::with_capacity(nals.len() * 12);
            for nal in &nals {
                let kind = match nal.kind {
                    Kind::Base => 0u32,
                    Kind::Enhancement => 1,
                    Kind::Rpu => 2,
                };
                let offset = (nal.bytes.as_ptr() as usize - data.as_ptr() as usize) as u32;
                for w in [offset, nal.bytes.len() as u32, kind] {
                    out.extend(w.to_le_bytes());
                }
            }
            set_result(out);
            nals.len() as i32
        }
        Err(e) => -(100 + e as i32),
    }
}

/// Result: u32 LE `[profile, enhancement (0 unknown, 1 FEL, 2 MEL), disable_residual,
/// use_previous_mapping, has_mapping, has_display_metadata, bl_depth, el_depth, vdr_depth,
/// coefficient_log2_denom]`. Input is one type-62 NAL. Negative: `-(300 + code)`.
///
/// # Safety
/// `ptr` must reference `len` readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn aether_dovi_inspect_rpu(ptr: *const u8, len: usize) -> i32 {
    match semantic::inspect_rpu(unsafe { input(ptr, len) }) {
        Ok(r) => {
            let e = match r.enhancement {
                semantic::EnhancementType::Unknown => 0u32,
                semantic::EnhancementType::Fel => 1,
                semantic::EnhancementType::Mel => 2,
            };
            let words = [
                u32::from(r.profile),
                e,
                u32::from(r.disable_residual),
                u32::from(r.use_previous_mapping),
                u32::from(r.has_mapping),
                u32::from(r.has_display_metadata),
                r.base_bit_depth,
                r.enhancement_bit_depth,
                r.vdr_bit_depth,
                r.coefficient_log2_denom,
            ];
            set_result(words.iter().flat_map(|w| w.to_le_bytes()).collect());
            0
        }
        Err(e) => -(300 + e as i32),
    }
}

/// One type-62 NAL rewritten to Profile 8.1; the result is the new NAL. Returns its length or `-(300 + code)`.
///
/// # Safety
/// `ptr` must reference `len` readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn aether_dovi_to_81(ptr: *const u8, len: usize) -> i32 {
    match semantic::convert_rpu_to_81(unsafe { input(ptr, len) }) {
        Ok(nal) => {
            let n = nal.len() as i32;
            set_result(nal);
            n
        }
        Err(e) => -(300 + e as i32),
    }
}

thread_local! {
    static FEL: RefCell<MappingHistory> = RefCell::new(MappingHistory::default());
}

/// Starts a new mapping epoch (call on load and on every seek).
#[unsafe(no_mangle)]
pub extern "C" fn aether_fel_reset(epoch: u64) {
    FEL.with_borrow_mut(|h| h.reset(epoch));
}

/// Result: `GPU_TABLE_WORDS` little-endian i32 words (header, per-component pivots,
/// polynomial / MMR coefficients, prediction and NLQ tables). Returns the word count,
/// or `-(1 + fel error index)`.
///
/// # Safety
/// `ptr` must reference `len` readable bytes holding one type-62 NAL.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn aether_fel_gpu_tables(epoch: u64, ptr: *const u8, len: usize) -> i32 {
    let nal = unsafe { input(ptr, len) };
    match FEL.with_borrow_mut(|h| fel::gpu_tables_for_rpu(h, epoch, nal)) {
        Ok(t) => {
            set_result(t.words().iter().flat_map(|w| w.to_le_bytes()).collect());
            t.words().len() as i32
        }
        Err(e) => -(1 + e as i32),
    }
}

fn patch(p: i32, t: i32, m: i32, full_range: i32) -> ColourPatch {
    let pick = |v: i32| u8::try_from(v).ok();
    ColourPatch { primaries: pick(p), transfer: pick(t), matrix: pick(m), full_range: full_range > 0 }
}

/// Rewrites the VUI colour fields of one SPS NAL; negative arguments keep the stream value
/// (use `1, 13, -1` for the SDR carrier). Result: `[orig primaries, orig transfer, orig
/// matrix, 0]` followed by the new NAL, whose length may differ by emulation prevention
/// bytes. An SPS that states no colour gets the description added; `full_range` (1 or 0) is the
/// range flag written where the stream has no video signal type. Returns the new NAL length, or
/// `-(1 + error index)`.
///
/// # Safety
/// `ptr` must reference `len` readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn aether_dovi_relabel_sps(ptr: *const u8, len: usize, primaries: i32, transfer: i32, matrix: i32, full_range: i32) -> i32 {
    match vui::relabel_sps(unsafe { input(ptr, len) }, patch(primaries, transfer, matrix, full_range)) {
        Ok(r) => {
            let mut out = vec![r.original.primaries, r.original.transfer, r.original.matrix, 0];
            out.extend_from_slice(&r.nal);
            set_result(out);
            r.nal.len() as i32
        }
        Err(e) => -(1 + e as i32),
    }
}

/// Same for every SPS inside an `hvcC` record; returns the new record length. The header
/// carries the first SPS's original colour.
///
/// # Safety
/// `ptr` must reference `len` readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn aether_dovi_relabel_hvcc(ptr: *const u8, len: usize, primaries: i32, transfer: i32, matrix: i32, full_range: i32) -> i32 {
    match vui::relabel_hvcc(unsafe { input(ptr, len) }, patch(primaries, transfer, matrix, full_range)) {
        Ok((record, originals)) => {
            let o = originals.first().copied().unwrap_or(vui::Colour { primaries: 0, transfer: 0, matrix: 0 });
            let mut out = vec![o.primaries, o.transfer, o.matrix, 0];
            out.extend_from_slice(&record);
            set_result(out);
            record.len() as i32
        }
        Err(e) => -(1 + e as i32),
    }
}

/// Display metadata of one RPU as UTF-8 JSON (layout in `aether_dovi::display`). Input is one
/// type-62 NAL, guarded by the CRC and `rpu_guard` preflight before libdovi parses it. Returns
/// the JSON length (result buffer) or `-(300 + code)` like `aether_dovi_inspect_rpu`.
///
/// # Safety
/// `ptr` must reference `len` readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn aether_dovi_rpu_display(ptr: *const u8, len: usize) -> i32 {
    match aether_dovi::display::display_json(unsafe { input(ptr, len) }) {
        Ok(json) => {
            let n = json.len() as i32;
            set_result(json.into_bytes());
            n
        }
        Err(e) => -(300 + e as i32),
    }
}
