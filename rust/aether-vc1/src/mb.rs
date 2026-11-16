//! Macroblock layer and picture decode loops (FFmpeg vc1_block.c).
use crate::bits::BitReader;
use crate::blocks::BLOCK_MAP;
use crate::core::*;
use crate::dsp;
use crate::tables as t;

const SIZE_TABLE: [i32; 6] = [0, 2, 3, 4, 5, 8];
const OFFSET_TABLE1: [i32; 9] = [0, 1, 3, 7, 15, 31, 63, 127, 255];

const CS_HIGH_MOT_INTRA: usize = 0;
const CS_HIGH_MOT_INTER: usize = 1;
const CS_LOW_MOT_INTRA: usize = 2;
const CS_LOW_MOT_INTER: usize = 3;
const CS_MID_RATE_INTRA: usize = 4;
const CS_MID_RATE_INTER: usize = 5;
const CS_HIGH_RATE_INTRA: usize = 6;
const CS_HIGH_RATE_INTER: usize = 7;

impl Core {
    fn blk(&self, ring: usize, k: usize) -> [i16; 64] {
        let o = (ring * 6 + k) * 64;
        self.block[o..o + 64].try_into().unwrap()
    }
    fn set_blk(&mut self, ring: usize, k: usize, b: &[i16; 64]) {
        let o = (ring * 6 + k) * 64;
        self.block[o..o + 64].copy_from_slice(b);
    }

    fn mquant(&mut self, gb: &mut BitReader, mq: &mut i32) {
        // GET_MQUANT, exposed to this module
        self.get_mquant(gb, mq);
    }

    fn select_coding_sets(&mut self, intra_idx: u32, inter_idx: u32) {
        self.codingset = match intra_idx {
            0 => {
                if self.pqindex <= 8 {
                    CS_HIGH_RATE_INTRA
                } else {
                    CS_LOW_MOT_INTRA
                }
            }
            1 => CS_HIGH_MOT_INTRA,
            _ => CS_MID_RATE_INTRA,
        };
        self.codingset2 = match inter_idx {
            0 => {
                if self.pqindex <= 8 {
                    CS_HIGH_RATE_INTER
                } else {
                    CS_LOW_MOT_INTER
                }
            }
            1 => CS_HIGH_MOT_INTER,
            _ => CS_MID_RATE_INTER,
        };
    }

    /// GET_MVDATA: returns (dmv_x, dmv_y, mb_has_coeffs); sets self.mb_intra.
    fn get_mvdata(&mut self, gb: &mut BitReader) -> R<(i32, i32, bool)> {
        let vl = self.vl;
        let c = gb.get_vlc(&vl.mv_diff[self.mv_table_index]);
        if c < 0 {
            return inv("mv diff vlc");
        }
        let mut index = 1 + c;
        let mb_has_coeffs;
        if index > 36 {
            mb_has_coeffs = true;
            index -= 37;
        } else {
            mb_has_coeffs = false;
        }
        self.mb_intra = false;
        let (dx, dy);
        if index == 0 {
            dx = 0;
            dy = 0;
        } else if index == 35 {
            dx = gb.get((self.k_x - 1 + self.quarter_sample as i32) as u32) as i32;
            dy = gb.get((self.k_y - 1 + self.quarter_sample as i32) as u32) as i32;
        } else if index == 36 {
            dx = 0;
            dy = 0;
            self.mb_intra = true;
        } else {
            let mut index1 = (index % 6) as usize;
            let mut x = OFFSET_TABLE1[index1];
            let mut val = SIZE_TABLE[index1] - (!self.quarter_sample && index1 == 5) as i32;
            if val > 0 {
                let v = gb.get(val as u32) as i32;
                let sign = 0 - (v & 1);
                x = (sign ^ ((v >> 1) + x)) - sign;
            }
            dx = x;
            index1 = (index / 6) as usize;
            let mut y = OFFSET_TABLE1[index1];
            val = SIZE_TABLE[index1] - (!self.quarter_sample && index1 == 5) as i32;
            if val > 0 {
                let v = gb.get(val as u32) as i32;
                let sign = 0 - (v & 1);
                y = (sign ^ ((v >> 1) + y)) - sign;
            }
            dy = y;
        }
        Ok((dx, dy, mb_has_coeffs))
    }

