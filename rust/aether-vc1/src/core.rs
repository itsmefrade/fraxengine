//! Decoder state and header parsing (FFmpeg vc1.c).
use crate::bits::BitReader;
use crate::dsp::clip_u8;
use crate::tables as t;
use std::rc::Rc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Invalid(&'static str),
    Unsupported(&'static str),
    NoSequenceHeader,
}
pub type R<T> = Result<T, Error>;
pub fn inv<T>(m: &'static str) -> R<T> {
    Err(Error::Invalid(m))
}

// picture types
pub const PT_I: u8 = 1;
pub const PT_P: u8 = 2;
pub const PT_B: u8 = 3;
pub const PT_BI: u8 = 7;

// FCM
pub const PROGRESSIVE: u8 = 0;
pub const ILACE_FRAME: u8 = 1;
pub const ILACE_FIELD: u8 = 2;

// MV modes
pub const MV_PMODE_1MV_HPEL_BILIN: u8 = 0;
pub const MV_PMODE_1MV: u8 = 1;
pub const MV_PMODE_1MV_HPEL: u8 = 2;
pub const MV_PMODE_MIXED_MV: u8 = 3;
pub const MV_PMODE_INTENSITY_COMP: u8 = 4;

// transform types
pub const TT_8X8: i32 = 0;
pub const TT_8X4_BOTTOM: i32 = 1;
pub const TT_8X4_TOP: i32 = 2;
pub const TT_8X4: i32 = 3;
pub const TT_4X8_RIGHT: i32 = 4;
pub const TT_4X8_LEFT: i32 = 5;
pub const TT_4X8: i32 = 6;
pub const TT_4X4: i32 = 7;

pub const BMV_TYPE_BACKWARD: i32 = 0;
pub const BMV_TYPE_FORWARD: i32 = 1;
pub const BMV_TYPE_INTERPOLATED: i32 = 2;
pub const BMV_TYPE_DIRECT: i32 = 3;

pub const CONDOVER_NONE: u8 = 0;
pub const CONDOVER_ALL: u8 = 1;
pub const CONDOVER_SELECT: u8 = 2;

pub const QUANT_FRAME_IMPLICIT: u8 = 0;
pub const QUANT_FRAME_EXPLICIT: u8 = 1;
pub const QUANT_NON_UNIFORM: u8 = 2;

pub const DQPROFILE_FOUR_EDGES: u8 = 0;
pub const DQPROFILE_DOUBLE_EDGES: u8 = 1;
pub const DQPROFILE_SINGLE_EDGE: u8 = 2;
pub const DQPROFILE_ALL_MBS: u8 = 3;

/// Largest supported coded picture (about 4K UHD); larger headers are rejected to bound memory.
pub const MAX_PIXELS: usize = 4096 * 2304;

pub const B_FRACTION_DEN: i32 = 256;
pub const BFRACTION_LUT: [i16; 23] = [
    128, 85, 170, 64, 192, 51, 102, 153, 204, 43, 215, 37, 74, 111, 148, 185, 222, 32, 96, 160, 224, -1, 0,
];

// per macroblock kind stored in Pic::mb_type
pub const MBT_NONE: u8 = 0;
pub const MBT_INTRA: u8 = 1;
pub const MBT_16X16: u8 = 2;
pub const MBT_SKIP: u8 = 3;

/// Padding (in entries) in front of every block-indexed array so that FFmpeg's negative
/// neighbour offsets stay in range.
pub const PAD: usize = 64;

/// Geometry of the block-index address space (identical arithmetic to FFmpeg's `block_index`).
#[derive(Clone, Copy, Default, Debug)]
pub struct Layout {
    pub mb_width: usize,
    /// number of macroblock rows allocated (even)
    pub mbh_a: usize,
    pub b8_stride: usize,
    pub mb_stride: usize,
    pub lsize: usize,
    pub csize: usize,
    pub total: usize,
}

impl Layout {
    pub fn new(mb_width: usize, mb_height: usize) -> Layout {
        let mbh_a = (mb_height + 1) & !1;
        let b8_stride = mb_width * 2 + 1;
        let mb_stride = mb_width + 1;
        let lsize = b8_stride * (2 * mbh_a + 2) + 8;
        let csize = mb_stride * (mbh_a + 2) + 8;
        Layout { mb_width, mbh_a, b8_stride, mb_stride, lsize, csize, total: lsize + 2 * csize + 2 * PAD + 16 }
    }
}

/// A decoded picture (reference or output).
pub struct Pic {
    pub planes: [Vec<u8>; 3],
    pub stride: [usize; 3],
    /// Motion vectors per 8x8 luma block (block-index space, offset by PAD).
    pub motion_val: [Vec<[i16; 2]>; 2],
    pub mb_type: Vec<u8>,
    pub pts: i64,
    pub tff: bool,
    pub rff: bool,
    pub rptfrm: u8,
    pub interlaced: bool,
    pub field_picture: bool,
    pub pict_type: u8,
    pub coded_width: usize,
    pub coded_height: usize,
    /// display order key assigned by the decoder (monotonic per sequence)
    pub poc: u64,
    pub key: bool,
}

impl Pic {
    pub fn new(lay: &Layout) -> Pic {
        let w = lay.mb_width * 16;
        let h = lay.mbh_a * 16;
        Pic {
            planes: [vec![128; w * h], vec![128; w * h / 4], vec![128; w * h / 4]],
            stride: [w, w / 2, w / 2],
            motion_val: [vec![[0; 2]; lay.lsize + 2 * PAD], vec![[0; 2]; lay.lsize + 2 * PAD]],
            mb_type: vec![0; lay.mb_stride * (lay.mbh_a + 2) + 2 * PAD],
            pts: 0,
            tff: true,
            rff: false,
            rptfrm: 0,
            interlaced: false,
            field_picture: false,
            pict_type: 0,
            coded_width: 0,
            coded_height: 0,
            poc: 0,
            key: false,
        }
    }
}

#[derive(Clone)]
pub struct Lut {
    pub y: [[u8; 256]; 2],
    pub uv: [[u8; 256]; 2],
    pub use_ic: bool,
}
impl Lut {
    pub fn ident() -> Lut {
        let mut l = Lut { y: [[0; 256]; 2], uv: [[0; 256]; 2], use_ic: false };
        for k in 0..2 {
            for i in 0..256 {
                l.y[k][i] = i as u8;
                l.uv[k][i] = i as u8;
            }
        }
        l
    }
}

pub const LAST: usize = 0;
pub const NEXT: usize = 1;
pub const AUX: usize = 2;

/// Reference picture selector for motion compensation.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum RefSel {
    Cur,
    Last,
    Next,
}

