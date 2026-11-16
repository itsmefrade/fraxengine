//! Block and macroblock level decoding (FFmpeg vc1_block.c), progressive paths.
use crate::bits::BitReader;
use crate::core::*;
use crate::dsp;
use crate::tables as t;

pub const BLOCK_MAP: [usize; 6] = [0, 2, 1, 3, 4, 5];
#[inline]
fn dq(q: i32) -> u32 {
    t::DQSCALE[(q - 1) as usize] as u32
}

impl Core {
    // -------------------------------------------------------------- mb position bookkeeping

    /// Equivalent of init_block_index + update_block_index for the current (mb_x, mb_y).
    pub fn set_mb_pos(&mut self) {
        let bs = self.lay.b8_stride;
        let cs = self.lay.mb_stride;
        let lsize = self.lay.lsize;
        let csize = self.lay.csize;
        let (x, y) = (self.mb_x, self.mb_y);
        let b0 = PAD + (2 * y + 1) * bs + 2 * x + 1;
        self.block_index = [b0, b0 + 1, b0 + bs, b0 + bs + 1, PAD + lsize + (y + 1) * cs + x + 1, 0];
        self.block_index[5] = self.block_index[4] + csize;
        self.block_wrap = [bs, bs, bs, bs, cs, cs];
        let ls = self.linesize;
        let uvls = self.uvlinesize;
        let mut d0 = y * 16 * ls + x * 16;
        let mut d1 = y * 8 * uvls + x * 8;
        if self.field_mode && !(self.second_field ^ self.tff) {
            let pic = self.cur.as_ref().unwrap();
            d0 += pic.stride[0];
            d1 += pic.stride[1];
        }
        self.dest = [d0, d1, d1];
    }

    pub fn get_mquant(&mut self, gb: &mut BitReader, mquant: &mut i32) {
        if !self.dquantfrm {
            return;
        }
        let mut edges = 0;
        if self.dqprofile == DQPROFILE_ALL_MBS {
            if self.dqbilevel {
                *mquant = if gb.get1() != 0 { -self.altpq } else { self.pq };
            } else {
                let mqdiff = gb.get(3) as i32;
                if mqdiff != 7 {
                    *mquant = -self.pq - mqdiff;
                } else {
                    *mquant = -(gb.get(5) as i32);
                }
            }
        }
        if self.dqprofile == DQPROFILE_SINGLE_EDGE {
            edges = 1 << self.dqsbedge;
        } else if self.dqprofile == DQPROFILE_DOUBLE_EDGES {
            edges = (3 << self.dqsbedge) % 15;
        } else if self.dqprofile == DQPROFILE_FOUR_EDGES {
            edges = 15;
        }
        if edges & 1 != 0 && self.mb_x == 0 {
            *mquant = -self.altpq;
        }
        if edges & 2 != 0 && self.mb_y == 0 {
            *mquant = -self.altpq;
        }
        if edges & 4 != 0 && self.mb_x == self.mb_width - 1 {
            *mquant = -self.altpq;
        }
        if edges & 8 != 0 && self.mb_y == (self.mb_height >> self.field_mode as usize) - 1 {
            *mquant = -self.altpq;
        }
        if *mquant == 0 || *mquant > 31 || *mquant < -31 {
            *mquant = 1;
        }
    }

    // -------------------------------------------------------------- coefficient parsing

