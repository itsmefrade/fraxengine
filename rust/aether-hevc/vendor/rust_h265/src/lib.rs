//! Pure Rust H.265 / HEVC video decoder.
//!
//! Decodes Main and Main 10 profile HEVC bitstreams, and 4:2:0 and 4:4:4
//! range-extensions streams up to 12 bits with every range-extension coding
//! tool (cross-component prediction, RDPCM, transform-skip rotation and
//! context, extended precision, persistent Rice adaptation, CABAC bypass
//! alignment, chroma QP offset lists, ...). 4:4:4 covers HEIF/HEIC images.
//! Streams the decoder cannot decode correctly are rejected with
//! [`DecodeError::Unsupported`] instead of being decoded wrongly: 4:2:2,
//! monochrome, bit depths above 12, and screen content coding. Input can be
//! Annex B (start-code delimited,
//! `.h265` files) via [`parse_annex_b`], length-prefixed NAL units from
//! MP4/MKV containers via [`parse_hvcc`], or single NAL units via
//! [`parse_nal`].
//!
//! The decoder passes all 147 vectors of the JCT-VC HEVC_v1 conformance
//! suite and the 34 JCT-VC-RExt vectors in its formats (4:2:0 and 4:4:4, up
//! to 12 bits), and is byte-exact against FFmpeg on its test fixtures,
//! including real 1080p Big Buck Bunny encodes in 8-bit and 10-bit.
//!
//! # Quick start
//!
//! [`Decoder::with_display_order`] returns frames in display order, as the
//! spec's output process does. It is the recommended mode; decoding order
//! remains the default only for compatibility (see below). Take frames with
//! [`next_output`](Decoder::next_output) after each NAL unit, and call
//! [`flush`](Decoder::flush) until it returns `None` at the end of the
//! stream:
//!
//! ```no_run
//! use rust_h265::{Decoder, PixelData, parse_annex_b};
//!
//! let data = std::fs::read("input.h265")?;
//! let mut decoder = Decoder::new().with_display_order();
//! let mut frames = Vec::new();
//!
//! for nal in parse_annex_b(&data) {
//!     decoder.decode_nal(&nal)?;
//!     while let Some(frame) = decoder.next_output() {
//!         frames.push(frame);
//!     }
//! }
//! while let Some(frame) = decoder.flush() {
//!     frames.push(frame);
//! }
//!
//! for frame in &frames {
//!     match &frame.y {
//!         PixelData::U8(luma) => { /* 8-bit samples */ }
//!         PixelData::U16(luma) => { /* 10-bit samples */ }
//!     }
//! }
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! # Decoding order
//!
//! Prefer display order. A decoder made with [`Decoder::new`] alone
//! returns each picture from [`decode_nal`](Decoder::decode_nal) once it
//! has ended (when the next picture starts, at an access unit delimiter,
//! end of sequence or end of bitstream NAL unit, or at
//! [`flush`](Decoder::flush)), in decoding order, and returns every decoded
//! picture, including those the spec does not output. This suits callers
//! that pair each frame with the container packet it came from. To display
//! such frames, sort them by
//! [`pic_order_cnt`](Frame::pic_order_cnt) within each coded video
//! sequence: POC restarts at every IDR or BLA picture, and at a CRA
//! picture that follows an end-of-sequence NAL unit.
//!
//! # Frames
//!
//! A [`Frame`] holds cropped `y`, `u` and `v` planes as [`PixelData`]:
//! `U8` for 8-bit streams, `U16` for deeper ones. `chroma_format_idc` is 1
//! for 4:2:0, where `u` and `v` have half the width and height, and 3 for
//! 4:4:4, where they are full size.
//!
//! # Threads
//!
//! [`Decoder::with_threads`] decodes on several threads with output
//! identical to single-threaded decoding: each picture on its own thread,
//! overlapping with the pictures it references (it waits only for the
//! reference rows it reads). Frames, and errors in slice data, come out of
//! later calls than with one thread; [`Decoder::next_error`] returns the
//! errors still waiting after the last NAL unit. The default is one
//! thread.
//!
//! # Resource limits
//!
//! [`Decoder::new`] rejects any SPS whose picture exceeds the Level 6.2
//! maximum of 35,651,584 luma samples or an estimated 512 MB of
//! per-picture state, with [`DecodeError::ResourceLimit`]. Use
//! [`Decoder::with_limits`] to change either bound.

