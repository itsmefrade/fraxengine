//! Motion vector prediction (FFmpeg vc1_pred.c).
use crate::bits::BitReader;
use crate::core::*;
use crate::tables as t;

#[inline]
pub fn mid_pred(a: i32, b: i32, c: i32) -> i32 {
    a.max(b).min(a.min(b).max(c))
}

#[inline]
pub fn scale_mv(value: i32, bfrac: i32, inv: bool, qs: bool) -> i32 {
    let mut n = bfrac;
    if inv {
        n -= 256;
    }
    if !qs {
        return 2 * ((value * n + 255) >> 9);
    }
    (value * n + 128) >> 8
}

impl Core {
    #[inline]
    pub fn cmv(&self, dir: usize, idx: usize) -> [i16; 2] {
        self.cur.as_ref().unwrap().motion_val[dir][idx]
    }
    #[inline]
    pub fn set_cmv(&mut self, dir: usize, idx: usize, v: [i16; 2]) {
        self.cur.as_mut().unwrap().motion_val[dir][idx] = v;
    }

    fn scaleforsame_x(&self, n: i32, dir: usize) -> i32 {
        let table_index = dir ^ self.second_field as usize;
        let mut refdist = if self.pict_type != PT_B {
            self.refdist
        } else if dir != 0 {
            self.brfd
        } else {
            self.frfd
        };
        if refdist > 3 {
            refdist = 3;
        }
        let rd = refdist as usize;
        let sc = |row: usize| t::FIELD_MVPRED_SCALES[table_index * 28 + row * 4 + rd] as i32;
        let scalesame1 = sc(1);
        let scalesame2 = sc(2);
        let scalezone1_x = sc(3);
        let zone1offset_x = sc(5);
        let scaledvalue = if n.abs() > 255 {
            n
        } else if n.abs() < scalezone1_x {
            (n * scalesame1) >> 8
        } else if n < 0 {
            ((n * scalesame2) >> 8) - zone1offset_x
        } else {
            ((n * scalesame2) >> 8) + zone1offset_x
        };
        scaledvalue.clamp(-self.range_x, self.range_x - 1)
    }

    fn scaleforsame_y(&self, n: i32, dir: usize) -> i32 {
        let table_index = dir ^ self.second_field as usize;
        let mut refdist = if self.pict_type != PT_B {
            self.refdist
        } else if dir != 0 {
            self.brfd
        } else {
            self.frfd
        };
        if refdist > 3 {
            refdist = 3;
        }
        let rd = refdist as usize;
        let sc = |row: usize| t::FIELD_MVPRED_SCALES[table_index * 28 + row * 4 + rd] as i32;
        let scalesame1 = sc(1);
        let scalesame2 = sc(2);
        let scalezone1_y = sc(4);
        let zone1offset_y = sc(6);
        let scaledvalue = if n.abs() > 63 {
            n
        } else if n.abs() < scalezone1_y {
            (n * scalesame1) >> 8
        } else if n < 0 {
            ((n * scalesame2) >> 8) - zone1offset_y
        } else {
            ((n * scalesame2) >> 8) + zone1offset_y
        };
        if self.cur_field_type != 0 && self.ref_field_type[dir] == 0 {
            scaledvalue.clamp(-self.range_y / 2 + 1, self.range_y / 2)
        } else {
            scaledvalue.clamp(-self.range_y / 2, self.range_y / 2 - 1)
        }
    }

    fn scaleforopp_x(&self, n: i32) -> i32 {
        let brfd = self.brfd.min(3) as usize;
        let sc = |row: usize| t::B_FIELD_MVPRED_SCALES[row * 4 + brfd] as i32;
        let scalezone1_x = sc(3);
        let zone1offset_x = sc(5);
        let scaleopp1 = sc(1);
        let scaleopp2 = sc(2);
        let scaledvalue = if n.abs() > 255 {
            n
        } else if n.abs() < scalezone1_x {
            (n * scaleopp1) >> 8
        } else if n < 0 {
            ((n * scaleopp2) >> 8) - zone1offset_x
        } else {
            ((n * scaleopp2) >> 8) + zone1offset_x
        };
        scaledvalue.clamp(-self.range_x, self.range_x - 1)
    }

