//! Interlaced field / frame picture macroblock decoding and filters
//! (FFmpeg vc1_block.c, vc1_pred.c, vc1_loopfilter.c).
use crate::bits::BitReader;
use crate::blocks::BLOCK_MAP;
use crate::core::*;
use crate::dsp;
use crate::pred::scale_mv;
use crate::tables as t;

const OFFSET_TABLE: [[i32; 9]; 2] = [[0, 1, 2, 4, 8, 16, 32, 64, 128], [0, 1, 3, 7, 15, 31, 63, 127, 255]];

const INTFR_1MV: u8 = 0;
const INTFR_2MV_FIELD: u8 = 1;
const INTFR_4MV_FIELD: u8 = 3;
const INTFR_4MV: u8 = 4;
const INTFR_INTRA: u8 = 5;

// (type, FIELDTX, 1-MV differential present, residual CBP present)
const MBMODE_INTFRP_NON4MV: [[u8; 4]; 9] = [
    [INTFR_1MV, 0, 1, 1],
    [INTFR_1MV, 1, 1, 1],
    [INTFR_1MV, 0, 1, 0],
    [INTFR_1MV, 0, 0, 1],
    [INTFR_1MV, 1, 0, 1],
    [INTFR_2MV_FIELD, 0, 0, 1],
    [INTFR_2MV_FIELD, 1, 0, 1],
    [INTFR_2MV_FIELD, 1, 0, 0],
    [INTFR_INTRA, 0, 0, 0],
];
const MBMODE_INTFRP_4MV: [[u8; 4]; 15] = [
    [INTFR_1MV, 0, 1, 1],
    [INTFR_1MV, 1, 1, 1],
    [INTFR_1MV, 0, 1, 0],
    [INTFR_1MV, 0, 0, 1],
    [INTFR_1MV, 1, 0, 1],
    [INTFR_2MV_FIELD, 0, 0, 1],
    [INTFR_2MV_FIELD, 1, 0, 1],
    [INTFR_2MV_FIELD, 1, 0, 0],
    [INTFR_4MV, 0, 0, 1],
    [INTFR_4MV, 1, 0, 1],
    [INTFR_4MV, 0, 0, 0],
    [INTFR_4MV_FIELD, 0, 0, 1],
    [INTFR_4MV_FIELD, 1, 0, 1],
    [INTFR_4MV_FIELD, 1, 0, 0],
    [INTFR_INTRA, 0, 0, 0],
];

const LEFT_EDGE: u32 = 1;
const RIGHT_EDGE: u32 = 2;
const TOP_EDGE: u32 = 4;
const BOTTOM_EDGE: u32 = 8;

impl Core {
    fn mbmode_row(&self, idx: usize, four: bool) -> [u8; 4] {
        if four {
            MBMODE_INTFRP_4MV[idx.min(14)]
        } else {
            MBMODE_INTFRP_NON4MV[idx.min(8)]
        }
    }

    /// get_mvdata_interlaced
    pub fn get_mvdata_interlaced(&mut self, gb: &mut BitReader, pred_flag: Option<&mut i32>) -> R<(i32, i32)> {
        let vl = self.vl;
        let esc: i32 = if self.numref != 0 { 125 } else { 71 };
        let extend_x = (self.dmvrange & 1) as usize;
        let extend_y = ((self.dmvrange >> 1) & 1) as usize;
        let numref = self.numref as i32;
        let table = if self.fcm == ILACE_FRAME || self.numref == 0 {
            &vl.ref1_mvdata[self.imvtab]
        } else {
            &vl.ref2_mvdata[self.imvtab]
        };
        let index = gb.get_vlc(table);
        if index < 0 || index > esc {
            return inv("mvdata vlc");
        }
        let (dx, mut dy);
        if index == esc {
            dx = gb.get(self.k_x as u32) as i32;
            dy = gb.get(self.k_y as u32) as i32;
            if self.numref != 0 {
                if let Some(pf) = pred_flag {
                    *pf = dy & 1;
                }
                dy = (dy + (dy & 1)) >> 1;
            }
        } else {
            let mut index1 = ((index + 1) % 9) as usize;
            if index1 != 0 {
                let val = gb.get((index1 + extend_x) as u32) as i32;
                let sign = 0 - (val & 1);
                dx = (sign ^ ((val >> 1) + OFFSET_TABLE[extend_x][index1])) - sign;
            } else {
                dx = 0;
            }
            index1 = ((index + 1) / 9) as usize;
            if index1 as i32 > numref {
                let val = gb.get(((index1 >> numref) + extend_y) as u32) as i32;
                let sign = 0 - (val & 1);
                dy = (sign ^ ((val >> 1) + OFFSET_TABLE[extend_y][index1 >> numref])) - sign;
            } else {
                dy = 0;
            }
            if numref != 0 {
                if let Some(pf) = pred_flag {
                    *pf = (index1 & 1) as i32;
                }
            }
        }
        Ok((dx, dy))
    }

    fn set_blk_mv_type(&mut self, v: u8) {
        for k in 0..4 {
            let b = self.block_index[k];
            self.blk_mv_type[b] = v;
        }
    }

    fn mbmode_vlc_p_field(&self) -> usize {
        // 1 when mixed-MV tables apply
        (self.mv_mode == MV_PMODE_MIXED_MV || (self.mv_mode == MV_PMODE_INTENSITY_COMP && self.mv_mode2 == MV_PMODE_MIXED_MV)) as usize
    }

    // ---------------------------------------------------------------- P field picture MB