    fn decode_ac_coeff(&mut self, gb: &mut BitReader, codingset: usize) -> R<(bool, usize, i32)> {
        let vl = self.vl;
        let size = t::AC_SIZES[codingset];
        let index = gb.get_vlc(&vl.ac[codingset]);
        if index < 0 {
            return inv("ac vlc");
        }
        let (mut run, mut level, lst, sign);
        let cs = codingset;
        if index != size - 1 {
            run = t::INDEX_DECODE_TABLE[(cs * 185 + index as usize) * 2] as i32;
            level = t::INDEX_DECODE_TABLE[(cs * 185 + index as usize) * 2 + 1] as i32;
            lst = index >= t::LAST_DECODE_TABLE[cs] || gb.bits_left() < 0;
            sign = gb.get1() as i32;
        } else {
            let escape = gb.decode210();
            if escape != 2 {
                let index = gb.get_vlc(&vl.ac[codingset]);
                if index < 0 || index >= size - 1 {
                    return inv("ac escape");
                }
                run = t::INDEX_DECODE_TABLE[(cs * 185 + index as usize) * 2] as i32;
                level = t::INDEX_DECODE_TABLE[(cs * 185 + index as usize) * 2 + 1] as i32;
                lst = index >= t::LAST_DECODE_TABLE[cs];
                if escape == 0 {
                    if lst {
                        level += t::LAST_DELTA_LEVEL_TABLE[cs * 44 + run as usize] as i32;
                    } else {
                        level += t::DELTA_LEVEL_TABLE[cs * 31 + run as usize] as i32;
                    }
                } else if lst {
                    run += t::LAST_DELTA_RUN_TABLE[cs * 10 + level as usize] as i32 + 1;
                } else {
                    run += t::DELTA_RUN_TABLE[cs * 57 + level as usize] as i32 + 1;
                }
                sign = gb.get1() as i32;
            } else {
                lst = gb.get1() != 0;
                if self.esc3_level_length == 0 {
                    if self.pq < 8 || self.dquantfrm {
                        self.esc3_level_length = gb.get(3);
                        if self.esc3_level_length == 0 {
                            self.esc3_level_length = gb.get(2) + 8;
                        }
                    } else {
                        self.esc3_level_length = gb.unary(1, 6) + 2;
                    }
                    self.esc3_run_length = 3 + gb.get(2);
                }
                run = gb.get(self.esc3_run_length) as i32;
                sign = gb.get1() as i32;
                level = gb.get(self.esc3_level_length) as i32;
            }
        }
        Ok((lst, run as usize, (level ^ -sign) + sign))
    }

    fn pred_dc(&mut self, n: usize, a_avail: bool, c_avail: bool) -> (i32, usize, bool) {
        let mb_pos = self.mb_x + self.mb_y * self.lay.mb_stride;
        let q1 = (self.qscale_table[mb_pos] as i32).abs();
        let dqscale_index = t::WMV3_DC_SCALE_TABLE[q1 as usize] as i32 - 1;
        let bi = self.block_index[n];
        if dqscale_index < 0 {
            return (0, bi, true);
        }
        let wrap = self.block_wrap[n];
        let mut c = self.dc_val[bi - 1] as i32;
        let mut b = self.dc_val[bi - 1 - wrap] as i32;
        let mut a = self.dc_val[bi - wrap] as i32;
        let ms = self.lay.mb_stride;
        let dqs = t::DQSCALE[dqscale_index as usize] as u32;
        let sc = |v: i32, q2: i32| -> i32 {
            ((v as u32).wrapping_mul(t::WMV3_DC_SCALE_TABLE[q2 as usize] as u32).wrapping_mul(dqs).wrapping_add(0x20000) as i32) >> 18
        };
        if c_avail && n != 1 && n != 3 {
            let q2 = (self.qscale_table[mb_pos - 1] as i32).abs();
            if q2 != 0 && q2 != q1 {
                c = sc(c, q2);
            }
        }
        if a_avail && n != 2 && n != 3 {
            let q2 = (self.qscale_table[mb_pos - ms] as i32).abs();
            if q2 != 0 && q2 != q1 {
                a = sc(a, q2);
            }
        }
        if a_avail && c_avail && n != 3 {
            let mut off = mb_pos;
            if n != 1 {
                off -= 1;
            }
            if n != 2 {
                off -= ms;
            }
            let q2 = (self.qscale_table[off] as i32).abs();
            if q2 != 0 && q2 != q1 {
                b = sc(b, q2);
            }
        }
        if c_avail && (!a_avail || (a - b).abs() <= (b - c).abs()) {
            (c, bi, true)
        } else if a_avail {
            (a, bi, false)
        } else {
            (0, bi, true)
        }
    }

    pub fn coded_block_pred(&self, n: usize) -> (u8, usize) {
        let xy = self.block_index[n];
        let wrap = self.lay.b8_stride;
        let a = self.coded_block[xy - 1];
        let b = self.coded_block[xy - 1 - wrap];
        let c = self.coded_block[xy - wrap];
        (if b == c { a } else { c }, xy)
    }