    fn scaleforopp_y(&self, n: i32, dir: usize) -> i32 {
        let brfd = self.brfd.min(3) as usize;
        let sc = |row: usize| t::B_FIELD_MVPRED_SCALES[row * 4 + brfd] as i32;
        let scalezone1_y = sc(4);
        let zone1offset_y = sc(6);
        let scaleopp1 = sc(1);
        let scaleopp2 = sc(2);
        let scaledvalue = if n.abs() > 63 {
            n
        } else if n.abs() < scalezone1_y {
            (n * scaleopp1) >> 8
        } else if n < 0 {
            ((n * scaleopp2) >> 8) - zone1offset_y
        } else {
            ((n * scaleopp2) >> 8) + zone1offset_y
        };
        if self.cur_field_type != 0 && self.ref_field_type[dir] == 0 {
            scaledvalue.clamp(-self.range_y / 2 + 1, self.range_y / 2)
        } else {
            scaledvalue.clamp(-self.range_y / 2, self.range_y / 2 - 1)
        }
    }

    fn scaleforsame(&self, n: i32, dim: usize, dir: usize) -> i32 {
        let hpel = 1 - self.quarter_sample as i32;
        let n = n >> hpel;
        if self.pict_type != PT_B || self.second_field || dir == 0 {
            return if dim != 0 { self.scaleforsame_y(n, dir) * (1 << hpel) } else { self.scaleforsame_x(n, dir) * (1 << hpel) };
        }
        let brfd = self.brfd.min(3) as usize;
        let scalesame = t::B_FIELD_MVPRED_SCALES[brfd] as i32;
        ((n * scalesame) >> 8) * (1 << hpel)
    }

    fn scaleforopp(&self, n: i32, dim: usize, dir: usize) -> i32 {
        let hpel = 1 - self.quarter_sample as i32;
        let n = n >> hpel;
        if self.pict_type == PT_B && !self.second_field && dir == 1 {
            return if dim != 0 { self.scaleforopp_y(n, dir) * (1 << hpel) } else { self.scaleforopp_x(n) * (1 << hpel) };
        }
        let refdist = if self.pict_type != PT_B {
            self.refdist
        } else if dir != 0 {
            self.brfd
        } else {
            self.frfd
        };
        let refdist = refdist.min(3) as usize;
        let scaleopp = t::FIELD_MVPRED_SCALES[(dir ^ self.second_field as usize) * 28 + refdist] as i32;
        ((n * scaleopp) >> 8) * (1 << hpel)
    }