    pub fn decode_p_mb_intfi(&mut self, gb: &mut BitReader) -> R<()> {
        let vl = self.vl;
        let mb_pos = self.mb_x + self.mb_y * self.lay.mb_stride;
        let mut cbp: i32 = 0;
        let mut mquant = self.pq;
        let mut ttmb = self.ttfrm;
        let mut first_block = true;
        let mut block_cbp: u32 = 0;
        let mut block_tt: i32 = 0;
        let ci = self.cur_blk_idx;
        let ls = self.linesize;
        let uvls = self.uvlinesize;
        let bo = self.blocks_off;
        let mo = self.mb_off;
        let ms = self.lay.mb_stride;
        let c = if self.mbmode_vlc_p_field() != 0 {
            gb.get_vlc(&vl.if_mmv_mbmode[self.mbmodetab])
        } else {
            gb.get_vlc(&vl.if_1mv_mbmode[self.mbmodetab])
        };
        if c < 0 {
            return inv("mbmode");
        }
        let idx_mbmode = c;
        if idx_mbmode <= 1 {
            self.is_intra[2 * ms + self.mb_x] = 0x3f;
            self.mb_intra = true;
            let b0 = self.block_index[0] + bo;
            self.set_cmv(1, b0, [0, 0]);
            self.cur.as_mut().unwrap().mb_type[mb_pos + mo] = MBT_INTRA;
            self.get_mquant(gb, &mut mquant);
            self.qscale_table[mb_pos] = mquant as i8;
            self.y_dc_scale = t::WMV3_DC_SCALE_TABLE[mquant.unsigned_abs() as usize] as i32;
            self.c_dc_scale = self.y_dc_scale;
            self.ac_pred = gb.get1() != 0;
            self.acpred_plane[mb_pos] = self.ac_pred as u8;
            let mb_has_coeffs = idx_mbmode & 1 != 0;
            if mb_has_coeffs {
                let v = gb.get_vlc(&vl.icbpcy[self.icbptab]);
                if v < 0 {
                    return inv("icbpcy");
                }
                cbp = 1 + v;
            }
            for i in 0..6usize {
                let bi = self.block_index[i];
                self.mb_type_b[bi] = 1;
                self.dc_val[bi] = 0;
                let val = ((cbp >> (5 - i)) & 1) != 0;
                self.intra_avail_pub(i);
                let cs = if i & 4 != 0 { self.codingset2 } else { self.codingset };
                let mut b = [0i16; 64];
                self.decode_intra_block(gb, &mut b, i, val, mquant, cs)?;
                dsp::inv_trans_8x8(&mut b);
                let o = (ci * 6 + BLOCK_MAP[i]) * 64;
                self.block[o..o + 64].copy_from_slice(&b);
                block_cbp |= 0xf << (i << 2);
            }
        } else {
            self.mb_intra = false;
            self.is_intra[2 * ms + self.mb_x] = 0;
            self.cur.as_mut().unwrap().mb_type[mb_pos + mo] = MBT_16X16;
            for i in 0..6 {
                let bi = self.block_index[i];
                self.mb_type_b[bi] = 0;
            }
            let mb_has_coeffs;
            if idx_mbmode <= 5 {
                let (mut dmv_x, mut dmv_y, mut pred_flag) = (0, 0, 0);
                if idx_mbmode & 1 != 0 {
                    let r = self.get_mvdata_interlaced(gb, Some(&mut pred_flag))?;
                    dmv_x = r.0;
                    dmv_y = r.1;
                }
                self.pred_mv(gb, 0, dmv_x, dmv_y, true, self.range_x, self.range_y, pred_flag, 0);
                self.mc_1mv(0);
                mb_has_coeffs = idx_mbmode & 2 == 0;
            } else {
                let v = gb.get_vlc(&vl.fourmv_bp[self.fourmvbptab]);
                if v < 0 {
                    return inv("4mvbp");
                }
                self.fourmvbp = v as u32;
                for i in 0..4usize {
                    let (mut dmv_x, mut dmv_y, mut pred_flag) = (0, 0, 0);
                    if self.fourmvbp & (8 >> i) != 0 {
                        let r = self.get_mvdata_interlaced(gb, Some(&mut pred_flag))?;
                        dmv_x = r.0;
                        dmv_y = r.1;
                    }
                    self.pred_mv(gb, i, dmv_x, dmv_y, false, self.range_x, self.range_y, pred_flag, 0);
                    self.mc_4mv_luma(i, 0, false);
                }
                self.mc_4mv_chroma(0);
                mb_has_coeffs = idx_mbmode & 1 != 0;
            }
            if mb_has_coeffs {
                let v = gb.get_vlc(&vl.icbpcy[self.icbptab]);
                if v < 0 {
                    return inv("icbpcy");
                }
                cbp = 1 + v;
            }
            if cbp != 0 {
                self.get_mquant(gb, &mut mquant);
            }
            self.qscale_table[mb_pos] = mquant as i8;
            if !self.ttmbf && cbp != 0 {
                let v = gb.get_vlc(&vl.ttmb[self.tt_index]);
                if v < 0 {
                    return inv("ttmb");
                }
                ttmb = v;
            }
            let mut dst_idx = 0usize;
            for i in 0..6usize {
                let bi = self.block_index[i];
                self.dc_val[bi] = 0;
                dst_idx += i >> 2;
                let val = ((cbp >> (5 - i)) & 1) != 0;
                let off = if i & 4 != 0 { 0 } else { (i & 1) * 8 + (i & 2) * 4 * ls };
                if val {
                    let mut b = [0i16; 64];
                    let stride = if i & 4 != 0 { uvls } else { ls };
                    let doff = self.dest[dst_idx] + off;
                    let pat = self.decode_p_block(gb, &mut b, i, mquant, ttmb, first_block, dst_idx, doff, stride, &mut block_tt)?;
                    block_cbp |= (pat as u32) << (i << 2);
                    if !self.ttmbf && ttmb < 8 {
                        ttmb = -1;
                    }
                    first_block = false;
                }
            }
        }
        if self.overlap && self.pq >= 9 {
            self.p_overlap_filter();
        }
        self.put_blocks_clamped(true);
        let x = 2 * ms + self.mb_x;
        self.cbp[x] = block_cbp;
        self.ttblk[x] = block_tt;
        Ok(())
    }

    // ---------------------------------------------------------------- P frame picture (interlaced frame) MB

