//! Motion compensation (FFmpeg vc1_mc.c).
use crate::core::*;
use crate::dsp;

const POPCOUNT4: [usize; 16] = [0, 1, 1, 2, 1, 2, 2, 3, 1, 2, 2, 3, 2, 3, 3, 4];

#[inline]
fn median4(a: i32, b: i32, c: i32, d: i32) -> i32 {
    if a < b {
        if c < d {
            (b.min(d) + a.max(c)) / 2
        } else {
            (b.min(c) + a.max(d)) / 2
        }
    } else if c < d {
        (a.min(d) + b.max(c)) / 2
    } else {
        (a.min(c) + b.max(d)) / 2
    }
}

use crate::pred::mid_pred;

/// Scale `k` x `k` luma samples in the edge buffer by a LUT pair (even/odd rows).
fn lut_scale_luma(buf: &mut [u8], off: usize, lut1: &[u8; 256], lut2: &[u8; 256], k: usize, stride: usize) {
    let mut p = off;
    let mut j = 0;
    while j < k {
        for i in 0..k {
            buf[p + i] = lut1[buf[p + i] as usize];
        }
        p += stride;
        if j + 1 == k {
            break;
        }
        for i in 0..k {
            buf[p + i] = lut2[buf[p + i] as usize];
        }
        p += stride;
        j += 2;
    }
}

fn scale_luma_rr(buf: &mut [u8], k: usize, stride: usize) {
    for j in 0..k {
        for i in 0..k {
            let v = buf[j * stride + i] as i32;
            buf[j * stride + i] = (((v - 128) >> 1) + 128) as u8;
        }
    }
}

/// Copy a `w` x `rows` window of `plane` (first pixel at `ptr`, row pitch `src_stride`) into `dst`
/// (row pitch `dst_pitch`); samples outside the plane read as 128.
fn copy_window(dst: &mut [u8], dst_pitch: usize, plane: &[u8], ptr: isize, src_stride: isize, w: usize, rows: usize) {
    for j in 0..rows {
        let o = ptr + j as isize * src_stride;
        let d = &mut dst[j * dst_pitch..j * dst_pitch + w];
        if o >= 0 && (o as usize) + w <= plane.len() {
            d.copy_from_slice(&plane[o as usize..o as usize + w]);
        } else {
            for i in 0..w {
                let idx = o + i as isize;
                d[i] = if idx >= 0 && (idx as usize) < plane.len() { plane[idx as usize] } else { 128 };
            }
        }
    }
}

impl Core {
    fn ref_pic(&self, sel: RefSel) -> Option<&Pic> {
        match sel {
            RefSel::Cur => self.cur.as_deref(),
            RefSel::Last => self.last.as_deref(),
            RefSel::Next => self.next.as_deref(),
        }
    }

    fn get_luma_mv(&self, dir: usize) -> (i32, i32, usize) {
        let bo = self.blocks_off;
        let bi = &self.block_index;
        let f = &self.mv_f[dir];
        let idx = (f[bi[0] + bo] as usize)
            | ((f[bi[1] + bo] as usize) << 1)
            | ((f[bi[2] + bo] as usize) << 2)
            | ((f[bi[3] + bo] as usize) << 3);
        const INDEX2: [u8; 16] = [0, 0, 0, 0x23, 0, 0x13, 0x03, 0, 0, 0x12, 0x02, 0, 0x01, 0, 0, 0];
        let opp_count = POPCOUNT4[idx];
        let mv = &self.mv[dir];
        let (tx, ty);
        match opp_count {
            0 | 4 => {
                tx = median4(mv[0][0], mv[1][0], mv[2][0], mv[3][0]);
                ty = median4(mv[0][1], mv[1][1], mv[2][1], mv[3][1]);
            }
            1 => {
                let a = (idx < 2) as usize;
                let b = 1 + (idx < 4) as usize;
                let c = 2 + (idx < 8) as usize;
                tx = mid_pred(mv[a][0], mv[b][0], mv[c][0]);
                ty = mid_pred(mv[a][1], mv[b][1], mv[c][1]);
            }
            3 => {
                let a = (idx > 0xd) as usize;
                let b = 1 + (idx > 0xb) as usize;
                let c = 2 + (idx > 0x7) as usize;
                tx = mid_pred(mv[a][0], mv[b][0], mv[c][0]);
                ty = mid_pred(mv[a][1], mv[b][1], mv[c][1]);
            }
            _ => {
                let i2 = INDEX2[idx] as usize;
                tx = (mv[i2 >> 4][0] + mv[i2 & 0xf][0]) / 2;
                ty = (mv[i2 >> 4][1] + mv[i2 & 0xf][1]) / 2;
            }
        }
        (tx, ty, opp_count)
    }

    fn get_chroma_mv(&self, dir: usize) -> Option<(i32, i32)> {
        let bi = &self.block_index;
        let t = &self.mb_type_b;
        let idx = (t[bi[0]] == 0) as usize
            | (((t[bi[1]] == 0) as usize) << 1)
            | (((t[bi[2]] == 0) as usize) << 2)
            | (((t[bi[3]] == 0) as usize) << 3);
        const INDEX2: [u8; 16] = [0, 0, 0, 0x01, 0, 0x02, 0x12, 0, 0, 0x03, 0x13, 0, 0x23, 0, 0, 0];
        let valid = POPCOUNT4[idx];
        let mv = &self.mv[dir];
        match valid {
            4 => Some((
                median4(mv[0][0], mv[1][0], mv[2][0], mv[3][0]),
                median4(mv[0][1], mv[1][1], mv[2][1], mv[3][1]),
            )),
            3 => {
                let a = (idx > 0xd) as usize;
                let b = 1 + (idx > 0xb) as usize;
                let c = 2 + (idx > 0x7) as usize;
                Some((mid_pred(mv[a][0], mv[b][0], mv[c][0]), mid_pred(mv[a][1], mv[b][1], mv[c][1])))
            }
            2 => {
                let i2 = INDEX2[idx] as usize;
                Some(((mv[i2 >> 4][0] + mv[i2 & 0xf][0]) / 2, (mv[i2 >> 4][1] + mv[i2 & 0xf][1]) / 2))
            }
            _ => None,
        }
    }