    fn read_dcdiff(&mut self, gb: &mut BitReader, n: usize, quant: i32) -> R<i32> {
        let vl = self.vl;
        let mut dcdiff = gb.get_vlc(&vl.msmp4_dc[self.dc_table_index * 2 + (n >= 4) as usize]);
        if dcdiff < 0 {
            return inv("dc vlc");
        }
        if dcdiff != 0 {
            let m = if quant == 1 || quant == 2 { 3 - quant } else { 0 } as u32;
            if dcdiff == 119 {
                dcdiff = gb.get(8 + m) as i32;
            } else if m != 0 {
                dcdiff = (dcdiff << m) + gb.get(m) as i32 - ((1 << m) - 1);
            }
            if gb.get1() != 0 {
                dcdiff = -dcdiff;
            }
        }
        Ok(dcdiff)
    }

    /// vc1_decode_i_block_adv
    pub fn decode_i_block_adv(
        &mut self,
        gb: &mut BitReader,
        block: &mut [i16; 64],
        n: usize,
        coded: bool,
        codingset: usize,
        mquant: i32,
    ) -> R<()> {
        let a_avail = self.a_avail;
        let c_avail = self.c_avail;
        let mut use_pred = self.ac_pred;
        let mb_pos = self.mb_x + self.mb_y * self.lay.mb_stride;
        let quant = mquant.abs();
        let dcdiff0 = self.read_dcdiff(gb, n, quant)?;
        let (pred, dc_idx, dir) = self.pred_dc(n, a_avail, c_avail);
        let dcdiff = dcdiff0 + pred;
        let dc_pred_dir = dir;
        self.dc_val[dc_idx] = dcdiff as i16;
        let scale_dc = if n < 4 { self.y_dc_scale } else { self.c_dc_scale };
        block[0] = (dcdiff * scale_dc) as i16;
        if !a_avail && !c_avail {
            use_pred = false;
        }
        let scale = quant * 2 + if mquant < 0 { 0 } else { self.halfpq };
        let bi = self.block_index[n];
        let wrap = self.block_wrap[n];
        let ac2 = bi;
        let ac_src = if dc_pred_dir { bi - 1 } else { bi - wrap };
        let mut q1 = self.qscale_table[mb_pos] as i32;
        let mut q2 = 0i32;
        if n == 3 {
            q2 = q1;
        } else if dc_pred_dir {
            if n == 1 {
                q2 = q1;
            } else if c_avail && mb_pos != 0 {
                q2 = self.qscale_table[mb_pos - 1] as i32;
            }
        } else if n == 2 {
            q2 = q1;
        } else if a_avail && mb_pos >= self.lay.mb_stride {
            q2 = self.qscale_table[mb_pos - self.lay.mb_stride] as i32;
        }
        let mut i = 1usize;
        let lsh = self.left_blk_sh;
        let tsh = self.top_blk_sh;
        if coded {
            let mut last = false;
            let zz: &[u8; 64] = if self.ac_pred {
                if !use_pred && self.fcm == ILACE_FRAME {
                    &self.zzi_8x8
                } else if !dc_pred_dir {
                    &self.zz_8x8[2]
                } else {
                    &self.zz_8x8[3]
                }
            } else if self.fcm != ILACE_FRAME {
                &self.zz_8x8[1]
            } else {
                &self.zzi_8x8
            };
            let zz = *zz;
            while !last {
                let (l, skip, value) = self.decode_ac_coeff(gb, codingset)?;
                last = l;
                i += skip;
                if i > 63 {
                    break;
                }
                block[zz[i] as usize] = value as i16;
                i += 1;
            }
            if use_pred {
                let (sh, off) = if dc_pred_dir { (lsh, 0usize) } else { (tsh, 8usize) };
                q1 = q1.abs() * 2 + if q1 < 0 { 0 } else { self.halfpq } - 1;
                if q1 < 1 {
                    return inv("q1");
                }
                if q2 != 0 {
                    q2 = q2.abs() * 2 + if q2 < 0 { 0 } else { self.halfpq } - 1;
                }
                if q2 != 0 && q1 != q2 {
                    for k in 1..8 {
                        let av = self.ac_val[ac_src][off + k] as i32;
                        let add = ((av as u32).wrapping_mul(q2 as u32).wrapping_mul(dq(q1)).wrapping_add(0x20000) as i32) >> 18;
                        block[k << sh] = (block[k << sh] as i32 + add) as i16;
                    }
                } else {
                    for k in 1..8 {
                        block[k << sh] = block[k << sh].wrapping_add(self.ac_val[ac_src][off + k]);
                    }
                }
            }
            for k in 1..8 {
                self.ac_val[ac2][k] = block[k << lsh];
                self.ac_val[ac2][k + 8] = block[k << tsh];
            }
            for k in 1..64 {
                if block[k] != 0 {
                    let mut v = (block[k] as i32 * scale) as i16;
                    if !self.pquantizer {
                        v = (v as i32 + if v < 0 { -quant } else { quant }) as i16;
                    }
                    block[k] = v;
                }
            }
        } else {
            self.ac_val[ac2] = [0; 16];
            if use_pred {
                let (sh, off) = if dc_pred_dir { (lsh, 0usize) } else { (tsh, 8usize) };
                for k in 0..8 {
                    self.ac_val[ac2][off + k] = self.ac_val[ac_src][off + k];
                }
                q1 = q1.abs() * 2 + if q1 < 0 { 0 } else { self.halfpq } - 1;
                if q1 < 1 {
                    return inv("q1");
                }
                if q2 != 0 {
                    q2 = q2.abs() * 2 + if q2 < 0 { 0 } else { self.halfpq } - 1;
                }
                if q2 != 0 && q1 != q2 {
                    for k in 1..8 {
                        let av = self.ac_val[ac2][off + k] as i32;
                        self.ac_val[ac2][off + k] =
                            (((av as u32).wrapping_mul(q2 as u32).wrapping_mul(dq(q1)).wrapping_add(0x20000) as i32) >> 18) as i16;
                    }
                }
                for k in 1..8 {
                    let mut v = (self.ac_val[ac2][off + k] as i32 * scale) as i16;
                    if !self.pquantizer && v != 0 {
                        v = (v as i32 + if v < 0 { -quant } else { quant }) as i16;
                    }
                    block[k << sh] = v;
                }
                i = 63;
            }
        }
        if use_pred && coded {
            i = 63;
        }
        let _ = i;
        Ok(())
    }