// --- Internal modules (not part of the public API) ---
// allow(dead_code): internal modules contain utility functions, struct fields,
// and table constants kept for spec completeness / future use.
#[allow(dead_code)]
mod bitstream;
#[allow(dead_code)]
mod cabac;
#[allow(dead_code)]
mod cabac_tables;
#[allow(dead_code)]
mod cu_tree;
#[allow(dead_code)]
mod deblock;
#[allow(dead_code)]
mod inter_pred;
#[allow(dead_code)]
mod intra_pred;
#[allow(dead_code)]
mod inverse_transform;
mod output;
mod parallel;
#[allow(dead_code)]
pub(crate) mod pixel;
#[allow(dead_code)]
mod pps;
#[allow(dead_code)]
mod profile_tier_level;
#[allow(dead_code)]
mod residual_coding;
mod rowfilter;
#[allow(dead_code)]
mod sao;
#[allow(dead_code)]
mod scaling_list;
#[allow(dead_code)]
mod shared;
#[allow(dead_code)]
mod slice;
#[allow(dead_code)]
mod sps;
#[allow(dead_code)]
mod vps;
mod thr;
mod wavefront;
/// Reconstruction command stream and its CPU replay (feature `recon-stream`).
#[cfg(feature = "recon-stream")]
pub mod recon;

// --- Internal but needs pub(crate) access from decoder ---
#[allow(dead_code)]
pub(crate) mod dpb;
pub(crate) mod simd;
/// Minimal SHA-256 for the in-source tests (the vendored copy has no dev-dependencies).
#[cfg(test)]
pub(crate) mod sha2 {
    pub trait Digest {
        fn new() -> Self;
        fn update(&mut self, data: impl AsRef<[u8]>);
        fn finalize(self) -> Out;
    }
    pub struct Out([u8; 32]);
    impl std::fmt::LowerHex for Out {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            self.0.iter().try_for_each(|b| write!(f, "{b:02x}"))
        }
    }
    impl std::ops::Deref for Out {
        type Target = [u8; 32];
        fn deref(&self) -> &[u8; 32] {
            &self.0
        }
    }
    impl AsRef<[u8]> for Out {
        fn as_ref(&self) -> &[u8] {
            &self.0
        }
    }
    pub struct Sha256 {
        h: [u32; 8],
        buf: Vec<u8>,
        len: u64,
    }
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
        0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
        0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
        0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
        0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
        0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
        0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
    ];
    impl Sha256 {
        fn block(h: &mut [u32; 8], b: &[u8]) {
            let mut w = [0u32; 64];
            for i in 0..16 {
                w[i] = u32::from_be_bytes([b[4 * i], b[4 * i + 1], b[4 * i + 2], b[4 * i + 3]]);
            }
            for i in 16..64 {
                let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
                let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
                w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
            }
            let mut v = *h;
            for i in 0..64 {
                let s1 = v[4].rotate_right(6) ^ v[4].rotate_right(11) ^ v[4].rotate_right(25);
                let ch = (v[4] & v[5]) ^ (!v[4] & v[6]);
                let t1 = v[7].wrapping_add(s1).wrapping_add(ch).wrapping_add(K[i]).wrapping_add(w[i]);
                let s0 = v[0].rotate_right(2) ^ v[0].rotate_right(13) ^ v[0].rotate_right(22);
                let mj = (v[0] & v[1]) ^ (v[0] & v[2]) ^ (v[1] & v[2]);
                let t2 = s0.wrapping_add(mj);
                v = [t1.wrapping_add(t2), v[0], v[1], v[2], v[3].wrapping_add(t1), v[4], v[5], v[6]];
            }
            for (x, y) in h.iter_mut().zip(v) {
                *x = x.wrapping_add(y);
            }
        }
    }
    impl Digest for Sha256 {
        fn new() -> Self {
            Self {
                h: [
                    0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
                    0x5be0cd19,
                ],
                buf: Vec::new(),
                len: 0,
            }
        }
        fn update(&mut self, data: impl AsRef<[u8]>) {
            let d = data.as_ref();
            self.len += d.len() as u64;
            self.buf.extend_from_slice(d);
            let full = self.buf.len() / 64 * 64;
            for c in self.buf[..full].chunks_exact(64) {
                Self::block(&mut self.h, c);
            }
            self.buf.drain(..full);
        }
        fn finalize(mut self) -> Out {
            let bits = self.len * 8;
            let mut pad = vec![0x80u8];
            while (self.buf.len() + pad.len()) % 64 != 56 {
                pad.push(0);
            }
            pad.extend(bits.to_be_bytes());
            self.buf.extend(pad);
            for c in self.buf.chunks_exact(64) {
                Self::block(&mut self.h, c);
            }
            let mut o = [0u8; 32];
            for (i, x) in self.h.iter().enumerate() {
                o[4 * i..4 * i + 4].copy_from_slice(&x.to_be_bytes());
            }
            Out(o)
        }
    }
}
pub(crate) mod mc_simd;
pub(crate) mod intra_simd;
pub(crate) mod tx_simd;
pub(crate) mod deblock_simd;

