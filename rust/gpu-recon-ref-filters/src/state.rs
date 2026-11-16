//! Stand-ins for the parts of rust_h265's `PictureState`, `Sps`, `Pps` and `Pixel` that the copied loop filters
//! read. Field names and types match rust_h265 (`cu_tree.rs` PictureState, `pixel.rs` Pixel) so the copied code
//! compiles unchanged; tables are plain `Vec`s (rust_h265's `SharedTable` derefs to a slice the same way).

use crate::sao_ref::SaoParams;

pub type SharedTable<T> = Vec<T>;

/// rust_h265 `pixel::Pixel`, reduced to what the loop filters use.
pub trait Pixel: Copy + Clone + Default + Send + Sync + Sized + 'static {
    fn from_i32_clamped(val: i32, bit_depth: u8) -> Self;
    fn to_i32(self) -> i32;
    fn zero() -> Self;
}

impl Pixel for u8 {
    #[inline(always)]
    fn from_i32_clamped(val: i32, _bit_depth: u8) -> Self {
        val.clamp(0, 255) as u8
    }
    #[inline(always)]
    fn to_i32(self) -> i32 {
        self as i32
    }
    fn zero() -> Self {
        0
    }
}

impl Pixel for u16 {
    #[inline(always)]
    fn from_i32_clamped(val: i32, bit_depth: u8) -> Self {
        val.clamp(0, (1i32 << bit_depth) - 1) as u16
    }
    #[inline(always)]
    fn to_i32(self) -> i32 {
        self as i32
    }
    fn zero() -> Self {
        0
    }
}

pub struct Sps {
    pub bit_depth_luma: u8,
    pub bit_depth_chroma: u8,
    pub ctb_log2_size_y: u8,
    pub sample_adaptive_offset_enabled_flag: bool,
}

impl Sps {
    /// 4:2:0 only (the GPU kernels' target).
    pub fn chroma_array_type(&self) -> u32 {
        1
    }
}

pub struct Pps {
    pub pps_cb_qp_offset: i32,
    pub pps_cr_qp_offset: i32,
}

/// The `PictureState` fields the loop filters read, sized as `PictureState::new_recycled` sizes them
/// (per-min-CB / per-4x4 tables over CTB-aligned dimensions, per-CTB tables over `div_ceil`).
pub struct PictureState<P: Pixel> {
    pub width: u32,
    pub height: u32,
    pub y_stride: usize,
    pub uv_stride: usize,
    pub chroma_array_type: u32,
    pub chroma_shift_w: u8,
    pub chroma_shift_h: u8,
    pub log2_ctb_size: u8,
    pub width_in_ctbs: usize,
    pub log2_min_cb_size: u8,
    pub min_cb_width: usize,
    pub min_pu_width: usize,
    pub tab_qp_y: SharedTable<i8>,
    pub bs_vertical: SharedTable<u8>,
    pub bs_horizontal: SharedTable<u8>,
    pub sao_params: SharedTable<SaoParams>,
    pub tab_slice_addr_rs: SharedTable<i32>,
    pub filter_slice_edges: SharedTable<bool>,
    pub tab_tile_id: SharedTable<u32>,
    pub tab_ctb_ts: SharedTable<u32>,
    pub tab_deblock_offsets: SharedTable<(i8, i8)>,
    pub tab_no_filter: SharedTable<u8>,
    pub any_no_filter: bool,
    pub filter_across_tiles: bool,
    pub sao_snapshot: [Vec<P>; 3],
    pub y_plane: Vec<P>,
    pub u_plane: Vec<P>,
    pub v_plane: Vec<P>,
}