    /// vc1_decode_intra_block (intra blocks in P/B pictures)
    pub fn decode_intra_block(
        &mut self,
        gb: &mut BitReader,
        block: &mut [i16; 64],
        n: usize,
        coded: bool,
        mquant: i32,
        codingset: usize,
    ) -> R<()> {
        let mb_pos = self.mb_x + self.mb_y * self.lay.mb_stride;
        let a_avail = self.a_avail;
        let c_avail = self.c_avail;
        let mut use_pred = self.ac_pred;
        let mut quant = mquant.abs();
        *block = [0; 64];
        quant = quant.clamp(0, 31);
        self.y_dc_scale = t::WMV3_DC_SCALE_TABLE[quant as usize] as i32;
        self.c_dc_scale = self.y_dc_scale;
        let dcdiff0 = self.read_dcdiff(gb, n, quant)?;
        let (pred, dc_idx, dir) = self.pred_dc(n, a_avail, c_avail);
        let dcdiff = dcdiff0 + pred;
        let mut dc_pred_dir = dir;
        self.dc_val[dc_idx] = dcdiff as i16;
        block[0] = (dcdiff * if n < 4 { self.y_dc_scale } else { self.c_dc_scale }) as i16;
        let mut i = 1usize;
        if !a_avail {
            dc_pred_dir = true;
        }
        if !c_avail {
            dc_pred_dir = false;
        }
        if !a_avail && !c_avail {
            use_pred = false;
        }
        let bi = self.block_index[n];
        let wrap = self.block_wrap[n];
        let ac2 = bi;
        let ac_src = if dc_pred_dir { bi - 1 } else { bi - wrap };
        let scale = quant * 2 + if mquant < 0 { 0 } else { self.halfpq };
        let mut q1 = self.qscale_table[mb_pos] as i32;
        let mut q2 = 0i32;
        if dc_pred_dir && c_avail && mb_pos != 0 {
            q2 = self.qscale_table[mb_pos - 1] as i32;
        }
        if !dc_pred_dir && a_avail && mb_pos >= self.lay.mb_stride {
            q2 = self.qscale_table[mb_pos - self.lay.mb_stride] as i32;
        }
        if dc_pred_dir && n == 1 {
            q2 = q1;
        }
        if !dc_pred_dir && n == 2 {
            q2 = q1;
        }
        if n == 3 {
            q2 = q1;
        }
        let lsh = self.left_blk_sh;
        let tsh = self.top_blk_sh;
        if coded {
            let mut last = false;
            while !last {
                let (l, skip, value) = self.decode_ac_coeff(gb, codingset)?;
                last = l;
                i += skip;
                if i > 63 {
                    break;
                }
                if self.fcm == PROGRESSIVE {
                    block[self.zz_8x8[0][i] as usize] = value as i16;
                } else if use_pred && self.fcm == ILACE_FRAME {
                    if !dc_pred_dir {
                        block[self.zz_8x8[2][i] as usize] = value as i16;
                    } else {
                        block[self.zz_8x8[3][i] as usize] = value as i16;
                    }
                } else {
                    block[self.zzi_8x8[i] as usize] = value as i16;
                }
                i += 1;
            }
            if use_pred {
                q1 = q1.abs() * 2 + if q1 < 0 { 0 } else { self.halfpq } - 1;
                if q1 < 1 {
                    return inv("q1");
                }
                if q2 != 0 {
                    q2 = q2.abs() * 2 + if q2 < 0 { 0 } else { self.halfpq } - 1;
                }
                let (sh, off) = if dc_pred_dir { (lsh, 0usize) } else { (tsh, 8usize) };
                if q2 != 0 && q1 != q2 {
                    for k in 1..8 {
                        let av = self.ac_val[ac_src][off + k] as i32;
                        let add = ((av as u32).wrapping_mul(q2 as u32).wrapping_mul(dq(q1)).wrapping_add(0x20000) as i32) >> 18;
                        block[k << sh] = (block[k << sh] as i32 + add) as i16;
                    }
                } else {
                    for k in 1..8 {
                        block[k << sh] = block[k << sh].wrapping_add(self.ac_val[ac_src][off + k]);
                    }
                }
            }
            for k in 1..8 {
                self.ac_val[ac2][k] = block[k << lsh];
                self.ac_val[ac2][k + 8] = block[k << tsh];
            }
            for k in 1..64 {
                if block[k] != 0 {
                    let mut v = (block[k] as i32 * scale) as i16;
                    if !self.pquantizer {
                        v = (v as i32 + if v < 0 { -quant } else { quant }) as i16;
                    }
                    block[k] = v;
                }
            }
        } else {
            self.ac_val[ac2] = [0; 16];
            if use_pred {
                let (sh, off) = if dc_pred_dir { (lsh, 0usize) } else { (tsh, 8usize) };
                for k in 0..8 {
                    self.ac_val[ac2][off + k] = self.ac_val[ac_src][off + k];
                }
                q1 = q1.abs() * 2 + if q1 < 0 { 0 } else { self.halfpq } - 1;
                if q1 < 1 {
                    return inv("q1");
                }
                if q2 != 0 {
                    q2 = q2.abs() * 2 + if q2 < 0 { 0 } else { self.halfpq } - 1;
                }
                if q2 != 0 && q1 != q2 {
                    for k in 1..8 {
                        let av = self.ac_val[ac2][off + k] as i32;
                        self.ac_val[ac2][off + k] =
                            (((av as u32).wrapping_mul(q2 as u32).wrapping_mul(dq(q1)).wrapping_add(0x20000) as i32) >> 18) as i16;
                    }
                }
                for k in 1..8 {
                    let mut v = (self.ac_val[ac2][off + k] as i32 * scale) as i16;
                    if !self.pquantizer && v != 0 {
                        v = (v as i32 + if v < 0 { -quant } else { quant }) as i16;
                    }
                    block[k << sh] = v;
                }
            }
        }
        Ok(())
    }