    /// ff_vc1_pred_mv
    #[allow(clippy::too_many_arguments)]
    pub fn pred_mv(
        &mut self,
        gb: &mut BitReader,
        n: usize,
        mut dmv_x: i32,
        mut dmv_y: i32,
        mv1: bool,
        r_x: i32,
        mut r_y: i32,
        pred_flag: i32,
        dir: usize,
    ) {
        let mixedmv_pic = self.mv_mode == MV_PMODE_MIXED_MV
            || (self.mv_mode == MV_PMODE_INTENSITY_COMP && self.mv_mode2 == MV_PMODE_MIXED_MV);
        if !self.quarter_sample {
            dmv_x *= 2;
            dmv_y *= 2;
        }
        let wrap = self.lay.b8_stride;
        let xy = self.block_index[n];
        let bo = self.blocks_off;
        let off: isize;

        if self.mb_intra {
            self.mv[0][n] = [0, 0];
            self.set_cmv(0, xy + bo, [0, 0]);
            self.set_cmv(1, xy + bo, [0, 0]);
            if mv1 {
                for &o in &[1, wrap, wrap + 1] {
                    self.set_cmv(0, xy + o + bo, [0, 0]);
                    self.set_cmv(1, xy + o + bo, [0, 0]);
                }
                let mx = self.mb_x;
                self.luma_mv[2 * self.lay.mb_stride + mx] = [0, 0];
            }
            return;
        }

        let mut a_valid = !self.first_slice_line || (n == 2 || n == 3);
        let mut b_valid = a_valid;
        let mut c_valid = self.mb_x != 0 || (n == 1 || n == 3);
        if mv1 {
            if self.field_mode && mixedmv_pic {
                off = if self.mb_x == self.mb_width - 1 { -2 } else { 2 };
            } else {
                off = if self.mb_x == self.mb_width - 1 { -1 } else { 2 };
            }
            b_valid = b_valid && self.mb_width > 1;
        } else {
            match n {
                0 => {
                    off = if self.res_rtm_flag {
                        if self.mb_x != 0 { -1 } else { 1 }
                    } else if self.mb_x != 0 {
                        -1
                    } else {
                        2 * self.mb_width as isize - wrap as isize - 1
                    };
                }
                1 => off = if self.mb_x == self.mb_width - 1 { -1 } else { 1 },
                2 => off = 1,
                _ => off = -1,
            }
            if self.field_mode && self.mb_width == 1 {
                b_valid = b_valid && c_valid;
            }
        }

        if self.field_mode {
            a_valid = a_valid && self.mb_type_b[xy - wrap] == 0;
            b_valid = b_valid && self.mb_type_b[(xy as isize - wrap as isize + off) as usize] == 0;
            c_valid = c_valid && self.mb_type_b[xy - 1] == 0;
        }

        let mut num_oppfield = 0;
        let mut num_samefield = 0;
        let (mut pa, mut pb, mut pc) = ([0i32; 2], [0i32; 2], [0i32; 2]);
        let (mut a_f, mut b_f, mut c_f) = (0u8, 0u8, 0u8);
        if a_valid {
            let a = self.cmv(dir, xy - wrap + bo);
            a_f = self.mv_f[dir][xy - wrap + bo];
            num_oppfield += a_f as i32;
            num_samefield += 1 - a_f as i32;
            pa = [a[0] as i32, a[1] as i32];
        }
        if b_valid {
            let bi = (xy as isize - wrap as isize + off) as usize + bo;
            let b = self.cmv(dir, bi);
            b_f = self.mv_f[dir][bi];
            num_oppfield += b_f as i32;
            num_samefield += 1 - b_f as i32;
            pb = [b[0] as i32, b[1] as i32];
        }
        if c_valid {
            let c = self.cmv(dir, xy - 1 + bo);
            c_f = self.mv_f[dir][xy - 1 + bo];
            num_oppfield += c_f as i32;
            num_samefield += 1 - c_f as i32;
            pc = [c[0] as i32, c[1] as i32];
        }

        let opposite;
        if self.field_mode {
            if self.numref == 0 {
                opposite = 1 - self.reffield as i32;
            } else if num_samefield <= num_oppfield {
                opposite = 1 - pred_flag;
            } else {
                opposite = pred_flag;
            }
        } else {
            opposite = 0;
        }
        if opposite != 0 {
            self.mv_f[dir][xy + bo] = 1;
            self.ref_field_type[dir] = (self.cur_field_type == 0) as usize;
            if a_valid && a_f == 0 {
                pa = [self.scaleforopp(pa[0], 0, dir), self.scaleforopp(pa[1], 1, dir)];
            }
            if b_valid && b_f == 0 {
                pb = [self.scaleforopp(pb[0], 0, dir), self.scaleforopp(pb[1], 1, dir)];
            }
            if c_valid && c_f == 0 {
                pc = [self.scaleforopp(pc[0], 0, dir), self.scaleforopp(pc[1], 1, dir)];
            }
        } else {
            self.mv_f[dir][xy + bo] = 0;
            self.ref_field_type[dir] = self.cur_field_type;
            if a_valid && a_f != 0 {
                pa = [self.scaleforsame(pa[0], 0, dir), self.scaleforsame(pa[1], 1, dir)];
            }
            if b_valid && b_f != 0 {
                pb = [self.scaleforsame(pb[0], 0, dir), self.scaleforsame(pb[1], 1, dir)];
            }
            if c_valid && c_f != 0 {
                pc = [self.scaleforsame(pc[0], 0, dir), self.scaleforsame(pc[1], 1, dir)];
            }
        }

        let (mut px, mut py);
        if a_valid {
            px = pa[0];
            py = pa[1];
        } else if c_valid {
            px = pc[0];
            py = pc[1];
        } else if b_valid {
            px = pb[0];
            py = pb[1];
        } else {
            px = 0;
            py = 0;
        }
        if num_samefield + num_oppfield > 1 {
            px = mid_pred(pa[0], pb[0], pc[0]);
            py = mid_pred(pa[1], pb[1], pc[1]);
        }

        // pullback (8.3.5.3.4)
        if !self.field_mode {
            let mv = if mv1 { -60 } else { -28 };
            let qx = ((self.mb_x as i32) << 6) + if n == 1 || n == 3 { 32 } else { 0 };
            let qy = ((self.mb_y as i32) << 6) + if n == 2 || n == 3 { 32 } else { 0 };
            let xx = ((self.mb_width as i32) << 6) - 4;
            let yy = ((self.mb_height as i32) << 6) - 4;
            if qx + px < mv {
                px = mv - qx;
            }
            if qy + py < mv {
                py = mv - qy;
            }
            if qx + px > xx {
                px = xx - qx;
            }
            if qy + py > yy {
                py = yy - qy;
            }
        }

        if !self.field_mode || self.pict_type != PT_B {
            // hybrid prediction (8.3.5.3.5)
            let thresh = 32;
            if a_valid && c_valid {
                let sum = if self.mb_type_b[xy - wrap] != 0 {
                    px.abs() + py.abs()
                } else {
                    (px - pa[0]).abs() + (py - pa[1]).abs()
                };
                if sum > thresh {
                    if gb.get1() != 0 {
                        px = pa[0];
                        py = pa[1];
                    } else {
                        px = pc[0];
                        py = pc[1];
                    }
                } else {
                    let sum = if self.mb_type_b[xy - 1] != 0 {
                        px.abs() + py.abs()
                    } else {
                        (px - pc[0]).abs() + (py - pc[1]).abs()
                    };
                    if sum > thresh {
                        if gb.get1() != 0 {
                            px = pa[0];
                            py = pa[1];
                        } else {
                            px = pc[0];
                            py = pc[1];
                        }
                    }
                }
            }
        }

        if self.field_mode && self.numref != 0 {
            r_y >>= 1;
        }
        let y_bias = if self.field_mode && self.cur_field_type != 0 && self.ref_field_type[dir] == 0 { 1 } else { 0 };
        let mx = ((px + dmv_x + r_x) & ((r_x << 1) - 1)) - r_x;
        let my = ((py + dmv_y + r_y - y_bias) & ((r_y << 1) - 1)) - r_y + y_bias;
        self.mv[dir][n] = [mx, my];
        let v = [mx as i16, my as i16];
        self.set_cmv(dir, xy + bo, v);
        if mv1 {
            for &o in &[1, wrap, wrap + 1] {
                self.set_cmv(dir, xy + o + bo, v);
            }
            let f = self.mv_f[dir][xy + bo];
            self.mv_f[dir][xy + 1 + bo] = f;
            self.mv_f[dir][xy + wrap + bo] = f;
            self.mv_f[dir][xy + wrap + 1 + bo] = f;
        }
    }