// --- Public modules ---
pub mod decoder;
pub mod error;
pub mod nal;

// --- Top-level re-exports for convenience ---
pub use decoder::{DecodeLimits, Decoder, Frame, PicPlan};
pub use error::DecodeError;
pub use nal::{NalUnit, NalUnitType, parse_annex_b, parse_hvcc, parse_nal};
pub use pixel::{Pixel, PixelData};

/// The body of host worker `index` in a `wasm-threads` build (see `thr.rs`): the host sets the
/// instance's stack pointer to the worker's own stack, then calls this; it never returns.
#[cfg(all(target_arch = "wasm32", feature = "wasm-threads"))]
pub fn wasm_thread_main(index: usize) {
    thr::thread_main(index)
}

/// Record that bytes `lo..hi` are a host worker's stack (`wasm-threads` builds; call before the worker
/// starts, from one thread): per-thread decoder state is told apart by stack (`thr::thread_slot`).
#[cfg(all(target_arch = "wasm32", feature = "wasm-threads"))]
pub fn wasm_register_thread_stack(lo: usize, hi: usize) {
    thr::register_stack(lo, hi)
}

/// Workers waiting for a job (`wasm-threads` builds).
#[cfg(all(target_arch = "wasm32", feature = "wasm-threads"))]
pub fn wasm_idle_workers() -> u32 {
    thr::idle_workers()
}

#[cfg(test)]
mod parameter_set_tests {
    use crate::nal::{NalUnitType, parse_annex_b};

    /// Decode VPS+SPS+PPS from a real x265-encoded fixture and assert the
    /// fields a downstream slice/CTU loop will need.
    ///
    /// Fixture command (run from the crate root):
    /// ```text
    /// ffmpeg -hide_banner -loglevel error \
    ///   -f lavfi -i color=gray:size=16x16:rate=30:duration=0.04 \
    ///   -frames:v 1 -pix_fmt yuv420p -f rawvideo testdata/tiny_input.yuv
    /// x265 --input-res 16x16 --fps 30 --frames 1 --keyint 1 --bframes 0 \
    ///   --ctu 16 --input testdata/tiny_input.yuv -o testdata/tiny_intra.h265
    /// ```
    #[test]
    fn parse_tiny_intra_parameter_sets() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/testdata/tiny_intra.h265");
        let data = std::fs::read(path).expect("read fixture");
        let nals = parse_annex_b(&data);

        // Find the parameter set NALs. The fixture also contains a PrefixSei
        // (the x265 build banner) and the IDR slice NAL - we don't need them.
        let vps_nal = nals
            .iter()
            .find(|n| n.nal_unit_type == NalUnitType::Vps)
            .expect("VPS NAL");
        let sps_nal = nals
            .iter()
            .find(|n| n.nal_unit_type == NalUnitType::Sps)
            .expect("SPS NAL");
        let pps_nal = nals
            .iter()
            .find(|n| n.nal_unit_type == NalUnitType::Pps)
            .expect("PPS NAL");