    /// vc1_decode_p_block: decodes one inter block and adds the residual into the picture.
    #[allow(clippy::too_many_arguments)]
    pub fn decode_p_block(
        &mut self,
        gb: &mut BitReader,
        block: &mut [i16; 64],
        n: usize,
        mquant: i32,
        ttmb: i32,
        first_block: bool,
        plane: usize,
        doff: usize,
        linesize: usize,
        ttmb_out: &mut i32,
    ) -> R<i32> {
        let vl = self.vl;
        *block = [0; 64];
        let quant = mquant.abs();
        let mut ttblk = ttmb & 7;
        let mut subblkpat: i32 = 0;
        if ttmb == -1 {
            let c = gb.get_vlc(&vl.ttblk[self.tt_index]);
            if c < 0 {
                return inv("ttblk");
            }
            ttblk = TTBLK_TO_TT[self.tt_index][c as usize];
        }
        if ttblk == TT_4X4 {
            let c = gb.get_vlc(&vl.subblkpat[self.tt_index]);
            if c < 0 {
                return inv("subblkpat");
            }
            subblkpat = !(c + 1);
        }
        if ttblk != TT_8X8
            && ttblk != TT_4X4
            && ((self.ttmbf || (ttmb != -1 && (ttmb & 8) != 0 && !first_block)) || (!self.res_rtm_flag && !first_block))
        {
            subblkpat = gb.decode012() as i32;
            if subblkpat != 0 {
                subblkpat ^= 3;
            }
            if ttblk == TT_8X4_TOP || ttblk == TT_8X4_BOTTOM {
                ttblk = TT_8X4;
            }
            if ttblk == TT_4X8_RIGHT || ttblk == TT_4X8_LEFT {
                ttblk = TT_4X8;
            }
        }
        let scale = quant * 2 + if mquant < 0 { 0 } else { self.halfpq };
        if ttblk == TT_8X4_TOP || ttblk == TT_8X4_BOTTOM {
            subblkpat = 2 - (ttblk == TT_8X4_TOP) as i32;
            ttblk = TT_8X4;
        }
        if ttblk == TT_4X8_RIGHT || ttblk == TT_4X8_LEFT {
            subblkpat = 2 - (ttblk == TT_4X8_LEFT) as i32;
            ttblk = TT_4X8;
        }
        let pat;
        let pq_uniform = self.pquantizer;
        let fcm = self.fcm;
        let codingset2 = self.codingset2;
        let mut pic = self.cur.take().unwrap();
        let dst: &mut [u8] = &mut pic.planes[plane];
        let mut res: R<()> = Ok(());
        macro_rules! store {
            ($idx:expr, $value:expr) => {{
                let idx = $idx;
                let mut v = ($value * scale) as i16;
                if !pq_uniform {
                    v = (v as i32 + if v < 0 { -quant } else { quant }) as i16;
                }
                block[idx] = v;
            }};
        }
        match ttblk {
            TT_8X8 => {
                pat = 0xF;
                let mut i = 0usize;
                let mut last = false;
                while !last {
                    match self.decode_ac_coeff(gb, codingset2) {
                        Err(e) => {
                            res = Err(e);
                            break;
                        }
                        Ok((l, skip, value)) => {
                            last = l;
                            i += skip;
                            if i > 63 {
                                break;
                            }
                            let idx = if fcm == PROGRESSIVE { self.zz_8x8[0][i] } else { self.zzi_8x8[i] } as usize;
                            i += 1;
                            store!(idx, value);
                        }
                    }
                }
                if res.is_ok() {
                    if i == 1 {
                        dsp::inv_trans_8x8_dc(dst, doff, linesize, block);
                    } else {
                        dsp::inv_trans_8x8(block);
                        dsp::add_pixels_clamped(block, dst, doff, linesize);
                    }
                }
            }
            TT_4X4 => {
                pat = !subblkpat & 0xF;
                for j in 0..4usize {
                    let mut last = subblkpat & (1 << (3 - j)) != 0;
                    let mut i = 0usize;
                    let off = (j & 1) * 4 + (j & 2) * 16;
                    while !last {
                        match self.decode_ac_coeff(gb, codingset2) {
                            Err(e) => {
                                res = Err(e);
                                break;
                            }
                            Ok((l, skip, value)) => {
                                last = l;
                                i += skip;
                                if i > 15 {
                                    break;
                                }
                                let idx = if fcm == PROGRESSIVE {
                                    t::SIMPLE_PROGRESSIVE_4X4_ZZ[i]
                                } else {
                                    t::ADV_INTERLACED_4X4_ZZ[i]
                                } as usize;
                                i += 1;
                                store!(idx + off, value);
                            }
                        }
                    }
                    if res.is_err() {
                        break;
                    }
                    if subblkpat & (1 << (3 - j)) == 0 {
                        let o = doff + (j & 1) * 4 + (j & 2) * 2 * linesize;
                        if i == 1 {
                            dsp::inv_trans_4x4_dc(dst, o, linesize, &block[off..]);
                        } else {
                            dsp::inv_trans_4x4(dst, o, linesize, &mut block[off..]);
                        }
                    }
                }
            }
            TT_8X4 => {
                pat = !((subblkpat & 2) * 6 + (subblkpat & 1) * 3) & 0xF;
                for j in 0..2usize {
                    let mut last = subblkpat & (1 << (1 - j)) != 0;
                    let mut i = 0usize;
                    let off = j * 32;
                    while !last {
                        match self.decode_ac_coeff(gb, codingset2) {
                            Err(e) => {
                                res = Err(e);
                                break;
                            }
                            Ok((l, skip, value)) => {
                                last = l;
                                i += skip;
                                if i > 31 {
                                    break;
                                }
                                let idx = if fcm == PROGRESSIVE {
                                    t::ADV_PROGRESSIVE_8X4_ZZ[i]
                                } else {
                                    t::ADV_INTERLACED_8X4_ZZ[i]
                                } as usize
                                    + off;
                                i += 1;
                                store!(idx, value);
                            }
                        }
                    }
                    if res.is_err() {
                        break;
                    }
                    if subblkpat & (1 << (1 - j)) == 0 {
                        let o = doff + j * 4 * linesize;
                        if i == 1 {
                            dsp::inv_trans_8x4_dc(dst, o, linesize, &block[off..]);
                        } else {
                            dsp::inv_trans_8x4(dst, o, linesize, &mut block[off..]);
                        }
                    }
                }
            }
            _ => {
                // TT_4X8
                pat = !(subblkpat * 5) & 0xF;
                for j in 0..2usize {
                    let mut last = subblkpat & (1 << (1 - j)) != 0;
                    let mut i = 0usize;
                    let off = j * 4;
                    while !last {
                        match self.decode_ac_coeff(gb, codingset2) {
                            Err(e) => {
                                res = Err(e);
                                break;
                            }
                            Ok((l, skip, value)) => {
                                last = l;
                                i += skip;
                                if i > 31 {
                                    break;
                                }
                                let idx = if fcm == PROGRESSIVE {
                                    t::ADV_PROGRESSIVE_4X8_ZZ[i]
                                } else {
                                    t::ADV_INTERLACED_4X8_ZZ[i]
                                } as usize
                                    + off;
                                i += 1;
                                store!(idx, value);
                            }
                        }
                    }
                    if res.is_err() {
                        break;
                    }
                    if subblkpat & (1 << (1 - j)) == 0 {
                        let o = doff + j * 4;
                        if i == 1 {
                            dsp::inv_trans_4x8_dc(dst, o, linesize, &block[off..]);
                        } else {
                            dsp::inv_trans_4x8(dst, o, linesize, &mut block[off..]);
                        }
                    }
                }
            }
        }
        self.cur = Some(pic);
        res?;
        *ttmb_out |= ttblk << (n * 4);
        Ok(pat)
    }
}

pub const TTBLK_TO_TT: [[i32; 8]; 3] = [
    [TT_8X4, TT_4X8, TT_8X8, TT_4X4, TT_8X4_TOP, TT_8X4_BOTTOM, TT_4X8_RIGHT, TT_4X8_LEFT],
    [TT_8X8, TT_4X8_RIGHT, TT_4X8_LEFT, TT_4X4, TT_8X4, TT_4X8, TT_8X4_BOTTOM, TT_8X4_TOP],
    [TT_8X8, TT_4X8, TT_4X4, TT_8X4_BOTTOM, TT_4X8_RIGHT, TT_4X8_LEFT, TT_8X4, TT_8X4_TOP],
];