    /// ff_vc1_pred_mv_intfr
    #[allow(clippy::too_many_arguments)]
    pub fn pred_mv_intfr(&mut self, n: usize, dmv_x: i32, dmv_y: i32, mvn: i32, r_x: i32, r_y: i32, dir: usize) {
        let wrap = self.lay.b8_stride;
        let ms = self.lay.mb_stride;
        let xy = self.block_index[n];
        if self.mb_intra {
            self.mv[0][n] = [0, 0];
            self.set_cmv(0, xy, [0, 0]);
            self.set_cmv(1, xy, [0, 0]);
            if mvn == 1 {
                for &o in &[1, wrap, wrap + 1] {
                    self.set_cmv(0, xy + o, [0, 0]);
                    self.set_cmv(1, xy + o, [0, 0]);
                }
                let mx = self.mb_x;
                self.luma_mv[2 * ms + mx] = [0, 0];
            }
            return;
        }
        let off: isize = if n == 0 || n == 1 { 1 } else { -1 };
        let mut a = [0i32; 2];
        let mut b = [0i32; 2];
        let mut c = [0i32; 2];
        let (mut a_valid, mut b_valid, mut c_valid) = (0i32, 0i32, 0i32);
        let mbx = self.mb_x;
        let cm = |s: &Core, i: usize| -> [i32; 2] {
            let v = s.cmv(dir, i);
            [v[0] as i32, v[1] as i32]
        };
        if self.mb_x != 0 || n == 1 || n == 3 {
            if self.blk_mv_type[xy] != 0 || (self.blk_mv_type[xy] == 0 && self.blk_mv_type[xy - 1] == 0) {
                a = cm(self, xy - 1);
                a_valid = 1;
            } else {
                let p = cm(self, xy - 1);
                let q = cm(self, (xy as isize - 1 + off * wrap as isize) as usize);
                a = [(p[0] + q[0] + 1) >> 1, (p[1] + q[1] + 1) >> 1];
                a_valid = 1;
            }
            if n & 1 == 0 && self.is_intra[2 * ms + mbx - 1] != 0 {
                a_valid = 0;
                a = [0, 0];
            }
        }
        if n == 0 || n == 1 || self.blk_mv_type[xy] != 0 {
            if !self.first_slice_line {
                if self.is_intra[2 * ms + mbx - ms] == 0 {
                    b_valid = 1;
                    let mut n_adj = n | 2;
                    let pos_b = self.block_index[n_adj] - 2 * wrap;
                    if self.blk_mv_type[pos_b] != 0 && self.blk_mv_type[xy] != 0 {
                        n_adj = (n & 2) | (n & 1);
                    }
                    b = cm(self, self.block_index[n_adj] - 2 * wrap);
                    if self.blk_mv_type[pos_b] != 0 && self.blk_mv_type[xy] == 0 {
                        let q = cm(self, self.block_index[n_adj ^ 2] - 2 * wrap);
                        b = [(b[0] + q[0] + 1) >> 1, (b[1] + q[1] + 1) >> 1];
                    }
                }
                if self.mb_width > 1 && self.is_intra[2 * ms + mbx - ms + 1] == 0 {
                    c_valid = 1;
                    let mut n_adj = 2;
                    let pos_c = self.block_index[2] - 2 * wrap + 2;
                    if self.blk_mv_type[pos_c] != 0 && self.blk_mv_type[xy] != 0 {
                        n_adj = n & 2;
                    }
                    c = cm(self, self.block_index[n_adj] - 2 * wrap + 2);
                    if self.blk_mv_type[pos_c] != 0 && self.blk_mv_type[xy] == 0 {
                        let q = cm(self, self.block_index[n_adj ^ 2] - 2 * wrap + 2);
                        c = [(1 + c[0] + q[0]) >> 1, (1 + c[1] + q[1]) >> 1];
                    }
                    if self.mb_x == self.mb_width - 1 {
                        if self.is_intra[2 * ms + mbx - ms - 1] == 0 {
                            c_valid = 1;
                            let mut n_adj = 3;
                            let pos_c = self.block_index[3] - 2 * wrap - 2;
                            if self.blk_mv_type[pos_c] != 0 && self.blk_mv_type[xy] != 0 {
                                n_adj = n | 1;
                            }
                            c = cm(self, self.block_index[n_adj] - 2 * wrap - 2);
                            if self.blk_mv_type[pos_c] != 0 && self.blk_mv_type[xy] == 0 {
                                let q = cm(self, self.block_index[1] - 2 * wrap - 2);
                                c = [(1 + c[0] + q[0]) >> 1, (1 + c[1] + q[1]) >> 1];
                            }
                        } else {
                            c_valid = 0;
                        }
                    }
                }
            }
        } else {
            b = cm(self, self.block_index[1]);
            b_valid = 1;
            c = cm(self, self.block_index[0]);
            c_valid = 1;
        }
        let total_valid = a_valid + b_valid + c_valid;
        if self.mb_x == 0 && !(n == 1 || n == 3) {
            a = [0, 0];
        }
        if (self.first_slice_line && self.blk_mv_type[xy] != 0) || (self.first_slice_line && n & 2 == 0) {
            b = [0, 0];
            c = [0, 0];
        }
        let (mut px, mut py) = (0, 0);
        if self.blk_mv_type[xy] == 0 {
            if self.mb_width == 1 {
                px = b[0];
                py = b[1];
            } else if total_valid >= 2 {
                px = mid_pred(a[0], b[0], c[0]);
                py = mid_pred(a[1], b[1], c[1]);
            } else if total_valid != 0 {
                if a_valid != 0 {
                    px = a[0];
                    py = a[1];
                } else if b_valid != 0 {
                    px = b[0];
                    py = b[1];
                } else {
                    px = c[0];
                    py = c[1];
                }
            }
        } else {
            let field_a = if a_valid != 0 { (a[1] & 4 != 0) as i32 } else { 0 };
            let field_b = if b_valid != 0 { (b[1] & 4 != 0) as i32 } else { 0 };
            let field_c = if c_valid != 0 { (c[1] & 4 != 0) as i32 } else { 0 };
            let num_oppfield = field_a + field_b + field_c;
            let num_samefield = total_valid - num_oppfield;
            if total_valid == 3 {
                if num_samefield == 3 || num_oppfield == 3 {
                    px = mid_pred(a[0], b[0], c[0]);
                    py = mid_pred(a[1], b[1], c[1]);
                } else if num_samefield >= num_oppfield {
                    px = if field_a == 0 { a[0] } else { b[0] };
                    py = if field_a == 0 { a[1] } else { b[1] };
                } else {
                    px = if field_a != 0 { a[0] } else { b[0] };
                    py = if field_a != 0 { a[1] } else { b[1] };
                }
            } else if total_valid == 2 {
                if num_samefield >= num_oppfield {
                    if field_a == 0 && a_valid != 0 {
                        px = a[0];
                        py = a[1];
                    } else if field_b == 0 && b_valid != 0 {
                        px = b[0];
                        py = b[1];
                    } else {
                        px = c[0];
                        py = c[1];
                    }
                } else if field_a != 0 && a_valid != 0 {
                    px = a[0];
                    py = a[1];
                } else {
                    px = b[0];
                    py = b[1];
                }
            } else if total_valid == 1 {
                if a_valid != 0 {
                    px = a[0];
                    py = a[1];
                } else if b_valid != 0 {
                    px = b[0];
                    py = b[1];
                } else {
                    px = c[0];
                    py = c[1];
                }
            }
        }
        let mx = ((px + dmv_x + r_x) & ((r_x << 1) - 1)) - r_x;
        let my = ((py + dmv_y + r_y) & ((r_y << 1) - 1)) - r_y;
        self.mv[dir][n] = [mx, my];
        let v = [mx as i16, my as i16];
        self.set_cmv(dir, xy, v);
        if mvn == 1 {
            for &o in &[1, wrap, wrap + 1] {
                self.set_cmv(dir, xy + o, v);
            }
        } else if mvn == 2 {
            self.set_cmv(dir, xy + 1, v);
            self.mv[dir][n + 1] = [mx, my];
        }
    }

