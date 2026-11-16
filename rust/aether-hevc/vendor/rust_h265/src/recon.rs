//! Reconstruction command stream (feature `recon-stream`).
//!
//! Splits decoding into (a) syntax and entropy decoding, which records for every picture what reconstruction
//! needs, and (b) reconstruction from that record alone. The record of a picture ([`PicRecord`]) holds:
//!
//! - transform units ([`TuRec`]): position, size, component, transform kind, QP, scaling list, raw levels;
//! - inter prediction units ([`PuRec`]): motion vectors, reference DPB slots, explicit weight entries;
//! - intra transform blocks ([`IntraRec`]): mode, neighbour availability, the dependency wave they run in and
//!   the residual blocks that belong to them;
//! - PCM blocks with their samples;
//! - loop filter inputs ([`LoopFilterInputs`]): deblocking edge codes (the GPU derives the boundary strengths
//!   from them, the PU and the TU records: [`resolve_boundary_strengths`]), QP map, per-CTB slice / tile /
//!   offsets, SAO.
//!
//! [`Recorder`] collects the records while the normal decoder runs (`Decoder::with_recon_stream`), assigns DPB
//! slots and reports output order ([`Event`]). [`encode_event`] / [`decode_event`] give the packed `u32` layout
//! documented in `web/gpu-recon/FORMAT-STREAM.md`. [`Replayer`] rebuilds the pictures from decoded events with
//! the decoder's own reconstruction functions, in the order a GPU would run them (all motion compensation,
//! inter residuals, PCM, then the intra waves), which proves the stream complete.
use std::sync::Arc;
use std::sync::atomic::Ordering;

use crate::cu_tree::{Mv, MvField, PictureState};
use crate::decoder::Frame;
use crate::dpb::{DecodedPicture, DecodedPictureBuffer};
use crate::intra_pred::ReferenceAvailability;
use crate::pixel::{Pixel, PixelData};
use crate::pps::Pps;
use crate::residual_coding::ResidualBlock;
use crate::sao::SaoType;
use crate::slice::{PredWeightTable, SliceHeader};
use crate::sps::Sps;

/// "No index" / "no slot".
pub const NONE: u32 = u32::MAX;

/// Availability bits of an intra block's reference samples.
/// Availability bits of an intra block's reference samples (the intra kernel's order: bottom-left, left,
/// up-left, up, up-right).
pub const AVAIL_BOTTOM_LEFT: u8 = 1;
pub const AVAIL_LEFT: u8 = 2;
pub const AVAIL_UP_LEFT: u8 = 4;
pub const AVAIL_UP: u8 = 8;
pub const AVAIL_UP_RIGHT: u8 = 16;

/// Transform unit flags (bit positions of FORMAT-idct.md word 1, shifted down by 4).
/// 4x4 DST (intra luma 4x4) instead of the DCT.
pub const TU_DST: u8 = 1;
/// transform_skip_flag.
pub const TU_TRANSFORM_SKIP: u8 = 2;
/// cu_transquant_bypass_flag: the levels are the residual.
pub const TU_BYPASS: u8 = 4;
/// Scaling list index meaning "flat 16" (scaling lists off).
pub const SL_FLAT: u8 = 31;

/// One transform block's residual (one component), as raw coefficient levels (FORMAT-idct.md).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TuRec {
    /// 0 = Y, 1 = Cb, 2 = Cr.
    pub c: u8,
    /// Top-left in this component's sample coordinates.
    pub x: u16,
    pub y: u16,
    /// log2 of the block size (2..=5).
    pub log2: u8,
    /// `TU_*` bits.
    pub flags: u8,
    /// Bit depth of the component.
    pub bit_depth: u8,
    /// Scaling list `sizeId * 6 + matrixId`, or [`SL_FLAT`].
    pub sl_idx: u8,
    /// qP + QpBdOffset of the component (what `compute_dequant_scale` receives).
    pub qp_prime: u8,
    /// First entry in [`PicRecord::coeffs`] and entry count. An entry is `pos | (level as i16 as u16) << 16`,
    /// `pos = y * n + x`, `level` = TransCoeffLevel with its final sign, not dequantised.
    pub coef: u32,
    pub num_coefs: u32,
    /// Index of the intra block ([`PicRecord::intra`]) this residual belongs to, or [`NONE`] (inter).
    pub owner: u32,
}

// Raw levels of the residual block being decoded, collected by `decode_residual_simple`. One sink per thread: the
// residual decode and the recorder call that takes its levels run on the same thread (a wavefront row's worker).
// The sink also holds the thread's levels-only mode, so both reach a wavefront worker through `levels_for` and
// share the per-stack storage of a threaded WASM build.
struct LevelSink {
    on: bool,
    /// Entropy-only recording: `decode_residual_simple` records the levels and skips the dense block.
    levels_only: bool,
    valid: bool,
    levels: Vec<u32>,
    qp_prime: i32,
}

impl LevelSink {
    const fn new() -> Self {
        Self {
            on: false,
            levels_only: false,
            valid: false,
            levels: Vec::new(),
            qp_prime: 0,
        }
    }
}

#[cfg(not(all(target_arch = "wasm32", feature = "wasm-threads")))]
thread_local! {
    static LEVELS: std::cell::RefCell<LevelSink> = const { std::cell::RefCell::new(LevelSink::new()) };
}

#[cfg(not(all(target_arch = "wasm32", feature = "wasm-threads")))]
fn with_sink<R>(f: impl FnOnce(&mut LevelSink) -> R) -> R {
    LEVELS.with(|l| f(&mut l.borrow_mut()))
}

// A threaded WASM build links the prebuilt std, whose `thread_local!` is one plain static shared by every host
// worker, so the sinks are kept per stack instead (`thr::thread_slot`).
#[cfg(all(target_arch = "wasm32", feature = "wasm-threads"))]
fn with_sink<R>(f: impl FnOnce(&mut LevelSink) -> R) -> R {
    struct Sinks([std::cell::UnsafeCell<LevelSink>; crate::thr::THREAD_SLOTS]);
    // SAFETY: slot k is only used by the thread running on stack k (below).
    unsafe impl Sync for Sinks {}
    static SINKS: Sinks =
        Sinks([const { std::cell::UnsafeCell::new(LevelSink::new()) }; crate::thr::THREAD_SLOTS]);
    // SAFETY: threads running at the same time have distinct slots, and `f` (one of this module's sink accesses)
    // does not re-enter `with_sink`, so this is the only reference to the slot.
    f(unsafe { &mut *SINKS.0[crate::thr::thread_slot()].get() })
}

/// Start collecting the levels of one residual block (`None` when no recorder is active on this thread), and
/// whether only the levels are needed (entropy-only recording: no dense block, dequantisation or transform-skip
/// scaling).
pub(crate) fn levels_begin() -> (Option<Vec<u32>>, bool) {
    with_sink(|l| {
        if !l.on {
            return (None, false);
        }
        l.valid = false;
        let mut v = std::mem::take(&mut l.levels);
        v.clear();
        // `append_levels` reads LEVEL_COPY words.
        v.reserve(LEVEL_COPY);
        (Some(v), l.levels_only)
    })
}

/// Words `append_levels` copies at once.
const LEVEL_COPY: usize = 16;

/// Append the levels of one residual block (from the level sink, capacity at least [`LEVEL_COPY`]). Most blocks
/// have a few levels: a fixed-size copy (plain loads and stores) instead of a `memcpy` call per block, which was
/// most of `PicRecorder::residual` on WebAssembly.
#[inline]
fn append_levels(dst: &mut Vec<u32>, src: &Vec<u32>) {
    type Chunk = [std::mem::MaybeUninit<u32>; LEVEL_COPY];
    let n = src.len();
    if n > LEVEL_COPY || src.capacity() < LEVEL_COPY {
        dst.extend_from_slice(src);
        return;
    }
    dst.reserve(LEVEL_COPY);
    let len = dst.len();
    // SAFETY: both buffers hold at least LEVEL_COPY words of capacity from the pointers used (words past `src.len()`
    // are copied as uninitialised memory and stay past `dst.len()`); the first `n` words written are initialised.
    unsafe {
        let chunk = std::ptr::read(src.as_ptr() as *const Chunk);
        std::ptr::write(dst.as_mut_ptr().add(len) as *mut Chunk, chunk);
        dst.set_len(len + n);
    }
}

/// End of one residual block: keep its levels and qP' for the recorder.
pub(crate) fn levels_end(v: Option<Vec<u32>>, qp_prime: i32) {
    if let Some(v) = v {
        with_sink(|l| {
            l.levels = v;
            l.qp_prime = qp_prime;
            l.valid = true;
        });
    }
}

/// Turn level collection on this thread on or off.
fn levels_on(on: bool) {
    with_sink(|l| l.on = on);
}

/// Level sink and levels-only mode of this thread, for a decoder about to decode (`Decoder::decode_nal`) or a
/// wavefront row about to record (`crate::wavefront::decode_rows`): `None` without a recorder, else whether the
/// recorder is entropy-only. Returns the previous mode, for a worker to restore.
pub(crate) fn levels_for(recorder: Option<bool>) -> Option<bool> {
    with_sink(|l| {
        let was = l.on.then_some(l.levels_only);
        l.on = recorder.is_some();
        l.levels_only = recorder == Some(true);
        was
    })
}

/// One inter prediction unit (the MC kernel's record).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PuRec {
    /// Luma position and size.
    pub x: u16,
    pub y: u16,
    pub w: u8,
    pub h: u8,
    /// Bit 0: list 0 used, bit 1: list 1 used.
    pub pred_flag: u8,
    /// Explicit weighted prediction (weighted_pred_flag / weighted_bipred_flag of the slice).
    pub weighted: bool,
    /// Quarter-sample luma motion vectors per list (0 for an unused list).
    pub mv: [Mv; 2],
    /// DPB slot of the reference picture per list ([`NONE`] when the list is unused).
    pub slot: [u32; 2],
    /// Weight entry ([`PicRecord::weights`]) per used list when `weighted`, else 0.
    pub wt: [u32; 2],
}

/// One intra prediction block (luma, or both chroma planes).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IntraRec {
    /// 0 = luma, 1 = chroma (Cb and Cr, same mode and availability).
    pub c: u8,
    /// Top-left in this component's sample coordinates.
    pub x: u16,
    pub y: u16,
    pub log2: u8,
    /// Intra mode (0 planar, 1 DC, 2..=34 angular), in luma mode space for chroma (mode 4 already mapped).
    pub mode: u8,
    /// `AVAIL_*` bits.
    pub avail: u8,
    /// constrained_intra_pred_flag: reference samples of non-intra blocks are unavailable.
    pub cip: bool,
    /// disableIntraBoundaryFilter (implicit RDPCM with bypass): DC / angular edge filters off.
    pub no_boundary_filter: bool,
    /// Dependency wave (1..): every block it reads from is in an earlier wave (inter and PCM are wave 0).
    pub wave: u32,
    /// Residual blocks of this intra block: `[Y]` for luma, `[Cb, Cr]` for chroma ([`NONE`] when not coded).
    pub tu: [u32; 2],
}

/// The intra kernel's schedule buffer (`buildSchedule` layout: groups, steps at `step_base`, scheduled records at
/// `tu_base`) and its dispatches as `groupBase, groupCount` pairs.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IntraSchedule {
    pub step_base: u32,
    pub tu_base: u32,
    pub sched: Vec<u32>,
    pub dispatches: Vec<u32>,
}

/// One PCM coding unit.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PcmRec {
    pub x: u16,
    pub y: u16,
    pub log2: u8,
    /// Offset in [`PicRecord::pcm_samples`]: `n * n` luma samples, then Cb and Cr (`n / 2` squared each for
    /// 4:2:0), already at the picture bit depth.
    pub samples: u32,
}

/// Deblocking and SAO inputs in the kernels' layouts (FORMAT-deblock.md, FORMAT-sao.md), packed from the tables
/// the decoder holds at the end of the picture (the packing is `rust/gpu-recon-ref-filters/src/pack.rs`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LoopFilterInputs {
    /// One u32 per 8x8 luma block, `width / 8` per row: edge code (or, without `edge_codes`, bS) of its left / top
    /// edge halves, the four 4x4 no-filter flags, QpY.
    pub block_map: Vec<u32>,
    /// Bits 0..7 of the block map are edge codes ([`EDGE_TRANSFORM`], [`EDGE_PU`], [`EDGE_INTRA`]), from which
    /// [`resolve_boundary_strengths`] (and the GPU kernel `bs.wgsl.mjs`) derive the boundary strengths. Without it
    /// (streams of version 1) they are the strengths themselves.
    pub edge_codes: bool,
    /// One u32 per CTB (raster): slice / tile skip of its left and top edges, beta_offset, tc_offset.
    pub deblock_ctb: Vec<u32>,
    /// Four u32 per CTB (raster): 3x3 neighbour availability, SAO type / class per plane, offsets.
    pub sao_ctb: Vec<u32>,
    /// Three u32 per CTB (raster): slice address, tile-scan address, tile id | filter_slice_edges << 31. Not
    /// needed by the kernels (the skip and availability bits above carry the slice / tile rules); the replay
    /// rebuilds the decoder's tables from it and checks the packed bits against them.
    pub ctb_info: Vec<u32>,
    pub any_no_filter: bool,
    pub filter_across_tiles: bool,
}

/// Deblocking edge codes (FORMAT-deblock.md "Edge codes"), 2 bits per 4-sample edge segment on the 8x8 grid.
/// No edge (or an edge of a slice with deblocking disabled): bS 0.
pub const EDGE_NONE: u8 = 0;
/// A transform block edge (also the edge of a CU without residual): bS 2 when either side is intra, else 1 when
/// either side's luma transform block has coded residual, else the motion comparison.
pub const EDGE_TRANSFORM: u8 = 1;
/// A prediction block edge inside a transform block: the motion comparison alone.
pub const EDGE_PU: u8 = 2;
/// A transform block edge whose Q side is intra (intra TU, PCM CU): bS 2.
pub const EDGE_INTRA: u8 = 3;

/// FORMAT-deblock.md block map, bits 0..7 from `v` / `h` (per 4x4 luma block, `width / 4` per row: the
/// recorder's edge codes, or the decoder's boundary strengths).
fn pack_block_map<P: Pixel>(st: &PictureState<P>, v: &[u8], hz: &[u8]) -> Vec<u32> {
    let (w, h) = (st.width as usize, st.height as usize);
    let (bw, bs_w, pu_w) = (w / 8, w >> 2, st.min_pu_width);
    let qp_shift = st.log2_min_cb_size as usize;
    let rows = if bw == 0 { 0 } else { h / 8 };
    // Written once, row by row: a zero-filled map (half a megabyte at 4K) overwritten afterwards cost a second pass
    // over it (a `memory.fill` call on WebAssembly).
    let mut out = Vec::with_capacity(bw * rows);
    // Row slices instead of per-field index arithmetic (this ran per 8x8 block of every picture). Block (bx, by)
    // reads 4x4 cells (2bx, 2bx+1) of rows 2by and 2by+1.
    for by in 0..rows {
        let (r0, r1) = (2 * by * bs_w, (2 * by + 1) * bs_w);
        let (v0, v1) = (&v[r0..r0 + bs_w], &v[r1..r1 + bs_w]);
        let h0 = &hz[r0..r0 + bs_w];
        // get_qp_y (deblock.rs) for a position inside the picture.
        let qrow = (by * 8 >> qp_shift) * st.min_cb_width;
        let qp = &st.tab_qp_y[qrow..qrow + ((w - 1) >> qp_shift) + 1];
        let start = out.len();
        // Pairs of cells (`bs_w / 2 == bw`) without an index check per cell, and with 8x8 minimum coding blocks the
        // qPs in sequence: a plain zip, which vectorises (this function went from 2.8 to 1.1% of the entropy-only
        // decode on WebAssembly).
        let cells = v0.chunks_exact(2).zip(v1.chunks_exact(2)).zip(h0.chunks_exact(2));
        let word = |((a, b), c): ((&[u8], &[u8]), &[u8]), q: i32| {
            (a[0] as u32 & 3)
                | (b[0] as u32 & 3) << 2
                | (c[0] as u32 & 3) << 4
                | (c[1] as u32 & 3) << 6
                | (q as i8 as u8 as u32) << 16
        };
        if qp_shift == 3 {
            out.extend(cells.zip(qp).map(|(e, &q)| word(e, q as i32)));
        } else {
            out.extend(cells.enumerate().map(|(bx, e)| word(e, qp[bx * 8 >> qp_shift] as i32)));
        }
        let o = &mut out[start..];
        if st.any_no_filter {
            let (n0, n1) = (2 * by * pu_w, (2 * by + 1) * pu_w);
            let nf = |i: usize| (st.tab_no_filter[i] != 0) as u32;
            for (bx, o) in o.iter_mut().enumerate() {
                let x2 = 2 * bx;
                *o |= nf(n0 + x2) << 8 | nf(n0 + x2 + 1) << 9 | nf(n1 + x2) << 10 | nf(n1 + x2 + 1) << 11;
            }
        }
    }
    out
}