pub struct Core {
    pub vl: &'static crate::vlcs::Vlcs,
    // ---- sequence header
    pub profile: u8,
    pub level: u8,
    pub chromaformat: u8,
    pub postprocflag: bool,
    pub broadcast: bool,
    pub interlace: bool,
    pub tfcntrflag: bool,
    pub finterpflag: bool,
    pub psf: bool,
    pub hrd_param_flag: bool,
    pub hrd_num_leaky_buckets: u32,
    pub max_coded_width: usize,
    pub max_coded_height: usize,
    pub color_prim: u8,
    pub transfer_char: u8,
    pub matrix_coef: u8,
    pub display_width: usize,
    pub display_height: usize,
    pub aspect_num: u32,
    pub aspect_den: u32,
    pub fps_num: u32,
    pub fps_den: u32,
    pub max_b_frames: u8,
    pub res_rtm_flag: bool,
    // ---- entry point
    pub broken_link: bool,
    pub closed_entry: bool,
    pub panscanflag: bool,
    pub refdist_flag: bool,
    pub loop_filter: bool,
    pub fastuvmc: bool,
    pub extended_mv: bool,
    pub extended_dmv: bool,
    pub dquant: u8,
    pub vstransform: bool,
    pub overlap: bool,
    pub quantizer_mode: u8,
    pub range_mapy_flag: bool,
    pub range_mapuv_flag: bool,
    pub range_mapy: u8,
    pub range_mapuv: u8,
    // ---- dimensions
    pub width: usize,
    pub height: usize,
    pub mb_width: usize,
    pub mb_height: usize,
    pub lay: Layout,
    pub have_seq: bool,
    pub have_ep: bool,
    // ---- picture header
    pub pict_type: u8,
    pub fcm: u8,
    pub field_mode: bool,
    pub second_field: bool,
    pub fptype: u32,
    pub first_pic_header_flag: bool,
    pub pic_header_flag: bool,
    pub tff: bool,
    pub rff: bool,
    pub rptfrm: u8,
    pub uvsamp: bool,
    pub postproc: u8,
    pub p_frame_skipped: bool,
    pub bi_type: bool,
    pub rnd: i32,
    pub interpfrm: bool,
    pub bfraction: i32,
    pub bfraction_lut_index: usize,
    pub pq: i32,
    pub altpq: i32,
    pub pqindex: i32,
    pub halfpq: i32,
    pub pquantizer: bool,
    pub dquantfrm: bool,
    pub dqprofile: u8,
    pub dqsbedge: u8,
    pub dqbilevel: bool,
    pub mvrange: u32,
    pub dmvrange: u32,
    pub k_x: i32,
    pub k_y: i32,
    pub range_x: i32,
    pub range_y: i32,
    pub mv_mode: u8,
    pub mv_mode2: u8,
    pub quarter_sample: bool,
    pub mspel: bool,
    pub qs_last: bool,
    pub lumscale: u8,
    pub lumshift: u8,
    pub lumscale2: u8,
    pub lumshift2: u8,
    pub intcomp: bool,
    pub intcompfield: u32,
    pub numref: u32,
    pub reffield: u32,
    pub refdist: i32,
    pub frfd: i32,
    pub brfd: i32,
    pub cur_field_type: usize,
    pub ref_field_type: [usize; 2],
    pub mv_type_is_raw: bool,
    pub dmb_is_raw: bool,
    pub fmb_is_raw: bool,
    pub skip_is_raw: bool,
    pub acpred_is_raw: bool,
    pub overflg_is_raw: bool,
    pub fieldtx_is_raw: bool,
    pub condover: u8,
    pub fourmvswitch: bool,
    pub tt_index: usize,
    pub ttmbf: bool,
    pub ttfrm: i32,
    pub c_ac_table_index: u32,
    pub y_ac_table_index: u32,
    pub dc_table_index: usize,
    pub cbptab: usize,
    pub mv_table_index: usize,
    pub mbmodetab: usize,
    pub imvtab: usize,
    pub icbptab: usize,
    pub twomvbptab: usize,
    pub fourmvbptab: usize,
    pub rangeredfrm: bool,
    pub codingset: usize,
    pub codingset2: usize,
    // ---- lookup tables for intensity compensation
    pub luts: [Lut; 3],
    pub curr: usize,
    // ---- block-index address space arrays
    pub dc_val: Vec<i16>,
    pub ac_val: Vec<[i16; 16]>,
    pub coded_block: Vec<u8>,
    pub mb_type_b: Vec<u8>, // v->mb_type[0]
    pub blk_mv_type: Vec<u8>,
    pub mv_f: [Vec<u8>; 2],
    pub mv_f_next: [Vec<u8>; 2],
    // ---- MB-indexed planes (stride mb_stride)
    pub mv_type_mb_plane: Vec<u8>,
    pub direct_mb_plane: Vec<u8>,
    pub forward_mb_plane: Vec<u8>,
    pub fieldtx_plane: Vec<u8>,
    pub acpred_plane: Vec<u8>,
    pub over_flags_plane: Vec<u8>,
    pub mbskip_table: Vec<u8>,
    pub qscale_table: Vec<i8>,
    // ---- row rings
    pub cbp: Vec<u32>,
    pub ttblk: Vec<i32>,
    pub is_intra: Vec<u8>,
    pub luma_mv: Vec<[i16; 2]>,
    // ---- block ring
    pub block: Vec<i16>, // ring of n_blks macroblocks x 6 blocks x 64, flat
    pub n_blks: usize,
    pub cur_blk_idx: usize,
    pub left_blk_idx: usize,
    pub topleft_blk_idx: usize,
    pub top_blk_idx: usize,
    // ---- scan tables
    pub zz_8x8: [[u8; 64]; 4],
    pub zzi_8x8: [u8; 64],
    pub left_blk_sh: usize,
    pub top_blk_sh: usize,
    // ---- per macroblock working state
    pub mb_x: usize,
    pub mb_y: usize,
    pub start_mb_y: usize,
    pub end_mb_y: usize,
    pub end_mb_x: usize,
    pub first_slice_line: bool,
    pub mb_intra: bool,
    pub ac_pred: bool,
    pub a_avail: bool,
    pub c_avail: bool,
    pub y_dc_scale: i32,
    pub c_dc_scale: i32,
    pub esc3_level_length: u32,
    pub esc3_run_length: u32,
    pub mv: [[[i32; 2]; 4]; 2],
    pub blocks_off: usize,
    pub mb_off: usize,
    pub bmvtype: i32,
    pub twomvbp: u32,
    pub fourmvbp: u32,
    pub block_index: [usize; 6],
    pub block_wrap: [usize; 6],
    pub dest: [usize; 3],
    pub linesize: usize,
    pub uvlinesize: usize,
    // ---- pictures
    pub cur: Option<Box<Pic>>,
    pub last: Option<Rc<Pic>>,
    pub next: Option<Rc<Pic>>,
    pub last_interlaced: bool,
    pub next_interlaced: bool,
    pub edge: Vec<u8>,
    pub edge_u: Vec<u8>,
    pub edge_v: Vec<u8>,
    pub apply_range_map: bool,
}

pub const EB: usize = 64; // edge emulation buffer stride