    /// Select the reference picture and its LUT set for `dir` (luma/chroma share this logic).
    fn select_ref(&self, dir: usize, chroma_ref_type: usize) -> (RefSel, usize, bool) {
        // returns (sel, lut index, interlace)
        if dir == 0 {
            if self.field_mode && self.cur_field_type != chroma_ref_type && self.second_field {
                (RefSel::Cur, self.curr, true)
            } else {
                (RefSel::Last, LAST, self.last_interlaced)
            }
        } else {
            (RefSel::Next, NEXT, self.next_interlaced)
        }
    }

    /// Fetch the luma window for a block into `self.edge` following the emulated-edge rules.
    /// Returns the filter stride used inside the buffer.
    #[allow(clippy::too_many_arguments)]
    fn fetch_luma(
        &mut self,
        sel: RefSel,
        interlace: bool,
        ptr: isize,
        bw: i32,
        bh_frame: i32,
        src_x: i32,
        src_y: i32,
        ref_field_type: usize,
        fieldmv: usize,
        mspel: i32,
        four_mv: bool,
    ) -> usize {
        // `ptr` already includes the -mspel offsets. This mirrors the three emulated_edge_mc variants.
        let field_mode = self.field_mode;
        let linesize = self.cur.as_ref().unwrap().stride[0] as isize;
        let h_edge = self.width as i32;
        let v_edge = self.height as i32;
        let mut eb = std::mem::take(&mut self.edge);
        {
            let rp = match self.ref_pic(sel) {
                Some(p) => p,
                None => {
                    self.edge = eb;
                    return EB;
                }
            };
            let plane = &rp.planes[0];
            if interlace {
                let dst_stride = if field_mode { EB } else { 2 * EB };
                // first call
                let bh = if field_mode {
                    bh_frame
                } else if four_mv {
                    ((bh_frame << fieldmv) + 1) >> 1
                } else {
                    (bh_frame + 1) >> 1
                };
                let y0 = if four_mv {
                    (src_y - (mspel << fieldmv)) >> (!field_mode) as i32
                } else {
                    (src_y - mspel) >> (!field_mode) as i32
                };
                let _ = y0;
                // callers pass y0 base via src_y already adjusted (see below)
                dsp::emu_edge(&mut eb, 0, dst_stride, plane, ptr, linesize * 2, bw, bh, src_x, y0, h_edge, v_edge >> 1);
                if !field_mode && (!four_mv || fieldmv == 0) {
                    let y1 = if four_mv { (src_y - mspel + 1) >> 1 } else { (src_y - mspel + 1) >> 1 };
                    dsp::emu_edge(
                        &mut eb,
                        EB,
                        dst_stride,
                        plane,
                        ptr + linesize,
                        linesize * 2,
                        bw,
                        bh_frame >> 1,
                        src_x,
                        y1,
                        h_edge,
                        v_edge >> 1,
                    );
                }
            } else {
                let bh = if field_mode { (bh_frame << 1) - 1 } else if four_mv { bh_frame << fieldmv } else { bh_frame };
                let y0 = if field_mode {
                    2 * (src_y - mspel) + ref_field_type as i32
                } else if four_mv {
                    src_y - (mspel << fieldmv)
                } else {
                    src_y - mspel
                };
                dsp::emu_edge(&mut eb, 0, EB, plane, ptr, linesize, bw, bh, src_x, y0, h_edge, v_edge);
            }
        }
        self.edge = eb;
        if field_mode && !interlace {
            2 * EB
        } else {
            EB
        }
    }