/// The bS kernel's cell map (`bs.wgsl.mjs` entries `bsPu` and `bsTu`): one u32 per 4x4 luma block, `width / 4` per
/// row, holding the index + 1 of the PU record that covers it (0: none, an intra or PCM block) and bit 31 when a
/// luma TU record (a luma transform block with coded residual, cbf_luma) covers it.
pub fn bs_cells(width: u32, height: u32, pus: &[PuRec], tus: &[TuRec]) -> Vec<u32> {
    let (cw, ch) = ((width / 4) as usize, (height / 4) as usize);
    let mut cells = vec![0u32; cw * ch];
    for (i, u) in pus.iter().enumerate() {
        let (x0, y0) = (u.x as usize / 4, u.y as usize / 4);
        let x1 = (x0 + u.w as usize / 4).min(cw);
        for y in y0..(y0 + u.h as usize / 4).min(ch) {
            if x0 < x1 {
                cells[y * cw + x0..y * cw + x1].fill(i as u32 + 1);
            }
        }
    }
    for t in tus.iter().filter(|t| t.c == 0) {
        let (x0, y0, n) = (t.x as usize / 4, t.y as usize / 4, 1usize << (t.log2 - 2));
        for y in y0..(y0 + n).min(ch) {
            for c in &mut cells[y * cw + x0.min(cw)..y * cw + (x0 + n).min(cw)] {
                *c |= 1 << 31;
            }
        }
    }
    cells
}

/// Boundary strength from motion alone (spec 8.7.2.4, `cu_tree.rs` `inter_boundary_strength_full`) between the
/// PUs on the P and Q side of an edge. Reference pictures are compared by DPB slot: the recorder marks a picture
/// unsupported when two slots it references hold the same POC, so equal slots and equal POCs agree.
fn motion_bs(p: &PuRec, q: &PuRec) -> u32 {
    let far = |a: Mv, b: Mv| (a.x as i32 - b.x as i32).abs() >= 4 || (a.y as i32 - b.y as i32).abs() >= 4;
    let (pf, qf) = (p.pred_flag & 3, q.pred_flag & 3);
    let b = if pf == 3 && qf == 3 {
        let ([p0, p1], [q0, q1]) = (p.slot, q.slot);
        if q0 == p0 && q0 == q1 && p0 == p1 {
            (far(p.mv[0], q.mv[0]) || far(p.mv[1], q.mv[1])) && (far(p.mv[1], q.mv[0]) || far(p.mv[0], q.mv[1]))
        } else if q0 == p0 && q1 == p1 {
            far(p.mv[0], q.mv[0]) || far(p.mv[1], q.mv[1])
        } else if q1 == p0 && q0 == p1 {
            far(p.mv[1], q.mv[0]) || far(p.mv[0], q.mv[1])
        } else {
            true
        }
    } else if pf != 3 && qf != 3 {
        let l = |u: &PuRec| if u.pred_flag & 1 != 0 { 0 } else { 1 };
        let (lp, lq) = (l(p), l(q));
        p.slot[lp] != q.slot[lq] || far(p.mv[lp], q.mv[lq])
    } else {
        true
    };
    b as u32
}

/// bS of one edge segment from its code and the cells on its P and Q side ([`bs_cells`]).
fn edge_bs(code: u32, cp: u32, cq: u32, pus: &[PuRec]) -> u32 {
    let (ip, iq) = (cp & 0x7fff_ffff, cq & 0x7fff_ffff);
    match code {
        0 => 0,
        3 => 2,
        _ if ip == 0 || iq == 0 => 2,
        1 if (cp | cq) >> 31 != 0 => 1,
        _ if ip == iq => 0,
        _ => motion_bs(&pus[ip as usize - 1], &pus[iq as usize - 1]),
    }
}

/// Replaces the edge codes in bits 0..7 of a block map with boundary strengths, exactly as the GPU kernel
/// `bs.wgsl.mjs` does (FORMAT-deblock.md "Edge codes"). `pus` and `tus` are the picture's records in stream
/// order.
pub fn resolve_boundary_strengths(map: &mut [u32], width: u32, height: u32, pus: &[PuRec], tus: &[TuRec]) {
    let cells = bs_cells(width, height, pus, tus);
    let (mw, cw) = ((width / 8) as usize, (width / 4) as usize);
    for by in 0..(height / 8) as usize {
        for bx in 0..mw {
            let m = map[by * mw + bx];
            if m & 0xff == 0 {
                continue;
            }
            let mut out = m & !0xff;
            for k in 0..4 {
                let code = (m >> (2 * k)) & 3;
                if code == 0 {
                    continue;
                }
                // Segments 0, 1: vertical edge at x, rows +0 / +4; 2, 3: horizontal edge at y, columns +0 / +4.
                // The picture's left and top edges are never filtered (the recorder writes no code there).
                let (qx, qy) = if k < 2 { (2 * bx, 2 * by + k) } else { (2 * bx + k - 2, 2 * by) };
                if (k < 2 && qx == 0) || (k >= 2 && qy == 0) {
                    continue;
                }
                let (px, py) = if k < 2 { (qx - 1, qy) } else { (qx, qy - 1) };
                let bs = edge_bs(code, cells[py * cw + px], cells[qy * cw + qx], pus);
                out |= bs << (2 * k);
            }
            map[by * mw + bx] = out;
        }
    }
}

fn ctb_dims<P: Pixel>(st: &PictureState<P>) -> (usize, usize, usize) {
    let l = st.log2_ctb_size as usize;
    ((st.width as usize).div_ceil(1 << l), (st.height as usize).div_ceil(1 << l), l)
}

/// FORMAT-deblock.md CTB records.
fn pack_deblock_ctbs<P: Pixel>(st: &PictureState<P>) -> Vec<u32> {
    let (wc, hc, l) = ctb_dims(st);
    let mut out = Vec::with_capacity(wc * hc);
    for cy in 0..hc {
        for cx in 0..wc {
            let (x, y) = (cx << l, cy << l);
            let skip_left = cx > 0 && crate::deblock::skip_vertical_slice_boundary(st, x, y);
            let skip_top = cy > 0 && crate::deblock::skip_horizontal_slice_boundary(st, x, y);
            let (b, t) = st.tab_deblock_offsets[cy * wc + cx];
            out.push(skip_left as u32 | (skip_top as u32) << 1 | (b as u8 as u32) << 8 | (t as u8 as u32) << 16);
        }
    }
    out
}

/// FORMAT-sao.md CTB records. `None` when an offset does not fit the record's i8.
fn pack_sao_ctbs<P: Pixel>(st: &PictureState<P>) -> Option<Vec<u32>> {
    let (wc, hc, _) = ctb_dims(st);
    let mut out = Vec::with_capacity(wc * hc * 4);
    for ry in 0..hc {
        for rx in 0..wc {
            let rs = ry * wc + rx;
            let sao = &st.sao_params[rs];
            let mut avail = 0u32;
            for dy in 0..3 {
                for dx in 0..3 {
                    let (nx, ny) = ((rx + dx).wrapping_sub(1), (ry + dy).wrapping_sub(1));
                    let ok = nx < wc
                        && ny < hc
                        && (nx == rx && ny == ry || !crate::sao::sao_skip_slice_boundary(st, rs, ny * wc + nx));
                    avail |= (ok as u32) << (dy * 3 + dx);
                }
            }
            let mut w0 = avail;
            let mut offs = [0u32; 3];
            for c in 0..3 {
                let (t, class) = match sao.type_idx[c] {
                    SaoType::NotApplied => (0u32, 0u32),
                    SaoType::Band => (1, sao.band_position[c] as u32 & 31),
                    SaoType::Edge => (2, sao.eo_class[c] as u32 & 3),
                };
                w0 |= t << (9 + 2 * c) | class << (15 + 5 * c);
                for k in 0..4 {
                    let o = sao.offset_val[c][k + 1];
                    if !(-128..=127).contains(&o) {
                        return None;
                    }
                    offs[c] |= (o as i8 as u8 as u32) << (8 * k);
                }
            }
            out.extend_from_slice(&[w0, offs[0], offs[1], offs[2]]);
        }
    }
    Some(out)
}

/// The decoder's loop filter tables from the packed records (inverse of the three `pack_*`; bS off the 8x8 grid,
/// which no filter reads, comes back as 0), then a check that repacking gives the same records. Edge codes are
/// resolved to boundary strengths first, from the picture's PU and TU records, as the GPU does.
fn unpack_filters<P: Pixel>(st: &mut PictureState<P>, p: &PicRecord) -> Result<(), String> {
    let f = &p.filters;
    let (w, h) = (st.width as usize, st.height as usize);
    let (bs_w, mw) = (w >> 2, w / 8);
    if f.block_map.len() != mw * (h / 8) {
        return Err("block map size".into());
    }
    let mut map = f.block_map.clone();
    if f.edge_codes {
        resolve_boundary_strengths(&mut map, st.width, st.height, &p.pus, &p.tus);
    }
    st.bs_vertical.fill(0);
    st.bs_horizontal.fill(0);
    for by in 0..h / 8 {
        for bx in 0..mw {
            let m = map[by * mw + bx];
            let (x, y) = (bx * 8, by * 8);
            let i = (y >> 2) * bs_w + (x >> 2);
            st.bs_vertical[i] = (m & 3) as u8;
            st.bs_vertical[i + bs_w] = ((m >> 2) & 3) as u8;
            st.bs_horizontal[i] = ((m >> 4) & 3) as u8;
            st.bs_horizontal[i + 1] = ((m >> 6) & 3) as u8;
            for (k, (dx, dy)) in [(0, 0), (4, 0), (0, 4), (4, 4)].into_iter().enumerate() {
                st.tab_no_filter[((y + dy) >> 2) * st.min_pu_width + ((x + dx) >> 2)] = ((m >> (8 + k)) & 1) as u8;
            }
            let cb = (y >> st.log2_min_cb_size) * st.min_cb_width + (x >> st.log2_min_cb_size);
            st.tab_qp_y[cb] = (m >> 16) as u8 as i8;
        }
    }
    st.any_no_filter = f.any_no_filter;
    st.filter_across_tiles = f.filter_across_tiles;
    let (wc, hc, _) = ctb_dims(st);
    if f.ctb_info.len() != 3 * wc * hc || f.deblock_ctb.len() != wc * hc || f.sao_ctb.len() != 4 * wc * hc {
        return Err("CTB record count".into());
    }
    for rs in 0..wc * hc {
        st.tab_slice_addr_rs[rs] = f.ctb_info[3 * rs] as i32;
        st.tab_ctb_ts[rs] = f.ctb_info[3 * rs + 1];
        st.tab_tile_id[rs] = f.ctb_info[3 * rs + 2] & 0x7fff_ffff;
        st.filter_slice_edges[rs] = f.ctb_info[3 * rs + 2] >> 31 != 0;
        let d = f.deblock_ctb[rs];
        st.tab_deblock_offsets[rs] = ((d >> 8) as u8 as i8, (d >> 16) as u8 as i8);
        let r = &f.sao_ctb[4 * rs..4 * rs + 4];
        let s = &mut st.sao_params[rs];
        for c in 0..3 {
            let class = ((r[0] >> (15 + 5 * c)) & 31) as u8;
            s.type_idx[c] = match (r[0] >> (9 + 2 * c)) & 3 {
                1 => SaoType::Band,
                2 => SaoType::Edge,
                _ => SaoType::NotApplied,
            };
            s.band_position[c] = class;
            s.eo_class[c] = class & 3;
            s.offset_val[c] = [0; 5];
            for k in 0..4 {
                s.offset_val[c][k + 1] = (r[1 + c] >> (8 * k)) as u8 as i8 as i16;
            }
        }
    }
    if pack_block_map(st, &st.bs_vertical, &st.bs_horizontal) != map {
        return Err("block map does not round trip".into());
    }
    if pack_deblock_ctbs(st) != f.deblock_ctb {
        return Err("deblocking CTB records disagree with the CTB slice / tile table".into());
    }
    if pack_sao_ctbs(st).as_deref() != Some(&f.sao_ctb[..]) {
        return Err("SAO CTB records disagree with the CTB slice / tile table".into());
    }
    Ok(())
}

/// Picture-level parameters.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PicHeader {
    pub pic_id: u32,
    pub slot: u32,
    pub poc: i32,
    pub width: u32,
    pub height: u32,
    pub bit_depth_y: u8,
    pub bit_depth_c: u8,
    pub chroma_format_idc: u8,
    pub log2_ctb: u8,
    pub log2_min_cb: u8,
    pub log2_min_tb: u8,
    pub sao_enabled: bool,
    pub strong_intra_smoothing: bool,
    pub intra_smoothing_disabled: bool,
    /// pic_output_flag (an Output event follows unless the picture is discarded).
    pub output: bool,
    pub pps_cb_qp_offset: i32,
    pub pps_cr_qp_offset: i32,
    /// Conformance window (left, right, top, bottom), in chroma sample units.
    pub conf_win: [u32; 4],
    /// Highest intra wave of the picture (luma and chroma together).
    pub waves: u32,
}

/// The command stream of one picture.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PicRecord {
    pub header: PicHeader,
    pub tus: Vec<TuRec>,
    /// Coefficient entries of all TUs (see [`TuRec::coef`]).
    pub coeffs: Vec<u32>,
    /// Active scaling lists packed as FORMAT-idct.md "Scaling list table" (24 * 17 words), empty when
    /// scaling_list_enabled_flag is 0.
    pub scaling_lists: Vec<u32>,
    pub pus: Vec<PuRec>,
    /// Explicit weight entries, 4 words each: `weight | offset << 16` for Y, Cb, Cr (i16 halves, offsets already
    /// scaled by `1 << (BitDepth - 8)` unless high_precision_offsets_enabled_flag), then
    /// `luma_log2_weight_denom | chroma_log2_weight_denom << 8`. One entry per distinct (list, reference) weight set.
    pub weights: Vec<[u32; 4]>,
    pub intra: Vec<IntraRec>,
    /// Constrained intra prediction only: one bit per 4x4 luma block (`width / 4` per row), set for intra blocks.
    /// Empty when no block of the picture uses constrained_intra_pred_flag.
    pub intra_map: Vec<u32>,
    /// The intra kernel's `list` schedule (FORMAT-intra.md "Dependencies and schedules").
    pub intra_sched: IntraSchedule,
    /// [`intra_records`] of this picture as the schedule built them, for the packed stream (not part of the record:
    /// equal for any two records, empty after `decode_event`).
    pub intra_packed: PackedIntra,
    pub pcm: Vec<PcmRec>,
    pub pcm_samples: Vec<u16>,
    pub filters: LoopFilterInputs,
    /// A coding tool the stream does not carry was used (4:4:4 cross-component prediction, extended
    /// precision coefficients): the record is incomplete.
    pub unsupported: Option<&'static str>,
}