    pub fn decode_p_mb_intfr(&mut self, gb: &mut BitReader) -> R<()> {
        let vl = self.vl;
        let ms = self.lay.mb_stride;
        let mb_pos = self.mb_x + self.mb_y * ms;
        let mut cbp: i32 = 0;
        let mut mquant = self.pq;
        let mut ttmb = self.ttfrm;
        let mut first_block = true;
        let mut block_cbp: u32 = 0;
        let mut block_tt: i32 = 0;
        let ci = self.cur_blk_idx;
        let ls = self.linesize;
        let uvls = self.uvlinesize;
        let skipped = if self.skip_is_raw { gb.get1() != 0 } else { self.mbskip_table[mb_pos] != 0 };
        let isx = 2 * ms + self.mb_x;
        if !skipped {
            let c = if self.fourmvswitch {
                gb.get_vlc(&vl.intfr_4mv_mbmode[self.mbmodetab])
            } else {
                gb.get_vlc(&vl.intfr_non4mv_mbmode[self.mbmodetab])
            };
            if c < 0 {
                return inv("mbmode");
            }
            let row = self.mbmode_row(c as usize, self.fourmvswitch);
            let (mut fourmv, mut twomv) = (false, false);
            match row[0] {
                INTFR_4MV => {
                    fourmv = true;
                    self.set_blk_mv_type(0);
                }
                INTFR_4MV_FIELD => {
                    fourmv = true;
                    self.set_blk_mv_type(1);
                }
                INTFR_2MV_FIELD => {
                    twomv = true;
                    self.set_blk_mv_type(1);
                }
                INTFR_1MV => self.set_blk_mv_type(0),
                _ => {}
            }
            if row[0] == INTFR_INTRA {
                for i in 0..4 {
                    let b = self.block_index[i];
                    self.set_cmv(1, b, [0, 0]);
                }
                self.is_intra[isx] = 0x3f;
                self.mb_intra = true;
                self.cur.as_mut().unwrap().mb_type[mb_pos] = MBT_INTRA;
                let fieldtx = gb.get1() as u8;
                self.fieldtx_plane[mb_pos] = fieldtx;
                let mb_has_coeffs = gb.get1() != 0;
                if mb_has_coeffs {
                    let v = gb.get_vlc(&vl.icbpcy[self.icbptab]);
                    if v < 0 {
                        return inv("icbpcy");
                    }
                    cbp = 1 + v;
                }
                self.ac_pred = gb.get1() != 0;
                self.acpred_plane[mb_pos] = self.ac_pred as u8;
                self.get_mquant(gb, &mut mquant);
                self.qscale_table[mb_pos] = mquant as i8;
                self.y_dc_scale = t::WMV3_DC_SCALE_TABLE[mquant.unsigned_abs() as usize] as i32;
                self.c_dc_scale = self.y_dc_scale;
                for i in 0..6usize {
                    let bi = self.block_index[i];
                    self.mb_type_b[bi] = 1;
                    self.dc_val[bi] = 0;
                    let val = ((cbp >> (5 - i)) & 1) != 0;
                    self.intra_avail_pub(i);
                    let cs = if i & 4 != 0 { self.codingset2 } else { self.codingset };
                    let mut b = [0i16; 64];
                    self.decode_intra_block(gb, &mut b, i, val, mquant, cs)?;
                    dsp::inv_trans_8x8(&mut b);
                    let o = (ci * 6 + BLOCK_MAP[i]) * 64;
                    self.block[o..o + 64].copy_from_slice(&b);
                    block_cbp |= 0xf << (i << 2);
                }
            } else {
                let mb_has_coeffs = row[3] != 0;
                if mb_has_coeffs {
                    let v = gb.get_vlc(&vl.icbpcy[self.icbptab]);
                    if v < 0 {
                        return inv("icbpcy");
                    }
                    cbp = 1 + v;
                }
                if row[0] == INTFR_2MV_FIELD {
                    let v = gb.get_vlc(&vl.twomv_bp[self.twomvbptab]);
                    if v < 0 {
                        return inv("2mvbp");
                    }
                    self.twomvbp = v as u32;
                } else if row[0] == INTFR_4MV || row[0] == INTFR_4MV_FIELD {
                    let v = gb.get_vlc(&vl.fourmv_bp[self.fourmvbptab]);
                    if v < 0 {
                        return inv("4mvbp");
                    }
                    self.fourmvbp = v as u32;
                }
                self.mb_intra = false;
                self.is_intra[isx] = 0;
                for i in 0..6 {
                    let bi = self.block_index[i];
                    self.mb_type_b[bi] = 0;
                }
                let fieldtx = row[1];
                self.fieldtx_plane[mb_pos] = fieldtx;
                if fourmv {
                    let mvbp = self.fourmvbp;
                    for i in 0..4usize {
                        let (mut dmv_x, mut dmv_y) = (0, 0);
                        if mvbp & (8 >> i) != 0 {
                            let r = self.get_mvdata_interlaced(gb, None)?;
                            dmv_x = r.0;
                            dmv_y = r.1;
                        }
                        self.pred_mv_intfr(i, dmv_x, dmv_y, 0, self.range_x, self.range_y, 0);
                        self.mc_4mv_luma(i, 0, false);
                    }
                    self.mc_4mv_chroma4(0, 0, false);
                } else if twomv {
                    let mvbp = self.twomvbp;
                    let (mut dmv_x, mut dmv_y) = (0, 0);
                    if mvbp & 2 != 0 {
                        let r = self.get_mvdata_interlaced(gb, None)?;
                        dmv_x = r.0;
                        dmv_y = r.1;
                    }
                    self.pred_mv_intfr(0, dmv_x, dmv_y, 2, self.range_x, self.range_y, 0);
                    self.mc_4mv_luma(0, 0, false);
                    self.mc_4mv_luma(1, 0, false);
                    let (mut dmv_x, mut dmv_y) = (0, 0);
                    if mvbp & 1 != 0 {
                        let r = self.get_mvdata_interlaced(gb, None)?;
                        dmv_x = r.0;
                        dmv_y = r.1;
                    }
                    self.pred_mv_intfr(2, dmv_x, dmv_y, 2, self.range_x, self.range_y, 0);
                    self.mc_4mv_luma(2, 0, false);
                    self.mc_4mv_luma(3, 0, false);
                    self.mc_4mv_chroma4(0, 0, false);
                } else {
                    let mvbp = row[2];
                    let (mut dmv_x, mut dmv_y) = (0, 0);
                    if mvbp != 0 {
                        let r = self.get_mvdata_interlaced(gb, None)?;
                        dmv_x = r.0;
                        dmv_y = r.1;
                    }
                    self.pred_mv_intfr(0, dmv_x, dmv_y, 1, self.range_x, self.range_y, 0);
                    self.mc_1mv(0);
                }
                if cbp != 0 {
                    self.get_mquant(gb, &mut mquant);
                }
                self.qscale_table[mb_pos] = mquant as i8;
                if !self.ttmbf && cbp != 0 {
                    let v = gb.get_vlc(&vl.ttmb[self.tt_index]);
                    if v < 0 {
                        return inv("ttmb");
                    }
                    ttmb = v;
                }
                let mut dst_idx = 0usize;
                for i in 0..6usize {
                    let bi = self.block_index[i];
                    self.dc_val[bi] = 0;
                    dst_idx += i >> 2;
                    let val = ((cbp >> (5 - i)) & 1) != 0;
                    let off = if fieldtx == 0 {
                        if i & 4 != 0 { 0 } else { (i & 1) * 8 + (i & 2) * 4 * ls }
                    } else if i & 4 != 0 {
                        0
                    } else {
                        (i & 1) * 8 + (i > 1) as usize * ls
                    };
                    if val {
                        let mut b = [0i16; 64];
                        let stride = if i & 4 != 0 { uvls } else { ls << fieldtx };
                        let doff = self.dest[dst_idx] + off;
                        let pat = self.decode_p_block(gb, &mut b, i, mquant, ttmb, first_block, dst_idx, doff, stride, &mut block_tt)?;
                        block_cbp |= (pat as u32) << (i << 2);
                        if !self.ttmbf && ttmb < 8 {
                            ttmb = -1;
                        }
                        first_block = false;
                    }
                }
            }
        } else {
            self.mb_intra = false;
            self.is_intra[isx] = 0;
            for i in 0..6 {
                let bi = self.block_index[i];
                self.mb_type_b[bi] = 0;
                self.dc_val[bi] = 0;
            }
            self.cur.as_mut().unwrap().mb_type[mb_pos] = MBT_SKIP;
            self.qscale_table[mb_pos] = 0;
            self.set_blk_mv_type(0);
            self.pred_mv_intfr(0, 0, 0, 1, self.range_x, self.range_y, 0);
            self.mc_1mv(0);
            self.fieldtx_plane[mb_pos] = 0;
        }
        if self.overlap && self.pq >= 9 {
            self.p_overlap_filter();
        }
        self.put_blocks_clamped(true);
        self.cbp[isx] = block_cbp;
        self.ttblk[isx] = block_tt;
        Ok(())
    }

    // ---------------------------------------------------------------- B field picture MB

    /// ff_vc1_pred_b_mv_intfi
    fn pred_b_mv_intfi(&mut self, gb: &mut BitReader, n: usize, dmv_x: &[i32; 2], dmv_y: &[i32; 2], mv1: bool, pred_flag: &[i32; 2]) {
        let dir: usize = (self.bmvtype == BMV_TYPE_BACKWARD) as usize;
        let mb_pos = self.mb_x + self.mb_y * self.lay.mb_stride;
        let bo = self.blocks_off;
        let mo = self.mb_off;
        if self.bmvtype == BMV_TYPE_DIRECT {
            let f;
            let qs = self.quarter_sample;
            let bf = self.bfraction;
            let intra = self.next.as_ref().map(|p| p.mb_type[mb_pos + mo] == MBT_INTRA).unwrap_or(true);
            if !intra {
                let nm = self.next.as_ref().unwrap().motion_val[1][self.block_index[0] + bo];
                self.mv[0][0] = [scale_mv(nm[0] as i32, bf, false, qs), scale_mv(nm[1] as i32, bf, false, qs)];
                self.mv[1][0] = [scale_mv(nm[0] as i32, bf, true, qs), scale_mv(nm[1] as i32, bf, true, qs)];
                let total_opp: i32 = (0..4).map(|k| self.mv_f_next[0][self.block_index[k] + bo] as i32).sum();
                f = if total_opp > 2 { 1 } else { 0 };
            } else {
                self.mv[0][0] = [0, 0];
                self.mv[1][0] = [0, 0];
                f = 0;
            }
            self.ref_field_type[0] = self.cur_field_type ^ f as usize;
            self.ref_field_type[1] = self.ref_field_type[0];
            for k in 0..4 {
                let b = self.block_index[k] + bo;
                self.set_cmv(0, b, [self.mv[0][0][0] as i16, self.mv[0][0][1] as i16]);
                self.set_cmv(1, b, [self.mv[1][0][0] as i16, self.mv[1][0][1] as i16]);
                self.mv_f[0][b] = f;
                self.mv_f[1][b] = f;
            }
            return;
        }
        if self.bmvtype == BMV_TYPE_INTERPOLATED {
            self.pred_mv(gb, 0, dmv_x[0], dmv_y[0], true, self.range_x, self.range_y, pred_flag[0], 0);
            self.pred_mv(gb, 0, dmv_x[1], dmv_y[1], true, self.range_x, self.range_y, pred_flag[1], 1);
            return;
        }
        if dir != 0 {
            self.pred_mv(gb, n, dmv_x[1], dmv_y[1], mv1, self.range_x, self.range_y, pred_flag[1], 1);
            if n == 3 || mv1 {
                self.pred_mv(gb, 0, dmv_x[0], dmv_y[0], true, self.range_x, self.range_y, 0, 0);
            }
        } else {
            self.pred_mv(gb, n, dmv_x[0], dmv_y[0], mv1, self.range_x, self.range_y, pred_flag[0], 0);
            if n == 3 || mv1 {
                self.pred_mv(gb, 0, dmv_x[1], dmv_y[1], true, self.range_x, self.range_y, 0, 1);
            }
        }
    }