    /// Fetch a 9x9 (or 5x5) chroma window for planes 1 and 2.
    #[allow(clippy::too_many_arguments)]
    fn fetch_chroma(
        &mut self,
        sel: RefSel,
        interlace: bool,
        ptr: isize,
        bw: i32,
        uvsrc_x: i32,
        uvsrc_y: i32,
        chroma_ref_type: usize,
        rows_field: i32,   // block_h for field interlace (9 or (5<<fieldmv)+1>>1)
        rows_half: i32,    // block_h of second interlaced call (4 or 2)
        rows_prog: i32,    // block_h of progressive window (9 / 17 / 5<<fieldmv)
        do_second: bool,
        uvy_second: i32,
    ) -> usize {
        let field_mode = self.field_mode;
        let uvlinesize = self.cur.as_ref().unwrap().stride[1] as isize;
        let h_edge = (self.width as i32) >> 1;
        let v_edge = self.height as i32;
        let mut eu = std::mem::take(&mut self.edge_u);
        let mut ev = std::mem::take(&mut self.edge_v);
        {
            if let Some(rp) = self.ref_pic(sel) {
                let (pu, pv) = (&rp.planes[1], &rp.planes[2]);
                if interlace {
                    let dst_stride = if field_mode { EB } else { 2 * EB };
                    let y0 = uvsrc_y >> (!field_mode) as i32;
                    dsp::emu_edge(&mut eu, 0, dst_stride, pu, ptr, uvlinesize * 2, bw, rows_field, uvsrc_x, y0, h_edge, v_edge >> 2);
                    dsp::emu_edge(&mut ev, 0, dst_stride, pv, ptr, uvlinesize * 2, bw, rows_field, uvsrc_x, y0, h_edge, v_edge >> 2);
                    if do_second {
                        dsp::emu_edge(
                            &mut eu, EB, dst_stride, pu, ptr + uvlinesize, uvlinesize * 2, bw, rows_half, uvsrc_x, uvy_second, h_edge, v_edge >> 2,
                        );
                        dsp::emu_edge(
                            &mut ev, EB, dst_stride, pv, ptr + uvlinesize, uvlinesize * 2, bw, rows_half, uvsrc_x, uvy_second, h_edge, v_edge >> 2,
                        );
                    }
                } else {
                    let y0 = if field_mode { 2 * uvsrc_y + chroma_ref_type as i32 } else { uvsrc_y };
                    dsp::emu_edge(&mut eu, 0, EB, pu, ptr, uvlinesize, bw, rows_prog, uvsrc_x, y0, h_edge, v_edge >> 1);
                    dsp::emu_edge(&mut ev, 0, EB, pv, ptr, uvlinesize, bw, rows_prog, uvsrc_x, y0, h_edge, v_edge >> 1);
                }
            }
        }
        self.edge_u = eu;
        self.edge_v = ev;
        if field_mode && !interlace {
            2 * EB
        } else {
            EB
        }
    }

    /// Copy a luma window straight from the reference (FFmpeg's non-emulated fast path).
    fn fetch_luma_direct(&mut self, sel: RefSel, ptr: isize, k: usize, rows: usize, src_stride: isize, row_mult: usize) -> usize {
        let mut eb = std::mem::take(&mut self.edge);
        if let Some(rp) = self.ref_pic(sel) {
            copy_window(&mut eb, EB * row_mult, &rp.planes[0], ptr, src_stride, k, rows);
        }
        self.edge = eb;
        EB
    }

    #[allow(clippy::too_many_arguments)]
    fn fetch_chroma_direct(&mut self, sel: RefSel, ptr: isize, bw: usize, rows: usize, src_stride: isize, row_mult: usize) -> usize {
        let mut eu = std::mem::take(&mut self.edge_u);
        let mut ev = std::mem::take(&mut self.edge_v);
        if let Some(rp) = self.ref_pic(sel) {
            copy_window(&mut eu, EB * row_mult, &rp.planes[1], ptr, src_stride, bw, rows);
            copy_window(&mut ev, EB * row_mult, &rp.planes[2], ptr, src_stride, bw, rows);
        }
        self.edge_u = eu;
        self.edge_v = ev;
        EB
    }