    pub fn b_mc_pub(&mut self, direct: bool, mode: i32) {
        self.b_mc(direct, mode)
    }

    fn b_mc(&mut self, direct: bool, mode: i32) {
        if direct {
            self.mc_1mv(0);
            self.interp_mc();
            return;
        }
        if mode == BMV_TYPE_INTERPOLATED {
            self.mc_1mv(0);
            self.interp_mc();
            return;
        }
        self.mc_1mv((mode == BMV_TYPE_BACKWARD) as usize);
    }

    pub fn intra_avail_pub(&mut self, i: usize) {
        self.intra_avail(i)
    }

    fn intra_avail(&mut self, i: usize) {
        self.a_avail = false;
        self.c_avail = false;
        if i == 2 || i == 3 || !self.first_slice_line {
            self.a_avail = self.mb_type_b[self.block_index[i] - self.block_wrap[i]] != 0;
        }
        if i == 1 || i == 3 || self.mb_x != 0 {
            self.c_avail = self.mb_type_b[self.block_index[i] - 1] != 0;
        }
    }

    // ------------------------------------------------------------------ P macroblock

    fn decode_p_mb(&mut self, gb: &mut BitReader) -> R<()> {
        let vl = self.vl;
        let mb_pos = self.mb_x + self.mb_y * self.lay.mb_stride;
        let cbp: i32;
        let mut mquant = self.pq;
        let mut ttmb = self.ttfrm;
        let mut first_block = true;
        let mut block_cbp: u32 = 0;
        let mut block_tt: i32 = 0;
        let mut block_intra: u8 = 0;
        let fourmv = if self.mv_type_is_raw { gb.get1() != 0 } else { self.mv_type_mb_plane[mb_pos] != 0 };
        let skipped = if self.skip_is_raw { gb.get1() != 0 } else { self.mbskip_table[mb_pos] != 0 };
        let bo0 = self.block_index[0];
        let ls = self.linesize;
        let uvls = self.uvlinesize;
        let ci = self.cur_blk_idx;

        if !fourmv {
            if !skipped {
                let (dmv_x, dmv_y, mb_has_coeffs) = self.get_mvdata(gb)?;
                if self.mb_intra {
                    self.set_cmv(1, bo0, [0, 0]);
                }
                self.cur.as_mut().unwrap().mb_type[mb_pos] = if self.mb_intra { MBT_INTRA } else { MBT_16X16 };
                self.pred_mv(gb, 0, dmv_x, dmv_y, true, self.range_x, self.range_y, 0, 0);
                if self.mb_intra && !mb_has_coeffs {
                    self.mquant(gb, &mut mquant);
                    self.ac_pred = gb.get1() != 0;
                    cbp = 0;
                } else if mb_has_coeffs {
                    if self.mb_intra {
                        self.ac_pred = gb.get1() != 0;
                    }
                    cbp = gb.get_vlc(&vl.cbpcy_p[self.cbptab]);
                    if cbp < 0 {
                        return inv("cbpcy");
                    }
                    self.mquant(gb, &mut mquant);
                } else {
                    mquant = self.pq;
                    cbp = 0;
                }
                self.qscale_table[mb_pos] = mquant as i8;
                if !self.ttmbf && !self.mb_intra && mb_has_coeffs {
                    let c = gb.get_vlc(&vl.ttmb[self.tt_index]);
                    if c < 0 {
                        return inv("ttmb");
                    }
                    ttmb = c;
                }
                if !self.mb_intra {
                    self.mc_1mv(0);
                }
                let mut dst_idx = 0usize;
                for i in 0..6usize {
                    let bi = self.block_index[i];
                    self.dc_val[bi] = 0;
                    dst_idx += i >> 2;
                    let val = ((cbp >> (5 - i)) & 1) != 0;
                    let off = if i & 4 != 0 { 0 } else { (i & 1) * 8 + (i & 2) * 4 * ls };
                    self.mb_type_b[bi] = self.mb_intra as u8;
                    if self.mb_intra {
                        self.intra_avail(i);
                        let cs = if i & 4 != 0 { self.codingset2 } else { self.codingset };
                        let mut b = [0i16; 64];
                        self.decode_intra_block(gb, &mut b, i, val, mquant, cs)?;
                        dsp::inv_trans_8x8(&mut b);
                        if self.rangeredfrm {
                            for v in b.iter_mut() {
                                *v = v.wrapping_mul(2);
                            }
                        }
                        self.set_blk(ci, BLOCK_MAP[i], &b);
                        block_cbp |= 0xF << (i << 2);
                        block_intra |= 1 << i;
                    } else if val {
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
            } else {
                self.mb_intra = false;
                for i in 0..6 {
                    let bi = self.block_index[i];
                    self.mb_type_b[bi] = 0;
                    self.dc_val[bi] = 0;
                }
                self.cur.as_mut().unwrap().mb_type[mb_pos] = MBT_SKIP;
                self.qscale_table[mb_pos] = 0;
                self.pred_mv(gb, 0, 0, 0, true, self.range_x, self.range_y, 0, 0);
                self.mc_1mv(0);
            }
        } else {
            // 4MV mode
            if !skipped {
                let mut intra_count = 0;
                let mut coded_inter = false;
                let mut is_intra = [false; 6];
                let mut is_coded = [false; 6];
                cbp = gb.get_vlc(&vl.cbpcy_p[self.cbptab]);
                if cbp < 0 {
                    return inv("cbpcy");
                }
                for i in 0..6usize {
                    let val = ((cbp >> (5 - i)) & 1) != 0;
                    let bi = self.block_index[i];
                    self.dc_val[bi] = 0;
                    self.mb_intra = false;
                    if i < 4 {
                        let (mut dmv_x, mut dmv_y) = (0, 0);
                        self.mb_intra = false;
                        let mut mb_has_coeffs = false;
                        if val {
                            let r = self.get_mvdata(gb)?;
                            dmv_x = r.0;
                            dmv_y = r.1;
                            mb_has_coeffs = r.2;
                        }
                        self.pred_mv(gb, i, dmv_x, dmv_y, false, self.range_x, self.range_y, 0, 0);
                        if !self.mb_intra {
                            self.mc_4mv_luma(i, 0, false);
                        }
                        intra_count += self.mb_intra as i32;
                        is_intra[i] = self.mb_intra;
                        is_coded[i] = mb_has_coeffs;
                    }
                    if i & 4 != 0 {
                        is_intra[i] = intra_count >= 3;
                        is_coded[i] = val;
                    }
                    if i == 4 {
                        self.mc_4mv_chroma(0);
                    }
                    self.mb_type_b[bi] = is_intra[i] as u8;
                    if !coded_inter {
                        coded_inter = !is_intra[i] & is_coded[i];
                    }
                }
                if intra_count == 0 && !coded_inter {
                    return self.p_mb_end(block_cbp, block_tt, block_intra);
                }
                self.mquant(gb, &mut mquant);
                self.qscale_table[mb_pos] = mquant as i8;
                let mut intrapred = false;
                for i in 0..6usize {
                    if is_intra[i] {
                        let bi = self.block_index[i];
                        if ((!self.first_slice_line || i == 2 || i == 3) && self.mb_type_b[bi - self.block_wrap[i]] != 0)
                            || ((self.mb_x != 0 || i == 1 || i == 3) && self.mb_type_b[bi - 1] != 0)
                        {
                            intrapred = true;
                            break;
                        }
                    }
                }
                self.ac_pred = if intrapred { gb.get1() != 0 } else { false };
                if !self.ttmbf && coded_inter {
                    let c = gb.get_vlc(&vl.ttmb[self.tt_index]);
                    if c < 0 {
                        return inv("ttmb");
                    }
                    ttmb = c;
                }
                let mut dst_idx = 0usize;
                for i in 0..6usize {
                    dst_idx += i >> 2;
                    let off = if i & 4 != 0 { 0 } else { (i & 1) * 8 + (i & 2) * 4 * ls };
                    self.mb_intra = is_intra[i];
                    if is_intra[i] {
                        self.intra_avail(i);
                        let cs = if i & 4 != 0 { self.codingset2 } else { self.codingset };
                        let mut b = [0i16; 64];
                        self.decode_intra_block(gb, &mut b, i, is_coded[i], mquant, cs)?;
                        dsp::inv_trans_8x8(&mut b);
                        if self.rangeredfrm {
                            for v in b.iter_mut() {
                                *v = v.wrapping_mul(2);
                            }
                        }
                        self.set_blk(ci, BLOCK_MAP[i], &b);
                        block_cbp |= 0xF << (i << 2);
                        block_intra |= 1 << i;
                    } else if is_coded[i] {
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
            } else {
                self.mb_intra = false;
                self.qscale_table[mb_pos] = 0;
                for i in 0..6 {
                    let bi = self.block_index[i];
                    self.mb_type_b[bi] = 0;
                    self.dc_val[bi] = 0;
                }
                for i in 0..4 {
                    self.pred_mv(gb, i, 0, 0, false, self.range_x, self.range_y, 0, 0);
                    self.mc_4mv_luma(i, 0, false);
                }
                self.mc_4mv_chroma(0);
                self.qscale_table[mb_pos] = 0;
            }
        }
        self.p_mb_end(block_cbp, block_tt, block_intra)
    }

    fn p_mb_end(&mut self, block_cbp: u32, block_tt: i32, block_intra: u8) -> R<()> {
        if self.overlap && self.pq >= 9 {
            self.p_overlap_filter();
        }
        self.put_blocks_clamped(true);
        let ms = self.lay.mb_stride;
        let x = 2 * ms + self.mb_x;
        self.cbp[x] = block_cbp;
        self.ttblk[x] = block_tt;
        self.is_intra[x] = block_intra;
        Ok(())
    }

    // ------------------------------------------------------------------ B macroblock (progressive)

    fn decode_b_mb(&mut self, gb: &mut BitReader) -> R<()> {
        let vl = self.vl;
        let mb_pos = self.mb_x + self.mb_y * self.lay.mb_stride;
        let cbp: i32;
        let mut mquant = self.pq;
        let mut ttmb = self.ttfrm;
        let mut mb_has_coeffs = false;
        let mut first_block = true;
        let mut bmvtype = BMV_TYPE_BACKWARD;
        self.mb_intra = false;
        let direct = if self.dmb_is_raw { gb.get1() != 0 } else { self.direct_mb_plane[mb_pos] != 0 };
        let skipped = if self.skip_is_raw { gb.get1() != 0 } else { self.mbskip_table[mb_pos] != 0 };
        let mut dmv_x = [0i32; 2];
        let mut dmv_y = [0i32; 2];
        for i in 0..6 {
            let bi = self.block_index[i];
            self.mb_type_b[bi] = 0;
            self.dc_val[bi] = 0;
        }
        self.qscale_table[mb_pos] = 0;
        if !direct {
            if !skipped {
                let r = self.get_mvdata(gb)?;
                dmv_x[0] = r.0;
                dmv_y[0] = r.1;
                mb_has_coeffs = r.2;
                dmv_x[1] = dmv_x[0];
                dmv_y[1] = dmv_y[0];
            }
            if skipped || !self.mb_intra {
                bmvtype = gb.decode012() as i32;
                match bmvtype {
                    0 => {
                        bmvtype = if self.bfraction >= B_FRACTION_DEN / 2 { BMV_TYPE_BACKWARD } else { BMV_TYPE_FORWARD };
                    }
                    1 => {
                        bmvtype = if self.bfraction >= B_FRACTION_DEN / 2 { BMV_TYPE_FORWARD } else { BMV_TYPE_BACKWARD };
                    }
                    _ => {
                        bmvtype = BMV_TYPE_INTERPOLATED;
                        dmv_x[0] = 0;
                        dmv_y[0] = 0;
                    }
                }
            }
        }
        for i in 0..6 {
            let bi = self.block_index[i];
            self.mb_type_b[bi] = self.mb_intra as u8;
        }
        if skipped {
            if direct {
                bmvtype = BMV_TYPE_INTERPOLATED;
            }
            self.pred_b_mv(gb, &mut dmv_x, &mut dmv_y, direct, bmvtype);
            self.b_mc(direct, bmvtype);
            return Ok(());
        }
        if direct {
            cbp = gb.get_vlc(&vl.cbpcy_p[self.cbptab]);
            if cbp < 0 {
                return inv("cbpcy");
            }
            self.mquant(gb, &mut mquant);
            self.mb_intra = false;
            self.qscale_table[mb_pos] = mquant as i8;
            if !self.ttmbf {
                let c = gb.get_vlc(&vl.ttmb[self.tt_index]);
                if c < 0 {
                    return inv("ttmb");
                }
                ttmb = c;
            }
            dmv_x = [0; 2];
            dmv_y = [0; 2];
            self.pred_b_mv(gb, &mut dmv_x, &mut dmv_y, direct, bmvtype);
            self.b_mc(direct, bmvtype);
        } else {
            if !mb_has_coeffs && !self.mb_intra {
                self.pred_b_mv(gb, &mut dmv_x, &mut dmv_y, direct, bmvtype);
                self.b_mc(direct, bmvtype);
                return Ok(());
            }
            if self.mb_intra && !mb_has_coeffs {
                self.mquant(gb, &mut mquant);
                self.qscale_table[mb_pos] = mquant as i8;
                self.ac_pred = gb.get1() != 0;
                cbp = 0;
                self.pred_b_mv(gb, &mut dmv_x, &mut dmv_y, direct, bmvtype);
            } else {
                if bmvtype == BMV_TYPE_INTERPOLATED {
                    let r = self.get_mvdata(gb)?;
                    dmv_x[0] = r.0;
                    dmv_y[0] = r.1;
                    mb_has_coeffs = r.2;
                    if !mb_has_coeffs {
                        self.pred_b_mv(gb, &mut dmv_x, &mut dmv_y, direct, bmvtype);
                        self.b_mc(direct, bmvtype);
                        return Ok(());
                    }
                }
                self.pred_b_mv(gb, &mut dmv_x, &mut dmv_y, direct, bmvtype);
                if !self.mb_intra {
                    self.b_mc(direct, bmvtype);
                }
                if self.mb_intra {
                    self.ac_pred = gb.get1() != 0;
                }
                cbp = gb.get_vlc(&vl.cbpcy_p[self.cbptab]);
                if cbp < 0 {
                    return inv("cbpcy");
                }
                self.mquant(gb, &mut mquant);
                self.qscale_table[mb_pos] = mquant as i8;
                if !self.ttmbf && !self.mb_intra && mb_has_coeffs {
                    let c = gb.get_vlc(&vl.ttmb[self.tt_index]);
                    if c < 0 {
                        return inv("ttmb");
                    }
                    ttmb = c;
                }
            }
        }
        let ls = self.linesize;
        let uvls = self.uvlinesize;
        let mut dst_idx = 0usize;
        let mut dummy_tt = 0;
        for i in 0..6usize {
            let bi = self.block_index[i];
            self.dc_val[bi] = 0;
            dst_idx += i >> 2;
            let val = ((cbp >> (5 - i)) & 1) != 0;
            let off = if i & 4 != 0 { 0 } else { (i & 1) * 8 + (i & 2) * 4 * ls };
            self.mb_type_b[bi] = self.mb_intra as u8;
            if self.mb_intra {
                self.intra_avail(i);
                let cs = if i & 4 != 0 { self.codingset2 } else { self.codingset };
                let mut b = [0i16; 64];
                self.decode_intra_block(gb, &mut b, i, val, mquant, cs)?;
                dsp::inv_trans_8x8(&mut b);
                if self.rangeredfrm {
                    for v in b.iter_mut() {
                        *v = v.wrapping_mul(2);
                    }
                }
                let doff = self.dest[dst_idx] + off;
                let stride = if i & 4 != 0 { uvls } else { ls };
                let pic = self.cur.as_mut().unwrap();
                dsp::put_signed_pixels_clamped(&b, &mut pic.planes[dst_idx], doff, stride);
            } else if val {
                let mut b = [0i16; 64];
                let stride = if i & 4 != 0 { uvls } else { ls };
                let doff = self.dest[dst_idx] + off;
                self.decode_p_block(gb, &mut b, i, mquant, ttmb, first_block, dst_idx, doff, stride, &mut dummy_tt)?;
                if !self.ttmbf && ttmb < 8 {
                    ttmb = -1;
                }
                first_block = false;
            }
        }
        Ok(())
    }

    // ------------------------------------------------------------------ I picture (advanced)

    fn decode_i_blocks_adv(&mut self, gb: &mut BitReader) -> R<()> {
        let vl = self.vl;
        if gb.bits_left() <= 1 {
            return inv("empty");
        }
        self.select_coding_sets(self.y_ac_table_index, self.c_ac_table_index);
        self.mb_intra = true;
        self.first_slice_line = true;
        self.mb_x = 0;
        self.mb_y = self.start_mb_y;
        if self.start_mb_y != 0 {
            let bs = self.lay.b8_stride;
            let a = PAD + 2 * self.mb_y * bs - 1;
            for k in 0..(1 + bs) {
                if a + k < self.coded_block.len() {
                    self.coded_block[a + k] = 0;
                }
            }
        }
        let ms = self.lay.mb_stride;
        while self.mb_y < self.end_mb_y {
            self.mb_x = 0;
            while self.mb_x < self.mb_width {
                let mut mquant = self.pq;
                self.set_mb_pos();
                let mb_pos = self.mb_x + self.mb_y * ms;
                let ci = self.cur_blk_idx;
                for k in 0..6 {
                    let o = (ci * 6 + k) * 64;
                    for v in self.block[o..o + 64].iter_mut() {
                        *v = 0;
                    }
                }
                let mo = self.mb_off;
                let bo = self.blocks_off;
                self.cur.as_mut().unwrap().mb_type[mb_pos + mo] = MBT_INTRA;
                for i in 0..4 {
                    let b = self.block_index[i] + bo;
                    self.set_cmv(1, b, [0, 0]);
                }
                if self.fieldtx_is_raw {
                    self.fieldtx_plane[mb_pos] = gb.get1() as u8;
                }
                if gb.bits_left() <= 1 {
                    return inv("overconsumption");
                }
                let mut cbp = gb.get_vlc(&vl.msmp4_mb_i);
                if cbp < 0 {
                    return inv("mb_i cbp");
                }
                self.ac_pred = if self.acpred_is_raw { gb.get1() != 0 } else { self.acpred_plane[mb_pos] != 0 };
                if self.condover == CONDOVER_SELECT && self.overflg_is_raw {
                    self.over_flags_plane[mb_pos] = gb.get1() as u8;
                }
                self.mquant(gb, &mut mquant);
                self.qscale_table[mb_pos] = mquant as i8;
                self.y_dc_scale = t::WMV3_DC_SCALE_TABLE[mquant.unsigned_abs() as usize] as i32;
                self.c_dc_scale = self.y_dc_scale;
                for k in 0..6usize {
                    let bi = self.block_index[k];
                    self.mb_type_b[bi] = 1;
                    let mut val = (cbp >> (5 - k)) & 1;
                    if k < 4 {
                        let (pred, xy) = self.coded_block_pred(k);
                        val ^= pred as i32;
                        self.coded_block[xy] = val as u8;
                    }
                    cbp |= val << (5 - k);
                    self.a_avail = !self.first_slice_line || k == 2 || k == 3;
                    self.c_avail = self.mb_x != 0 || k == 1 || k == 3;
                    let cs = if k < 4 { self.codingset } else { self.codingset2 };
                    let mut b = self.blk(ci, BLOCK_MAP[k]);
                    self.decode_i_block_adv(gb, &mut b, k, val != 0, cs, mquant)?;
                    dsp::inv_trans_8x8(&mut b);
                    self.set_blk(ci, BLOCK_MAP[k], &b);
                }
                if self.overlap && (self.pq >= 9 || self.condover != CONDOVER_NONE) {
                    self.i_overlap_filter();
                }
                self.put_blocks_clamped(true);
                if self.loop_filter {
                    self.i_loop_filter();
                }
                if gb.bits_left() < 0 {
                    return inv("overconsumption");
                }
                self.inc_blk();
                self.mb_x += 1;
            }
            self.first_slice_line = false;
            self.mb_y += 1;
        }
        Ok(())
    }

    fn inc_blk(&mut self) {
        let n = self.n_blks;
        self.topleft_blk_idx = (self.topleft_blk_idx + 1) % n;
        self.top_blk_idx = (self.top_blk_idx + 1) % n;
        self.left_blk_idx = (self.left_blk_idx + 1) % n;
        self.cur_blk_idx = (self.cur_blk_idx + 1) % n;
    }

    fn memmove_rows(&mut self) {
        let ms = self.lay.mb_stride;
        self.cbp.copy_within(ms..3 * ms, 0);
        self.ttblk.copy_within(ms..3 * ms, 0);
        self.is_intra.copy_within(ms..3 * ms, 0);
        self.luma_mv.copy_within(ms..3 * ms, 0);
    }

    fn decode_p_blocks(&mut self, gb: &mut BitReader) -> R<()> {
        self.select_coding_sets(self.c_ac_table_index, self.c_ac_table_index);
        let apply_loop_filter = self.loop_filter;
        self.first_slice_line = true;
        for v in self.cbp.iter_mut() {
            *v = 0;
        }
        self.mb_y = self.start_mb_y;
        while self.mb_y < self.end_mb_y {
            self.mb_x = 0;
            while self.mb_x < self.mb_width {
                self.set_mb_pos();
                if self.fcm == ILACE_FIELD || (self.fcm == PROGRESSIVE && self.mv_type_is_raw) || self.skip_is_raw {
                    if gb.bits_left() <= 1 {
                        return inv("overconsumption");
                    }
                }
                let ret = if self.fcm == ILACE_FIELD {
                    let r = self.decode_p_mb_intfi(gb);
                    if apply_loop_filter {
                        self.p_loop_filter();
                    }
                    r
                } else if self.fcm == ILACE_FRAME {
                    let r = self.decode_p_mb_intfr(gb);
                    if apply_loop_filter {
                        self.p_intfr_loop_filter();
                    }
                    r
                } else {
                    let r = self.decode_p_mb(gb);
                    if apply_loop_filter {
                        self.p_loop_filter();
                    }
                    r
                };
                if ret.is_err() {
                    return ret;
                }
                if gb.bits_left() < 0 {
                    return inv("overconsumption");
                }
                self.inc_blk();
                self.mb_x += 1;
            }
            self.memmove_rows();
            self.first_slice_line = false;
            self.mb_y += 1;
        }
        Ok(())
    }

    fn decode_b_blocks(&mut self, gb: &mut BitReader) -> R<()> {
        self.select_coding_sets(self.c_ac_table_index, self.c_ac_table_index);
        self.first_slice_line = true;
        self.mb_y = self.start_mb_y;
        while self.mb_y < self.end_mb_y {
            self.mb_x = 0;
            while self.mb_x < self.mb_width {
                self.set_mb_pos();
                if self.fcm == ILACE_FIELD || self.skip_is_raw || self.dmb_is_raw {
                    if gb.bits_left() <= 1 {
                        return inv("overconsumption");
                    }
                }
                if self.fcm == ILACE_FIELD {
                    self.decode_b_mb_intfi(gb)?;
                    if self.loop_filter {
                        self.b_intfi_loop_filter();
                    }
                } else if self.fcm == ILACE_FRAME {
                    self.decode_b_mb_intfr(gb)?;
                    if self.loop_filter {
                        self.p_intfr_loop_filter();
                    }
                } else {
                    self.decode_b_mb(gb)?;
                    if self.loop_filter {
                        self.i_loop_filter();
                    }
                }
                if gb.bits_left() < 0 {
                    return inv("overconsumption");
                }
                self.mb_x += 1;
            }
            self.memmove_rows();
            self.first_slice_line = false;
            self.mb_y += 1;
        }
        Ok(())
    }

    fn decode_skip_blocks(&mut self) {
        if self.last.is_none() {
            return;
        }
        // all macroblocks are skipped with zero motion; keep the motion data defined for later
        // B pictures that use this picture as backward reference (direct mode)
        {
            let pic = self.cur.as_mut().unwrap();
            for d in 0..2 {
                for v in pic.motion_val[d].iter_mut() {
                    *v = [0, 0];
                }
            }
            for v in pic.mb_type.iter_mut() {
                *v = MBT_SKIP;
            }
        }
        self.first_slice_line = true;
        self.mb_y = self.start_mb_y;
        while self.mb_y < self.end_mb_y {
            self.mb_x = 0;
            self.set_mb_pos();
            let last = self.last.clone().unwrap();
            let pic = self.cur.as_mut().unwrap();
            for p in 0..3 {
                let (rows, mb_rows) = if p == 0 { (16, self.mb_y) } else { (8, self.mb_y) };
                let stride = pic.stride[p];
                let a = mb_rows * rows * stride;
                let n = stride * rows;
                if a + n <= pic.planes[p].len() && a + n <= last.planes[p].len() {
                    pic.planes[p][a..a + n].copy_from_slice(&last.planes[p][a..a + n]);
                }
            }
            self.first_slice_line = false;
            self.mb_y += 1;
        }
    }

    /// ff_vc1_decode_blocks
    pub fn decode_blocks(&mut self, gb: &mut BitReader) -> R<()> {
        self.esc3_level_length = 0;
        self.cur_blk_idx = 0;
        self.left_blk_idx = self.n_blks - 1;
        self.topleft_blk_idx = 1 % self.n_blks;
        self.top_blk_idx = 2 % self.n_blks;
        match self.pict_type {
            PT_I => self.decode_i_blocks_adv(gb),
            PT_P => {
                if self.p_frame_skipped {
                    self.decode_skip_blocks();
                    Ok(())
                } else {
                    self.decode_p_blocks(gb)
                }
            }
            PT_B => {
                if self.bi_type {
                    self.decode_i_blocks_adv(gb)
                } else {
                    self.decode_b_blocks(gb)
                }
            }
            _ => Ok(()),
        }
    }
}