        // ---- VPS ----
        let vps = crate::vps::parse_vps(&vps_nal.rbsp).expect("parse VPS");
        assert_eq!(vps.vps_video_parameter_set_id, 0);
        assert_eq!(vps.vps_max_layers_minus1, 0);
        assert_eq!(vps.vps_max_sub_layers_minus1, 0);
        assert!(vps.vps_temporal_id_nesting_flag);
        // Main Still Picture profile, level 1.0
        assert_eq!(vps.profile_tier_level.general_profile_idc, 3);
        assert_eq!(vps.profile_tier_level.general_level_idc, 30);
        assert!(!vps.profile_tier_level.general_tier_flag);

        // ---- SPS ----
        let sps = crate::sps::parse_sps(&sps_nal.rbsp).expect("parse SPS");
        assert_eq!(sps.sps_video_parameter_set_id, 0);
        assert_eq!(sps.sps_seq_parameter_set_id, 0);
        assert_eq!(sps.chroma_format_idc, 1);
        assert_eq!(sps.pic_width_in_luma_samples, 16);
        assert_eq!(sps.pic_height_in_luma_samples, 16);
        assert_eq!(sps.bit_depth_luma, 8);
        assert_eq!(sps.bit_depth_chroma, 8);
        // log2_max_pic_order_cnt_lsb_minus4 = 4 -> MaxPicOrderCntLsb = 256
        assert_eq!(sps.log2_max_pic_order_cnt_lsb, 8);

        // CTU derivation: log2_min_cb_minus3=0, log2_diff=1
        // -> MinCbLog2SizeY=3, CtbLog2SizeY=4, CtbSizeY=16.
        assert_eq!(sps.min_cb_log2_size_y, 3);
        assert_eq!(sps.ctb_log2_size_y, 4);
        assert_eq!(sps.ctb_size_y, 16);
        assert_eq!(sps.pic_width_in_ctbs_y(), 1);
        assert_eq!(sps.pic_height_in_ctbs_y(), 1);

        // TU derivation: min_tb_minus2=0, diff=2 -> 2..=4 (4x4..16x16)
        assert_eq!(sps.min_tb_log2_size_y, 2);
        assert_eq!(sps.max_tb_log2_size_y, 4);

        assert!(!sps.scaling_list_enabled_flag);
        assert!(!sps.amp_enabled_flag);
        assert!(!sps.sample_adaptive_offset_enabled_flag);
        assert!(!sps.pcm_enabled_flag);
        assert_eq!(sps.num_short_term_ref_pic_sets, 0);
        assert!(!sps.long_term_ref_pics_present_flag);
        assert!(sps.sps_temporal_mvp_enabled_flag);
        assert!(!sps.strong_intra_smoothing_enabled_flag);

        // ---- PPS ----
        let pps = crate::pps::parse_pps(&pps_nal.rbsp).expect("parse PPS");
        assert_eq!(pps.pps_pic_parameter_set_id, 0);
        assert_eq!(pps.pps_seq_parameter_set_id, 0);
        assert!(!pps.dependent_slice_segments_enabled_flag);
        assert!(!pps.output_flag_present_flag);
        assert_eq!(pps.num_extra_slice_header_bits, 0);
        assert!(!pps.sign_data_hiding_enabled_flag);
        assert!(!pps.cabac_init_present_flag);
        assert_eq!(pps.num_ref_idx_l0_default_active_minus1, 0);
        assert_eq!(pps.num_ref_idx_l1_default_active_minus1, 0);
        assert_eq!(pps.init_qp, 26);
        assert!(pps.cu_qp_delta_enabled_flag);
        assert_eq!(pps.diff_cu_qp_delta_depth, 0);
        assert_eq!(pps.pps_cb_qp_offset, 0);
        assert_eq!(pps.pps_cr_qp_offset, 0);
        assert!(!pps.weighted_pred_flag);
        assert!(!pps.weighted_bipred_flag);
        assert!(!pps.tiles_enabled_flag);
        assert!(!pps.entropy_coding_sync_enabled_flag);
        assert!(pps.deblocking_filter_control_present_flag);
        assert!(!pps.deblocking_filter_override_enabled_flag);
        assert!(pps.pps_deblocking_filter_disabled_flag);
        assert!(!pps.lists_modification_present_flag);
        assert_eq!(pps.log2_parallel_merge_level_minus2, 0);
        assert!(!pps.slice_segment_header_extension_present_flag);
    }
}