    /// ff_vc1_mc_1mv
    pub fn mc_1mv(&mut self, dir: usize) {
        let field_mode = self.field_mode;
        if (!field_mode || (self.ref_field_type[dir] == 1 && self.cur_field_type == 1)) && self.last.is_none() && dir == 0 {
            // no reference available yet
            return;
        }
        let linesize = self.cur.as_ref().unwrap().stride[0] as isize;
        let uvlinesize = self.cur.as_ref().unwrap().stride[1] as isize;
        let mx = self.mv[dir][0][0];
        let mut my = self.mv[dir][0][1];
        let bo = self.blocks_off;
        if self.pict_type == PT_P {
            for i in 0..4 {
                let bi = self.block_index[i] + bo;
                self.set_cmv(1, bi, [mx as i16, my as i16]);
            }
        }
        let mut uvmx = (mx + ((mx & 3) == 3) as i32) >> 1;
        let mut uvmy = (my + ((my & 3) == 3) as i32) >> 1;
        let ms = self.lay.mb_stride;
        let mbx = self.mb_x;
        self.luma_mv[2 * ms + mbx] = [uvmx as i16, uvmy as i16];
        if field_mode && self.cur_field_type != self.ref_field_type[dir] {
            my = my - 2 + 4 * self.cur_field_type as i32;
            uvmy = uvmy - 2 + 4 * self.cur_field_type as i32;
        }
        if self.fastuvmc && self.fcm != ILACE_FRAME {
            uvmx += if uvmx < 0 { uvmx & 1 } else { -(uvmx & 1) };
            uvmy += if uvmy < 0 { uvmy & 1 } else { -(uvmy & 1) };
        }
        let (sel, lut, interlace) = self.select_ref(dir, self.ref_field_type[dir]);
        let (sel, lut, interlace) = if dir == 0 { (sel, lut, interlace) } else { (RefSel::Next, NEXT, self.next_interlaced) };
        if self.ref_pic(sel).is_none() {
            return;
        }
        let use_ic = self.luts[lut].use_ic;
        let mspel = self.mspel as i32;
        let w = self.width as i32;
        let h = self.height as i32;
        let mut src_x = self.mb_x as i32 * 16 + (mx >> 2);
        let mut src_y = self.mb_y as i32 * 16 + (my >> 2);
        let mut uvsrc_x = self.mb_x as i32 * 8 + (uvmx >> 2);
        let mut uvsrc_y = self.mb_y as i32 * 8 + (uvmy >> 2);
        src_x = src_x.clamp(-17, w);
        uvsrc_x = uvsrc_x.clamp(-8, w >> 1);
        if self.fcm == ILACE_FRAME {
            src_y = src_y.clamp(-18 + (src_y & 1), h + (src_y & 1));
            uvsrc_y = uvsrc_y.clamp(-8 + (uvsrc_y & 1), (h >> 1) + (uvsrc_y & 1));
        } else {
            src_y = src_y.clamp(-18, h + 1);
            uvsrc_y = uvsrc_y.clamp(-8, h >> 1);
        }
        let ls = self.linesize as isize;
        let uvls = self.uvlinesize as isize;
        let mut ptr_y = src_y as isize * ls + src_x as isize;
        let mut ptr_u = uvsrc_y as isize * uvls + uvsrc_x as isize;
        if field_mode && self.ref_field_type[dir] != 0 {
            ptr_y += linesize;
            ptr_u += uvlinesize;
        }
        let k = (17 + mspel * 2) as usize;
        ptr_y -= mspel as isize * (1 + ls);
        let rft = self.ref_field_type[dir];
        let v_edge_pos = h >> field_mode as i32;
        let emu = self.rangeredfrm
            || use_ic
            || w < 22
            || v_edge_pos < 22
            || ((src_x - mspel) as u32) > ((w - (mx & 3) - 16 - mspel * 3) as u32)
            || ((src_y - 1) as u32) > ((v_edge_pos - (my & 3) - 16 - 3) as u32);
        let (fs, fsc);
        if emu {
            fs = self.fetch_luma(sel, interlace, ptr_y, k as i32, k as i32, src_x - mspel, src_y, rft, 0, mspel, false);
            fsc = self.fetch_chroma(
                sel,
                interlace,
                ptr_u,
                9,
                uvsrc_x,
                uvsrc_y,
                rft,
                if field_mode { 9 } else { 5 },
                4,
                if field_mode { 17 } else { 9 },
                !field_mode,
                (uvsrc_y + 1) >> 1,
            );
        } else {
            fs = self.fetch_luma_direct(sel, ptr_y, k, k, ls, 1);
            fsc = self.fetch_chroma_direct(sel, ptr_u, 9, 9, uvls, 1);
        }
        if self.rangeredfrm {
            scale_luma_rr(&mut self.edge, k, fs);
            for buf in [&mut self.edge_u, &mut self.edge_v] {
                for j in 0..9 {
                    for i in 0..9 {
                        let v = buf[j * fsc + i] as i32;
                        buf[j * fsc + i] = (((v - 128) >> 1) + 128) as u8;
                    }
                }
            }
        }
        if use_ic {
            let l = self.luts[lut].clone();
            let ly = if field_mode { rft } else { ((src_y - mspel) & 1) as usize };
            let ly2 = if field_mode { rft } else { ((1 + src_y - mspel) & 1) as usize };
            lut_scale_luma(&mut self.edge, 0, &l.y[ly], &l.y[ly2], k, fs);
            let cu = if field_mode { rft } else { (uvsrc_y & 1) as usize };
            let cv = if field_mode { rft } else { ((1 + uvsrc_y) & 1) as usize };
            lut_scale_luma(&mut self.edge_u, 0, &l.uv[cu], &l.uv[cv], 9, fsc);
            lut_scale_luma_w(&mut self.edge_v, 0, &l.uv[cu], &l.uv[cv], 9, fsc);
        }
        let spos = (mspel as usize) * (1 + fs);
        let rnd = self.rnd;
        let d0 = self.dest[0];
        let (d1, d2) = (self.dest[1], self.dest[2]);
        let lsu = self.linesize;
        let uvlsu = self.uvlinesize;
        let pic = self.cur.as_mut().unwrap();
        if self.mspel {
            dsp::mspel_mc(&mut pic.planes[0], d0, lsu, &self.edge, spos, fs, (mx & 3) as usize, (my & 3) as usize, rnd, 16, false);
        } else {
            let dxy = ((my & 2) | ((mx & 2) >> 1)) as usize;
            dsp::hpel_mc(&mut pic.planes[0], d0, lsu, &self.edge, spos, fs, dxy, 16, rnd != 0, false);
        }
        let ux = (uvmx & 3) << 1;
        let uy = (uvmy & 3) << 1;
        dsp::chroma_mc(&mut pic.planes[1], d1, uvlsu, &self.edge_u, 0, fsc, 8, 8, ux, uy, rnd != 0, false);
        dsp::chroma_mc(&mut pic.planes[2], d2, uvlsu, &self.edge_v, 0, fsc, 8, 8, ux, uy, rnd != 0, false);
        if field_mode {
            let v = (self.cur_field_type != self.ref_field_type[dir]) as u8;
            let mo = self.mb_off;
            self.mv_f[dir][self.block_index[4] + mo] = v;
            self.mv_f[dir][self.block_index[5] + mo] = v;
        }
    }