    /// ff_vc1_pred_b_mv (progressive B frames)
    pub fn pred_b_mv(&mut self, gb: &mut BitReader, dmv_x: &mut [i32; 2], dmv_y: &mut [i32; 2], direct: bool, mvtype: i32) {
        let r_x = self.range_x;
        let r_y = self.range_y;
        if !self.quarter_sample {
            dmv_x[0] *= 2;
            dmv_y[0] *= 2;
            dmv_x[1] *= 2;
            dmv_y[1] *= 2;
        }
        let wrap = self.lay.b8_stride;
        let xy = self.block_index[0];
        if self.mb_intra {
            self.set_cmv(0, xy, [0, 0]);
            self.set_cmv(1, xy, [0, 0]);
            return;
        }
        let nxt = match &self.next {
            Some(p) => p.motion_val[1][xy],
            None => [0, 0],
        };
        let qs = self.quarter_sample;
        let bf = self.bfraction;
        self.mv[0][0] = [scale_mv(nxt[0] as i32, bf, false, qs), scale_mv(nxt[1] as i32, bf, false, qs)];
        self.mv[1][0] = [scale_mv(nxt[0] as i32, bf, true, qs), scale_mv(nxt[1] as i32, bf, true, qs)];
        let mbx = self.mb_x as i32;
        let mby = self.mb_y as i32;
        let mbw = self.mb_width as i32;
        let mbh = self.mb_height as i32;
        for d in 0..2 {
            self.mv[d][0][0] = self.mv[d][0][0].clamp(-60 - (mbx << 6), (mbw << 6) - 4 - (mbx << 6));
            self.mv[d][0][1] = self.mv[d][0][1].clamp(-60 - (mby << 6), (mbh << 6) - 4 - (mby << 6));
        }
        if direct {
            self.set_cmv(0, xy, [self.mv[0][0][0] as i16, self.mv[0][0][1] as i16]);
            self.set_cmv(1, xy, [self.mv[1][0][0] as i16, self.mv[1][0][1] as i16]);
            return;
        }
        let _ = gb;
        for dir in 0..2usize {
            let want = if dir == 0 {
                mvtype == BMV_TYPE_FORWARD || mvtype == BMV_TYPE_INTERPOLATED
            } else {
                mvtype == BMV_TYPE_BACKWARD || mvtype == BMV_TYPE_INTERPOLATED
            };
            if !want {
                continue;
            }
            if self.mb_x == 0 {
                self.set_cmv(dir, xy - 2, [0, 0]);
            }
            let c = self.cmv(dir, xy - 2);
            let off: isize = if self.mb_x == self.mb_width - 1 { -2 } else { 2 };
            let (px, py);
            if !self.first_slice_line {
                let a = self.cmv(dir, xy - wrap * 2);
                if self.mb_width == 1 {
                    px = a[0] as i32;
                    py = a[1] as i32;
                } else {
                    let b = self.cmv(dir, (xy as isize - (wrap * 2) as isize + off) as usize);
                    px = mid_pred(a[0] as i32, b[0] as i32, c[0] as i32);
                    py = mid_pred(a[1] as i32, b[1] as i32, c[1] as i32);
                }
            } else if self.mb_x != 0 {
                px = c[0] as i32;
                py = c[1] as i32;
            } else {
                px = 0;
                py = 0;
            }
            let (mut px, mut py) = (px, py);
            // pullback (8.3.5.3.4); advanced profile: sh = 6
            let sh = 6;
            let mv = 4 - (1 << sh);
            let qx = mbx << sh;
            let qy = mby << sh;
            let xx = (mbw << sh) - 4;
            let yy = (mbh << sh) - 4;
            if qx + px < mv {
                px = mv - qx;
            }
            if qy + py < mv {
                py = mv - qy;
            }
            if qx + px > xx {
                px = xx - qx;
            }
            if qy + py > yy {
                py = yy - qy;
            }
            self.mv[dir][0] = [
                ((px + dmv_x[dir] + r_x) & ((r_x << 1) - 1)) - r_x,
                ((py + dmv_y[dir] + r_y) & ((r_y << 1) - 1)) - r_y,
            ];
        }
        self.set_cmv(0, xy, [self.mv[0][0][0] as i16, self.mv[0][0][1] as i16]);
        self.set_cmv(1, xy, [self.mv[1][0][0] as i16, self.mv[1][0][1] as i16]);
    }
}