    pub fn decode_b_mb_intfi(&mut self, gb: &mut BitReader) -> R<()> {
        let vl = self.vl;
        let ms = self.lay.mb_stride;
        let mb_pos = self.mb_x + self.mb_y * ms;
        let mut cbp: i32 = 0;
        let mut mquant = self.pq;
        let mut ttmb = self.ttfrm;
        let mut first_block = true;
        let mut block_cbp: u32 = 0;
        let mut block_tt: i32 = 0;
        let ls = self.linesize;
        let uvls = self.uvlinesize;
        let mo = self.mb_off;
        self.mb_intra = false;
        let mixed = self.mv_mode == MV_PMODE_MIXED_MV;
        let c = if mixed {
            gb.get_vlc(&vl.if_mmv_mbmode[self.mbmodetab])
        } else {
            gb.get_vlc(&vl.if_1mv_mbmode[self.mbmodetab])
        };
        if c < 0 {
            return inv("mbmode");
        }
        let idx_mbmode = c;
        if idx_mbmode <= 1 {
            self.is_intra[2 * ms + self.mb_x] = 0x3f;
            self.mb_intra = true;
            let b0 = self.block_index[0];
            self.set_cmv(1, b0, [0, 0]);
            self.cur.as_mut().unwrap().mb_type[mb_pos + mo] = MBT_INTRA;
            self.get_mquant(gb, &mut mquant);
            self.qscale_table[mb_pos] = mquant as i8;
            self.y_dc_scale = t::WMV3_DC_SCALE_TABLE[mquant.unsigned_abs() as usize] as i32;
            self.c_dc_scale = self.y_dc_scale;
            self.ac_pred = gb.get1() != 0;
            self.acpred_plane[mb_pos] = self.ac_pred as u8;
            let mb_has_coeffs = idx_mbmode & 1 != 0;
            if mb_has_coeffs {
                let v = gb.get_vlc(&vl.icbpcy[self.icbptab]);
                if v < 0 {
                    return inv("icbpcy");
                }
                cbp = 1 + v;
            }
            let mut dst_idx = 0usize;
            for i in 0..6usize {
                let bi = self.block_index[i];
                self.mb_type_b[bi] = 1;
                self.dc_val[bi] = 0;
                dst_idx += i >> 2;
                let val = ((cbp >> (5 - i)) & 1) != 0;
                self.intra_avail_pub(i);
                let cs = if i & 4 != 0 { self.codingset2 } else { self.codingset };
                let mut b = [0i16; 64];
                self.decode_intra_block(gb, &mut b, i, val, mquant, cs)?;
                dsp::inv_trans_8x8(&mut b);
                if self.rangeredfrm {
                    for v in b.iter_mut() {
                        *v = v.wrapping_shl(1);
                    }
                }
                let off = if i & 4 != 0 { 0 } else { (i & 1) * 8 + (i & 2) * 4 * ls };
                let stride = if i & 4 != 0 { uvls } else { ls };
                let doff = self.dest[dst_idx] + off;
                let pic = self.cur.as_mut().unwrap();
                dsp::put_signed_pixels_clamped(&b, &mut pic.planes[dst_idx], doff, stride);
            }
        } else {
            self.mb_intra = false;
            self.is_intra[2 * ms + self.mb_x] = 0;
            self.cur.as_mut().unwrap().mb_type[mb_pos + mo] = MBT_16X16;
            for i in 0..6 {
                let bi = self.block_index[i];
                self.mb_type_b[bi] = 0;
            }
            let fwd = if self.fmb_is_raw {
                let v = gb.get1() as u8;
                self.forward_mb_plane[mb_pos] = v;
                v != 0
            } else {
                self.forward_mb_plane[mb_pos] != 0
            };
            let mut dmv_x = [0i32; 2];
            let mut dmv_y = [0i32; 2];
            let mut pred_flag = [0i32; 2];
            let mut bmvtype = BMV_TYPE_BACKWARD;
            let mb_has_coeffs;
            if idx_mbmode <= 5 {
                let mut interpmvp = false;
                if fwd {
                    bmvtype = BMV_TYPE_FORWARD;
                } else {
                    bmvtype = gb.decode012() as i32;
                    match bmvtype {
                        0 => bmvtype = BMV_TYPE_BACKWARD,
                        1 => bmvtype = BMV_TYPE_DIRECT,
                        _ => {
                            bmvtype = BMV_TYPE_INTERPOLATED;
                            interpmvp = gb.get1() != 0;
                        }
                    }
                }
                self.bmvtype = bmvtype;
                if bmvtype != BMV_TYPE_DIRECT && idx_mbmode & 1 != 0 {
                    let k = (bmvtype == BMV_TYPE_BACKWARD) as usize;
                    let mut pf = 0;
                    let r = self.get_mvdata_interlaced(gb, Some(&mut pf))?;
                    dmv_x[k] = r.0;
                    dmv_y[k] = r.1;
                    pred_flag[k] = pf;
                }
                if interpmvp {
                    let mut pf = 0;
                    let r = self.get_mvdata_interlaced(gb, Some(&mut pf))?;
                    dmv_x[1] = r.0;
                    dmv_y[1] = r.1;
                    pred_flag[1] = pf;
                }
                if bmvtype == BMV_TYPE_DIRECT {
                    dmv_x = [0; 2];
                    dmv_y = [0; 2];
                    pred_flag = [0; 2];
                    if !self.next.as_ref().map(|p| p.field_picture).unwrap_or(false) {
                        return inv("mixed field/frame direct mode");
                    }
                }
                self.pred_b_mv_intfi(gb, 0, &dmv_x, &dmv_y, true, &pred_flag);
                self.b_mc_pub(bmvtype == BMV_TYPE_DIRECT, bmvtype);
                mb_has_coeffs = idx_mbmode & 2 == 0;
            } else {
                if fwd {
                    bmvtype = BMV_TYPE_FORWARD;
                }
                self.bmvtype = bmvtype;
                let v = gb.get_vlc(&vl.fourmv_bp[self.fourmvbptab]);
                if v < 0 {
                    return inv("4mvbp");
                }
                self.fourmvbp = v as u32;
                for i in 0..4usize {
                    dmv_x = [0; 2];
                    dmv_y = [0; 2];
                    pred_flag = [0; 2];
                    if self.fourmvbp & (8 >> i) != 0 {
                        let k = (bmvtype == BMV_TYPE_BACKWARD) as usize;
                        let mut pf = 0;
                        let r = self.get_mvdata_interlaced(gb, Some(&mut pf))?;
                        dmv_x[k] = r.0;
                        dmv_y[k] = r.1;
                        pred_flag[k] = pf;
                    }
                    self.pred_b_mv_intfi(gb, i, &dmv_x, &dmv_y, false, &pred_flag);
                    self.mc_4mv_luma(i, (bmvtype == BMV_TYPE_BACKWARD) as usize, false);
                }
                self.mc_4mv_chroma((bmvtype == BMV_TYPE_BACKWARD) as usize);
                mb_has_coeffs = idx_mbmode & 1 != 0;
            }
            if mb_has_coeffs {
                let v = gb.get_vlc(&vl.icbpcy[self.icbptab]);
                if v < 0 {
                    return inv("icbpcy");
                }
                cbp = 1 + v;
            }
            if cbp != 0 {
                self.get_mquant(gb, &mut mquant);
            }
            self.qscale_table[mb_pos] = mquant as i8;
            if !self.ttmbf && cbp != 0 {
                let v = gb.get_vlc(&vl.ttmb[self.tt_index]);
                if v < 0 {
                    return inv("ttmb");
                }
                ttmb = v;
            }
            let mut dst_idx = 0usize;
            for i in 0..6usize {
                let bi = self.block_index[i];
                self.dc_val[bi] = 0;
                dst_idx += i >> 2;
                let val = ((cbp >> (5 - i)) & 1) != 0;
                let off = if i & 4 != 0 { 0 } else { (i & 1) * 8 + (i & 2) * 4 * ls };
                if val {
                    let mut b = [0i16; 64];
                    let stride = if i & 4 != 0 { uvls } else { ls };
                    let doff = self.dest[dst_idx] + off;
                    let pat = self.decode_p_block(gb, &mut b, i, mquant, ttmb, first_block, dst_idx, doff, stride, &mut block_tt)?;
                    block_cbp |= (pat as u32) << (i << 2);
                    if !self.ttmbf && ttmb < 8 {
                        ttmb = -1;
                    }
                    first_block = false;
                }
            }
        }
        let x = 2 * ms + self.mb_x;
        self.cbp[x] = block_cbp;
        self.ttblk[x] = block_tt;
        Ok(())
    }