    /// ff_vc1_mc_4mv_luma
    pub fn mc_4mv_luma(&mut self, n: usize, dir: usize, avg: bool) {
        let field_mode = self.field_mode;
        let fieldmv = if self.fcm == ILACE_FRAME { self.blk_mv_type[self.block_index[n]] as usize } else { 0 };
        if (!field_mode || (self.ref_field_type[dir] == 1 && self.cur_field_type == 1)) && self.last.is_none() && dir == 0 {
            return;
        }
        let linesize = self.cur.as_ref().unwrap().stride[0] as isize;
        let mut mx = self.mv[dir][n][0];
        let mut my = self.mv[dir][n][1];
        let (sel, lut, interlace) = self.select_ref(dir, self.ref_field_type[dir]);
        if self.ref_pic(sel).is_none() {
            return;
        }
        let use_ic = self.luts[lut].use_ic;
        if field_mode && self.cur_field_type != self.ref_field_type[dir] {
            my = my - 2 + 4 * self.cur_field_type as i32;
        }
        let bo = self.blocks_off;
        if self.pict_type == PT_P && n == 3 && field_mode {
            let (tx, ty, opp) = self.get_luma_mv(0);
            let bi0 = self.block_index[0] + bo;
            self.set_cmv(1, bi0, [tx as i16, ty as i16]);
            let f = (opp > 2) as u8;
            for k in 0..4 {
                let b = self.block_index[k] + bo;
                self.mv_f[1][b] = f;
            }
        }
        let w = self.width as i32;
        let h = self.height as i32;
        if self.fcm == ILACE_FRAME {
            let width = w;
            let height = h >> 1;
            if self.pict_type == PT_P {
                let b = self.block_index[n] + bo;
                self.set_cmv(1, b, [mx as i16, my as i16]);
            }
            let qx = (self.mb_x as i32 * 16) + (mx >> 2);
            let qy = (self.mb_y as i32 * 8) + (my >> 3);
            if qx < -17 {
                mx -= 4 * (qx + 17);
            } else if qx > width {
                mx -= 4 * (qx - width);
            }
            if qy < -18 {
                my -= 8 * (qy + 18);
            } else if qy > height + 1 {
                my -= 8 * (qy - height - 1);
            }
        }
        let ls = self.linesize as isize;
        let off: usize = if self.fcm == ILACE_FRAME && fieldmv != 0 {
            (if n > 1 { self.linesize } else { 0 }) + (n & 1) * 8
        } else {
            self.linesize * 4 * (n & 2) + (n & 1) * 8
        };
        let mut src_x = self.mb_x as i32 * 16 + (n & 1) as i32 * 8 + (mx >> 2);
        let mut src_y = if fieldmv == 0 {
            self.mb_y as i32 * 16 + (n & 2) as i32 * 4 + (my >> 2)
        } else {
            self.mb_y as i32 * 16 + if n > 1 { 1 } else { 0 } + (my >> 2)
        };
        src_x = src_x.clamp(-17, w);
        if self.fcm == ILACE_FRAME {
            src_y = src_y.clamp(-18 + (src_y & 1), h + (src_y & 1));
        } else {
            src_y = src_y.clamp(-18, h + 1);
        }
        let mut ptr = src_y as isize * ls + src_x as isize;
        if field_mode && self.ref_field_type[dir] != 0 {
            ptr += linesize;
        }
        let mspel = self.mspel as i32;
        let k = (9 + mspel * 2) as usize;
        ptr -= mspel as isize * (1 + (ls << fieldmv));
        let rft = self.ref_field_type[dir];
        let v_edge_pos = h >> field_mode as i32;
        let emu = self.rangeredfrm
            || use_ic
            || w < 13
            || v_edge_pos < 23
            || ((src_x - mspel) as u32) > ((w - (mx & 3) - 8 - mspel * 2) as u32)
            || ((src_y - (mspel << fieldmv)) as u32) > ((v_edge_pos - (my & 3) - ((8 + mspel * 2) << fieldmv)) as u32);
        let fs = if emu {
            self.fetch_luma(sel, interlace, ptr, k as i32, k as i32, src_x - mspel, src_y, rft, fieldmv, mspel, true)
        } else {
            self.fetch_luma_direct(sel, ptr, k, k, ls << fieldmv, 1 << fieldmv)
        };
        if self.rangeredfrm {
            scale_luma_rr(&mut self.edge, k, fs << fieldmv);
        }
        if use_ic {
            let l = self.luts[lut].clone();
            let ya = if field_mode { rft } else { (((0 << fieldmv) + src_y - (mspel << fieldmv)) & 1) as usize };
            let yb = if field_mode { rft } else { (((1 << fieldmv) + src_y - (mspel << fieldmv)) & 1) as usize };
            lut_scale_luma(&mut self.edge, 0, &l.y[ya], &l.y[yb], k, fs << fieldmv);
        }
        let spos = (mspel as usize) * (1 + (fs << fieldmv));
        let rnd = self.rnd;
        let d0 = self.dest[0];
        let lsu = self.linesize;
        let pic = self.cur.as_mut().unwrap();
        if self.mspel {
            dsp::mspel_mc(
                &mut pic.planes[0],
                d0 + off,
                lsu << fieldmv,
                &self.edge,
                spos,
                fs << fieldmv,
                (mx & 3) as usize,
                (my & 3) as usize,
                rnd,
                8,
                avg,
            );
        } else {
            let dxy = ((my & 2) | ((mx & 2) >> 1)) as usize;
            dsp::hpel_mc(&mut pic.planes[0], d0 + off, lsu, &self.edge, spos, fs, dxy, 8, rnd != 0, false);
        }
    }