impl Core {
    pub fn new() -> Core {
        let lut = Lut::ident();
        Core {
            vl: crate::vlcs::get(),
            profile: 3,
            level: 0,
            chromaformat: 1,
            postprocflag: false,
            broadcast: false,
            interlace: false,
            tfcntrflag: false,
            finterpflag: false,
            psf: false,
            hrd_param_flag: false,
            hrd_num_leaky_buckets: 0,
            max_coded_width: 0,
            max_coded_height: 0,
            color_prim: 0,
            transfer_char: 0,
            matrix_coef: 0,
            display_width: 0,
            display_height: 0,
            aspect_num: 0,
            aspect_den: 1,
            fps_num: 0,
            fps_den: 1,
            max_b_frames: 7,
            res_rtm_flag: true,
            broken_link: false,
            closed_entry: false,
            panscanflag: false,
            refdist_flag: false,
            loop_filter: false,
            fastuvmc: false,
            extended_mv: false,
            extended_dmv: false,
            dquant: 0,
            vstransform: false,
            overlap: false,
            quantizer_mode: 0,
            range_mapy_flag: false,
            range_mapuv_flag: false,
            range_mapy: 0,
            range_mapuv: 0,
            width: 0,
            height: 0,
            mb_width: 0,
            mb_height: 0,
            lay: Layout::default(),
            have_seq: false,
            have_ep: false,
            pict_type: 0,
            fcm: 0,
            field_mode: false,
            second_field: false,
            fptype: 0,
            first_pic_header_flag: true,
            pic_header_flag: false,
            tff: true,
            rff: false,
            rptfrm: 0,
            uvsamp: false,
            postproc: 0,
            p_frame_skipped: false,
            bi_type: false,
            rnd: 0,
            interpfrm: false,
            bfraction: 0,
            bfraction_lut_index: 0,
            pq: -1,
            altpq: 0,
            pqindex: 0,
            halfpq: 0,
            pquantizer: false,
            dquantfrm: false,
            dqprofile: 0,
            dqsbedge: 0,
            dqbilevel: false,
            mvrange: 0,
            dmvrange: 0,
            k_x: 9,
            k_y: 8,
            range_x: 256,
            range_y: 128,
            mv_mode: 0,
            mv_mode2: 0,
            quarter_sample: false,
            mspel: false,
            qs_last: false,
            lumscale: 32,
            lumshift: 0,
            lumscale2: 32,
            lumshift2: 0,
            intcomp: false,
            intcompfield: 0,
            numref: 0,
            reffield: 0,
            refdist: 0,
            frfd: 0,
            brfd: 0,
            cur_field_type: 0,
            ref_field_type: [0; 2],
            mv_type_is_raw: false,
            dmb_is_raw: false,
            fmb_is_raw: false,
            skip_is_raw: false,
            acpred_is_raw: false,
            overflg_is_raw: false,
            fieldtx_is_raw: false,
            condover: 0,
            fourmvswitch: false,
            tt_index: 0,
            ttmbf: false,
            ttfrm: 0,
            c_ac_table_index: 0,
            y_ac_table_index: 0,
            dc_table_index: 0,
            cbptab: 0,
            mv_table_index: 0,
            mbmodetab: 0,
            imvtab: 0,
            icbptab: 0,
            twomvbptab: 0,
            fourmvbptab: 0,
            rangeredfrm: false,
            codingset: 0,
            codingset2: 0,
            luts: [lut.clone(), lut.clone(), lut],
            curr: NEXT,
            dc_val: Vec::new(),
            ac_val: Vec::new(),
            coded_block: Vec::new(),
            mb_type_b: Vec::new(),
            blk_mv_type: Vec::new(),
            mv_f: [Vec::new(), Vec::new()],
            mv_f_next: [Vec::new(), Vec::new()],
            mv_type_mb_plane: Vec::new(),
            direct_mb_plane: Vec::new(),
            forward_mb_plane: Vec::new(),
            fieldtx_plane: Vec::new(),
            acpred_plane: Vec::new(),
            over_flags_plane: Vec::new(),
            mbskip_table: Vec::new(),
            qscale_table: Vec::new(),
            cbp: Vec::new(),
            ttblk: Vec::new(),
            is_intra: Vec::new(),
            luma_mv: Vec::new(),
            block: Vec::new(),
            n_blks: 0,
            cur_blk_idx: 0,
            left_blk_idx: 0,
            topleft_blk_idx: 0,
            top_blk_idx: 0,
            zz_8x8: [[0; 64]; 4],
            zzi_8x8: [0; 64],
            left_blk_sh: 0,
            top_blk_sh: 3,
            mb_x: 0,
            mb_y: 0,
            start_mb_y: 0,
            end_mb_y: 0,
            end_mb_x: 0,
            first_slice_line: false,
            mb_intra: false,
            ac_pred: false,
            a_avail: false,
            c_avail: false,
            y_dc_scale: 0,
            c_dc_scale: 0,
            esc3_level_length: 0,
            esc3_run_length: 0,
            mv: [[[0; 2]; 4]; 2],
            blocks_off: 0,
            mb_off: 0,
            bmvtype: 0,
            twomvbp: 0,
            fourmvbp: 0,
            block_index: [0; 6],
            block_wrap: [0; 6],
            dest: [0; 3],
            linesize: 0,
            uvlinesize: 0,
            cur: None,
            last: None,
            next: None,
            last_interlaced: false,
            next_interlaced: false,
            edge: vec![0; EB * 64],
            edge_u: vec![0; EB * 32],
            edge_v: vec![0; EB * 32],
            apply_range_map: true,
        }
    }

    /// (Re)allocate all per-dimension state.
    pub fn alloc(&mut self) {
        let lay = Layout::new(self.mb_width, self.mb_height);
        self.lay = lay;
        let n = lay.total;
        self.dc_val = vec![0; n];
        self.ac_val = vec![[0; 16]; n];
        self.coded_block = vec![0; n];
        self.mb_type_b = vec![0; n];
        self.blk_mv_type = vec![0; n];
        for d in 0..2 {
            self.mv_f[d] = vec![0; n];
            self.mv_f_next[d] = vec![0; n];
        }
        let m = lay.mb_stride * (lay.mbh_a + 2) + 2 * PAD;
        self.mv_type_mb_plane = vec![0; m];
        self.direct_mb_plane = vec![0; m];
        self.forward_mb_plane = vec![0; m];
        self.fieldtx_plane = vec![0; m];
        self.acpred_plane = vec![0; m];
        self.over_flags_plane = vec![0; m];
        self.mbskip_table = vec![0; m];
        self.qscale_table = vec![0; m];
        let ms = lay.mb_stride;
        self.cbp = vec![0; 3 * ms + PAD];
        self.ttblk = vec![0; 3 * ms + PAD];
        self.is_intra = vec![0; 3 * ms + PAD];
        self.luma_mv = vec![[0; 2]; 3 * ms + PAD];
        self.n_blks = self.mb_width + 2;
        self.block = vec![0; self.n_blks * 6 * 64];
        self.last = None;
        self.next = None;
        self.cur = None;
        for i in 0..64 {
            let tr = |x: u8| ((x >> 3) | ((x & 7) << 3)) as u8;
            for k in 0..4 {
                self.zz_8x8[k][i] = tr(t::WMV1_SCANTABLE[k * 64 + i]);
            }
            self.zzi_8x8[i] = tr(t::ADV_INTERLACED_8X8_ZZ[i]);
        }
        self.left_blk_sh = 0;
        self.top_blk_sh = 3;
    }