/// Cached [`intra_records`] of a picture and the intra block count they were built for. Compares equal to any
/// other, so a record parsed back from the stream (no cache) equals the recorded one.
#[derive(Clone, Debug, Default)]
pub struct PackedIntra {
    blocks: usize,
    recs: Vec<[u32; 4]>,
}

impl PartialEq for PackedIntra {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}

/// A mid-grey picture the decoder generated for a missing reference (spec 8.3.3.2).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GeneratedPic {
    pub pic_id: u32,
    pub slot: u32,
    pub poc: i32,
    pub width: u32,
    pub height: u32,
    pub bit_depth_y: u8,
    pub bit_depth_c: u8,
    pub chroma_format_idc: u8,
}

/// Command stream events, in the order a host processes them.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    /// Fill `slot` with a generated grey reference picture.
    Generated(GeneratedPic),
    /// Reconstruct a picture into `header.slot`.
    Picture(Box<PicRecord>),
    /// Display the picture in `slot` (display order).
    Output { pic_id: u32, slot: u32 },
    /// The picture in `slot` will never be displayed (NoOutputOfPriorPicsFlag).
    Discard { pic_id: u32, slot: u32 },
}

// ---- Recording ----------------------------------------------------------------------------------------------

/// The record of the picture being decoded (lives in its `PictureState`).
pub struct PicRecorder {
    /// Parse and record only: no prediction, residual or loop filter runs.
    pub entropy_only: bool,
    rec: PicRecord,
    /// Last intra block per class (luma, chroma) for linking residuals.
    last_intra: [u32; 2],
    /// Deblocking edge codes per 4x4 luma block (`EDGE_*`), `edge_stride` per row, of the vertical edge on its
    /// left side and the horizontal edge on its top side; packed into the block map instead of the strengths. A
    /// row fragment holds only the cell rows of its CTB row, from cell row `edge_y0`; `append` copies the codes it
    /// wrote (every code is written inside the block that writes it, so inside the fragment's CTB row).
    edges_v: Vec<u8>,
    edges_h: Vec<u8>,
    edge_stride: usize,
    edge_y0: usize,
    /// Cell rows of the whole picture's tables.
    edge_rows: usize,
    /// POC of the picture in each DPB slot this picture references (bS compares reference pictures by slot). A
    /// row fragment starts empty and defers every reference it has not seen yet to `append`, which checks it
    /// against the picture's table.
    slot_poc: Vec<Option<i32>>,
    /// Full decode with `Recorder::check_bs`: compare the resolved strengths with the decoder's (in `finish_tables`,
    /// on the picture's recorder, after the row fragments are appended).
    check_bs: bool,
    /// The decoder's own boundary strengths packed as a block map, and the words where the strengths resolved from
    /// the edge codes differ (`check_bs`).
    decoder_bs: Option<Vec<u32>>,
    bs_mismatches: u64,
    /// A row fragment (`row_fragment`), not the picture's recorder.
    fragment: bool,
    /// Row fragment only: checks that depend on records made before the fragment, in recording order, each with
    /// whether the fragment had no unsupported tool yet; done by `append`.
    deferred: Vec<(Deferred, bool)>,
}

/// `PicRecorder::last_intra` of a row fragment before its first intra block of that class.
const INHERIT: u32 = NONE - 1;

/// A check a row fragment leaves to `PicRecorder::append`.
#[derive(Clone, Copy)]
enum Deferred {
    /// Intra residual (TU index in the fragment) whose owner is an intra block recorded before the fragment.
    IntraOwner(u32),
    /// A reference picture (DPB slot, POC) the fragment had not seen yet: `PicRecorder::reference` on the picture.
    Reference(usize, i32),
}

impl PicRecorder {
    /// An empty recorder for one CTB row of a wavefront decode (`crate::wavefront::decode_rows`), whose records
    /// `append` adds to this one's in decoding order.
    /// `ctb_row` is the CTB row it decodes (its edge code tables cover those cell rows only).
    pub(crate) fn row_fragment(&self, ctb_row: usize, log2_ctb: u8) -> Box<PicRecorder> {
        let per_row = 1usize << (log2_ctb - 2);
        let edge_y0 = (ctb_row * per_row).min(self.edge_rows);
        let cells = self.edge_stride * per_row.min(self.edge_rows - edge_y0);
        Box::new(PicRecorder {
            entropy_only: self.entropy_only,
            rec: PicRecord {
                header: self.rec.header.clone(),
                scaling_lists: self.rec.scaling_lists.clone(),
                ..Default::default()
            },
            last_intra: [INHERIT; 2],
            edges_v: vec![EDGE_NONE; cells],
            edges_h: vec![EDGE_NONE; cells],
            edge_stride: self.edge_stride,
            edge_y0,
            edge_rows: self.edge_rows,
            slot_poc: Vec::new(),
            check_bs: false,
            decoder_bs: None,
            bs_mismatches: 0,
            fragment: true,
            deferred: Vec::new(),
        })
    }

    /// Append a row fragment's records as if this recorder had recorded them itself: indices into the TU,
    /// coefficient, intra, PCM sample and weight lists renumbered, weight entries deduplicated against this
    /// recorder's, inherited intra owners resolved, and the first unsupported tool kept.
    pub(crate) fn append(&mut self, f: Box<PicRecorder>) {
        let PicRecorder {
            rec: r,
            last_intra,
            deferred,
            edges_v,
            edges_h,
            edge_y0,
            ..
        } = *f;
        // Codes the fragment wrote replace the picture's, as later writes of one thread would (no code is 0).
        let at = (edge_y0 - self.edge_y0) * self.edge_stride;
        for (dst, src) in [(&mut self.edges_v, &edges_v), (&mut self.edges_h, &edges_h)] {
            for (d, &c) in dst[at..at + src.len()].iter_mut().zip(src) {
                if c != EDGE_NONE {
                    *d = c;
                }
            }
        }
        let (tu0, coef0, intra0) =
            (self.rec.tus.len() as u32, self.rec.coeffs.len() as u32, self.rec.intra.len() as u32);
        let pcm0 = self.rec.pcm_samples.len() as u32;
        let shift = |i: u32, base: u32| if i == NONE { NONE } else { i + base };
        let wmap: Vec<u32> = r.weights.iter().map(|e| weight_entry(&mut self.rec.weights, *e)).collect();
        self.rec.coeffs.extend_from_slice(&r.coeffs);
        let inherited = self.last_intra;
        self.rec.tus.extend(r.tus.iter().map(|t| TuRec {
            coef: t.coef + coef0,
            owner: if t.owner == INHERIT { inherited[(t.c != 0) as usize] } else { shift(t.owner, intra0) },
            ..*t
        }));
        self.rec.intra.extend(r.intra.iter().map(|b| IntraRec {
            tu: [shift(b.tu[0], tu0), shift(b.tu[1], tu0)],
            ..*b
        }));
        self.rec.pus.extend(r.pus.iter().map(|u| {
            let mut u = *u;
            if u.weighted {
                for l in 0..2 {
                    if u.pred_flag & (1 << l) != 0 {
                        u.wt[l] = wmap[u.wt[l] as usize];
                    }
                }
            }
            u
        }));
        self.rec.pcm.extend(r.pcm.iter().map(|p| PcmRec { samples: p.samples + pcm0, ..*p }));
        self.rec.pcm_samples.extend_from_slice(&r.pcm_samples);
        // In recording order, so the first unsupported tool is the one a single recorder would have kept: a
        // failed check reports its own reason while the fragment had none yet, else the fragment's first.
        for (d, before_unsupported) in deferred {
            let failed = match d {
                Deferred::IntraOwner(k) => {
                    let index = tu0 + k;
                    let t = self.rec.tus[index as usize];
                    match self.rec.intra.get_mut(t.owner as usize) {
                        Some(i) if i.log2 == t.log2 && i.x == t.x && i.y == t.y => {
                            i.tu[(t.c as usize).saturating_sub(1)] = index;
                            None
                        }
                        _ => Some("intra residual without its prediction block"),
                    }
                }
                Deferred::Reference(slot, poc) => self.reference(slot, poc),
            };
            if let Some(what) = failed.and_then(|what| if before_unsupported { Some(what) } else { r.unsupported }) {
                self.unsupported(what);
            }
        }
        if let Some(what) = r.unsupported {
            self.unsupported(what);
        }
        for (last, frag) in self.last_intra.iter_mut().zip(last_intra) {
            if frag != INHERIT {
                *last = frag + intra0;
            }
        }
    }