    /// ff_vc1_mc_4mv_chroma
    pub fn mc_4mv_chroma(&mut self, dir: usize) {
        let field_mode = self.field_mode;
        if !field_mode && self.last.is_none() {
            return;
        }
        let bo = self.blocks_off;
        let (tx, ty);
        let chroma_ref_type;
        if !field_mode || self.numref == 0 {
            match self.get_chroma_mv(dir) {
                None => {
                    let b = self.block_index[0] + bo;
                    self.set_cmv(1, b, [0, 0]);
                    let ms = self.lay.mb_stride;
                    let mbx = self.mb_x;
                    self.luma_mv[2 * ms + mbx] = [0, 0];
                    return;
                }
                Some((a, b)) => {
                    tx = a;
                    ty = b;
                }
            }
            chroma_ref_type = self.ref_field_type[dir];
        } else {
            let (a, b, opp) = self.get_luma_mv(dir);
            tx = a;
            ty = b;
            chroma_ref_type = self.cur_field_type ^ (opp > 2) as usize;
        }
        if field_mode && chroma_ref_type == 1 && self.cur_field_type == 1 && self.last.is_none() {
            return;
        }
        let b = self.block_index[0] + bo;
        self.set_cmv(1, b, [tx as i16, ty as i16]);
        let uvlinesize = self.cur.as_ref().unwrap().stride[1] as isize;
        let mut uvmx = (tx + ((tx & 3) == 3) as i32) >> 1;
        let mut uvmy = (ty + ((ty & 3) == 3) as i32) >> 1;
        let ms = self.lay.mb_stride;
        let mbx = self.mb_x;
        self.luma_mv[2 * ms + mbx] = [uvmx as i16, uvmy as i16];
        if self.fastuvmc {
            uvmx += if uvmx < 0 { uvmx & 1 } else { -(uvmx & 1) };
            uvmy += if uvmy < 0 { uvmy & 1 } else { -(uvmy & 1) };
        }
        if self.cur_field_type != chroma_ref_type {
            uvmy += 2 - 4 * chroma_ref_type as i32;
        }
        let w = self.width as i32;
        let h = self.height as i32;
        let mut uvsrc_x = self.mb_x as i32 * 8 + (uvmx >> 2);
        let mut uvsrc_y = self.mb_y as i32 * 8 + (uvmy >> 2);
        uvsrc_x = uvsrc_x.clamp(-8, w >> 1);
        uvsrc_y = uvsrc_y.clamp(-8, h >> 1);
        let (sel, lut, interlace) = self.select_ref(dir, chroma_ref_type);
        if self.ref_pic(sel).is_none() {
            return;
        }
        let use_ic = self.luts[lut].use_ic;
        let uvls = self.uvlinesize as isize;
        let mut ptr = uvsrc_y as isize * uvls + uvsrc_x as isize;
        if field_mode && chroma_ref_type != 0 {
            ptr += uvlinesize;
        }
        let v_edge_pos = h >> field_mode as i32;
        let emu = self.rangeredfrm
            || use_ic
            || w < 18
            || v_edge_pos < 18
            || (uvsrc_x as u32) > (((w >> 1) - 9) as u32)
            || (uvsrc_y as u32) > (((v_edge_pos >> 1) - 9) as u32);
        let fsc = if emu {
            self.fetch_chroma(
                sel,
                interlace,
                ptr,
                9,
                uvsrc_x,
                uvsrc_y,
                chroma_ref_type,
                if field_mode { 9 } else { 5 },
                4,
                if field_mode { 17 } else { 9 },
                !field_mode,
                (uvsrc_y + 1) >> 1,
            )
        } else {
            self.fetch_chroma_direct(sel, ptr, 9, 9, uvls, 1)
        };
        if self.rangeredfrm {
            for buf in [&mut self.edge_u, &mut self.edge_v] {
                for j in 0..9 {
                    for i in 0..9 {
                        let v = buf[j * fsc + i] as i32;
                        buf[j * fsc + i] = (((v - 128) >> 1) + 128) as u8;
                    }
                }
            }
        }
        if use_ic {
            let l = self.luts[lut].clone();
            let cu = if field_mode { chroma_ref_type } else { (uvsrc_y & 1) as usize };
            let cv = if field_mode { chroma_ref_type } else { ((1 + uvsrc_y) & 1) as usize };
            lut_scale_luma(&mut self.edge_u, 0, &l.uv[cu], &l.uv[cv], 9, fsc);
            lut_scale_luma_w(&mut self.edge_v, 0, &l.uv[cu], &l.uv[cv], 9, fsc);
        }
        let ux = (uvmx & 3) << 1;
        let uy = (uvmy & 3) << 1;
        let rnd = self.rnd != 0;
        let (d1, d2) = (self.dest[1], self.dest[2]);
        let uvlsu = self.uvlinesize;
        let pic = self.cur.as_mut().unwrap();
        dsp::chroma_mc(&mut pic.planes[1], d1, uvlsu, &self.edge_u, 0, fsc, 8, 8, ux, uy, rnd, false);
        dsp::chroma_mc(&mut pic.planes[2], d2, uvlsu, &self.edge_v, 0, fsc, 8, 8, ux, uy, rnd, false);
        if field_mode {
            let v = (self.cur_field_type != chroma_ref_type) as u8;
            let mo = self.mb_off;
            self.mv_f[dir][self.block_index[4] + mo] = v;
            self.mv_f[dir][self.block_index[5] + mo] = v;
        }
    }