    // ------------------------------------------------------------------ headers

    pub fn parse_sequence_header(&mut self, gb: &mut BitReader) -> R<()> {
        let profile = gb.get(2) as u8;
        if profile != 3 {
            return Err(Error::Unsupported("only Advanced Profile is supported"));
        }
        self.profile = profile;
        self.res_rtm_flag = true;
        self.level = gb.get(3) as u8;
        self.chromaformat = gb.get(2) as u8;
        if self.chromaformat != 1 {
            return inv("only 4:2:0 supported");
        }
        gb.skip(3); // frmrtq_postproc
        gb.skip(5); // bitrtq_postproc
        self.postprocflag = gb.flag();
        self.max_coded_width = ((gb.get(12) + 1) << 1) as usize;
        self.max_coded_height = ((gb.get(12) + 1) << 1) as usize;
        self.broadcast = gb.flag();
        self.interlace = gb.flag();
        self.tfcntrflag = gb.flag();
        self.finterpflag = gb.flag();
        gb.skip(1);
        self.psf = gb.flag();
        if self.psf {
            return Err(Error::Unsupported("progressive segmented frame"));
        }
        self.max_b_frames = 7;
        self.display_width = self.max_coded_width;
        self.display_height = self.max_coded_height;
        self.aspect_num = 0;
        self.aspect_den = 1;
        if gb.flag() {
            let mut w = gb.get(14) + 1;
            let mut h = gb.get(14) + 1;
            self.display_width = w as usize;
            self.display_height = h as usize;
            let mut ar = 0;
            if gb.flag() {
                ar = gb.get(4);
            }
            const PIXEL_ASPECT: [(u32, u32); 16] = [
                (0, 1),
                (1, 1),
                (12, 11),
                (10, 11),
                (16, 11),
                (40, 33),
                (24, 11),
                (20, 11),
                (32, 11),
                (80, 33),
                (18, 11),
                (15, 11),
                (64, 33),
                (160, 99),
                (0, 1),
                (0, 1),
            ];
            if ar != 0 && ar < 14 {
                self.aspect_num = PIXEL_ASPECT[ar as usize].0;
                self.aspect_den = PIXEL_ASPECT[ar as usize].1;
            } else if ar == 15 {
                w = gb.get(8) + 1;
                h = gb.get(8) + 1;
                self.aspect_num = w;
                self.aspect_den = h;
            }
            if gb.flag() {
                if gb.flag() {
                    self.fps_den = 32;
                    self.fps_num = gb.get(16) + 1;
                } else {
                    let nr = gb.get(8) as usize;
                    let dr = gb.get(4) as usize;
                    const FPS_NR: [u32; 7] = [24, 25, 30, 50, 60, 48, 72];
                    const FPS_DR: [u32; 2] = [1000, 1001];
                    if nr > 0 && nr < 8 && dr > 0 && dr < 3 {
                        self.fps_den = FPS_DR[dr - 1];
                        self.fps_num = FPS_NR[nr - 1] * 1000;
                    }
                }
            }
            if gb.flag() {
                self.color_prim = gb.get(8) as u8;
                self.transfer_char = gb.get(8) as u8;
                self.matrix_coef = gb.get(8) as u8;
            }
        }
        self.hrd_param_flag = gb.flag();
        if self.hrd_param_flag {
            self.hrd_num_leaky_buckets = gb.get(5);
            gb.skip(8);
            for _ in 0..self.hrd_num_leaky_buckets {
                gb.skip(32);
            }
        }
        self.have_seq = true;
        Ok(())
    }

    pub fn parse_entry_point(&mut self, gb: &mut BitReader) -> R<()> {
        if !self.have_seq {
            return Err(Error::NoSequenceHeader);
        }
        self.broken_link = gb.flag();
        self.closed_entry = gb.flag();
        self.panscanflag = gb.flag();
        self.refdist_flag = gb.flag();
        self.loop_filter = gb.flag();
        self.fastuvmc = gb.flag();
        self.extended_mv = gb.flag();
        self.dquant = gb.get(2) as u8;
        self.vstransform = gb.flag();
        self.overlap = gb.flag();
        self.quantizer_mode = gb.get(2) as u8;
        if self.hrd_param_flag {
            for _ in 0..self.hrd_num_leaky_buckets {
                gb.skip(8);
            }
        }
        let (w, h);
        if gb.flag() {
            w = ((gb.get(12) + 1) << 1) as usize;
            h = ((gb.get(12) + 1) << 1) as usize;
        } else {
            w = self.max_coded_width;
            h = self.max_coded_height;
        }
        if w < 2 || h < 2 || w > 8192 || h > 8192 {
            return inv("bad dimensions");
        }
        if w * h > MAX_PIXELS {
            return Err(Error::Unsupported("picture too large"));
        }
        if self.extended_mv {
            self.extended_dmv = gb.flag();
        }
        self.range_mapy_flag = gb.flag();
        if self.range_mapy_flag {
            self.range_mapy = gb.get(3) as u8;
        }
        self.range_mapuv_flag = gb.flag();
        if self.range_mapuv_flag {
            self.range_mapuv = gb.get(3) as u8;
        }
        if w != self.width || h != self.height || self.mb_width == 0 {
            self.width = w;
            self.height = h;
            self.mb_width = (w + 15) >> 4;
            self.mb_height = (h + 15) >> 4;
            self.alloc();
        }
        self.have_ep = true;
        Ok(())
    }

    fn init_lut(lumscale: u8, lumshift: u8, luty: &mut [u8; 256], lutuv: &mut [u8; 256], chain: bool) {
        let (lumscale, lumshift) = (lumscale as i32, lumshift as i32);
        let (scale, mut shift);
        if lumscale == 0 {
            scale = -64;
            shift = (255 - lumshift * 2) * 64;
            if lumshift > 31 {
                shift += 128 << 6;
            }
        } else {
            scale = lumscale + 32;
            if lumshift > 31 {
                shift = (lumshift - 64) * 64;
            } else {
                shift = lumshift << 6;
            }
        }
        for i in 0..256 {
            let iy = if chain { luty[i] as i32 } else { i as i32 };
            let iu = if chain { lutuv[i] as i32 } else { i as i32 };
            luty[i] = clip_u8((scale * iy + shift + 32) >> 6);
            lutuv[i] = clip_u8((scale * (iu - 128) + 128 * 64 + 32) >> 6);
        }
    }