    // ---------------------------------------------------------------- B frame picture (interlaced frame) MB

    pub fn decode_b_mb_intfr(&mut self, gb: &mut BitReader) -> R<()> {
        let vl = self.vl;
        let ms = self.lay.mb_stride;
        let mb_pos = self.mb_x + self.mb_y * ms;
        let mut cbp: i32 = 0;
        let mut mquant = self.pq;
        let mut ttmb = self.ttfrm;
        let mut mvsw = false;
        let mut first_block = true;
        let mut block_cbp: u32 = 0;
        let mut block_tt: i32 = 0;
        let ls = self.linesize;
        let uvls = self.uvlinesize;
        let isx = 2 * ms + self.mb_x;
        let mut twomv = false;
        let mut bmvtype = BMV_TYPE_BACKWARD;
        self.mb_intra = false;
        let skipped = if self.skip_is_raw { gb.get1() != 0 } else { self.mbskip_table[mb_pos] != 0 };
        let mut row = MBMODE_INTFRP_NON4MV[0];
        if !skipped {
            let c = gb.get_vlc(&vl.intfr_non4mv_mbmode[self.mbmodetab]);
            if c < 0 {
                return inv("mbmode");
            }
            row = self.mbmode_row(c as usize, false);
            if row[0] == INTFR_2MV_FIELD {
                twomv = true;
                self.set_blk_mv_type(1);
            } else {
                self.set_blk_mv_type(0);
            }
        }
        if row[0] == INTFR_INTRA {
            for i in 0..4 {
                let b = self.block_index[i];
                self.mv[0][i] = [0, 0];
                self.mv[1][i] = [0, 0];
                self.set_cmv(0, b, [0, 0]);
                self.set_cmv(1, b, [0, 0]);
            }
            self.is_intra[isx] = 0x3f;
            self.mb_intra = true;
            self.cur.as_mut().unwrap().mb_type[mb_pos] = MBT_INTRA;
            let fieldtx = gb.get1() as u8;
            self.fieldtx_plane[mb_pos] = fieldtx;
            let mb_has_coeffs = gb.get1() != 0;
            if mb_has_coeffs {
                let v = gb.get_vlc(&vl.icbpcy[self.icbptab]);
                if v < 0 {
                    return inv("icbpcy");
                }
                cbp = 1 + v;
            }
            self.ac_pred = gb.get1() != 0;
            self.acpred_plane[mb_pos] = self.ac_pred as u8;
            self.get_mquant(gb, &mut mquant);
            self.qscale_table[mb_pos] = mquant as i8;
            self.y_dc_scale = t::WMV3_DC_SCALE_TABLE[mquant.unsigned_abs() as usize] as i32;
            self.c_dc_scale = self.y_dc_scale;
            let mut dst_idx = 0usize;
            for i in 0..6usize {
                let bi = self.block_index[i];
                self.mb_type_b[bi] = 1;
                self.dc_val[bi] = 0;
                dst_idx += i >> 2;
                let val = ((cbp >> (5 - i)) & 1) != 0;
                self.intra_avail_pub(i);
                let cs = if i & 4 != 0 { self.codingset2 } else { self.codingset };
                let mut b = [0i16; 64];
                self.decode_intra_block(gb, &mut b, i, val, mquant, cs)?;
                dsp::inv_trans_8x8(&mut b);
                let (stride_y, off) = if i < 4 {
                    if fieldtx != 0 {
                        (ls << 1, (i & 1) * 8 + ((i & 2) >> 1) * ls)
                    } else {
                        (ls, (i & 1) * 8 + 4 * (i & 2) * ls)
                    }
                } else {
                    (uvls, 0)
                };
                let doff = self.dest[dst_idx] + off;
                let pic = self.cur.as_mut().unwrap();
                dsp::put_signed_pixels_clamped(&b, &mut pic.planes[dst_idx], doff, stride_y);
            }
        } else {
            self.mb_intra = false;
            self.is_intra[isx] = 0;
            let direct = if self.dmb_is_raw { gb.get1() != 0 } else { self.direct_mb_plane[mb_pos] != 0 };
            let qs = self.quarter_sample;
            let bf = self.bfraction;
            if direct {
                let bi0 = self.block_index[0];
                let nm = self.next.as_ref().map(|p| p.motion_val[1][bi0]).unwrap_or([0, 0]);
                self.mv[0][0] = [scale_mv(nm[0] as i32, bf, false, qs), scale_mv(nm[1] as i32, bf, false, qs)];
                self.mv[1][0] = [scale_mv(nm[0] as i32, bf, true, qs), scale_mv(nm[1] as i32, bf, true, qs)];
                self.set_cmv(0, bi0, [self.mv[0][0][0] as i16, self.mv[0][0][1] as i16]);
                self.set_cmv(1, bi0, [self.mv[1][0][0] as i16, self.mv[1][0][1] as i16]);
                if twomv {
                    let bi2 = self.block_index[2];
                    let nm = self.next.as_ref().map(|p| p.motion_val[1][bi2]).unwrap_or([0, 0]);
                    self.mv[0][2] = [scale_mv(nm[0] as i32, bf, false, qs), scale_mv(nm[1] as i32, bf, false, qs)];
                    self.mv[1][2] = [scale_mv(nm[0] as i32, bf, true, qs), scale_mv(nm[1] as i32, bf, true, qs)];
                    self.set_cmv(0, bi2, [self.mv[0][2][0] as i16, self.mv[0][2][1] as i16]);
                    self.set_cmv(1, bi2, [self.mv[1][2][0] as i16, self.mv[1][2][1] as i16]);
                    for i in [1usize, 3] {
                        let b = self.block_index[i];
                        for d in 0..2 {
                            self.mv[d][i] = self.mv[d][i - 1];
                            self.set_cmv(d, b, [self.mv[d][i][0] as i16, self.mv[d][i][1] as i16]);
                        }
                    }
                } else {
                    for i in 1..4 {
                        let b = self.block_index[i];
                        for d in 0..2 {
                            self.mv[d][i] = self.mv[d][0];
                            self.set_cmv(d, b, [self.mv[d][i][0] as i16, self.mv[d][i][1] as i16]);
                        }
                    }
                }
            }
            if !direct {
                if skipped || !self.mb_intra {
                    bmvtype = gb.decode012() as i32;
                    match bmvtype {
                        0 => bmvtype = if bf >= B_FRACTION_DEN / 2 { BMV_TYPE_BACKWARD } else { BMV_TYPE_FORWARD },
                        1 => bmvtype = if bf >= B_FRACTION_DEN / 2 { BMV_TYPE_FORWARD } else { BMV_TYPE_BACKWARD },
                        _ => bmvtype = BMV_TYPE_INTERPOLATED,
                    }
                }
                if twomv && bmvtype != BMV_TYPE_INTERPOLATED {
                    mvsw = gb.get1() != 0;
                }
            }
            let range = (self.range_x, self.range_y);
            if !skipped {
                let mb_has_coeffs = row[3] != 0;
                if mb_has_coeffs {
                    let v = gb.get_vlc(&vl.icbpcy[self.icbptab]);
                    if v < 0 {
                        return inv("icbpcy");
                    }
                    cbp = 1 + v;
                }
                if !direct {
                    if bmvtype == BMV_TYPE_INTERPOLATED && twomv {
                        let v = gb.get_vlc(&vl.fourmv_bp[self.fourmvbptab]);
                        if v < 0 {
                            return inv("4mvbp");
                        }
                        self.fourmvbp = v as u32;
                    } else if bmvtype == BMV_TYPE_INTERPOLATED || twomv {
                        let v = gb.get_vlc(&vl.twomv_bp[self.twomvbptab]);
                        if v < 0 {
                            return inv("2mvbp");
                        }
                        self.twomvbp = v as u32;
                    }
                }
                for i in 0..6 {
                    let bi = self.block_index[i];
                    self.mb_type_b[bi] = 0;
                }
                let fieldtx = row[1];
                self.fieldtx_plane[mb_pos] = fieldtx;
                if direct {
                    if twomv {
                        for i in 0..4 {
                            self.mc_4mv_luma(i, 0, false);
                            self.mc_4mv_luma(i, 1, true);
                        }
                        self.mc_4mv_chroma4(0, 0, false);
                        self.mc_4mv_chroma4(1, 1, true);
                    } else {
                        self.mc_1mv(0);
                        self.interp_mc();
                    }
                } else if twomv && bmvtype == BMV_TYPE_INTERPOLATED {
                    let mvbp = self.fourmvbp;
                    for i in 0..4usize {
                        let dir = (i == 1 || i == 3) as usize;
                        let (mut dmv_x, mut dmv_y) = (0, 0);
                        if (mvbp >> (3 - i)) & 1 != 0 {
                            let r = self.get_mvdata_interlaced(gb, None)?;
                            dmv_x = r.0;
                            dmv_y = r.1;
                        }
                        let j = if i > 1 { 2 } else { 0 };
                        self.pred_mv_intfr(j, dmv_x, dmv_y, 2, range.0, range.1, dir);
                        self.mc_4mv_luma(j, dir, dir != 0);
                        self.mc_4mv_luma(j + 1, dir, dir != 0);
                    }
                    self.mc_4mv_chroma4(0, 0, false);
                    self.mc_4mv_chroma4(1, 1, true);
                } else if bmvtype == BMV_TYPE_INTERPOLATED {
                    let mvbp = self.twomvbp;
                    let (mut dmv_x, mut dmv_y) = (0, 0);
                    if mvbp & 2 != 0 {
                        let r = self.get_mvdata_interlaced(gb, None)?;
                        dmv_x = r.0;
                        dmv_y = r.1;
                    }
                    self.pred_mv_intfr(0, dmv_x, dmv_y, 1, range.0, range.1, 0);
                    self.mc_1mv(0);
                    let (mut dmv_x, mut dmv_y) = (0, 0);
                    if mvbp & 1 != 0 {
                        let r = self.get_mvdata_interlaced(gb, None)?;
                        dmv_x = r.0;
                        dmv_y = r.1;
                    }
                    self.pred_mv_intfr(0, dmv_x, dmv_y, 1, range.0, range.1, 1);
                    self.interp_mc();
                } else if twomv {
                    let dir = (bmvtype == BMV_TYPE_BACKWARD) as usize;
                    let mut dir2 = dir;
                    if mvsw {
                        dir2 = dir ^ 1;
                    }
                    let mvbp = self.twomvbp;
                    let (mut dmv_x, mut dmv_y) = (0, 0);
                    if mvbp & 2 != 0 {
                        let r = self.get_mvdata_interlaced(gb, None)?;
                        dmv_x = r.0;
                        dmv_y = r.1;
                    }
                    self.pred_mv_intfr(0, dmv_x, dmv_y, 2, range.0, range.1, dir);
                    let (mut dmv_x, mut dmv_y) = (0, 0);
                    if mvbp & 1 != 0 {
                        let r = self.get_mvdata_interlaced(gb, None)?;
                        dmv_x = r.0;
                        dmv_y = r.1;
                    }
                    self.pred_mv_intfr(2, dmv_x, dmv_y, 2, range.0, range.1, dir2);
                    if mvsw {
                        self.mvsw_copy(dir, dir2);
                    } else {
                        self.pred_mv_intfr(0, 0, 0, 2, range.0, range.1, dir ^ 1);
                        self.pred_mv_intfr(2, 0, 0, 2, range.0, range.1, dir ^ 1);
                    }
                    self.mc_4mv_luma(0, dir, false);
                    self.mc_4mv_luma(1, dir, false);
                    self.mc_4mv_luma(2, dir2, false);
                    self.mc_4mv_luma(3, dir2, false);
                    self.mc_4mv_chroma4(dir, dir2, false);
                } else {
                    let dir = (bmvtype == BMV_TYPE_BACKWARD) as usize;
                    let mvbp = row[2];
                    let (mut dmv_x, mut dmv_y) = (0, 0);
                    if mvbp != 0 {
                        let r = self.get_mvdata_interlaced(gb, None)?;
                        dmv_x = r.0;
                        dmv_y = r.1;
                    }
                    self.pred_mv_intfr(0, dmv_x, dmv_y, 1, range.0, range.1, dir);
                    self.set_blk_mv_type(1);
                    self.pred_mv_intfr(0, 0, 0, 2, range.0, range.1, dir ^ 1);
                    self.copy_top_to_bottom(dir ^ 1);
                    self.mc_1mv(dir);
                }
                if cbp != 0 {
                    self.get_mquant(gb, &mut mquant);
                }
                self.qscale_table[mb_pos] = mquant as i8;
                if !self.ttmbf && cbp != 0 {
                    let v = gb.get_vlc(&vl.ttmb[self.tt_index]);
                    if v < 0 {
                        return inv("ttmb");
                    }
                    ttmb = v;
                }
                let mut dst_idx = 0usize;
                for i in 0..6usize {
                    let bi = self.block_index[i];
                    self.dc_val[bi] = 0;
                    dst_idx += i >> 2;
                    let val = ((cbp >> (5 - i)) & 1) != 0;
                    let off = if fieldtx == 0 {
                        if i & 4 != 0 { 0 } else { (i & 1) * 8 + (i & 2) * 4 * ls }
                    } else if i & 4 != 0 {
                        0
                    } else {
                        (i & 1) * 8 + (i > 1) as usize * ls
                    };
                    if val {
                        let mut b = [0i16; 64];
                        let stride = if i & 4 != 0 { uvls } else { ls << fieldtx };
                        let doff = self.dest[dst_idx] + off;
                        let pat = self.decode_p_block(gb, &mut b, i, mquant, ttmb, first_block, dst_idx, doff, stride, &mut block_tt)?;
                        block_cbp |= (pat as u32) << (i << 2);
                        if !self.ttmbf && ttmb < 8 {
                            ttmb = -1;
                        }
                        first_block = false;
                    }
                }
            } else {
                // skipped
                let mut dir = 0usize;
                for i in 0..6 {
                    let bi = self.block_index[i];
                    self.mb_type_b[bi] = 0;
                    self.dc_val[bi] = 0;
                }
                self.cur.as_mut().unwrap().mb_type[mb_pos] = MBT_SKIP;
                self.qscale_table[mb_pos] = 0;
                self.set_blk_mv_type(0);
                if !direct {
                    if bmvtype == BMV_TYPE_INTERPOLATED {
                        self.pred_mv_intfr(0, 0, 0, 1, range.0, range.1, 0);
                        self.pred_mv_intfr(0, 0, 0, 1, range.0, range.1, 1);
                    } else {
                        dir = (bmvtype == BMV_TYPE_BACKWARD) as usize;
                        self.pred_mv_intfr(0, 0, 0, 1, range.0, range.1, dir);
                        if mvsw {
                            let dir2 = dir ^ 1;
                            self.mvsw_copy(dir, dir2);
                        } else {
                            self.set_blk_mv_type(1);
                            self.pred_mv_intfr(0, 0, 0, 2, range.0, range.1, dir ^ 1);
                            self.copy_top_to_bottom(dir ^ 1);
                        }
                    }
                }
                self.mc_1mv(dir);
                if direct || bmvtype == BMV_TYPE_INTERPOLATED {
                    self.interp_mc();
                }
                self.fieldtx_plane[mb_pos] = 0;
            }
        }
        self.cbp[isx] = block_cbp;
        self.ttblk[isx] = block_tt;
        Ok(())
    }