    /// ff_vc1_mc_4mv_chroma4 (interlaced frame pictures)
    pub fn mc_4mv_chroma4(&mut self, dir: usize, dir2: usize, avg: bool) {
        let fieldmv = self.blk_mv_type[self.block_index[0]] as usize;
        const S_RNDTBLFIELD: [i32; 16] = [0, 0, 1, 2, 4, 4, 5, 6, 2, 2, 3, 8, 6, 6, 7, 12];
        let v_dist: i32 = if fieldmv != 0 { 1 } else { 4 };
        let mut uvmx_field = [0i32; 4];
        let mut uvmy_field = [0i32; 4];
        for i in 0..4 {
            let d = if i < 2 { dir } else { dir2 };
            let tx = self.mv[d][i][0];
            uvmx_field[i] = (tx + ((tx & 3) == 3) as i32) >> 1;
            let ty = self.mv[d][i][1];
            uvmy_field[i] = if fieldmv != 0 {
                (ty >> 4) * 8 + S_RNDTBLFIELD[(ty & 0xF) as usize]
            } else {
                (ty + ((ty & 3) == 3) as i32) >> 1
            };
        }
        let w = self.width as i32;
        let h = self.height as i32;
        let uvlinesize = self.cur.as_ref().unwrap().stride[1] as isize;
        for i in 0..4usize {
            let off = (i & 1) * 4 + if i & 2 != 0 { (v_dist as usize) * self.uvlinesize } else { 0 };
            let mut uvsrc_x = self.mb_x as i32 * 8 + (i & 1) as i32 * 4 + (uvmx_field[i] >> 2);
            let mut uvsrc_y = self.mb_y as i32 * 8 + if i & 2 != 0 { v_dist } else { 0 } + (uvmy_field[i] >> 2);
            uvsrc_x = uvsrc_x.clamp(-8, w >> 1);
            if self.fcm == ILACE_FRAME {
                uvsrc_y = uvsrc_y.clamp(-8 + (uvsrc_y & 1), (h >> 1) + (uvsrc_y & 1));
            } else {
                uvsrc_y = uvsrc_y.clamp(-8, h >> 1);
            }
            let d = if i < 2 { dir } else { dir2 };
            let (sel, lut, interlace) = if d != 0 { (RefSel::Next, NEXT, self.next_interlaced) } else { (RefSel::Last, LAST, self.last_interlaced) };
            if self.ref_pic(sel).is_none() {
                return;
            }
            let use_ic = self.luts[lut].use_ic;
            let ptr = uvsrc_y as isize * self.uvlinesize as isize + uvsrc_x as isize;
            let fsc;
            {
                // 5x5 window (rows depend on fieldmv)
                let rows_prog = 5 << fieldmv;
                let rows_field = ((5 << fieldmv) + 1) >> 1;
                let v_edge_pos = h >> 1;
                let emu = use_ic
                    || w < 10
                    || v_edge_pos < (5 << fieldmv)
                    || (uvsrc_x as u32) > (((w >> 1) - 5) as u32)
                    || (uvsrc_y as u32) > ((v_edge_pos - (5 << fieldmv)) as u32);
                fsc = if emu {
                    self.fetch_chroma(sel, interlace, ptr, 5, uvsrc_x, uvsrc_y, 0, rows_field, 2, rows_prog, fieldmv == 0, (uvsrc_y + 1) >> 1)
                } else {
                    self.fetch_chroma_direct(sel, ptr, 5, 5, (self.uvlinesize as isize) << fieldmv, 1 << fieldmv)
                };
            }
            let ux = (uvmx_field[i] & 3) << 1;
            let uy = (uvmy_field[i] & 3) << 1;
            if use_ic {
                let l = self.luts[lut].clone();
                let a = ((uvsrc_y + (0 << fieldmv)) & 1) as usize;
                let b = ((uvsrc_y + (1 << fieldmv)) & 1) as usize;
                lut_scale_luma(&mut self.edge_u, 0, &l.uv[a], &l.uv[b], 5, fsc << fieldmv);
                lut_scale_luma_w(&mut self.edge_v, 0, &l.uv[a], &l.uv[b], 5, fsc << fieldmv);
            }
            let rnd = self.rnd != 0;
            let (d1, d2) = (self.dest[1], self.dest[2]);
            let uvlsu = self.uvlinesize;
            let pic = self.cur.as_mut().unwrap();
            let _ = uvlinesize;
            dsp::chroma_mc(&mut pic.planes[1], d1 + off, uvlsu << fieldmv, &self.edge_u, 0, fsc << fieldmv, 4, 4, ux, uy, rnd, avg);
            dsp::chroma_mc(&mut pic.planes[2], d2 + off, uvlsu << fieldmv, &self.edge_v, 0, fsc << fieldmv, 4, 4, ux, uy, rnd, avg);
        }
    }