    pub fn lut_set(&mut self, which: usize, plane: usize, lumscale: u8, lumshift: u8, chain: bool) {
        let l = &mut self.luts[which];
        let (y, uv) = (&mut l.y[plane], &mut l.uv[plane]);
        Self::init_lut(lumscale, lumshift, y, uv, chain);
    }

    fn rotate_luts(&mut self) {
        if self.pict_type == PT_BI || self.pict_type == PT_B {
            self.curr = AUX;
        } else {
            self.luts.swap(LAST, NEXT);
            self.curr = NEXT;
        }
        let c = self.curr;
        self.lut_set(c, 0, 32, 0, false);
        self.lut_set(c, 1, 32, 0, false);
        self.luts[c].use_ic = false;
    }

    fn read_bfraction(&mut self, gb: &mut BitReader) -> R<()> {
        let mut idx = gb.get(3) as usize;
        if idx == 7 {
            idx = 7 + gb.get(4) as usize;
        }
        if idx == 21 || idx > 22 {
            return inv("bfraction invalid");
        }
        self.bfraction_lut_index = idx;
        self.bfraction = BFRACTION_LUT[idx] as i32;
        Ok(())
    }

    fn vop_dquant_decoding(&mut self, gb: &mut BitReader) {
        if self.dquant != 2 {
            self.dquantfrm = gb.flag();
            if !self.dquantfrm {
                return;
            }
            self.dqprofile = gb.get(2) as u8;
            match self.dqprofile {
                DQPROFILE_SINGLE_EDGE | DQPROFILE_DOUBLE_EDGES => {
                    self.dqsbedge = gb.get(2) as u8;
                }
                DQPROFILE_ALL_MBS => {
                    self.dqbilevel = gb.flag();
                    if !self.dqbilevel {
                        self.halfpq = 0;
                        return;
                    }
                }
                _ => {}
            }
        }
        let pqdiff = gb.get(3) as i32;
        if pqdiff == 7 {
            self.altpq = gb.get(5) as i32;
        } else {
            self.altpq = self.pq + pqdiff + 1;
        }
    }

    // ------------------------------------------------------------------ bitplanes

    /// Decode a bitplane into `plane` (stride mb_stride). Returns Ok(raw) where raw = true when
    /// the plane is coded per macroblock.
    pub fn bitplane_decoding(&mut self, gb: &mut BitReader, which: u8) -> R<bool> {
        let vl = crate::vlcs::get();
        let width = self.mb_width;
        let height = self.mb_height >> (self.field_mode as usize);
        let stride = self.lay.mb_stride;
        let invert = gb.get1() as u8;
        let imode = gb.get_vlc(&vl.imode);
        if imode < 0 {
            return inv("bad imode");
        }
        // take the plane out to avoid aliasing
        let mut data = std::mem::take(match which {
            0 => &mut self.mv_type_mb_plane,
            1 => &mut self.direct_mb_plane,
            2 => &mut self.forward_mb_plane,
            3 => &mut self.fieldtx_plane,
            4 => &mut self.acpred_plane,
            5 => &mut self.over_flags_plane,
            _ => &mut self.mbskip_table,
        });
        let r = Self::bitplane_inner(gb, &mut data, width, height, stride, invert, imode as u32);
        *match which {
            0 => &mut self.mv_type_mb_plane,
            1 => &mut self.direct_mb_plane,
            2 => &mut self.forward_mb_plane,
            3 => &mut self.fieldtx_plane,
            4 => &mut self.acpred_plane,
            5 => &mut self.over_flags_plane,
            _ => &mut self.mbskip_table,
        } = data;
        r
    }

    fn rowskip(plane: &mut [u8], base: usize, width: usize, height: usize, stride: usize, gb: &mut BitReader) {
        let mut p = base;
        for _ in 0..height {
            if gb.get1() == 0 {
                for x in 0..width {
                    plane[p + x] = 0;
                }
            } else {
                for x in 0..width {
                    plane[p + x] = gb.get1() as u8;
                }
            }
            p += stride;
        }
    }

    fn colskip(plane: &mut [u8], base: usize, width: usize, height: usize, stride: usize, gb: &mut BitReader) {
        for x in 0..width {
            if gb.get1() == 0 {
                for y in 0..height {
                    plane[base + y * stride + x] = 0;
                }
            } else {
                for y in 0..height {
                    plane[base + y * stride + x] = gb.get1() as u8;
                }
            }
        }
    }

    fn bitplane_inner(
        gb: &mut BitReader,
        data: &mut [u8],
        width: usize,
        height: usize,
        stride: usize,
        invert: u8,
        imode: u32,
    ) -> R<bool> {
        let vl = crate::vlcs::get();
        if data.len() < stride * height + 1 {
            return inv("plane too small");
        }
        match imode {
            0 => return Ok(true), // IMODE_RAW
            1 | 2 => {
                let mut p = 0usize;
                let (mut y, mut offset);
                if (height * width) & 1 != 0 {
                    data[p] = gb.get1() as u8;
                    p += 1;
                    y = 1;
                    offset = 1;
                    if offset == width {
                        offset = 0;
                        p += stride - width;
                    }
                } else {
                    y = 0;
                    offset = 0;
                }
                while y < height * width {
                    let code = gb.get_vlc(&vl.norm2);
                    if code < 0 {
                        return inv("norm2");
                    }
                    data[p] = (code & 1) as u8;
                    p += 1;
                    offset += 1;
                    if offset == width {
                        offset = 0;
                        p += stride - width;
                    }
                    data[p] = (code >> 1) as u8;
                    p += 1;
                    offset += 1;
                    if offset == width {
                        offset = 0;
                        p += stride - width;
                    }
                    y += 2;
                }
            }
            3 | 4 => {
                if height % 3 == 0 && width % 3 != 0 {
                    let mut p = 0usize;
                    let mut y = 0;
                    while y < height {
                        let mut x = width & 1;
                        while x < width {
                            let code = gb.get_vlc(&vl.norm6);
                            if code < 0 {
                                return inv("norm6");
                            }
                            let c = code as u32;
                            data[p + x] = (c & 1) as u8;
                            data[p + x + 1] = ((c >> 1) & 1) as u8;
                            data[p + x + stride] = ((c >> 2) & 1) as u8;
                            data[p + x + 1 + stride] = ((c >> 3) & 1) as u8;
                            data[p + x + stride * 2] = ((c >> 4) & 1) as u8;
                            data[p + x + 1 + stride * 2] = ((c >> 5) & 1) as u8;
                            x += 2;
                        }
                        p += stride * 3;
                        y += 3;
                    }
                    if width & 1 != 0 {
                        Self::colskip(data, 0, 1, height, stride, gb);
                    }
                } else {
                    let mut p = (height & 1) * stride;
                    let mut y = height & 1;
                    while y < height {
                        let mut x = width % 3;
                        while x < width {
                            let code = gb.get_vlc(&vl.norm6);
                            if code < 0 {
                                return inv("norm6");
                            }
                            let c = code as u32;
                            data[p + x] = (c & 1) as u8;
                            data[p + x + 1] = ((c >> 1) & 1) as u8;
                            data[p + x + 2] = ((c >> 2) & 1) as u8;
                            data[p + x + stride] = ((c >> 3) & 1) as u8;
                            data[p + x + 1 + stride] = ((c >> 4) & 1) as u8;
                            data[p + x + 2 + stride] = ((c >> 5) & 1) as u8;
                            x += 3;
                        }
                        p += stride * 2;
                        y += 2;
                    }
                    let x = width % 3;
                    if x != 0 {
                        Self::colskip(data, 0, x, height, stride, gb);
                    }
                    if height & 1 != 0 {
                        Self::rowskip(data, x, width - x, 1, stride, gb);
                    }
                }
            }
            5 => Self::rowskip(data, 0, width, height, stride, gb),
            6 => Self::colskip(data, 0, width, height, stride, gb),
            _ => {}
        }
        if imode == 2 || imode == 4 {
            data[0] ^= invert;
            for x in 1..width {
                data[x] ^= data[x - 1];
            }
            for y in 1..height {
                let p = y * stride;
                data[p] ^= data[p - stride];
                for x in 1..width {
                    if data[p + x - 1] != data[p + x - stride] {
                        data[p + x] ^= invert;
                    } else {
                        data[p + x] ^= data[p + x - 1];
                    }
                }
            }
        } else if invert != 0 {
            for y in 0..height {
                for x in 0..width {
                    let v = &mut data[y * stride + x];
                    *v = (*v == 0) as u8;
                }
            }
        }
        Ok(false)
    }