    /// for (i = 0..2) { mv[d][i+2] = mv[d][i] = motion_val[d][bi[i+2]] = motion_val[d][bi[i]]; (dir)
    ///                  mv[d2][i+2] = mv[d2][i] = motion_val[d2][bi[i]] = motion_val[d2][bi[i+2]] (dir2) }
    fn mvsw_copy(&mut self, dir: usize, dir2: usize) {
        for i in 0..2usize {
            let (b0, b2) = (self.block_index[i], self.block_index[i + 2]);
            let v = self.cmv(dir, b0);
            self.set_cmv(dir, b2, v);
            self.mv[dir][i] = [v[0] as i32, v[1] as i32];
            self.mv[dir][i + 2] = self.mv[dir][i];
            let w = self.cmv(dir2, b2);
            self.set_cmv(dir2, b0, w);
            self.mv[dir2][i] = [w[0] as i32, w[1] as i32];
            self.mv[dir2][i + 2] = self.mv[dir2][i];
        }
    }

    fn copy_top_to_bottom(&mut self, d: usize) {
        for i in 0..2usize {
            let (b0, b2) = (self.block_index[i], self.block_index[i + 2]);
            let v = self.cmv(d, b0);
            self.set_cmv(d, b2, v);
            self.mv[d][i] = [v[0] as i32, v[1] as i32];
            self.mv[d][i + 2] = self.mv[d][i];
        }
    }