    /// The picture references the picture of POC `poc` in DPB slot `slot`: the GPU compares reference pictures by
    /// slot, the decoder by POC, and both agree while no two slots this picture references hold the same POC.
    /// Returns the unsupported tool when they do not.
    fn reference(&mut self, slot: usize, poc: i32) -> Option<&'static str> {
        if self.slot_poc.len() <= slot {
            self.slot_poc.resize(slot + 1, None);
        }
        if self.slot_poc[slot] == Some(poc) {
            return None;
        }
        let clash = self.slot_poc[slot].is_some() || self.slot_poc.contains(&Some(poc));
        self.slot_poc[slot] = Some(poc);
        clash.then_some("two reference slots with one POC")
    }

    pub fn pic_id(&self) -> u32 {
        self.rec.header.pic_id
    }

    pub fn slot(&self) -> u32 {
        self.rec.header.slot
    }

    pub fn unsupported(&mut self, what: &'static str) {
        self.rec.unsupported.get_or_insert(what);
    }

    /// Deblocking edges of a block whose boundary strengths the decoder computes at `(x0, y0)`, size
    /// `1 << log2` (a transform block, a CU without residual, a PCM CU): `code` on its top and left edges where they
    /// lie on the 8x8 grid inside the picture, and with `internal` [`EDGE_PU`] on every 8x8 grid line inside it
    /// (the lines `compute_deblocking_boundary_strengths` compares motion on; inside one PU that gives bS 0).
    pub(crate) fn deblock_edges(&mut self, x0: u32, y0: u32, log2: u8, code: u8, internal: bool) {
        let (s, n) = (self.edge_stride, 1usize << (log2 - 2));
        // Table rows count from the fragment's first cell row (0 for the picture's recorder).
        let (cx, cy) = ((x0 >> 2) as usize, (y0 >> 2) as usize - self.edge_y0);
        if y0 > 0 && y0 & 7 == 0 {
            self.edges_h[cy * s + cx..cy * s + cx + n].fill(code);
        }
        if x0 > 0 && x0 & 7 == 0 {
            for y in cy..cy + n {
                self.edges_v[y * s + cx] = code;
            }
        }
        if internal {
            for j in (2..n).step_by(2) {
                self.edges_h[(cy + j) * s + cx..(cy + j) * s + cx + n].fill(EDGE_PU);
            }
            for y in cy..cy + n {
                for i in (2..n).step_by(2) {
                    self.edges_v[y * s + cx + i] = EDGE_PU;
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn intra(
        &mut self,
        c: u8,
        x: u32,
        y: u32,
        log2: u8,
        mode: u8,
        avail: ReferenceAvailability,
        cip: bool,
        no_boundary_filter: bool,
    ) {
        let bit = |on: bool, b: u8| if on { b } else { 0 };
        let bits = bit(avail.up_left, AVAIL_UP_LEFT)
            | bit(avail.up, AVAIL_UP)
            | bit(avail.up_right, AVAIL_UP_RIGHT)
            | bit(avail.left, AVAIL_LEFT)
            | bit(avail.bottom_left, AVAIL_BOTTOM_LEFT);
        self.last_intra[c as usize] = self.rec.intra.len() as u32;
        self.rec.intra.push(IntraRec {
            c,
            x: x as u16,
            y: y as u16,
            log2,
            mode,
            avail: bits,
            cip,
            no_boundary_filter,
            wave: 0,
            tu: [NONE; 2],
        });
    }

    /// A residual block just parsed (its raw levels are in the thread's level sink).
    pub(crate) fn residual(&mut self, c: u8, x: u32, y: u32, log2: u8, block: &ResidualBlock, is_intra: bool) {
        if block.coeffs_wide.is_some() {
            self.unsupported("extended precision coefficients");
            return;
        }
        let mut flags = 0;
        if block.transform_skip {
            flags |= TU_TRANSFORM_SKIP;
        }
        if block.cu_transquant_bypass {
            flags |= TU_BYPASS;
        }
        if c == 0 && log2 == 2 && is_intra && flags == 0 {
            flags |= TU_DST;
        }
        let owner = if is_intra { self.last_intra[(c != 0) as usize] } else { NONE };
        // A row fragment that has not recorded an intra block of this class yet: the owner is the picture's last
        // one at the merge, which also checks it then.
        let inherited = is_intra && owner == INHERIT;
        let index = self.rec.tus.len() as u32;
        let h = &self.rec.header;
        let bit_depth = if c == 0 { h.bit_depth_y } else { h.bit_depth_c };
        let sl_idx = if self.rec.scaling_lists.is_empty() {
            SL_FLAT
        } else {
            let size_id = if log2 == 5 && c != 0 { 2 } else { log2 - 2 };
            size_id * 6 + if is_intra { 0 } else { 3 } + c
        };
        let coef = self.rec.coeffs.len() as u32;
        let qp_prime = with_sink(|l| {
            if !l.valid {
                return None;
            }
            l.valid = false;
            append_levels(&mut self.rec.coeffs, &l.levels);
            Some(l.qp_prime)
        });
        let Some(qp_prime) = qp_prime else {
            self.unsupported("residual decoded without raw levels");
            return;
        };
        self.rec.tus.push(TuRec {
            c,
            x: x as u16,
            y: y as u16,
            log2,
            flags,
            bit_depth,
            sl_idx,
            qp_prime: qp_prime as u8,
            coef,
            num_coefs: self.rec.coeffs.len() as u32 - coef,
            owner,
        });
        if inherited {
            self.deferred.push((Deferred::IntraOwner(index), self.rec.unsupported.is_none()));
        } else if is_intra {
            match self.rec.intra.get_mut(owner as usize) {
                Some(i) if i.log2 == log2 && i.x as u32 == x && i.y as u32 == y => {
                    i.tu[(c as usize).saturating_sub(1)] = index;
                }
                _ => self.unsupported("intra residual without its prediction block"),
            }
        }
    }

    pub(crate) fn inter(
        &mut self,
        (x, y, w, h): (u32, u32, u32, u32),
        mvf: &MvField,
        l0: &[Arc<DecodedPicture>],
        l1: &[Arc<DecodedPicture>],
        weights: Option<&PredWeightTable>,
    ) {
        let lists = [l0, l1];
        let mut slot = [NONE; 2];
        let mut idx = [0usize; 2];
        for l in 0..2 {
            if mvf.pred_flag & (1 << l) == 0 {
                continue;
            }
            idx[l] = mvf.ref_idx[l] as usize;
            // The decoder skips motion compensation for an index past the list (a damaged stream), and its
            // boundary strengths then compare against POC -1, which no record can express.
            let Some(p) = lists[l].get(idx[l]) else {
                self.unsupported("reference index past the list");
                return;
            };
            slot[l] = p.recon_slot.load(Ordering::Relaxed);
            if slot[l] == NONE {
                self.unsupported("reference picture without a slot");
                continue;
            }
            let s = slot[l] as usize;
            if self.slot_poc.get(s) != Some(&Some(p.poc)) {
                if self.fragment {
                    // A row fragment: the picture's table decides at the merge (the fragment notes the reference
                    // so that later uses of it in this row need no check, as on the picture's recorder).
                    if self.slot_poc.len() <= s {
                        self.slot_poc.resize(s + 1, None);
                    }
                    self.slot_poc[s] = Some(p.poc);
                    self.deferred.push((Deferred::Reference(s, p.poc), self.rec.unsupported.is_none()));
                } else if let Some(what) = self.reference(s, p.poc) {
                    self.unsupported(what);
                }
            }
        }
        let mut wt = [0u32; 2];
        if let Some(t) = weights {
            let h = &self.rec.header;
            let scale = |bd: u8| if t.high_precision_offsets { 1i32 } else { 1i32 << (bd - 8) };
            let (sy, sc) = (scale(h.bit_depth_y), scale(h.bit_depth_c));
            for l in 0..2 {
                if mvf.pred_flag & (1 << l) == 0 {
                    continue;
                }
                let i = idx[l];
                let (lw, lo, cw, co) = if l == 0 {
                    (t.luma_weight_l0[i], t.luma_offset_l0[i], t.chroma_weight_l0[i], t.chroma_offset_l0[i])
                } else {
                    (t.luma_weight_l1[i], t.luma_offset_l1[i], t.chroma_weight_l1[i], t.chroma_offset_l1[i])
                };
                let p = |w: i16, o: i16, s: i32| (w as u16 as u32) | (((o as i32 * s) as i16 as u16 as u32) << 16);
                let entry = [
                    p(lw, lo, sy),
                    p(cw[0], co[0], sc),
                    p(cw[1], co[1], sc),
                    t.luma_log2_weight_denom as u32 | (t.chroma_log2_weight_denom as u32) << 8,
                ];
                wt[l] = weight_entry(&mut self.rec.weights, entry);
            }
        }
        let keep = |l: usize, m: Mv| if mvf.pred_flag & (1 << l) != 0 { m } else { Mv::default() };
        self.rec.pus.push(PuRec {
            x: x as u16,
            y: y as u16,
            w: w as u8,
            h: h as u8,
            pred_flag: mvf.pred_flag,
            weighted: weights.is_some(),
            mv: [keep(0, mvf.mv[0]), keep(1, mvf.mv[1])],
            slot,
            wt,
        });
    }

    /// A PCM CU of a state without planes (entropy-only): its samples, luma then Cb then Cr, each in raster order.
    pub(crate) fn pcm_samples(&mut self, x: u32, y: u32, log2: u8, samples: &[u16]) {
        self.rec.pcm.push(PcmRec {
            x: x as u16,
            y: y as u16,
            log2,
            samples: self.rec.pcm_samples.len() as u32,
        });
        self.rec.pcm_samples.extend_from_slice(samples);
    }

    /// A PCM CU whose samples the decoder has just written to the planes.
    pub(crate) fn pcm<P: Pixel>(
        &mut self,
        x: u32,
        y: u32,
        log2: u8,
        state: &PictureState<P>,
    ) {
        let (stride_y, stride_c) = (state.y_stride, state.uv_stride);
        let (sw, sh) = (state.chroma_shift_w, state.chroma_shift_h);
        let n = 1usize << log2;
        self.rec.pcm.push(PcmRec {
            x: x as u16,
            y: y as u16,
            log2,
            samples: self.rec.pcm_samples.len() as u32,
        });
        // One sample row at a time: a wavefront worker's planes are views, which allow ranges within one row only.
        let s = &mut self.rec.pcm_samples;
        for j in 0..n {
            let row = (y as usize + j) * stride_y + x as usize;
            s.extend(state.y_plane[row..row + n].iter().map(|p| p.to_i32() as u16));
        }
        let (cw, ch) = (n >> sw, n >> sh);
        let (xc, yc) = ((x >> sw) as usize, (y >> sh) as usize);
        for plane in [&state.u_plane, &state.v_plane] {
            for j in 0..ch {
                let row = (yc + j) * stride_c + xc;
                s.extend(plane[row..row + cw].iter().map(|p| p.to_i32() as u16));
            }
        }
    }
}

/// Index of a weight entry in a picture's list, added when new. A picture has a few distinct entries (one per list
/// and reference of its slices), so a linear search.
fn weight_entry(weights: &mut Vec<[u32; 4]>, entry: [u32; 4]) -> u32 {
    match weights.iter().position(|e| *e == entry) {
        Some(i) => i as u32,
        None => {
            weights.push(entry);
            weights.len() as u32 - 1
        }
    }
}

/// Copy the loop filter inputs into the picture's record and compute its intra waves. Returns whether the
/// decoder runs entropy-only (and so skips the loop filters and the output copy).
pub(crate) fn finish_tables<P: Pixel>(state: &mut PictureState<P>, pps: &Pps) -> bool {
    let Some(mut r) = state.rec.take() else { return false };
    let st: &PictureState<P> = state;
    let f = &mut r.rec.filters;
    f.block_map = pack_block_map(st, &r.edges_v, &r.edges_h);
    f.edge_codes = true;
    f.deblock_ctb = pack_deblock_ctbs(st);
    match pack_sao_ctbs(st) {
        Some(v) => f.sao_ctb = v,
        None => r.rec.unsupported = Some("SAO offset outside i8"),
    }
    f.ctb_info = (0..st.tab_slice_addr_rs.len())
        .flat_map(|rs| {
            [
                st.tab_slice_addr_rs[rs] as u32,
                st.tab_ctb_ts[rs],
                st.tab_tile_id[rs] | (st.filter_slice_edges[rs] as u32) << 31,
            ]
        })
        .collect();
    f.any_no_filter = st.any_no_filter;
    f.filter_across_tiles = st.filter_across_tiles;
    r.rec.header.pps_cb_qp_offset = pps.pps_cb_qp_offset;
    r.rec.header.pps_cr_qp_offset = pps.pps_cr_qp_offset;
    if r.rec.intra.iter().any(|b| b.cip) {
        let w4 = (st.width as usize).div_ceil(4);
        let h4 = (st.height as usize).div_ceil(4);
        let mut map = vec![0u32; (w4 * h4).div_ceil(32)];
        for y in 0..h4 {
            for x in 0..w4 {
                if st.tab_mvf[y * st.min_pu_width + x].pred_flag == 0 {
                    let i = y * w4 + x;
                    map[i >> 5] |= 1 << (i & 31);
                }
            }
        }
        r.rec.intra_map = map;
    }
    // FORMAT-mc.md record ranges: PUs of at most 8x8 first (the MC kernel's small range), then the rest.
    let (mut small, large): (Vec<PuRec>, Vec<PuRec>) = r.rec.pus.iter().partition(|u| is_small_pu(u));
    small.extend_from_slice(&large);
    r.rec.pus = small;
    if r.check_bs && !r.entropy_only {
        let dec = pack_block_map(st, &st.bs_vertical, &st.bs_horizontal);
        let mut res = r.rec.filters.block_map.clone();
        resolve_boundary_strengths(&mut res, st.width, st.height, &r.rec.pus, &r.rec.tus);
        r.bs_mismatches = res.iter().zip(&dec).filter(|(a, b)| a != b).count() as u64;
        r.decoder_bs = Some(dec);
    }
    schedule_intra(&mut r.rec);
    let entropy_only = r.entropy_only;
    state.rec = Some(r);
    entropy_only
}

/// A PU for the MC kernel's small range (both dimensions at most 8).
pub fn is_small_pu(u: &PuRec) -> bool {
    u.w <= 8 && u.h <= 8
}

/// Rounds per dispatch of the intra kernel's `list` schedule (`intra.mjs` LIST_ROUNDS).
pub const INTRA_LIST_ROUNDS: u32 = 8;
/// TUs and reference samples per step of the intra kernel (`intra.wgsl.mjs` INTRA_MAXT, INTRA_REFCAP).
pub const INTRA_MAXT: usize = 64;
pub const INTRA_REFCAP: u32 = 1536;

/// The intra kernel's per-plane TU records (FORMAT-intra.md) of a picture, decoding order (luma, then Cb, Cr).
pub fn intra_records(p: &PicRecord) -> Vec<[u32; 4]> {
    // Residual offset of a TU (sizes of the TUs before it). The intra blocks reference their TUs in increasing order,
    // so a cursor walks the TU list once instead of a table of every TU's offset; it starts over on a smaller index.
    let (mut at, mut res) = (0usize, 0u32);
    let mut res_of = |tu: u32| {
        let tu = tu as usize;
        if tu < at {
            (at, res) = (0, 0);
        }
        for t in &p.tus[at..tu] {
            res += 1 << (2 * t.log2);
        }
        at = tu;
        res
    };
    let mut out = Vec::with_capacity(p.intra.len() * 3 / 2);
    for b in &p.intra {
        for k in 0..1 + b.c as usize {
            let tu = b.tu[k];
            out.push([
                b.x as u32 | (b.y as u32) << 16,
                b.log2 as u32
                    | (b.c as u32 + k as u32) << 3
                    | (b.mode as u32) << 5
                    | (b.avail as u32) << 11
                    | (b.no_boundary_filter as u32) << 16
                    | (b.cip as u32) << 17,
                if tu == NONE { NONE } else { res_of(tu) },
                b.wave,
            ]);
        }
    }
    out
}

/// Walk the intra records in decoding order and call `f(t, Some(d))` for every earlier record `d` whose samples
/// record `t` reads (its availability regions clamped to the plane, 4x4 cells), then `f(t, None)`. `intra.mjs`
/// `analyze`.
fn walk_intra_deps(recs: &[[u32; 4]], width: u32, height: u32, mut f: impl FnMut(usize, Option<usize>)) {
    let (w, h) = (width as usize, height as usize);
    // 4x4 cells per plane (`intra.mjs` uses width >> 2 / >> 3, equal for the multiple-of-8 sizes HEVC has).
    let gw = [w.div_ceil(4), w.div_ceil(8), w.div_ceil(8)];
    let gh = [h.div_ceil(4), h.div_ceil(8), h.div_ceil(8)];
    // The cell grids persist per thread and hold `base + t` for record `t`, `base` advancing by the record count
    // each picture, so a cell below `base` was written by an earlier picture and reads as unset: no grid is cleared
    // per picture (a fresh 4K grid was most of this function's time on inter pictures, and setting back only the
    // cells a picture wrote was a third of the walk).
    INTRA_GRID.with(|cell| {
    let (grid, next) = &mut *cell.borrow_mut();
    let n = recs.len() as u32;
    if *next == 0 || next.checked_add(n).is_none() {
        for g in grid.iter_mut() {
            g.fill(0);
        }
        *next = 1;
    }
    let base = *next;
    *next += n;
    for c in 0..3 {
        if grid[c].len() != gw[c] * gh[c] {
            grid[c] = vec![0; gw[c] * gh[c]];
        }
    }
    for (t, r) in recs.iter().enumerate() {
        let (x0, y0) = ((r[0] & 0xffff) as usize, (r[0] >> 16) as usize);
        let (log2, c, av) = (r[1] & 7, ((r[1] >> 3) & 3) as usize, (r[1] >> 11) & 31);
        let s = 1usize << log2;
        let (pw, ph) = if c > 0 { (w >> 1, h >> 1) } else { (w, h) };
        let (g, gwc) = (&mut grid[c], gw[c]);
        // Neighbouring cells mostly belong to the same record; the caller's update is a max, so a repeat is skipped.
        let mut last = 0;
        let mut d = |i: u32| {
            if i >= base && i != last {
                last = i;
                f(t, Some((i - base) as usize))
            }
        };
        // Records sit on the 4x4 grid, so the samples read map to runs of cells: a column on the left, a row on top
        // (above and above-right are one slice).
        if x0 > 0 {
            let (mut first, mut rows) = (s, 0);
            if av & 2 != 0 {
                first = 0;
                rows = s;
            }
            if av & 1 != 0 {
                rows = s + ph.saturating_sub(y0 + s).min(s);
            }
            let bx = (x0 - 1) >> 2;
            if rows > first {
                let cy = (y0 + first) >> 2;
                for c in g[cy * gwc + bx..].iter().step_by(gwc).take((rows - first).div_ceil(4)) {
                    d(*c);
                }
            }
            if av & 4 != 0 && y0 > 0 {
                d(g[((y0 - 1) >> 2) * gwc + bx]);
            }
        }
        if y0 > 0 && av & 24 != 0 {
            let row = ((y0 - 1) >> 2) * gwc;
            let mid = (x0 + s) >> 2;
            let start = if av & 8 != 0 { x0 >> 2 } else { mid };
            let end = if av & 16 != 0 { mid + pw.saturating_sub(x0 + s).min(s).div_ceil(4) } else { mid };
            for c in &g[row + start..row + end] {
                d(*c);
            }
        }
        f(t, None);
        for by in (y0 >> 2)..((y0 + s) >> 2) {
            g[by * gwc + (x0 >> 2)..by * gwc + ((x0 + s) >> 2)].fill(base + t as u32);
        }
    }
    });
}

thread_local! {
    /// Cell grids of `walk_intra_deps` and the stamp of the next picture's first record.
    static INTRA_GRID: std::cell::RefCell<([Vec<u32>; 3], u32)> = std::cell::RefCell::new(Default::default());
}

/// CTU of an intra record (luma CTB raster address).
fn record_ctu(r: &[u32; 4], log2_ctb: u8, w_ctb: usize) -> usize {
    let sh = (((r[1] >> 3) & 3) != 0) as u32;
    let (x0, y0) = ((r[0] & 0xffff) << sh, (r[0] >> 16) << sh);
    (y0 as usize >> log2_ctb) * w_ctb + (x0 as usize >> log2_ctb)
}

/// Waves of the intra blocks and the intra kernel's `list` schedule (`intra.mjs` `analyze` + `buildSchedule(...,
/// 'list')`, ported so the host does not run them per picture): one workgroup per CTU and dispatch, each record in
/// the earliest (dispatch, round) after the records it reads (a later round in its own CTU, a later dispatch for
/// another CTU), at most `INTRA_LIST_ROUNDS` rounds per dispatch.
fn schedule_intra(rec: &mut PicRecord) {
    // 4:2:0 geometry only; an unsupported picture takes the CPU path anyway.
    if rec.unsupported.is_some() {
        return;
    }
    let h = &rec.header;
    let (width, height, log2_ctb) = (h.width, h.height, h.log2_ctb);
    let w_ctb = (width as usize).div_ceil(1 << log2_ctb);
    let mut recs = intra_records(rec);
    let n = recs.len();
    // Per record, what a record reading it needs in one entry: its wave, its (dispatch, round + 1) as
    // `dispatch << 4 | round + 1` (rounds stay below 16, so the earliest slot after several records is the largest
    // key) and its CTU (set before the walk).
    let mut info = vec![[0u32; 3]; n];
    let mut ctus = 0;
    for (i, r) in info.iter_mut().zip(&recs) {
        i[2] = record_ctu(r, log2_ctb, w_ctb) as u32;
        ctus = ctus.max(i[2] + 1);
    }
    let (mut wmax, mut key) = (0u32, 0u32);
    let (mut max_wave, mut max_disp) = (0u32, 0u32);
    walk_intra_deps(&recs, width, height, |t, d| match d {
        Some(d) => {
            let [w, k, c] = info[d];
            wmax = wmax.max(w);
            // Same CTU: a later round of the same dispatch; another CTU: round 0 of the next dispatch.
            key = key.max(if c == info[t][2] { k } else { ((k >> 4) + 1) << 4 });
        }
        None => {
            let (mut sd, mut sr) = (key >> 4, key & 15);
            if sr >= INTRA_LIST_ROUNDS {
                sd += 1;
                sr = 0;
            }
            info[t][0] = wmax + 1;
            info[t][1] = sd << 4 | (sr + 1);
            max_wave = max_wave.max(wmax + 1);
            max_disp = max_disp.max(sd);
            (wmax, key) = (0, 0);
        }
    });
    // Waves back into the blocks (a chroma block's Cb and Cr records have the same wave).
    let mut k = 0;
    for b in rec.intra.iter_mut() {
        b.wave = info[k][0];
        k += 1 + b.c as usize;
    }
    for (r, i) in recs.iter_mut().zip(&info) {
        r[3] = i[0];
    }
    rec.header.waves = max_wave;

    // Order: dispatch, then CTU in first-appearance order within the dispatch, then round, then decoding order.
    // A stable counting sort over (group, round) keys; nested per-bucket vectors were half of this function.
    let nd = if n > 0 { max_disp as usize + 1 } else { 0 };
    // Local group (CTU in first-appearance order) of every record within its dispatch. The records of one CTU go
    // back and forth between dispatches: a (dispatch, CTU) table when it is not much larger than the record list,
    // else a short chain per CTU of its (dispatch, local group) entries, most recent first (a search of the
    // dispatch's CTU list here was a quarter of this function, and the chain walk still about a seventh).
    let mut disp_len = vec![0u32; nd];
    // Each record's local group, then its (group, round) slot.
    let ctus = ctus as usize;
    let mut slot = vec![0u32; n];
    if nd * ctus <= 4 * n + 65536 {
        let mut table = vec![u32::MAX; nd * ctus];
        for (s, i) in slot.iter_mut().zip(&info) {
            let d = (i[1] >> 4) as usize;
            let e = &mut table[d * ctus + i[2] as usize];
            if *e == u32::MAX {
                *e = disp_len[d];
                disp_len[d] += 1;
            }
            *s = *e;
        }
    } else {
        let mut head = vec![u32::MAX; ctus];
        let mut entries: Vec<(u32, u32, u32)> = Vec::new(); // (dispatch, local group, next)
        for (s, i) in slot.iter_mut().zip(&info) {
            let (d, c) = (i[1] >> 4, i[2] as usize);
            let mut e = head[c];
            while e != u32::MAX && entries[e as usize].0 != d {
                e = entries[e as usize].2;
            }
            *s = if e != u32::MAX {
                entries[e as usize].1
            } else {
                let l = disp_len[d as usize];
                disp_len[d as usize] += 1;
                entries.push((d, l, head[c]));
                head[c] = entries.len() as u32 - 1;
                l
            };
        }
    }
    // Global group ids (dispatch order), then one key per (group, round) slot.
    let mut group_base_of = vec![0u32; nd + 1];
    for d in 0..nd {
        group_base_of[d + 1] = group_base_of[d] + disp_len[d];
    }
    let ng = group_base_of[nd] as usize;
    let rounds = INTRA_LIST_ROUNDS as usize;
    let mut count = vec![0u32; ng * rounds + 1];
    for (s, i) in slot.iter_mut().zip(&info) {
        let k = (group_base_of[(i[1] >> 4) as usize] + *s) * INTRA_LIST_ROUNDS + (i[1] & 15) - 1;
        *s = k;
        count[k as usize + 1] += 1;
    }
    for k in 1..count.len() {
        count[k] += count[k - 1];
    }
    // Record indices in schedule order (the steps and the schedule then read them in sequence). `count[k]` is
    // the start of slot k before the placement and its end after it.
    let mut order = vec![0u32; n];
    for (t, &k) in slot.iter().enumerate() {
        let k = k as usize;
        order[count[k] as usize] = t as u32;
        count[k] += 1;
    }
    let refs_of = |r: &[u32; 4]| 4 * (1u32 << (r[1] & 7)) + 1;
    let pairs_of = |r: &[u32; 4]| {
        let s = 1u32 << (r[1] & 7);
        (s * s) >> 1
    };
    // Steps: (first index in `order`, length), split at INTRA_MAXT records or INTRA_REFCAP reference samples.
    let (mut groups, mut steps, mut dispatches): (Vec<[u32; 2]>, Vec<(u32, u32)>, Vec<u32>) = Default::default();
    steps.reserve(n);
    for d in 0..nd {
        let (g0, g1) = (group_base_of[d] as usize, group_base_of[d + 1] as usize);
        if g0 == g1 {
            continue;
        }
        let group_base = groups.len() as u32;
        for g in g0..g1 {
            let s0 = steps.len() as u32;
            for k in g * rounds..(g + 1) * rounds {
                let (a, b) = (if k == 0 { 0 } else { count[k - 1] as usize }, count[k] as usize);
                let (mut start, mut refs) = (a, 0u32);
                for (i, &t) in order[a..b].iter().enumerate() {
                    let r = refs_of(&recs[t as usize]);
                    if a + i - start == INTRA_MAXT || refs + r > INTRA_REFCAP {
                        steps.push((start as u32, (a + i - start) as u32));
                        start = a + i;
                        refs = 0;
                    }
                    refs += r;
                }
                if b > start {
                    steps.push((start as u32, (b - start) as u32));
                }
            }
            groups.push([s0, steps.len() as u32 - s0]);
        }
        dispatches.extend([group_base, groups.len() as u32 - group_base]);
    }
    let step_base = groups.len() * 3;
    let tu_base = step_base + steps.len() * 4;
    let mut sched = vec![0u32; tu_base + n * 4];
    // Records are laid out in `order`, so a step's first TU index is its start in `order`.
    for (i, &(first, len)) in steps.iter().enumerate() {
        let (mut refs, mut pairs) = (0u32, 0u32);
        let b = step_base + 4 * i;
        sched[b] = first;
        sched[b + 1] = len;
        for at in first as usize..(first + len) as usize {
            let r = &recs[order[at] as usize];
            let o = tu_base + at * 4;
            sched[o..o + 3].copy_from_slice(&r[..3]);
            sched[o + 3] = refs | pairs << 16;
            refs += refs_of(r);
            pairs += pairs_of(r);
        }
        sched[b + 2] = refs;
        sched[b + 3] = pairs;
    }
    for (i, g) in groups.iter().enumerate() {
        sched[3 * i] = g[0];
        sched[3 * i + 1] = g[1];
        sched[3 * i + 2] = steps.get(g[0] as usize).map_or(0, |s| s.0);
    }
    rec.intra_sched = IntraSchedule { step_base: step_base as u32, tu_base: tu_base as u32, sched, dispatches };
    rec.intra_packed = PackedIntra { blocks: rec.intra.len(), recs };
}

/// Check an intra schedule against the dependencies of the intra records: every record scheduled once, and every
/// record it reads in an earlier dispatch, or in the same workgroup at an earlier step.
fn check_intra_schedule(p: &PicRecord) -> Result<(), String> {
    let recs = intra_records(p);
    let s = &p.intra_sched;
    let (sb, tb) = (s.step_base as usize, s.tu_base as usize);
    if s.sched.len() != tb + 4 * recs.len() {
        return Err("intra schedule size".into());
    }
    let index: std::collections::HashMap<(u32, u32), usize> =
        recs.iter().enumerate().map(|(i, r)| ((r[0], (r[1] >> 3) & 3), i)).collect();
    let mut slot = vec![None::<(usize, usize, usize)>; recs.len()];
    for (d, pair) in s.dispatches.chunks(2).enumerate() {
        for g in pair[0] as usize..(pair[0] + pair[1]) as usize {
            let (first, count) = (s.sched[3 * g] as usize, s.sched[3 * g + 1] as usize);
            for st in first..first + count {
                let (t0, tn) = (s.sched[sb + 4 * st] as usize, s.sched[sb + 4 * st + 1] as usize);
                for k in t0..t0 + tn {
                    let w = &s.sched[tb + 4 * k..tb + 4 * k + 3];
                    let i = *index.get(&(w[0], (w[1] >> 3) & 3)).ok_or("scheduled record not in the picture")?;
                    if w != &recs[i][..3] || slot[i].replace((d, g, st)).is_some() {
                        return Err("intra schedule record mismatch or duplicate".into());
                    }
                }
            }
        }
    }
    let slot: Vec<(usize, usize, usize)> = slot.into_iter().map(|x| x.ok_or("intra record not scheduled")).collect::<Result<_, _>>()?;
    let mut bad = None;
    walk_intra_deps(&recs, p.header.width, p.header.height, |t, d| {
        if let Some(d) = d {
            let ((dt, gt, st), (dd, gd, sd)) = (slot[t], slot[d]);
            if !(dd < dt || (dd == dt && gd == gt && sd < st)) {
                bad.get_or_insert((t, d));
            }
        }
    });
    match bad {
        Some((t, d)) => Err(format!("intra schedule: record {t} runs no later than record {d} it reads")),
        None => Ok(()),
    }
}

/// Frame returned by an entropy-only decode: no samples.
pub(crate) fn placeholder_frame(sps: &Sps, poc: i32) -> Frame {
    let empty = || {
        if sps.needs_u16_samples() {
            PixelData::U16(Vec::new())
        } else {
            PixelData::U8(Vec::new())
        }
    };
    Frame {
        y: empty(),
        u: empty(),
        v: empty(),
        width: sps.cropped_width(),
        height: sps.cropped_height(),
        pic_order_cnt: poc,
        bit_depth: sps.bit_depth_luma,
        chroma_format_idc: sps.chroma_format_idc,
        refpic: None,
        recon_pic_id: NONE,
    }
}

/// The decoder-level recorder: picture ids, DPB slots and the event list.
#[derive(Default)]
pub struct Recorder {
    pub entropy_only: bool,
    pub events: Vec<Event>,
    next_id: u32,
    /// Pictures waiting for display: (id, slot). Their slots stay busy even after they leave the DPB.
    pending: Vec<(u32, u32)>,
    /// Highest slot used plus one.
    pub slots_used: u32,
    /// Edge code tables of finished pictures, reused by the next ones.
    edge_pool: Vec<Vec<u8>>,
    /// Full decode only: resolve every picture's edge codes to boundary strengths and compare them with the
    /// decoder's (`bs_checked` pictures, `bs_mismatches` differing block map words).
    pub check_bs: bool,
    pub bs_checked: u64,
    pub bs_mismatches: u64,
    /// With `check_bs`: keep the decoder's strengths of each picture as a packed block map (picture id, words).
    pub keep_decoder_bs: bool,
    pub decoder_bs: Vec<(u32, Vec<u32>)>,
}

impl Recorder {
    pub fn new(entropy_only: bool) -> Self {
        Self {
            entropy_only,
            ..Default::default()
        }
    }

    /// A picture starts (after its RPS marking and missing-reference generation): give generated references
    /// and the picture ids and slots. A slot is free when its picture is neither a reference nor waiting for
    /// display.
    pub(crate) fn start_picture(
        &mut self,
        dpb: &DecodedPictureBuffer,
        discarded: Vec<u32>,
        sps: &Sps,
        pps: &Pps,
        sh: &SliceHeader,
    ) -> Box<PicRecorder> {
        for id in discarded {
            if let Some(i) = self.pending.iter().position(|p| p.0 == id) {
                let (pic_id, slot) = self.pending.remove(i);
                self.events.push(Event::Discard { pic_id, slot });
            }
        }
        let mut busy: Vec<bool> = Vec::new();
        let mark = |busy: &mut Vec<bool>, s: u32| {
            if busy.len() <= s as usize {
                busy.resize(s as usize + 1, false);
            }
            busy[s as usize] = true;
        };
        for &(_, s) in &self.pending {
            mark(&mut busy, s);
        }
        for p in dpb.pictures() {
            let s = p.recon_slot.load(Ordering::Relaxed);
            if s != NONE && p.is_reference() {
                mark(&mut busy, s);
            }
        }
        let alloc = |busy: &mut Vec<bool>| -> u32 {
            let s = busy.iter().position(|b| !b).unwrap_or(busy.len()) as u32;
            mark(busy, s);
            s
        };
        for p in dpb.pictures() {
            if p.recon_slot.load(Ordering::Relaxed) == NONE && p.is_reference() {
                let (pic_id, slot) = (self.next_id, alloc(&mut busy));
                self.next_id += 1;
                p.recon_id.store(pic_id, Ordering::Relaxed);
                p.recon_slot.store(slot, Ordering::Relaxed);
                self.events.push(Event::Generated(GeneratedPic {
                    pic_id,
                    slot,
                    poc: p.poc,
                    width: sps.pic_width_in_luma_samples,
                    height: sps.pic_height_in_luma_samples,
                    bit_depth_y: sps.bit_depth_luma,
                    bit_depth_c: sps.bit_depth_chroma,
                    chroma_format_idc: sps.chroma_format_idc as u8,
                }));
            }
        }
        let slot = alloc(&mut busy);
        self.slots_used = self.slots_used.max(busy.len() as u32);
        let pic_id = self.next_id;
        self.next_id += 1;
        let header = PicHeader {
            pic_id,
            slot,
            poc: sh.poc,
            width: sps.pic_width_in_luma_samples,
            height: sps.pic_height_in_luma_samples,
            bit_depth_y: sps.bit_depth_luma,
            bit_depth_c: sps.bit_depth_chroma,
            chroma_format_idc: sps.chroma_format_idc as u8,
            log2_ctb: sps.ctb_log2_size_y,
            log2_min_cb: sps.min_cb_log2_size_y,
            log2_min_tb: sps.min_tb_log2_size_y,
            sao_enabled: sps.sample_adaptive_offset_enabled_flag,
            strong_intra_smoothing: sps.strong_intra_smoothing_enabled_flag,
            intra_smoothing_disabled: sps.range.intra_smoothing_disabled_flag,
            output: sh.pic_output_flag,
            pps_cb_qp_offset: pps.pps_cb_qp_offset,
            pps_cr_qp_offset: pps.pps_cr_qp_offset,
            conf_win: [
                sps.conf_win_left_offset,
                sps.conf_win_right_offset,
                sps.conf_win_top_offset,
                sps.conf_win_bottom_offset,
            ],
            waves: 0,
        };
        let mut rec = PicRecord {
            header,
            ..Default::default()
        };
        if sps.chroma_array_type() != 1 {
            rec.unsupported = Some("chroma format other than 4:2:0");
        }
        if sps.range != crate::sps::SpsRangeExtension::default() || pps.cross_component_prediction_enabled_flag {
            rec.unsupported = Some("range extension coding tools");
        }
        if sps.scaling_list_enabled_flag {
            let sl = if pps.pps_scaling_list_data_present_flag { &pps.scaling_list } else { &sps.scaling_list };
            if let Some(sl) = sl {
                rec.scaling_lists = pack_scaling_lists(sl);
            }
        }
        levels_on(true);
        let edge_stride = (sps.pic_width_in_luma_samples / 4) as usize;
        let cells = edge_stride * (sps.pic_height_in_luma_samples / 4) as usize;
        let mut table = || {
            let mut t = self.edge_pool.pop().unwrap_or_default();
            t.clear();
            t.resize(cells, EDGE_NONE);
            t
        };
        let (edges_v, edges_h) = (table(), table());
        Box::new(PicRecorder {
            entropy_only: self.entropy_only,
            rec,
            last_intra: [NONE; 2],
            edges_v,
            edges_h,
            edge_stride,
            edge_y0: 0,
            edge_rows: cells / edge_stride.max(1),
            slot_poc: Vec::new(),
            check_bs: self.check_bs,
            decoder_bs: None,
            bs_mismatches: 0,
            fragment: false,
            deferred: Vec::new(),
        })
    }

    pub(crate) fn finish_picture(&mut self, mut pr: Box<PicRecorder>, output: bool) {
        levels_on(false);
        // Only pictures the stream supports count: the others take the CPU path.
        if let Some(d) = pr.decoder_bs.take().filter(|_| pr.rec.unsupported.is_none()) {
            self.bs_checked += 1;
            self.bs_mismatches += pr.bs_mismatches;
            if self.keep_decoder_bs {
                self.decoder_bs.push((pr.rec.header.pic_id, d));
            }
        }
        self.edge_pool.push(std::mem::take(&mut pr.edges_v));
        self.edge_pool.push(std::mem::take(&mut pr.edges_h));
        let rec = pr.rec;
        if output {
            self.pending.push((rec.header.pic_id, rec.header.slot));
        }
        self.events.push(Event::Picture(Box::new(rec)));
    }

    pub(crate) fn output(&mut self, pic_id: u32) {
        if let Some(i) = self.pending.iter().position(|p| p.0 == pic_id) {
            let (pic_id, slot) = self.pending.remove(i);
            self.events.push(Event::Output { pic_id, slot });
        }
    }
}

/// FORMAT-idct.md scaling list table: list `sizeId * 6 + matrixId` at `17 * l`, 16 words of list bytes and the
/// DC value (`packScalingLists` in `web/gpu-recon/idct.mjs`).
pub fn pack_scaling_lists(sl: &crate::scaling_list::ScalingList) -> Vec<u32> {
    let mut out = vec![0u32; 24 * 17];
    for s in 0..4 {
        for m in 0..6 {
            let base = (s * 6 + m) * 17;
            for j in 0..16 {
                let b = &sl.sl[s][m];
                out[base + j] = u32::from_le_bytes([b[4 * j], b[4 * j + 1], b[4 * j + 2], b[4 * j + 3]]);
            }
            out[base + 16] = if s >= 2 { sl.sl_dc[s - 2][m] } else { sl.sl[s][m][0] } as u32;
        }
    }
    out
}

// ---- Packed u32 layout (web/gpu-recon/FORMAT-STREAM.md) ------------------------------------------------------

pub const STREAM_MAGIC: u32 = u32::from_le_bytes(*b"RCS1");
/// Stream file version after the magic: 2 since the block map carries deblocking edge codes (header flag bit 6)
/// instead of boundary strengths. Version 1 files still parse (flag bit 6 clear).
pub const STREAM_VERSION: u32 = 2;
pub const EV_PICTURE: u32 = 1;
pub const EV_GENERATED: u32 = 2;
pub const EV_OUTPUT: u32 = 3;
pub const EV_DISCARD: u32 = 4;
/// Words of the picture header, before the section directory.
pub const HEADER_WORDS: usize = 24;
/// Sections of a picture, in directory order.
pub const SECTIONS: usize = 15;
pub const SEC_TU: usize = 0;
pub const SEC_COEFF: usize = 1;
pub const SEC_PU: usize = 2;
pub const SEC_WEIGHTS: usize = 3;
pub const SEC_INTRA: usize = 4;
pub const SEC_PCM: usize = 5;
pub const SEC_PCM_SAMPLES: usize = 6;
pub const SEC_BLOCK_MAP: usize = 7;
pub const SEC_DEBLOCK_CTB: usize = 8;
pub const SEC_SAO: usize = 9;
pub const SEC_CTB_INFO: usize = 10;
pub const SEC_SCALING: usize = 11;
pub const SEC_INTRA_MAP: usize = 12;
pub const SEC_INTRA_SCHED: usize = 13;
pub const SEC_INTRA_DISPATCH: usize = 14;
pub const TU_WORDS: usize = 6;
pub const PU_WORDS: usize = 6;
pub const WEIGHT_WORDS: usize = 4;
pub const INTRA_WORDS: usize = 4;
pub const PCM_WORDS: usize = 3;
pub const SAO_WORDS: usize = 4;
pub const CTB_INFO_WORDS: usize = 3;

fn pack16(a: i16, b: i16) -> u32 {
    (a as u16 as u32) | ((b as u16 as u32) << 16)
}
fn lo16(w: u32) -> i16 {
    w as u16 as i16
}
fn hi16(w: u32) -> i16 {
    (w >> 16) as u16 as i16
}
fn push_i16s(out: &mut Vec<u32>, v: &[i16]) {
    for c in v.chunks(2) {
        out.push(pack16(c[0], *c.get(1).unwrap_or(&0)));
    }
}
fn read_i16s(w: &[u32], n: usize) -> Vec<i16> {
    w.iter().flat_map(|&x| [lo16(x), hi16(x)]).take(n).collect()
}
fn slot0(s: u32) -> u32 {
    if s == NONE { 0 } else { s }
}

/// Append one event in the packed layout: `[type, payload words, payload...]`.
pub fn encode_event(ev: &Event, out: &mut Vec<u32>) {
    let start = out.len();
    out.extend([0, 0]);
    let kind = match ev {
        Event::Generated(g) => {
            out.extend([
                g.pic_id,
                g.slot,
                g.poc as u32,
                g.width,
                g.height,
                g.bit_depth_y as u32 | (g.bit_depth_c as u32) << 8 | (g.chroma_format_idc as u32) << 16,
            ]);
            EV_GENERATED
        }
        Event::Output { pic_id, slot } => {
            out.extend([*pic_id, *slot]);
            EV_OUTPUT
        }
        Event::Discard { pic_id, slot } => {
            out.extend([*pic_id, *slot]);
            EV_DISCARD
        }
        Event::Picture(p) => {
            encode_picture(p, out);
            EV_PICTURE
        }
    };
    out[start] = kind;
    out[start + 1] = (out.len() - start - 2) as u32;
}

fn encode_picture(p: &PicRecord, out: &mut Vec<u32>) {
    let base = out.len();
    let fresh;
    let intra = if p.intra_packed.blocks == p.intra.len() && (p.intra.is_empty() || !p.intra_packed.recs.is_empty()) {
        &p.intra_packed.recs
    } else {
        fresh = intra_records(p);
        &fresh
    };
    // The whole picture at once: growing the output while sections are appended copied it several times.
    let f = &p.filters;
    out.reserve(
        HEADER_WORDS
            + 2 * SECTIONS
            + TU_WORDS * p.tus.len()
            + p.coeffs.len()
            + PU_WORDS * p.pus.len()
            + WEIGHT_WORDS * p.weights.len()
            + INTRA_WORDS * intra.len()
            + PCM_WORDS * p.pcm.len()
            + p.pcm_samples.len().div_ceil(2)
            + f.block_map.len()
            + f.deblock_ctb.len()
            + f.sao_ctb.len()
            + f.ctb_info.len()
            + p.scaling_lists.len()
            + p.intra_map.len()
            + p.intra_sched.sched.len()
            + p.intra_sched.dispatches.len(),
    );
    let h = &p.header;
    let flags = h.sao_enabled as u32
        | (h.strong_intra_smoothing as u32) << 1
        | (h.intra_smoothing_disabled as u32) << 2
        | (h.output as u32) << 3
        | (p.filters.any_no_filter as u32) << 4
        | (p.filters.filter_across_tiles as u32) << 5
        | (p.filters.edge_codes as u32) << 6
        | (p.unsupported.is_some() as u32) << 31;
    let mut hdr = [0u32; HEADER_WORDS];
    hdr[20] = p.intra_sched.step_base;
    hdr[21] = p.intra_sched.tu_base;
    hdr[..20].copy_from_slice(&[
        h.pic_id,
        h.slot,
        h.poc as u32,
        h.width,
        h.height,
        h.bit_depth_y as u32 | (h.bit_depth_c as u32) << 8 | (h.chroma_format_idc as u32) << 16,
        h.log2_ctb as u32 | (h.log2_min_cb as u32) << 8 | (h.log2_min_tb as u32) << 16,
        flags,
        h.pps_cb_qp_offset as u32,
        h.pps_cr_qp_offset as u32,
        h.conf_win[0],
        h.conf_win[1],
        h.conf_win[2],
        h.conf_win[3],
        h.waves,
        (h.width as usize).div_ceil(1 << h.log2_ctb) as u32,
        (h.height as usize).div_ceil(1 << h.log2_ctb) as u32,
        h.width / 8,
        h.width.div_ceil(4),
        p.pus.iter().take_while(|u| is_small_pu(u)).count() as u32,
    ]);
    out.extend(hdr);
    let dir = out.len();
    out.extend([0u32; 2 * SECTIONS]);
    let section = |out: &mut Vec<u32>, i: usize, count: usize, f: &mut dyn FnMut(&mut Vec<u32>)| {
        out[dir + 2 * i] = (out.len() - base) as u32;
        out[dir + 2 * i + 1] = count as u32;
        f(out);
    };
    section(out, SEC_TU, p.tus.len(), &mut |o| {
        let mut res = 0u32;
        for t in &p.tus {
            o.extend([
                t.x as u32 | (t.y as u32) << 16,
                (t.log2 - 2) as u32
                    | (t.c as u32) << 2
                    | (t.flags as u32) << 4
                    | ((t.bit_depth - 8) as u32) << 8
                    | (t.sl_idx as u32) << 11
                    | (t.qp_prime as u32) << 16,
                t.coef,
                t.num_coefs,
                res,
                t.owner,
            ]);
            res += 1 << (2 * t.log2);
        }
    });
    section(out, SEC_COEFF, p.coeffs.len(), &mut |o| o.extend_from_slice(&p.coeffs));
    section(out, SEC_PU, p.pus.len(), &mut |o| {
        for u in &p.pus {
            o.extend([
                u.x as u32 | (u.y as u32) << 16,
                u.w as u32 | (u.h as u32) << 8 | (u.pred_flag as u32 | (u.weighted as u32) << 2) << 16,
                slot0(u.slot[0]) | slot0(u.slot[1]) << 16,
                pack16(u.mv[0].x, u.mv[0].y),
                pack16(u.mv[1].x, u.mv[1].y),
                u.wt[0] | u.wt[1] << 16,
            ]);
        }
    });
    section(out, SEC_WEIGHTS, p.weights.len(), &mut |o| {
        for w in &p.weights {
            o.extend_from_slice(w);
        }
    });
    section(out, SEC_INTRA, intra.len(), &mut |o| o.extend_from_slice(intra.as_flattened()));
    section(out, SEC_PCM, p.pcm.len(), &mut |o| {
        for b in &p.pcm {
            o.extend([b.x as u32 | (b.y as u32) << 16, b.log2 as u32, b.samples]);
        }
    });
    section(out, SEC_PCM_SAMPLES, p.pcm_samples.len(), &mut |o| {
        let v: Vec<i16> = p.pcm_samples.iter().map(|&s| s as i16).collect();
        push_i16s(o, &v)
    });
    section(out, SEC_BLOCK_MAP, f.block_map.len(), &mut |o| o.extend_from_slice(&f.block_map));
    section(out, SEC_DEBLOCK_CTB, f.deblock_ctb.len(), &mut |o| o.extend_from_slice(&f.deblock_ctb));
    section(out, SEC_SAO, f.sao_ctb.len() / SAO_WORDS, &mut |o| o.extend_from_slice(&f.sao_ctb));
    section(out, SEC_CTB_INFO, f.ctb_info.len() / CTB_INFO_WORDS, &mut |o| o.extend_from_slice(&f.ctb_info));
    section(out, SEC_SCALING, p.scaling_lists.len(), &mut |o| o.extend_from_slice(&p.scaling_lists));
    section(out, SEC_INTRA_MAP, p.intra_map.len(), &mut |o| o.extend_from_slice(&p.intra_map));
    let is = &p.intra_sched;
    section(out, SEC_INTRA_SCHED, is.sched.len(), &mut |o| o.extend_from_slice(&is.sched));
    section(out, SEC_INTRA_DISPATCH, is.dispatches.len() / 2, &mut |o| o.extend_from_slice(&is.dispatches));
}

/// Parse one event at the start of `w`; returns it and the words it took.
pub fn decode_event(w: &[u32]) -> Result<(Event, usize), String> {
    let (&kind, &len) = (w.first().ok_or("eof")?, w.get(1).ok_or("eof")?);
    let len = len as usize;
    let p = w.get(2..2 + len).ok_or("truncated event")?;
    let ev = match kind {
        EV_GENERATED => Event::Generated(GeneratedPic {
            pic_id: p[0],
            slot: p[1],
            poc: p[2] as i32,
            width: p[3],
            height: p[4],
            bit_depth_y: p[5] as u8,
            bit_depth_c: (p[5] >> 8) as u8,
            chroma_format_idc: (p[5] >> 16) as u8,
        }),
        EV_OUTPUT => Event::Output { pic_id: p[0], slot: p[1] },
        EV_DISCARD => Event::Discard { pic_id: p[0], slot: p[1] },
        EV_PICTURE => Event::Picture(Box::new(decode_picture(p)?)),
        _ => return Err(format!("unknown event type {kind}")),
    };
    Ok((ev, 2 + len))
}

fn decode_picture(p: &[u32]) -> Result<PicRecord, String> {
    let h = &p[..HEADER_WORDS];
    let dir = &p[HEADER_WORDS..HEADER_WORDS + 2 * SECTIONS];
    let sec = |i: usize, words_per: usize| -> Result<(&[u32], usize), String> {
        let (off, count) = (dir[2 * i] as usize, dir[2 * i + 1] as usize);
        let words = count * words_per;
        Ok((p.get(off..off + words).ok_or("section out of range")?, count))
    };
    let i16s = |i: usize| -> Result<Vec<i16>, String> {
        let (off, count) = (dir[2 * i] as usize, dir[2 * i + 1] as usize);
        Ok(read_i16s(p.get(off..off + count.div_ceil(2)).ok_or("section out of range")?, count))
    };
    let header = PicHeader {
        pic_id: h[0],
        slot: h[1],
        poc: h[2] as i32,
        width: h[3],
        height: h[4],
        bit_depth_y: h[5] as u8,
        bit_depth_c: (h[5] >> 8) as u8,
        chroma_format_idc: (h[5] >> 16) as u8,
        log2_ctb: h[6] as u8,
        log2_min_cb: (h[6] >> 8) as u8,
        log2_min_tb: (h[6] >> 16) as u8,
        sao_enabled: h[7] & 1 != 0,
        strong_intra_smoothing: h[7] & 2 != 0,
        intra_smoothing_disabled: h[7] & 4 != 0,
        output: h[7] & 8 != 0,
        pps_cb_qp_offset: h[8] as i32,
        pps_cr_qp_offset: h[9] as i32,
        conf_win: [h[10], h[11], h[12], h[13]],
        waves: h[14],
    };
    let (tw, n) = sec(SEC_TU, TU_WORDS)?;
    let tus: Vec<TuRec> = (0..n)
        .map(|i| {
            let r = &tw[i * TU_WORDS..];
            TuRec {
                x: r[0] as u16,
                y: (r[0] >> 16) as u16,
                log2: (r[1] & 3) as u8 + 2,
                c: ((r[1] >> 2) & 3) as u8,
                flags: ((r[1] >> 4) & 7) as u8,
                bit_depth: ((r[1] >> 8) & 7) as u8 + 8,
                sl_idx: ((r[1] >> 11) & 31) as u8,
                qp_prime: ((r[1] >> 16) & 127) as u8,
                coef: r[2],
                num_coefs: r[3],
                owner: r[5],
            }
        })
        .collect();
    let (pw, n) = sec(SEC_PU, PU_WORDS)?;
    let pus = (0..n)
        .map(|i| {
            let r = &pw[i * PU_WORDS..];
            PuRec {
                x: r[0] as u16,
                y: (r[0] >> 16) as u16,
                w: r[1] as u8,
                h: (r[1] >> 8) as u8,
                pred_flag: ((r[1] >> 16) & 3) as u8,
                weighted: (r[1] >> 18) & 1 != 0,
                slot: [0, 1].map(|l| if (r[1] >> (16 + l)) & 1 != 0 { (r[2] >> (16 * l)) & 0xffff } else { NONE }),
                mv: [Mv { x: lo16(r[3]), y: hi16(r[3]) }, Mv { x: lo16(r[4]), y: hi16(r[4]) }],
                wt: [r[5] & 0xffff, r[5] >> 16],
            }
        })
        .collect();
    let (ww, n) = sec(SEC_WEIGHTS, WEIGHT_WORDS)?;
    let weights = (0..n).map(|i| ww[i * WEIGHT_WORDS..][..4].try_into().unwrap()).collect();
    let mut res_to_tu = std::collections::HashMap::new();
    let mut res = 0u32;
    for (i, t) in tus.iter().enumerate() {
        res_to_tu.insert(res, i as u32);
        res += 1 << (2 * t.log2);
    }
    let tu_of = |r: u32| -> Result<u32, String> {
        if r == NONE { Ok(NONE) } else { res_to_tu.get(&r).copied().ok_or_else(|| "intra residual offset".to_string()) }
    };
    let (iw, n) = sec(SEC_INTRA, INTRA_WORDS)?;
    let mut intra: Vec<IntraRec> = Vec::new();
    let mut i = 0;
    while i < n {
        let r = &iw[i * INTRA_WORDS..];
        let plane = (r[1] >> 3) & 3;
        let mut b = IntraRec {
            x: r[0] as u16,
            y: (r[0] >> 16) as u16,
            c: plane.min(1) as u8,
            log2: (r[1] & 7) as u8,
            mode: ((r[1] >> 5) & 63) as u8,
            avail: ((r[1] >> 11) & 31) as u8,
            no_boundary_filter: (r[1] >> 16) & 1 != 0,
            cip: (r[1] >> 17) & 1 != 0,
            wave: r[3],
            tu: [tu_of(r[2])?, NONE],
        };
        i += 1;
        if plane == 1 {
            // The Cr record of the same block follows its Cb record.
            let cr = iw.get(i * INTRA_WORDS..i * INTRA_WORDS + INTRA_WORDS).ok_or("Cb intra record without Cr")?;
            if (cr[1] >> 3) & 3 != 2 || cr[0] != r[0] {
                return Err("Cb intra record without Cr".into());
            }
            b.tu[1] = tu_of(cr[2])?;
            i += 1;
        }
        intra.push(b);
    }
    let (cw, n) = sec(SEC_PCM, PCM_WORDS)?;
    let pcm = (0..n)
        .map(|i| {
            let r = &cw[i * PCM_WORDS..];
            PcmRec {
                x: r[0] as u16,
                y: (r[0] >> 16) as u16,
                log2: r[1] as u8,
                samples: r[2],
            }
        })
        .collect();
    let filters = LoopFilterInputs {
        block_map: sec(SEC_BLOCK_MAP, 1)?.0.to_vec(),
        deblock_ctb: sec(SEC_DEBLOCK_CTB, 1)?.0.to_vec(),
        sao_ctb: sec(SEC_SAO, SAO_WORDS)?.0.to_vec(),
        ctb_info: sec(SEC_CTB_INFO, CTB_INFO_WORDS)?.0.to_vec(),
        any_no_filter: h[7] & 16 != 0,
        filter_across_tiles: h[7] & 32 != 0,
        edge_codes: h[7] & 64 != 0,
    };
    Ok(PicRecord {
        header,
        tus,
        coeffs: sec(SEC_COEFF, 1)?.0.to_vec(),
        scaling_lists: sec(SEC_SCALING, 1)?.0.to_vec(),
        intra_map: sec(SEC_INTRA_MAP, 1)?.0.to_vec(),
        intra_sched: IntraSchedule {
            step_base: h[20],
            tu_base: h[21],
            sched: sec(SEC_INTRA_SCHED, 1)?.0.to_vec(),
            dispatches: sec(SEC_INTRA_DISPATCH, 2)?.0.to_vec(),
        },
        pus,
        weights,
        intra,
        pcm,
        pcm_samples: i16s(SEC_PCM_SAMPLES)?.into_iter().map(|s| s as u16).collect(),
        filters,
        intra_packed: PackedIntra::default(),
        unsupported: (h[7] >> 31 != 0).then_some("unsupported coding tool (see recorder)"),
    })
}

// ---- Replay ------------------------------------------------------------------------------------------------

/// Reconstruction stages of one picture, for kernel test vectors. Planes are tight (`stride = width` of the
/// component), coded size (before the conformance crop).
#[derive(Clone, Debug, Default)]
pub struct Stages {
    /// Prediction samples of every block (intra, inter, PCM) as computed during reconstruction.
    pub pred: [Vec<u16>; 3],
    /// Residual added to each block after the inverse transform (0 where no residual is coded).
    pub residual: [Vec<i16>; 3],
    pub pre_deblock: [Vec<u16>; 3],
    pub post_deblock: [Vec<u16>; 3],
    pub post_sao: [Vec<u16>; 3],
    /// Component sizes.
    pub dims: [(usize, usize); 3],
}

/// Rebuilds pictures from command stream events with the decoder's reconstruction functions.
#[derive(Default)]
pub struct Replayer {
    slots: Vec<Option<Arc<DecodedPicture>>>,
    /// Cropped frames waiting for their Output event, by picture id.
    frames: Vec<(u32, Frame)>,
    /// Capture stage pictures of the next reconstructed pictures.
    pub capture_stages: bool,
    /// Stage pictures of the last reconstructed picture (when `capture_stages`).
    pub stages: Option<Stages>,
}

impl Replayer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Process one event; returns the frame an Output event displays.
    pub fn event(&mut self, ev: &Event) -> Result<Option<Frame>, String> {
        match ev {
            Event::Generated(g) => {
                let sps = generated_sps(g);
                let pic = if sps.needs_u16_samples() {
                    grey::<u16>(&sps, g.poc)
                } else {
                    grey::<u8>(&sps, g.poc)
                };
                self.put(g.slot, pic);
                Ok(None)
            }
            Event::Picture(p) => {
                if let Some(u) = p.unsupported {
                    return Err(format!("picture {} uses {u}", p.header.pic_id));
                }
                let (pic, frame, stages) = if p.header.bit_depth_y.max(p.header.bit_depth_c) > 8 {
                    replay_picture::<u16>(p, &self.slots, self.capture_stages)?
                } else {
                    replay_picture::<u8>(p, &self.slots, self.capture_stages)?
                };
                self.stages = stages;
                self.put(p.header.slot, pic);
                if p.header.output {
                    self.frames.push((p.header.pic_id, frame));
                }
                Ok(None)
            }
            Event::Output { pic_id, .. } => {
                let i = self
                    .frames
                    .iter()
                    .position(|f| f.0 == *pic_id)
                    .ok_or_else(|| format!("output of unknown picture {pic_id}"))?;
                Ok(Some(self.frames.remove(i).1))
            }
            Event::Discard { pic_id, .. } => {
                self.frames.retain(|f| f.0 != *pic_id);
                Ok(None)
            }
        }
    }

    fn put(&mut self, slot: u32, pic: Arc<DecodedPicture>) {
        let s = slot as usize;
        if self.slots.len() <= s {
            self.slots.resize(s + 1, None);
        }
        self.slots[s] = Some(pic);
    }
}

fn generated_sps(g: &GeneratedPic) -> Sps {
    let mut sps = Sps {
        pic_width_in_luma_samples: g.width,
        pic_height_in_luma_samples: g.height,
        bit_depth_luma: g.bit_depth_y,
        bit_depth_chroma: g.bit_depth_c,
        chroma_format_idc: g.chroma_format_idc as u32,
        ..Default::default()
    };
    sps.ctb_log2_size_y = 4;
    sps.ctb_size_y = 16;
    sps.min_cb_log2_size_y = 3;
    sps.min_tb_log2_size_y = 2;
    sps
}

fn header_sps(h: &PicHeader) -> Sps {
    let mut sps = Sps {
        pic_width_in_luma_samples: h.width,
        pic_height_in_luma_samples: h.height,
        bit_depth_luma: h.bit_depth_y,
        bit_depth_chroma: h.bit_depth_c,
        chroma_format_idc: h.chroma_format_idc as u32,
        conf_win_left_offset: h.conf_win[0],
        conf_win_right_offset: h.conf_win[1],
        conf_win_top_offset: h.conf_win[2],
        conf_win_bottom_offset: h.conf_win[3],
        ctb_log2_size_y: h.log2_ctb,
        ctb_size_y: 1 << h.log2_ctb,
        min_cb_log2_size_y: h.log2_min_cb,
        min_tb_log2_size_y: h.log2_min_tb,
        sample_adaptive_offset_enabled_flag: h.sao_enabled,
        strong_intra_smoothing_enabled_flag: h.strong_intra_smoothing,
        ..Default::default()
    };
    sps.range.intra_smoothing_disabled_flag = h.intra_smoothing_disabled;
    sps
}

fn into_picture<P: Pixel>(state: &mut PictureState<P>, poc: i32) -> Arc<DecodedPicture> {
    let (width, height) = (state.width, state.height);
    let (y_stride, uv_stride) = (state.y_stride, state.uv_stride);
    let (csw, csh) = (state.chroma_shift_w, state.chroma_shift_h);
    let (y, u, v, mvf, log2_min_pu, min_pu_width, log2_ctb) = state.take_planes_and_mvf();
    let mut dp = DecodedPicture::new_with_mvf(
        y,
        u,
        v,
        width,
        height,
        poc,
        mvf,
        log2_min_pu,
        min_pu_width,
        log2_ctb,
        [Vec::new(), Vec::new()],
    );
    dp.y_stride = y_stride;
    dp.uv_stride = uv_stride;
    dp.chroma_shift_w = csw;
    dp.chroma_shift_h = csh;
    Arc::new(dp)
}

fn grey<P: Pixel>(sps: &Sps, poc: i32) -> Arc<DecodedPicture> {
    let mut st = PictureState::<P>::new(sps);
    crate::decoder::fill_undecoded_ctbs(&mut st);
    into_picture(&mut st, poc)
}

/// Copy a `w x h` block of plane `c` of `state` into `dst` (tight, `dst_w` wide).
#[allow(clippy::too_many_arguments)]
fn grab<P: Pixel>(state: &PictureState<P>, c: usize, x: usize, y: usize, w: usize, h: usize, dst: &mut [u16], dst_w: usize) {
    let (plane, stride) = match c {
        0 => (&state.y_plane, state.y_stride),
        1 => (&state.u_plane, state.uv_stride),
        _ => (&state.v_plane, state.uv_stride),
    };
    for j in 0..h {
        for i in 0..w {
            dst[(y + j) * dst_w + x + i] = plane[(y + j) * stride + x + i].to_i32() as u16;
        }
    }
}

fn snapshot<P: Pixel>(state: &PictureState<P>, dims: &[(usize, usize); 3]) -> [Vec<u16>; 3] {
    std::array::from_fn(|c| {
        let (w, h) = dims[c];
        let mut v = vec![0u16; w * h];
        grab(state, c, 0, 0, w, h, &mut v, w);
        v
    })
}

/// Rebuild one picture from its record. Order: all inter PUs, PCM, inter residuals (wave 0), then each intra
/// wave (prediction, then the block's residual), then deblocking and SAO.
fn replay_picture<P: Pixel>(
    p: &PicRecord,
    slots: &[Option<Arc<DecodedPicture>>],
    capture: bool,
) -> Result<(Arc<DecodedPicture>, Frame, Option<Stages>), String> {
    let h = &p.header;
    let sps = header_sps(h);
    let pps = Pps {
        pps_cb_qp_offset: h.pps_cb_qp_offset,
        pps_cr_qp_offset: h.pps_cr_qp_offset,
        ..Default::default()
    };
    let mut st = PictureState::<P>::new(&sps);
    let (sw, sh) = (st.chroma_shift_w as usize, st.chroma_shift_h as usize);
    let (w, hh) = (h.width as usize, h.height as usize);
    let dims = [(w, hh), (w >> sw, hh >> sh), (w >> sw, hh >> sh)];
    let mut stages = capture.then(|| Stages {
        pred: dims.map(|(a, b)| vec![0u16; a * b]),
        residual: dims.map(|(a, b)| vec![0i16; a * b]),
        dims,
        ..Default::default()
    });

    // Inter prediction.
    let mut scratch = std::mem::take(&mut st.mc_scratch);
    let slot = |s: u32| -> Result<Arc<DecodedPicture>, String> {
        slots
            .get(s as usize)
            .and_then(|x| x.clone())
            .ok_or_else(|| format!("picture {}: reference slot {s} is empty", h.pic_id))
    };
    for u in &p.pus {
        let mvf = MvField {
            mv: u.mv,
            ref_idx: [0, 0],
            pred_flag: u.pred_flag,
        };
        let (x_pu, y_pu) = ((u.x >> 2) as usize, (u.y >> 2) as usize);
        for j in 0..(u.h as usize >> 2).max(1) {
            for i in 0..(u.w as usize >> 2).max(1) {
                st.tab_mvf[(y_pu + j) * st.min_pu_width + x_pu + i] = mvf;
            }
        }
        let l0 = if u.pred_flag & 1 != 0 { vec![slot(u.slot[0])?] } else { Vec::new() };
        let l1 = if u.pred_flag & 2 != 0 { vec![slot(u.slot[1])?] } else { Vec::new() };
        // Offsets in the entries are already scaled: high_precision_offsets keeps the decoder from scaling again.
        let mut table = PredWeightTable { high_precision_offsets: true, ..Default::default() };
        for l in 0..2 {
            if !u.weighted || u.pred_flag & (1 << l) == 0 {
                continue;
            }
            let e = p.weights.get(u.wt[l] as usize).ok_or("weight entry out of range")?;
            table.luma_log2_weight_denom = e[3] as u8;
            table.chroma_log2_weight_denom = (e[3] >> 8) as u8;
            let (w, o) = ([lo16(e[0]), lo16(e[1]), lo16(e[2])], [hi16(e[0]), hi16(e[1]), hi16(e[2])]);
            if l == 0 {
                (table.luma_weight_l0[0], table.luma_offset_l0[0]) = (w[0], o[0]);
                (table.chroma_weight_l0[0], table.chroma_offset_l0[0]) = ([w[1], w[2]], [o[1], o[2]]);
            } else {
                (table.luma_weight_l1[0], table.luma_offset_l1[0]) = (w[0], o[0]);
                (table.chroma_weight_l1[0], table.chroma_offset_l1[0]) = ([w[1], w[2]], [o[1], o[2]]);
            }
        }
        crate::inter_pred::motion_compensation_pu(
            &mut st,
            &mut scratch,
            &l0,
            &l1,
            u.x as u32,
            u.y as u32,
            u.w as u32,
            u.h as u32,
            u.weighted,
            &table,
        );
        if let Some(s) = stages.as_mut() {
            let (x, y, pw, ph) = (u.x as usize, u.y as usize, u.w as usize, u.h as usize);
            grab(&st, 0, x, y, pw, ph, &mut s.pred[0], dims[0].0);
            for c in 1..3 {
                grab(&st, c, x >> sw, y >> sh, pw >> sw, ph >> sh, &mut s.pred[c], dims[c].0);
            }
        }
    }
    st.mc_scratch = scratch;

    // PCM.
    for b in &p.pcm {
        let n = 1usize << b.log2;
        let mut k = b.samples as usize;
        let (x, y) = (b.x as usize, b.y as usize);
        for (c, (bx, by, bw, bh)) in [(x, y, n, n), (x >> sw, y >> sh, n >> sw, n >> sh), (x >> sw, y >> sh, n >> sw, n >> sh)]
            .into_iter()
            .enumerate()
        {
            let bd = if c == 0 { st.bit_depth } else { st.bit_depth_c };
            let stride = if c == 0 { st.y_stride } else { st.uv_stride };
            let plane = match c {
                0 => &mut st.y_plane,
                1 => &mut st.u_plane,
                _ => &mut st.v_plane,
            };
            for j in 0..bh {
                for i in 0..bw {
                    plane[(by + j) * stride + bx + i] = P::from_i32_clamped(p.pcm_samples[k] as i32, bd);
                    k += 1;
                }
            }
            if let Some(s) = stages.as_mut() {
                grab(&st, c, bx, by, bw, bh, &mut s.pred[c], dims[c].0);
            }
        }
    }

    // Inter residuals.
    for t in p.tus.iter().filter(|t| t.owner == NONE) {
        add_tu(&mut st, p, t, stages.as_mut());
    }

    // Constrained intra prediction reads intra / inter per 4x4 block: the map must be what the PUs give.
    if !p.intra_map.is_empty() {
        let (w4, h4) = ((h.width as usize).div_ceil(4), (h.height as usize).div_ceil(4));
        for y in 0..h4 {
            for x in 0..w4 {
                let i = y * w4 + x;
                let intra = (p.intra_map[i >> 5] >> (i & 31)) & 1 != 0;
                if intra != (st.tab_mvf[y * st.min_pu_width + x].pred_flag == 0) {
                    return Err(format!("picture {}: intra map disagrees with the PUs at ({x}, {y})", h.pic_id));
                }
            }
        }
    }

    // The intra schedule a GPU runs must respect every dependency (the replay itself runs by wave).
    check_intra_schedule(p).map_err(|e| format!("picture {}: {e}", h.pic_id))?;

    // Intra waves.
    let mut order: Vec<usize> = (0..p.intra.len()).collect();
    order.sort_by_key(|&i| p.intra[i].wave);
    for i in order {
        let b = &p.intra[i];
        let avail = ReferenceAvailability {
            up_left: b.avail & AVAIL_UP_LEFT != 0,
            up: b.avail & AVAIL_UP != 0,
            up_right: b.avail & AVAIL_UP_RIGHT != 0,
            left: b.avail & AVAIL_LEFT != 0,
            bottom_left: b.avail & AVAIL_BOTTOM_LEFT != 0,
        };
        let r = if b.c == 0 {
            crate::cu_tree::predict_intra_luma_at(
                &mut st,
                &sps,
                b.x as u32,
                b.y as u32,
                b.log2,
                b.mode,
                avail,
                b.cip,
                !b.no_boundary_filter,
            )
        } else {
            crate::cu_tree::predict_intra_chroma_at(&mut st, &sps, b.x as usize, b.y as usize, b.log2, b.mode, avail, b.cip)
        };
        r.map_err(|e| format!("intra block {i}: {e:?}"))?;
        if let Some(s) = stages.as_mut() {
            let n = 1usize << b.log2;
            let cs: &[usize] = if b.c == 0 { &[0] } else { &[1, 2] };
            for &c in cs {
                grab(&st, c, b.x as usize, b.y as usize, n, n, &mut s.pred[c], dims[c].0);
            }
        }
        for &t in &b.tu {
            if t != NONE {
                add_tu(&mut st, p, &p.tus[t as usize], stages.as_mut());
            }
        }
    }

    // Loop filters with the recorded inputs.
    unpack_filters(&mut st, p).map_err(|e| format!("picture {}: {e}", h.pic_id))?;
    crate::decoder::fill_undecoded_ctbs(&mut st);
    if let Some(s) = stages.as_mut() {
        s.pre_deblock = snapshot(&st, &dims);
    }
    crate::deblock::deblock_picture_planes(&mut st, &sps, &pps, 1);
    if let Some(s) = stages.as_mut() {
        s.post_deblock = snapshot(&st, &dims);
    }
    crate::sao::apply_sao_picture(&mut st, &sps, 1);
    if let Some(s) = stages.as_mut() {
        s.post_sao = snapshot(&st, &dims);
    }
    let frame = crate::decoder::crop_frame(
        &st,
        h.width,
        h.height,
        sps.cropped_width(),
        sps.cropped_height(),
        h.conf_win[0],
        h.conf_win[2],
        h.poc,
        h.bit_depth_y,
        false,
    )
    .map_err(|e| format!("crop: {e:?}"))?;
    Ok((into_picture(&mut st, h.poc), frame, stages))
}

/// Dequantise (mirroring `decode_residual_simple`), inverse transform one residual block and add it to its
/// plane.
fn add_tu<P: Pixel>(st: &mut PictureState<P>, p: &PicRecord, t: &TuRec, stages: Option<&mut Stages>) {
    let n = 1usize << t.log2;
    let entries = &p.coeffs[t.coef as usize..(t.coef + t.num_coefs) as usize];
    let mut r = vec![0i16; n * n];
    let bd = t.bit_depth;
    let (ts, bypass) = (t.flags & TU_TRANSFORM_SKIP != 0, t.flags & TU_BYPASS != 0);
    let level = |e: u32| (e >> 16) as u16 as i16 as i64;
    if bypass {
        for &e in entries {
            r[(e & 0x3ff) as usize] = level(e) as i16;
        }
    } else {
        let (_, _, scale) = crate::residual_coding::compute_dequant_scale(t.qp_prime as i32, t.log2, bd);
        let transform_shift = 15 - bd as i32 - t.log2 as i32;
        let dq_shift = (10 - transform_shift) as u32;
        let dq_add = 1i64 << (dq_shift - 1);
        let list = (t.sl_idx != SL_FLAT && !(ts && t.log2 > 2)).then(|| &p.scaling_lists[t.sl_idx as usize * 17..][..17]);
        for &e in entries {
            let pos = (e & 0x3ff) as usize;
            let (x, y) = (pos % n, pos / n);
            let m = match list {
                None => 16,
                Some(l) => {
                    if x != 0 || y != 0 || t.log2 < 4 {
                        let mpos = match t.log2 {
                            3 => (y << 3) + x,
                            4 => ((y >> 1) << 3) + (x >> 1),
                            5 => ((y >> 2) << 3) + (x >> 2),
                            _ => (y << 2) + x,
                        };
                        (l[mpos / 4] >> (8 * (mpos % 4))) & 0xff
                    } else {
                        l[16]
                    }
                }
            } as i64;
            r[pos] = ((level(e) * scale as i64 * m + dq_add) >> dq_shift).clamp(-32768, 32767) as i16;
        }
        if ts {
            for c in r.iter_mut() {
                let v = *c as i32;
                *c = if transform_shift > 0 {
                    (v + (1 << (transform_shift - 1))) >> transform_shift
                } else {
                    v << -transform_shift
                } as i16;
            }
        } else {
            let dc_only = entries.len() == 1 && entries[0] & 0x3ff == 0;
            let (mut cols, mut rows) = (0usize, 0usize);
            for &e in entries {
                let pos = (e & 0x3ff) as usize;
                cols = cols.max(pos % n + 1);
                rows = rows.max(pos / n + 1);
            }
            let last = if dc_only { 0 } else { 1 };
            crate::inverse_transform::apply_inverse_transform(
                &mut r,
                t.log2,
                last,
                last,
                (cols.next_multiple_of(4) as u8, rows.next_multiple_of(4) as u8),
                bd as u32,
                t.flags & TU_DST != 0,
            );
        }
    }
    if let Some(s) = stages {
        let w = s.dims[t.c as usize].0;
        for j in 0..n {
            for i in 0..n {
                s.residual[t.c as usize][(t.y as usize + j) * w + t.x as usize + i] = r[j * n + i];
            }
        }
    }
    let (stride, plane) = match t.c {
        0 => (st.y_stride, &mut st.y_plane),
        1 => (st.uv_stride, &mut st.u_plane),
        _ => (st.uv_stride, &mut st.v_plane),
    };
    let off = t.y as usize * stride + t.x as usize;
    crate::cu_tree::with_block(plane, &mut st.block_buf, false, off, stride, (n, n), |dst, s| {
        crate::intra_pred::add_residual(dst, s, &r, t.log2, bd)
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Decoder, parse_annex_b};

    fn planes_eq(a: &Frame, b: &Frame) -> bool {
        let same = |x: &PixelData, y: &PixelData| match (x, y) {
            (PixelData::U8(p), PixelData::U8(q)) => p == q,
            (PixelData::U16(p), PixelData::U16(q)) => p == q,
            _ => false,
        };
        same(&a.y, &b.y) && same(&a.u, &b.u) && same(&a.v, &b.v)
    }

    /// Every fixture in testdata: the replay of the packed stream equals the decoder frame for frame, and an
    /// entropy-only decoder records exactly the same events. Streams with tools the stream does not carry
    /// (range extensions, 4:4:4) stop at their first unsupported picture; damaged (fuzz) streams are skipped.
    #[test]
    fn replay_matches_decoder_on_fixtures() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/testdata");
        let mut names: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.extension().is_some_and(|e| e == "h265"))
            .filter(|p| !p.file_name().unwrap().to_string_lossy().starts_with("fuzz"))
            .collect();
        names.sort();
        let (mut full, mut frames_total, mut bs_checked) = (Vec::new(), 0, 0);
        for path in names {
            let data = std::fs::read(&path).unwrap();
            let mut dec = Decoder::new().with_display_order().with_recon_stream(false);
            dec.recon_recorder().unwrap().check_bs = true;
            let mut ent = Decoder::new().with_display_order().with_recon_stream(true);
            let mut rep = Replayer::new();
            let mut pending = std::collections::VecDeque::new();
            let (mut frames, mut stopped) = (0, None::<String>);
            let mut buf = Vec::new();
            let mut step = |dec: &mut Decoder, ent: &mut Decoder, pending: &mut std::collections::VecDeque<Frame>| {
                let (a, b) = (dec.take_recon_events(), ent.take_recon_events());
                assert!(a == b, "{}: entropy-only events differ", path.display());
                for ev in a {
                    if stopped.is_some() {
                        return;
                    }
                    buf.clear();
                    encode_event(&ev, &mut buf);
                    let (back, _) = decode_event(&buf).unwrap();
                    match rep.event(&back) {
                        Ok(Some(f)) => {
                            let d = pending.pop_front().expect("decoder frame");
                            assert!(planes_eq(&f, &d), "{}: frame {frames} differs", path.display());
                            frames += 1;
                        }
                        Ok(None) => {}
                        Err(e) => stopped = Some(e),
                    }
                }
            };
            let mut ok = true;
            for nal in parse_annex_b(&data) {
                let r = dec.decode_nal(&nal);
                ent.decode_nal(&nal).ok();
                if r.is_err() {
                    ok = false;
                    break;
                }
                while let Some(f) = dec.next_output() {
                    pending.push_back(f);
                }
                while ent.next_output().is_some() {}
                step(&mut dec, &mut ent, &mut pending);
            }
            if !ok {
                continue;
            }
            while let Some(f) = dec.flush() {
                pending.push_back(f);
            }
            while ent.flush().is_some() {}
            step(&mut dec, &mut ent, &mut pending);
            // The strengths the GPU resolves from the edge codes are the decoder's (every supported picture).
            let r = dec.recon_recorder().unwrap();
            assert_eq!(r.bs_mismatches, 0, "{}: boundary strengths from edge codes differ", path.display());
            bs_checked += r.bs_checked;
            frames_total += frames;
            if stopped.is_none() {
                full.push(path.file_name().unwrap().to_string_lossy().into_owned());
            }
        }
        eprintln!("{} fixtures replayed completely ({frames_total} frames, bS of {bs_checked} pictures checked): {full:?}", full.len());
        for must in ["pcm.h265", "constrained_intra_128x128.h265", "scaling_list.h265", "multi_slice.h265", "dep_slices.h265", "inter_b.h265", "open_gop_cra.h265"] {
            assert!(full.iter().any(|n| n == must), "{must} not replayed completely");
        }
    }

    /// Events and decode errors of `nals` recorded with `threads` threads (`entropy_only` or full decoding).
    /// With full decoding also the boundary strength check (`Recorder::check_bs`): pictures checked, block map words
    /// that differ, the decoder's strengths per picture.
    #[allow(clippy::type_complexity)]
    fn record(
        nals: &[crate::NalUnit],
        threads: usize,
        entropy_only: bool,
    ) -> (Vec<Event>, Vec<String>, (u64, u64, Vec<(u32, Vec<u32>)>)) {
        let mut dec = Decoder::new().with_display_order().with_threads(threads).with_recon_stream(entropy_only);
        let r = dec.recon_recorder().unwrap();
        r.check_bs = !entropy_only;
        r.keep_decoder_bs = !entropy_only;
        let (mut events, mut errors) = (Vec::new(), Vec::new());
        for nal in nals {
            if let Err(e) = dec.decode_nal(nal) {
                errors.push(e.to_string());
            }
            while dec.next_output().is_some() {}
            events.extend(dec.take_recon_events());
        }
        while dec.flush().is_some() {}
        events.extend(dec.take_recon_events());
        let r = dec.recon_recorder().unwrap();
        (events, errors, (r.bs_checked, r.bs_mismatches, std::mem::take(&mut r.decoder_bs)))
    }

    /// Recording with threads (pictures one at a time, the CTB rows of WPP slices on several threads, each row
    /// into its own fragment, deblocking edge codes included) gives exactly the events and boundary strength check
    /// of one thread, on every fixture and on WPP fixtures whose slice data is damaged at several depths (rows that
    /// fail or end the slice early while rows below them already decode, whose records must be dropped).
    #[test]
    fn threaded_recording_matches_one_thread() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/testdata");
        let mut names: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.extension().is_some_and(|e| e == "h265"))
            .collect();
        names.sort();
        for path in names {
            let data = std::fs::read(&path).unwrap();
            let nals = parse_annex_b(&data);
            for entropy_only in [true, false] {
                let one = record(&nals, 1, entropy_only);
                for threads in [2, 3] {
                    assert!(
                        record(&nals, threads, entropy_only) == one,
                        "{} (entropy only {entropy_only}) differs with {threads} threads",
                        path.display()
                    );
                }
            }
        }
        let mut damaged = 0;
        for name in ["wpp_ctu16.h265", "ctu64_wpp.h265"] {
            let data = std::fs::read(format!("{dir}/{name}")).unwrap();
            let clean = parse_annex_b(&data);
            let vcl: Vec<usize> = (0..clean.len()).filter(|&i| clean[i].nal_unit_type.is_vcl()).collect();
            for &k in vcl.iter().take(6) {
                for tenths in [2, 5, 8, 9] {
                    let mut nals = parse_annex_b(&data);
                    let mut b = nals[k].rbsp.to_vec();
                    let at = b.len() * tenths / 10;
                    let end = (at + 4).min(b.len());
                    for x in &mut b[at..end] {
                        *x ^= 0x5a;
                    }
                    nals[k].rbsp = b.into();
                    let one = record(&nals, 1, true);
                    damaged += !one.1.is_empty() as usize;
                    assert!(record(&nals, 3, true) == one, "{name}: NAL {k} damaged at {tenths}/10 differs with 3 threads");
                }
            }
        }
        assert!(damaged > 0, "no damaged run produced an error");
    }
}