    fn pq_common(&mut self, gb: &mut BitReader) -> R<()> {
        let pqindex = gb.get(5) as usize;
        if pqindex == 0 {
            return inv("pqindex 0");
        }
        if self.quantizer_mode == QUANT_FRAME_IMPLICIT {
            self.pq = t::PQUANT_TABLE[pqindex] as i32;
        } else {
            self.pq = t::PQUANT_TABLE[32 + pqindex] as i32;
        }
        self.pqindex = pqindex as i32;
        self.halfpq = if pqindex < 9 { gb.get1() as i32 } else { 0 };
        self.pquantizer = match self.quantizer_mode {
            QUANT_FRAME_IMPLICIT => pqindex < 9,
            QUANT_NON_UNIFORM => false,
            QUANT_FRAME_EXPLICIT => gb.flag(),
            _ => true,
        };
        self.dquantfrm = false;
        if self.postprocflag {
            self.postproc = gb.get(2) as u8;
        }
        Ok(())
    }

    fn ttmb_hdr(&mut self, gb: &mut BitReader) {
        if self.vstransform {
            self.ttmbf = gb.flag();
            if self.ttmbf {
                self.ttfrm = [TT_8X8, TT_8X4, TT_4X8, TT_4X4][gb.get(2) as usize];
            } else {
                self.ttfrm = 0;
            }
        } else {
            self.ttmbf = true;
            self.ttfrm = TT_8X8;
        }
    }

    fn set_mv_range(&mut self) {
        self.k_x = self.mvrange as i32 + 9 + (self.mvrange as i32 >> 1);
        self.k_y = self.mvrange as i32 + 8;
        self.range_x = 1 << (self.k_x - 1);
        self.range_y = 1 << (self.k_y - 1);
    }