    // ---------------------------------------------------------------- loop filters

    fn ring_ilace(&self, r: usize, c: usize) -> usize {
        2 * self.lay.mb_stride + self.mb_x - r * self.lay.mb_stride - c
    }

    fn geom(&self, r: usize, c: usize, i: usize) -> (usize, isize) {
        let ls = self.linesize as isize;
        let uvls = self.uvlinesize as isize;
        if i > 3 {
            (i - 3, self.dest[i - 3] as isize - 8 * r as isize * uvls - 8 * c as isize)
        } else {
            (0, self.dest[0] as isize - 16 * r as isize * ls - 16 * c as isize)
        }
    }

    fn lf(&mut self, plane: usize, horiz: bool, pos: isize, stride: usize, len: usize) {
        if pos < 0 {
            return;
        }
        let pq = self.pq;
        let buf = &mut self.cur.as_mut().unwrap().planes[plane];
        if horiz {
            dsp::h_loop_filter(buf, pos as usize, stride, len, pq);
        } else {
            dsp::v_loop_filter(buf, pos as usize, stride, len, pq);
        }
    }

    fn p_h_intfr_filter(&mut self, r: usize, c: usize, flags: u32, bn: usize) {
        let ms = self.lay.mb_stride;
        let mb_pos = self.mb_x + self.mb_y * ms;
        let fieldtx = self.fieldtx_plane[mb_pos - r * ms - c] != 0;
        let ring = self.ring_ilace(r, c);
        let (plane, dest) = self.geom(r, c, bn);
        let ls = if bn > 3 { self.uvlinesize } else { self.linesize };
        let dst = if bn > 3 { dest } else { dest + (bn & 2) as isize * 4 * self.linesize as isize + (bn & 1) as isize * 8 };
        let tt = (self.ttblk[ring] >> (bn * 4)) & 0xf;
        let lsi = ls as isize;
        let is44_48 = tt == TT_4X4 || tt == TT_4X8;
        if bn < 4 {
            if fieldtx {
                if bn < 2 {
                    if is44_48 {
                        self.lf(plane, true, dst + 4, 2 * ls, 8);
                    }
                    if flags & RIGHT_EDGE == 0 || bn == 0 {
                        self.lf(plane, true, dst + 8, 2 * ls, 8);
                    }
                } else {
                    if is44_48 {
                        self.lf(plane, true, dst - 7 * lsi + 4, 2 * ls, 8);
                    }
                    if flags & RIGHT_EDGE == 0 || bn == 2 {
                        self.lf(plane, true, dst - 7 * lsi + 8, 2 * ls, 8);
                    }
                }
            } else {
                if is44_48 {
                    self.lf(plane, true, dst + 4, 2 * ls, 4);
                    self.lf(plane, true, dst + lsi + 4, 2 * ls, 4);
                }
                if flags & RIGHT_EDGE == 0 || (bn & 5) == 0 {
                    self.lf(plane, true, dst + 8, 2 * ls, 4);
                    self.lf(plane, true, dst + lsi + 8, 2 * ls, 4);
                }
            }
        } else {
            if is44_48 {
                self.lf(plane, true, dst + 4, 2 * ls, 4);
                self.lf(plane, true, dst + lsi + 4, 2 * ls, 4);
            }
            if flags & RIGHT_EDGE == 0 {
                self.lf(plane, true, dst + 8, 2 * ls, 4);
                self.lf(plane, true, dst + lsi + 8, 2 * ls, 4);
            }
        }
    }

    fn p_v_intfr_filter(&mut self, r: usize, c: usize, flags: u32, bn: usize) {
        let ms = self.lay.mb_stride;
        let mb_pos = self.mb_x + self.mb_y * ms;
        let fieldtx = self.fieldtx_plane[mb_pos - r * ms - c] != 0;
        let ring = self.ring_ilace(r, c);
        let (plane, dest) = self.geom(r, c, bn);
        let ls = if bn > 3 { self.uvlinesize } else { self.linesize };
        let dst = if bn > 3 { dest } else { dest + (bn & 2) as isize * 4 * self.linesize as isize + (bn & 1) as isize * 8 };
        let tt = (self.ttblk[ring] >> (bn * 4)) & 0xf;
        let lsi = ls as isize;
        let is44_84 = tt == TT_4X4 || tt == TT_8X4;
        if bn < 4 {
            if fieldtx {
                if bn < 2 {
                    if is44_84 {
                        self.lf(plane, false, dst + 8 * lsi, 2 * ls, 8);
                    }
                    if flags & BOTTOM_EDGE == 0 {
                        self.lf(plane, false, dst + 16 * lsi, 2 * ls, 8);
                    }
                } else {
                    if is44_84 {
                        self.lf(plane, false, dst + lsi, 2 * ls, 8);
                    }
                    if flags & BOTTOM_EDGE == 0 {
                        self.lf(plane, false, dst + 9 * lsi, 2 * ls, 8);
                    }
                }
            } else if bn < 2 {
                if flags & TOP_EDGE == 0 && is44_84 {
                    self.lf(plane, false, dst + 4 * lsi, 2 * ls, 8);
                    self.lf(plane, false, dst + 5 * lsi, 2 * ls, 8);
                }
                self.lf(plane, false, dst + 8 * lsi, 2 * ls, 8);
                self.lf(plane, false, dst + 9 * lsi, 2 * ls, 8);
            } else if flags & BOTTOM_EDGE == 0 {
                if is44_84 {
                    self.lf(plane, false, dst + 4 * lsi, 2 * ls, 8);
                    self.lf(plane, false, dst + 5 * lsi, 2 * ls, 8);
                }
                self.lf(plane, false, dst + 8 * lsi, 2 * ls, 8);
                self.lf(plane, false, dst + 9 * lsi, 2 * ls, 8);
            }
        } else if flags & BOTTOM_EDGE == 0 {
            if flags & TOP_EDGE == 0 && is44_84 {
                self.lf(plane, false, dst + 4 * lsi, 2 * ls, 8);
                self.lf(plane, false, dst + 5 * lsi, 2 * ls, 8);
            }
            self.lf(plane, false, dst + 8 * lsi, 2 * ls, 8);
            self.lf(plane, false, dst + 9 * lsi, 2 * ls, 8);
        }
    }