    /// ff_vc1_interp_mc: backward prediction averaged into the current MB.
    pub fn interp_mc(&mut self) {
        let field_mode = self.field_mode;
        if !field_mode && self.next.is_none() {
            return;
        }
        let linesize = self.cur.as_ref().unwrap().stride[0] as isize;
        let uvlinesize = self.cur.as_ref().unwrap().stride[1] as isize;
        let use_ic = self.luts[NEXT].use_ic;
        let interlace = self.next_interlaced;
        let mx = self.mv[1][0][0];
        let mut my = self.mv[1][0][1];
        let mut uvmx = (mx + ((mx & 3) == 3) as i32) >> 1;
        let mut uvmy = (my + ((my & 3) == 3) as i32) >> 1;
        if field_mode && self.cur_field_type != self.ref_field_type[1] {
            my = my - 2 + 4 * self.cur_field_type as i32;
            uvmy = uvmy - 2 + 4 * self.cur_field_type as i32;
        }
        if self.fastuvmc {
            uvmx += if uvmx < 0 { -(uvmx & 1) } else { uvmx & 1 };
            uvmy += if uvmy < 0 { -(uvmy & 1) } else { uvmy & 1 };
        }
        if self.next.is_none() {
            return;
        }
        let w = self.width as i32;
        let h = self.height as i32;
        let mut src_x = self.mb_x as i32 * 16 + (mx >> 2);
        let mut src_y = self.mb_y as i32 * 16 + (my >> 2);
        let mut uvsrc_x = self.mb_x as i32 * 8 + (uvmx >> 2);
        let mut uvsrc_y = self.mb_y as i32 * 8 + (uvmy >> 2);
        src_x = src_x.clamp(-17, w);
        uvsrc_x = uvsrc_x.clamp(-8, w >> 1);
        if self.fcm == ILACE_FRAME {
            src_y = src_y.clamp(-18 + (src_y & 1), h + (src_y & 1));
            uvsrc_y = uvsrc_y.clamp(-8 + (uvsrc_y & 1), (h >> 1) + (uvsrc_y & 1));
        } else {
            src_y = src_y.clamp(-18, h + 1);
            uvsrc_y = uvsrc_y.clamp(-8, h >> 1);
        }
        let ls = self.linesize as isize;
        let uvls = self.uvlinesize as isize;
        let mut ptr_y = src_y as isize * ls + src_x as isize;
        let mut ptr_u = uvsrc_y as isize * uvls + uvsrc_x as isize;
        if field_mode && self.ref_field_type[1] != 0 {
            ptr_y += linesize;
            ptr_u += uvlinesize;
        }
        let mspel = self.mspel as i32;
        let k = (17 + mspel * 2) as usize;
        ptr_y -= mspel as isize * (1 + ls);
        let rft = self.ref_field_type[1];
        let v_edge_pos = h >> field_mode as i32;
        let emu = self.rangeredfrm
            || w < 22
            || v_edge_pos < 22
            || use_ic
            || ((src_x - 1) as u32) > ((w - (mx & 3) - 16 - 3) as u32)
            || ((src_y - 1) as u32) > ((v_edge_pos - (my & 3) - 16 - 3) as u32);
        let (fs, fsc);
        if emu {
            fs = self.fetch_luma(RefSel::Next, interlace, ptr_y, k as i32, k as i32, src_x - mspel, src_y, rft, 0, mspel, false);
            fsc = self.fetch_chroma(
                RefSel::Next,
                interlace,
                ptr_u,
                9,
                uvsrc_x,
                uvsrc_y,
                rft,
                if field_mode { 9 } else { 5 },
                4,
                if field_mode { 17 } else { 9 },
                !field_mode,
                (uvsrc_y + 1) >> 1,
            );
        } else {
            fs = self.fetch_luma_direct(RefSel::Next, ptr_y, k, k, ls, 1);
            fsc = self.fetch_chroma_direct(RefSel::Next, ptr_u, 9, 9, uvls, 1);
        }
        if self.rangeredfrm {
            scale_luma_rr(&mut self.edge, k, fs);
            for buf in [&mut self.edge_u, &mut self.edge_v] {
                for j in 0..9 {
                    for i in 0..9 {
                        let v = buf[j * fsc + i] as i32;
                        buf[j * fsc + i] = (((v - 128) >> 1) + 128) as u8;
                    }
                }
            }
        }
        if use_ic {
            let l = self.luts[NEXT].clone();
            let ly = if field_mode { rft } else { ((src_y - mspel) & 1) as usize };
            let ly2 = if field_mode { rft } else { ((1 + src_y - mspel) & 1) as usize };
            lut_scale_luma(&mut self.edge, 0, &l.y[ly], &l.y[ly2], k, fs);
            let cu = if field_mode { rft } else { (uvsrc_y & 1) as usize };
            let cv = if field_mode { rft } else { ((1 + uvsrc_y) & 1) as usize };
            lut_scale_luma(&mut self.edge_u, 0, &l.uv[cu], &l.uv[cv], 9, fsc);
            lut_scale_luma_w(&mut self.edge_v, 0, &l.uv[cu], &l.uv[cv], 9, fsc);
        }
        let spos = (mspel as usize) * (1 + fs);
        let rnd = self.rnd;
        let d0 = self.dest[0];
        let (d1, d2) = (self.dest[1], self.dest[2]);
        let lsu = self.linesize;
        let uvlsu = self.uvlinesize;
        let pic = self.cur.as_mut().unwrap();
        if self.mspel {
            dsp::mspel_mc(&mut pic.planes[0], d0, lsu, &self.edge, spos, fs, (mx & 3) as usize, (my & 3) as usize, rnd, 16, true);
        } else {
            let dxy = ((my & 2) | ((mx & 2) >> 1)) as usize;
            dsp::hpel_mc(&mut pic.planes[0], d0, lsu, &self.edge, spos, fs, dxy, 16, rnd != 0, true);
        }
        let ux = (uvmx & 3) << 1;
        let uy = (uvmy & 3) << 1;
        dsp::chroma_mc(&mut pic.planes[1], d1, uvlsu, &self.edge_u, 0, fsc, 8, 8, ux, uy, rnd != 0, true);
        dsp::chroma_mc(&mut pic.planes[2], d2, uvlsu, &self.edge_v, 0, fsc, 8, 8, ux, uy, rnd != 0, true);
    }
}

/// same as lut_scale_luma but kept separate to document the chroma V plane usage
fn lut_scale_luma_w(buf: &mut [u8], off: usize, l1: &[u8; 256], l2: &[u8; 256], k: usize, stride: usize) {
    lut_scale_luma(buf, off, l1, l2, k, stride)
}