    /// Advanced profile picture (frame or field) header. Mirrors ff_vc1_parse_frame_header_adv.
    pub fn parse_frame_header_adv(&mut self, gb: &mut BitReader) -> R<()> {
        let vl = crate::vlcs::get();
        self.numref = 0;
        self.p_frame_skipped = false;
        let mut goto_common = false;
        if self.second_field {
            if self.fcm != ILACE_FIELD || !self.field_mode {
                return inv("second field in non field picture");
            }
            if self.fptype & 4 != 0 {
                self.pict_type = if self.fptype & 1 != 0 { PT_BI } else { PT_B };
            } else {
                self.pict_type = if self.fptype & 1 != 0 { PT_P } else { PT_I };
            }
            if let Some(c) = self.cur.as_mut() {
                c.pict_type = self.pict_type;
            }
            if !self.pic_header_flag {
                goto_common = true;
            }
        }
        if !goto_common {
            let mut field_mode = false;
            let fcm;
            if self.interlace {
                fcm = gb.decode012() as u8;
                if fcm == ILACE_FIELD {
                    field_mode = true;
                }
            } else {
                fcm = PROGRESSIVE;
            }
            if !self.first_pic_header_flag && self.field_mode != field_mode {
                return inv("field mode changed");
            }
            self.field_mode = field_mode;
            self.fcm = fcm;
            if self.field_mode {
                self.mb_height = (self.height + 15) >> 4;
                self.mb_height = (self.mb_height + 1) & !1;
                self.fptype = gb.get(3);
                if self.fptype & 4 != 0 {
                    self.pict_type = if self.fptype & 2 != 0 { PT_BI } else { PT_B };
                } else {
                    self.pict_type = if self.fptype & 2 != 0 { PT_P } else { PT_I };
                }
            } else {
                self.mb_height = (self.height + 15) >> 4;
                match gb.unary(0, 4) {
                    0 => self.pict_type = PT_P,
                    1 => self.pict_type = PT_B,
                    2 => self.pict_type = PT_I,
                    3 => self.pict_type = PT_BI,
                    _ => {
                        self.pict_type = PT_P;
                        self.p_frame_skipped = true;
                    }
                }
            }
            if self.tfcntrflag {
                gb.skip(8);
            }
            // pull-down state belongs to one picture; a stale RFF or RPTFRM would stretch every later frame
            self.rff = false;
            self.rptfrm = 0;
            if self.broadcast {
                if !self.interlace || self.psf {
                    self.rptfrm = gb.get(2) as u8;
                } else {
                    self.tff = gb.flag();
                    self.rff = gb.flag();
                }
            } else {
                self.tff = true;
            }
            if self.panscanflag {
                return Err(Error::Unsupported("pan-scan"));
            }
            if self.p_frame_skipped {
                return Ok(());
            }
            self.rnd = gb.get1() as i32;
            if self.interlace {
                self.uvsamp = gb.flag();
            }
            if self.field_mode {
                if !self.refdist_flag {
                    self.refdist = 0;
                } else if self.pict_type != PT_B && self.pict_type != PT_BI {
                    self.refdist = gb.get(2) as i32;
                    if self.refdist == 3 {
                        self.refdist += gb.unary(0, 14) as i32;
                    }
                    if self.refdist > 16 {
                        return inv("refdist");
                    }
                }
                if self.pict_type == PT_B || self.pict_type == PT_BI {
                    self.read_bfraction(gb)?;
                    self.frfd = (self.bfraction * self.refdist) >> 8;
                    self.brfd = self.refdist - self.frfd - 1;
                    if self.brfd < 0 {
                        self.brfd = 0;
                    }
                }
                goto_common = true;
            }
            if !goto_common && self.fcm == PROGRESSIVE {
                if self.finterpflag {
                    self.interpfrm = gb.flag();
                }
                if self.pict_type == PT_B {
                    self.read_bfraction(gb)?;
                    if self.bfraction == 0 {
                        self.pict_type = PT_BI;
                    }
                }
            }
        }
        // parse_common_info
        if self.field_mode {
            self.cur_field_type = (!(self.tff ^ self.second_field)) as usize;
        }
        self.pq_common(gb)?;

        if self.first_pic_header_flag {
            self.rotate_luts();
        }

        match self.pict_type {
            PT_I | PT_BI => {
                if self.fcm == ILACE_FRAME {
                    self.fieldtx_is_raw = self.bitplane_decoding(gb, 3)?;
                } else {
                    self.fieldtx_is_raw = false;
                }
                self.acpred_is_raw = self.bitplane_decoding(gb, 4)?;
                self.condover = CONDOVER_NONE;
                if self.overlap && self.pq <= 8 {
                    self.condover = gb.decode012() as u8;
                    if self.condover == CONDOVER_SELECT {
                        self.overflg_is_raw = self.bitplane_decoding(gb, 5)?;
                    }
                }
            }
            PT_P => {
                if self.field_mode {
                    self.numref = gb.get1();
                    if self.numref == 0 {
                        self.reffield = gb.get1();
                        self.ref_field_type[0] = (self.reffield ^ (!self.cur_field_type as u32 & 1)) as usize;
                    }
                }
                if self.extended_mv {
                    self.mvrange = gb.unary(0, 3);
                } else {
                    self.mvrange = 0;
                }
                if self.interlace {
                    if self.extended_dmv {
                        self.dmvrange = gb.unary(0, 3);
                    } else {
                        self.dmvrange = 0;
                    }
                    if self.fcm == ILACE_FRAME {
                        self.fourmvswitch = gb.flag();
                        self.intcomp = gb.flag();
                        if self.intcomp {
                            self.lumscale = gb.get(6) as u8;
                            self.lumshift = gb.get(6) as u8;
                            let (s, h) = (self.lumscale, self.lumshift);
                            self.lut_set(LAST, 0, s, h, true);
                            self.lut_set(LAST, 1, s, h, true);
                            self.luts[LAST].use_ic = true;
                        }
                        self.skip_is_raw = self.bitplane_decoding(gb, 6)?;
                        self.mbmodetab = gb.get(2) as usize;
                        self.imvtab = gb.get(2) as usize;
                        self.icbptab = gb.get(3) as usize;
                        self.twomvbptab = gb.get(2) as usize;
                        if self.fourmvswitch {
                            self.fourmvbptab = gb.get(2) as usize;
                        }
                    }
                }
                self.set_mv_range();
                self.tt_index = (self.pq > 4) as usize + (self.pq > 12) as usize;
                if self.fcm != ILACE_FRAME {
                    let mvmode = gb.unary(1, 4) as usize;
                    let lowquant = if self.pq > 12 { 0 } else { 1 };
                    self.mv_mode = PMODE_TABLE[lowquant][mvmode];
                    if self.mv_mode == MV_PMODE_INTENSITY_COMP {
                        let mvmode2 = gb.unary(1, 3) as usize;
                        self.mv_mode2 = PMODE_TABLE2[lowquant][mvmode2];
                        if self.field_mode {
                            self.intcompfield = gb.decode210() ^ 3;
                        } else {
                            self.intcompfield = 3;
                        }
                        self.lumscale = 32;
                        self.lumscale2 = 32;
                        self.lumshift = 0;
                        self.lumshift2 = 0;
                        if self.intcompfield & 1 != 0 {
                            self.lumscale = gb.get(6) as u8;
                            self.lumshift = gb.get(6) as u8;
                        }
                        if self.intcompfield & 2 != 0 && self.field_mode {
                            self.lumscale2 = gb.get(6) as u8;
                            self.lumshift2 = gb.get(6) as u8;
                        } else if !self.field_mode {
                            self.lumscale2 = self.lumscale;
                            self.lumshift2 = self.lumshift;
                        }
                        let (ls, lh, ls2, lh2) = (self.lumscale, self.lumshift, self.lumscale2, self.lumshift2);
                        if self.field_mode && self.second_field {
                            let cft = self.cur_field_type;
                            let cu = self.curr;
                            if cft != 0 {
                                self.lut_set(cu, cft ^ 1, ls, lh, false);
                                self.lut_set(LAST, cft, ls2, lh2, true);
                            } else {
                                self.lut_set(cu, cft ^ 1, ls2, lh2, false);
                                self.lut_set(LAST, cft, ls, lh, true);
                            }
                            self.luts[NEXT].use_ic = true;
                            let cu = self.curr;
                            self.luts[cu].use_ic = true;
                        } else {
                            self.lut_set(LAST, 0, ls, lh, true);
                            self.lut_set(LAST, 1, ls2, lh2, true);
                        }
                        self.luts[LAST].use_ic = true;
                    }
                    self.qs_last = self.quarter_sample;
                    if self.mv_mode == MV_PMODE_INTENSITY_COMP {
                        self.quarter_sample =
                            self.mv_mode2 != MV_PMODE_1MV_HPEL && self.mv_mode2 != MV_PMODE_1MV_HPEL_BILIN;
                        self.mspel = self.mv_mode2 != MV_PMODE_1MV_HPEL_BILIN;
                    } else {
                        self.quarter_sample =
                            self.mv_mode != MV_PMODE_1MV_HPEL && self.mv_mode != MV_PMODE_1MV_HPEL_BILIN;
                        self.mspel = self.mv_mode != MV_PMODE_1MV_HPEL_BILIN;
                    }
                }
                if self.fcm == PROGRESSIVE {
                    if (self.mv_mode == MV_PMODE_INTENSITY_COMP && self.mv_mode2 == MV_PMODE_MIXED_MV)
                        || self.mv_mode == MV_PMODE_MIXED_MV
                    {
                        self.mv_type_is_raw = self.bitplane_decoding(gb, 0)?;
                    } else {
                        self.mv_type_is_raw = false;
                        let n = self.lay.mb_stride * self.mb_height;
                        for v in self.mv_type_mb_plane[..n].iter_mut() {
                            *v = 0;
                        }
                    }
                    self.skip_is_raw = self.bitplane_decoding(gb, 6)?;
                    self.mv_table_index = gb.get(2) as usize;
                    self.cbptab = gb.get(2) as usize;
                } else if self.fcm == ILACE_FRAME {
                    self.qs_last = self.quarter_sample;
                    self.quarter_sample = true;
                    self.mspel = true;
                } else {
                    self.mbmodetab = gb.get(3) as usize;
                    self.imvtab = gb.get(2 + self.numref) as usize;
                    self.icbptab = gb.get(3) as usize;
                    if (self.mv_mode == MV_PMODE_INTENSITY_COMP && self.mv_mode2 == MV_PMODE_MIXED_MV)
                        || self.mv_mode == MV_PMODE_MIXED_MV
                    {
                        self.fourmvbptab = gb.get(2) as usize;
                    }
                }
                if self.dquant != 0 {
                    self.vop_dquant_decoding(gb);
                }
                self.ttmb_hdr(gb);
            }
            PT_B => {
                if self.fcm == ILACE_FRAME {
                    self.read_bfraction(gb)?;
                    if self.bfraction == 0 {
                        return inv("bfraction 0 in frame B");
                    }
                }
                if self.extended_mv {
                    self.mvrange = gb.unary(0, 3);
                } else {
                    self.mvrange = 0;
                }
                self.set_mv_range();
                self.tt_index = (self.pq > 4) as usize + (self.pq > 12) as usize;
                if self.field_mode {
                    if self.extended_dmv {
                        self.dmvrange = gb.unary(0, 3);
                    }
                    let mvmode = gb.unary(1, 3) as usize;
                    let lowquant = if self.pq > 12 { 0 } else { 1 };
                    self.mv_mode = PMODE_TABLE2[lowquant][mvmode];
                    self.qs_last = self.quarter_sample;
                    self.quarter_sample = self.mv_mode == MV_PMODE_1MV || self.mv_mode == MV_PMODE_MIXED_MV;
                    self.mspel = self.mv_mode != MV_PMODE_1MV_HPEL_BILIN;
                    self.fmb_is_raw = self.bitplane_decoding(gb, 2)?;
                    self.mbmodetab = gb.get(3) as usize;
                    self.imvtab = gb.get(3) as usize;
                    self.icbptab = gb.get(3) as usize;
                    if self.mv_mode == MV_PMODE_MIXED_MV {
                        self.fourmvbptab = gb.get(2) as usize;
                    }
                    self.numref = 1;
                } else if self.fcm == ILACE_FRAME {
                    if self.extended_dmv {
                        self.dmvrange = gb.unary(0, 3);
                    }
                    let _ = gb.get1(); // intcomp, shall be 0
                    self.intcomp = false;
                    self.mv_mode = MV_PMODE_1MV;
                    self.fourmvswitch = false;
                    self.qs_last = self.quarter_sample;
                    self.quarter_sample = true;
                    self.mspel = true;
                    self.dmb_is_raw = self.bitplane_decoding(gb, 1)?;
                    self.skip_is_raw = self.bitplane_decoding(gb, 6)?;
                    self.mbmodetab = gb.get(2) as usize;
                    self.imvtab = gb.get(2) as usize;
                    self.icbptab = gb.get(3) as usize;
                    self.twomvbptab = gb.get(2) as usize;
                    self.fourmvbptab = gb.get(2) as usize;
                } else {
                    self.mv_mode = if gb.flag() { MV_PMODE_1MV } else { MV_PMODE_1MV_HPEL_BILIN };
                    self.qs_last = self.quarter_sample;
                    self.quarter_sample = self.mv_mode == MV_PMODE_1MV;
                    self.mspel = self.quarter_sample;
                    self.dmb_is_raw = self.bitplane_decoding(gb, 1)?;
                    self.skip_is_raw = self.bitplane_decoding(gb, 6)?;
                    self.mv_table_index = gb.get(2) as usize;
                    self.cbptab = gb.get(2) as usize;
                }
                if self.dquant != 0 {
                    self.vop_dquant_decoding(gb);
                }
                self.ttmb_hdr(gb);
            }
            _ => {}
        }
        let _ = vl;

        // AC syntax
        self.c_ac_table_index = gb.decode012();
        if self.pict_type == PT_I || self.pict_type == PT_BI {
            self.y_ac_table_index = gb.decode012();
        } else if self.fcm != PROGRESSIVE && !self.quarter_sample {
            self.range_x <<= 1;
            self.range_y <<= 1;
        }
        // DC syntax
        self.dc_table_index = gb.get1() as usize;
        if (self.pict_type == PT_I || self.pict_type == PT_BI) && self.dquant != 0 {
            self.vop_dquant_decoding(gb);
        }
        self.bi_type = self.pict_type == PT_BI;
        if self.bi_type {
            self.pict_type = PT_B;
        }
        Ok(())
    }
}