    pub fn p_intfr_loop_filter(&mut self) {
        let sty = self.start_mb_y;
        let ey = self.end_mb_y;
        let mbx = self.mb_x;
        let last_x = self.mb_width - 1;
        if mbx != 0 && self.mb_y > sty {
            let flags = if self.mb_y == sty + 1 { TOP_EDGE } else { 0 };
            for i in 0..6 {
                self.p_v_intfr_filter(1, 1, flags, i);
            }
        }
        if mbx == last_x && self.mb_y > sty {
            let flags = if self.mb_y == sty + 1 { TOP_EDGE } else { 0 };
            for i in 0..6 {
                self.p_v_intfr_filter(1, 0, flags, i);
            }
        }
        if self.mb_y == ey - 1 {
            let flags = if self.mb_y == sty { TOP_EDGE | BOTTOM_EDGE } else { BOTTOM_EDGE };
            if mbx != 0 {
                for i in 0..6 {
                    self.p_v_intfr_filter(0, 1, flags, i);
                }
            }
            if mbx == last_x {
                for i in 0..6 {
                    self.p_v_intfr_filter(0, 0, flags, i);
                }
            }
        }
        if self.mb_y >= sty + 2 {
            if mbx >= 2 {
                let flags = if mbx == 2 { LEFT_EDGE } else { 0 };
                for i in 0..6 {
                    self.p_h_intfr_filter(2, 2, flags, i);
                }
            }
            if mbx == last_x {
                if mbx >= 1 {
                    let flags = if mbx == 1 { LEFT_EDGE } else { 0 };
                    for i in 0..6 {
                        self.p_h_intfr_filter(2, 1, flags, i);
                    }
                }
                let flags = if mbx != 0 { RIGHT_EDGE } else { LEFT_EDGE | RIGHT_EDGE };
                for i in 0..6 {
                    self.p_h_intfr_filter(2, 0, flags, i);
                }
            }
        }
        if self.mb_y == ey - 1 {
            if self.mb_y > sty {
                if mbx >= 2 {
                    let flags = if mbx == 2 { LEFT_EDGE } else { 0 };
                    for i in 0..6 {
                        self.p_h_intfr_filter(1, 2, flags, i);
                    }
                }
                if mbx == last_x {
                    if mbx >= 1 {
                        let flags = if mbx == 1 { LEFT_EDGE } else { 0 };
                        for i in 0..6 {
                            self.p_h_intfr_filter(1, 1, flags, i);
                        }
                    }
                    let flags = if mbx != 0 { RIGHT_EDGE } else { LEFT_EDGE | RIGHT_EDGE };
                    for i in 0..6 {
                        self.p_h_intfr_filter(1, 0, flags, i);
                    }
                }
            }
            if mbx >= 2 {
                let flags = if mbx == 2 { LEFT_EDGE } else { 0 };
                for i in 0..6 {
                    self.p_h_intfr_filter(0, 2, flags, i);
                }
            }
            if mbx == last_x {
                if mbx >= 1 {
                    let flags = if mbx == 1 { LEFT_EDGE } else { 0 };
                    for i in 0..6 {
                        self.p_h_intfr_filter(0, 1, flags, i);
                    }
                }
                let flags = if mbx != 0 { RIGHT_EDGE } else { LEFT_EDGE | RIGHT_EDGE };
                for i in 0..6 {
                    self.p_h_intfr_filter(0, 0, flags, i);
                }
            }
        }
    }

    fn b_h_intfi_filter(&mut self, r: usize, c: usize, flags: u32, bn: usize) {
        let ring = self.ring_ilace(r, c);
        let (plane, dest) = self.geom(r, c, bn);
        let ls = if bn > 3 { self.uvlinesize } else { self.linesize };
        let dst = if bn > 3 { dest } else { dest + (bn & 2) as isize * 4 * self.linesize as isize + (bn & 1) as isize * 8 };
        let block_cbp = self.cbp[ring] >> (bn * 4);
        if flags & RIGHT_EDGE == 0 || (bn & 5) == 0 {
            self.lf(plane, true, dst + 8, ls, 8);
        }
        let tt = (self.ttblk[ring] >> (bn * 4)) & 0xf;
        if tt == TT_4X4 || tt == TT_4X8 {
            let idx = (block_cbp | (block_cbp >> 1)) & 5;
            if idx & 1 != 0 {
                self.lf(plane, true, dst + 4 * ls as isize + 4, ls, 4);
            }
            if idx & 4 != 0 {
                self.lf(plane, true, dst + 4, ls, 4);
            }
        }
    }

    fn b_v_intfi_filter(&mut self, r: usize, c: usize, flags: u32, bn: usize) {
        let ring = self.ring_ilace(r, c);
        let (plane, dest) = self.geom(r, c, bn);
        let ls = if bn > 3 { self.uvlinesize } else { self.linesize };
        let dst = if bn > 3 { dest } else { dest + (bn & 2) as isize * 4 * self.linesize as isize + (bn & 1) as isize * 8 };
        let block_cbp = self.cbp[ring] >> (bn * 4);
        if flags & BOTTOM_EDGE == 0 || bn < 2 {
            self.lf(plane, false, dst + 8 * ls as isize, ls, 8);
        }
        let tt = (self.ttblk[ring] >> (bn * 4)) & 0xf;
        if tt == TT_4X4 || tt == TT_8X4 {
            let idx = (block_cbp | (block_cbp >> 2)) & 3;
            if idx & 1 != 0 {
                self.lf(plane, false, dst + 4 * ls as isize + 4, ls, 4);
            }
            if idx & 2 != 0 {
                self.lf(plane, false, dst + 4 * ls as isize, ls, 4);
            }
        }
    }

    pub fn b_intfi_loop_filter(&mut self) {
        let sty = self.start_mb_y;
        let ey = self.end_mb_y;
        let mbx = self.mb_x;
        let last_x = self.mb_width - 1;
        if !self.first_slice_line {
            let flags = if self.mb_y == sty + 1 { TOP_EDGE } else { 0 };
            for i in 0..6 {
                self.b_v_intfi_filter(1, 0, flags, i);
            }
        }
        if self.mb_y == ey - 1 {
            let flags = if self.first_slice_line { TOP_EDGE | BOTTOM_EDGE } else { BOTTOM_EDGE };
            for i in 0..6 {
                self.b_v_intfi_filter(0, 0, flags, i);
            }
        }
        if !self.first_slice_line {
            if mbx != 0 {
                let flags = if mbx == 1 { LEFT_EDGE } else { 0 };
                for i in 0..6 {
                    self.b_h_intfi_filter(1, 1, flags, i);
                }
            }
            if mbx == last_x {
                let flags = if mbx == 0 { LEFT_EDGE | RIGHT_EDGE } else { RIGHT_EDGE };
                for i in 0..6 {
                    self.b_h_intfi_filter(1, 0, flags, i);
                }
            }
        }
        if self.mb_y == ey - 1 {
            if mbx != 0 {
                let flags = if mbx == 1 { LEFT_EDGE } else { 0 };
                for i in 0..6 {
                    self.b_h_intfi_filter(0, 1, flags, i);
                }
            }
            if mbx == last_x {
                let flags = if mbx == 0 { LEFT_EDGE | RIGHT_EDGE } else { RIGHT_EDGE };
                for i in 0..6 {
                    self.b_h_intfi_filter(0, 0, flags, i);
                }
            }
        }
    }
}