pub const PMODE_TABLE: [[u8; 5]; 2] = [
    [MV_PMODE_1MV_HPEL_BILIN, MV_PMODE_1MV, MV_PMODE_1MV_HPEL, MV_PMODE_INTENSITY_COMP, MV_PMODE_MIXED_MV],
    [MV_PMODE_1MV, MV_PMODE_MIXED_MV, MV_PMODE_1MV_HPEL, MV_PMODE_INTENSITY_COMP, MV_PMODE_1MV_HPEL_BILIN],
];
pub const PMODE_TABLE2: [[u8; 4]; 2] = [
    [MV_PMODE_1MV_HPEL_BILIN, MV_PMODE_1MV, MV_PMODE_1MV_HPEL, MV_PMODE_MIXED_MV],
    [MV_PMODE_1MV, MV_PMODE_MIXED_MV, MV_PMODE_1MV_HPEL, MV_PMODE_1MV_HPEL_BILIN],
];

#[cfg(test)]
mod pulldown_tests {
    use super::*;

    /// Skipped-P picture header (the parse stops right after the pull-down bits): optional FCM, picture type 1111, then the flags.
    fn header(interlace: bool, psf: bool, flag_bits: &[u8]) -> Core {
        let mut bits: Vec<u8> = Vec::new();
        if interlace {
            bits.push(0); // FCM progressive frame
        }
        bits.extend_from_slice(&[1, 1, 1, 1]); // unary 4: skipped P
        bits.extend_from_slice(flag_bits);
        while bits.len() % 8 != 0 {
            bits.push(0);
        }
        let bytes: Vec<u8> = bits.chunks(8).map(|c| c.iter().fold(0u8, |a, b| (a << 1) | b)).collect();
        let mut c = Core::new();
        c.interlace = interlace;
        c.psf = psf;
        c.broadcast = true;
        c.width = 64;
        c.height = 64;
        c.parse_frame_header_adv(&mut BitReader::new(&bytes)).expect("header");
        c
    }

    #[test]
    fn rptfrm_progressive() {
        for n in 0..4u8 {
            let c = header(false, false, &[n >> 1, n & 1]);
            assert_eq!((c.rptfrm, c.rff), (n, false));
        }
        // PSF interlaced sequence signals RPTFRM too
        assert_eq!(header(true, true, &[1, 0]).rptfrm, 2);
    }

    #[test]
    fn tff_rff_interlaced() {
        let c = header(true, false, &[1, 1]);
        assert_eq!((c.tff, c.rff, c.rptfrm), (true, true, 0));
        let c = header(true, false, &[0, 1]);
        assert_eq!((c.tff, c.rff), (false, true));
        let c = header(true, false, &[0, 0]);
        assert_eq!((c.tff, c.rff), (false, false));
    }
}
