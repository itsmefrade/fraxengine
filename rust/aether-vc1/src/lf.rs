//! In-loop deblocking and overlap smoothing (FFmpeg vc1_loopfilter.c).
use crate::core::*;
use crate::dsp;
use crate::blocks::BLOCK_MAP;

const LEFT_EDGE: u32 = 1;
const RIGHT_EDGE: u32 = 2;
const TOP_EDGE: u32 = 4;
const BOTTOM_EDGE: u32 = 8;

impl Core {
    #[inline]
    fn lf_v(&mut self, plane: usize, pos: isize, stride: usize, len: usize) {
        if pos < 0 {
            return;
        }
        let pq = self.pq;
        let buf = &mut self.cur.as_mut().unwrap().planes[plane];
        dsp::v_loop_filter(buf, pos as usize, stride, len, pq);
    }
    #[inline]
    fn lf_h(&mut self, plane: usize, pos: isize, stride: usize, len: usize) {
        if pos < 0 {
            return;
        }
        let pq = self.pq;
        let buf = &mut self.cur.as_mut().unwrap().planes[plane];
        dsp::h_loop_filter(buf, pos as usize, stride, len, pq);
    }

    // ------------------------------------------------------------ overlap on coefficient blocks

    /// Two disjoint mutable windows of the flat block ring starting at element offsets a and b.
    fn two(&mut self, a: usize, la: usize, b: usize, lb: usize) -> (&mut [i16], &mut [i16]) {
        let n = self.block.len();
        let la = la.min(n - a);
        let lb = lb.min(n - b);
        if a < b {
            let (l, r) = self.block.split_at_mut(b);
            (&mut l[a..(a + la).min(b)], &mut r[..lb])
        } else {
            let (l, r) = self.block.split_at_mut(a);
            (&mut r[..la], &mut l[b..(b + lb).min(a)])
        }
    }

    fn v_overlap(&mut self, top_ring: usize, bot_ring: usize, block_num: usize) {
        let (ta, ba) = match block_num {
            0 => ((top_ring, 1), (bot_ring, 0)),
            1 => ((top_ring, 3), (bot_ring, 2)),
            2 => ((bot_ring, 0), (bot_ring, 1)),
            3 => ((bot_ring, 2), (bot_ring, 3)),
            _ => ((top_ring, block_num), (bot_ring, block_num)),
        };
        let fa = (ta.0 * 6 + ta.1) * 64;
        let fb = (ba.0 * 6 + ba.1) * 64;
        if fa == fb {
            return;
        }
        let (t, b) = self.two(fa, 64, fb, 64);
        dsp::v_s_overlap_ab(t, b);
    }

    #[allow(clippy::too_many_arguments)]
    fn h_overlap(&mut self, left_ring: usize, right_ring: usize, lft: bool, rft: bool, block_num: usize) {
        let lf = lft as usize;
        let rf = rft as usize;
        let diff = lft ^ rft;
        // (left block, left offset, right block, right offset, lstride, rstride, flags)
        let (lb, lo, rb, ro, ls, rs, flags): (usize, usize, usize, usize, usize, usize, i32) = match block_num {
            0 => (
                left_ring * 6 + 2,
                0,
                right_ring * 6,
                0,
                if diff { 16 - 8 * lf } else { 8 },
                if diff { 16 - 8 * rf } else { 8 },
                if lft || rft { 0 } else { 1 },
            ),
            1 => (right_ring * 6, 0, right_ring * 6 + 2, 0, 8, 8, if rft { 0 } else { 1 }),
            2 => (
                if !lft && rft { left_ring * 6 + 2 } else { left_ring * 6 + 3 },
                if !lft && rft { 8 } else { 0 },
                if lft && !rft { right_ring * 6 } else { right_ring * 6 + 1 },
                if lft && !rft { 8 } else { 0 },
                if diff { 16 - 8 * lf } else { 8 },
                if diff { 16 - 8 * rf } else { 8 },
                if lft || rft { 2 } else { 1 },
            ),
            3 => (right_ring * 6 + 1, 0, right_ring * 6 + 3, 0, 8, 8, if rft { 2 } else { 1 }),
            _ => (left_ring * 6 + block_num, 0, right_ring * 6 + block_num, 0, 8, 8, 1),
        };
        let a = lb * 64 + lo;
        let b = rb * 64 + ro;
        if a == b {
            return;
        }
        // each window must cover 8 rows with the given stride plus the 8 columns
        let (l, r) = self.two(a, 7 * ls + 8, b, 7 * rs + 8);
        // windows may overlap in theory when the same ring entry is used; guard by length
        if l.len() < 7 * ls + 8 || r.len() < 7 * rs + 8 {
            return;
        }
        dsp::h_s_overlap(l, ls, r, rs, flags);
    }

    pub fn i_overlap_filter(&mut self) {
        let mb_pos = self.mb_x + self.mb_y * self.lay.mb_stride;
        let ms = self.lay.mb_stride;
        let (cur, left, top, topleft) = (self.cur_blk_idx, self.left_blk_idx, self.top_blk_idx, self.topleft_blk_idx);
        for i in 0..6usize {
            if self.mb_x == 0 && (i & 5) != 1 {
                continue;
            }
            let cond = self.pq >= 9
                || (self.profile == 3
                    && (self.condover == CONDOVER_ALL
                        || (self.over_flags_plane[mb_pos] != 0
                            && ((i & 5) == 1 || self.over_flags_plane[mb_pos - 1] != 0))));
            if cond {
                let lft = self.fcm == ILACE_FRAME && self.mb_x != 0 && self.fieldtx_plane[mb_pos - 1] != 0;
                let rft = self.fcm == ILACE_FRAME && self.fieldtx_plane[mb_pos] != 0;
                self.h_overlap(if self.mb_x != 0 { left } else { cur }, cur, lft, rft, i);
            }
        }
        if self.fcm != ILACE_FRAME {
            for i in 0..6usize {
                if self.first_slice_line && (i & 2) == 0 {
                    continue;
                }
                if self.mb_x != 0
                    && (self.pq >= 9
                        || (self.profile == 3
                            && (self.condover == CONDOVER_ALL
                                || (self.over_flags_plane[mb_pos - 1] != 0
                                    && ((i & 2) != 0 || self.over_flags_plane[mb_pos - 1 - ms] != 0)))))
                {
                    self.v_overlap(if self.first_slice_line { left } else { topleft }, left, i);
                }
                if self.mb_x == self.mb_width - 1
                    && (self.pq >= 9
                        || (self.profile == 3
                            && (self.condover == CONDOVER_ALL
                                || (self.over_flags_plane[mb_pos] != 0
                                    && ((i & 2) != 0 || self.over_flags_plane[mb_pos - ms] != 0)))))
                {
                    self.v_overlap(if self.first_slice_line { cur } else { top }, cur, i);
                }
            }
        }
    }

    pub fn p_overlap_filter(&mut self) {
        let mb_pos = self.mb_x + self.mb_y * self.lay.mb_stride;
        let (cur, left, top, topleft) = (self.cur_blk_idx, self.left_blk_idx, self.top_blk_idx, self.topleft_blk_idx);
        for i in 0..6usize {
            if self.mb_x == 0 && (i & 5) != 1 {
                continue;
            }
            let bi = self.block_index[i];
            if self.mb_type_b[bi] != 0 && self.mb_type_b[bi - 1] != 0 {
                let lft = self.fcm == ILACE_FRAME && self.mb_x != 0 && self.fieldtx_plane[mb_pos - 1] != 0;
                let rft = self.fcm == ILACE_FRAME && self.fieldtx_plane[mb_pos] != 0;
                self.h_overlap(if self.mb_x != 0 { left } else { cur }, cur, lft, rft, i);
            }
        }
        if self.fcm != ILACE_FRAME {
            for i in 0..6usize {
                if self.first_slice_line && (i & 2) == 0 {
                    continue;
                }
                let bi = self.block_index[i];
                let wr = self.block_wrap[i];
                let ch = (i > 3) as usize;
                if self.mb_x != 0 && self.mb_type_b[bi - 2 + ch] != 0 && self.mb_type_b[bi - wr - 2 + ch] != 0 {
                    self.v_overlap(if self.first_slice_line { left } else { topleft }, left, i);
                }
                if self.mb_x == self.mb_width - 1 && self.mb_type_b[bi] != 0 && self.mb_type_b[bi - wr] != 0 {
                    self.v_overlap(if self.first_slice_line { cur } else { top }, cur, i);
                }
            }
        }
    }

    // ------------------------------------------------------------ put blocks

    fn put_block(&mut self, ring: usize, k: usize, plane: usize, off: isize, stride: usize, signed: bool) {
        if off < 0 {
            return;
        }
        let o = (ring * 6 + BLOCK_MAP[k]) * 64;
        let blk: [i16; 64] = self.block[o..o + 64].try_into().unwrap();
        let buf = &mut self.cur.as_mut().unwrap().planes[plane];
        if signed {
            dsp::put_signed_pixels_clamped(&blk, buf, off as usize, stride);
        } else {
            dsp::put_pixels_clamped(&blk, buf, off as usize, stride);
        }
    }

    pub fn put_blocks_clamped(&mut self, signed: bool) {
        let ls = self.linesize as isize;
        let uvls = self.uvlinesize as isize;
        let d0 = self.dest[0] as isize;
        let dc = [self.dest[0] as isize, self.dest[1] as isize, self.dest[2] as isize];
        let bi = self.block_index;
        let bw = self.block_wrap;
        let end_mb_x = self.end_mb_x;
        if !self.first_slice_line && self.fcm != ILACE_FRAME {
            if self.mb_x != 0 {
                for i in 0..6usize {
                    let chk = if i > 3 { self.mb_type_b[bi[i] - bw[i] - 1] } else { self.mb_type_b[bi[i] - 2 * bw[i] - 2] };
                    if chk != 0 {
                        let (plane, off, stride) = if i > 3 {
                            (i - 3, dc[i - 3] - 8 * uvls - 8, uvls)
                        } else {
                            (0, d0 + ((i & 2) as isize - 4) * 4 * ls + ((i & 1) as isize - 2) * 8, ls)
                        };
                        self.put_block(self.topleft_blk_idx, i, plane, off, stride as usize, signed);
                    }
                }
            }
            if self.mb_x == end_mb_x - 1 {
                for i in 0..6usize {
                    let chk = if i > 3 { self.mb_type_b[bi[i] - bw[i]] } else { self.mb_type_b[bi[i] - 2 * bw[i]] };
                    if chk != 0 {
                        let (plane, off, stride) = if i > 3 {
                            (i - 3, dc[i - 3] - 8 * uvls, uvls)
                        } else {
                            (0, d0 + ((i & 2) as isize - 4) * 4 * ls + (i & 1) as isize * 8, ls)
                        };
                        self.put_block(self.top_blk_idx, i, plane, off, stride as usize, signed);
                    }
                }
            }
        }
        if self.mb_y == self.end_mb_y - 1 || self.fcm == ILACE_FRAME {
            let mut fieldtx = 0usize;
            let mb_pos = self.mb_x + self.mb_y * self.lay.mb_stride;
            if self.mb_x != 0 {
                if self.fcm == ILACE_FRAME {
                    fieldtx = self.fieldtx_plane[mb_pos - 1] as usize;
                }
                for i in 0..6usize {
                    let chk = if i > 3 { self.mb_type_b[bi[i] - 1] } else { self.mb_type_b[bi[i] - 2] };
                    if chk != 0 {
                        let (plane, off, stride) = if i > 3 {
                            (i - 3, dc[i - 3] - 8, uvls)
                        } else if fieldtx != 0 {
                            (0, d0 + ((i & 2) >> 1) as isize * ls + ((i & 1) as isize - 2) * 8, ls << 1)
                        } else {
                            (0, d0 + (i & 2) as isize * 4 * ls + ((i & 1) as isize - 2) * 8, ls)
                        };
                        self.put_block(self.left_blk_idx, i, plane, off, stride as usize, signed);
                    }
                }
            }
            if self.mb_x == end_mb_x - 1 {
                if self.fcm == ILACE_FRAME {
                    fieldtx = self.fieldtx_plane[mb_pos] as usize;
                }
                for i in 0..6usize {
                    if self.mb_type_b[bi[i]] != 0 {
                        let (plane, off, stride) = if i > 3 {
                            (i - 3, dc[i - 3], uvls)
                        } else if fieldtx != 0 {
                            (0, d0 + ((i & 2) >> 1) as isize * ls + (i & 1) as isize * 8, ls << 1)
                        } else {
                            (0, d0 + (i & 2) as isize * 4 * ls + (i & 1) as isize * 8, ls)
                        };
                        self.put_block(self.cur_blk_idx, i, plane, off, stride as usize, signed);
                    }
                }
            }
        }
    }

    // ------------------------------------------------------------ I-picture loop filter

    fn i_h_loop_filter(&mut self, dest: isize, flags: u32, block_num: usize) {
        if block_num & 2 != 0 {
            return;
        }
        if flags & LEFT_EDGE == 0 || (block_num & 5) == 1 {
            let ls = self.linesize as isize;
            let uvls = self.uvlinesize as isize;
            let dst = if block_num > 3 { dest } else { dest + (block_num & 2) as isize * 4 * ls + (block_num & 1) as isize * 8 };
            let plane = if block_num > 3 { block_num - 3 } else { 0 };
            if self.fcm == ILACE_FRAME {
                if block_num > 3 {
                    self.lf_h(plane, dst, (2 * uvls) as usize, 4);
                    self.lf_h(plane, dst + uvls, (2 * uvls) as usize, 4);
                } else {
                    self.lf_h(plane, dst, (2 * ls) as usize, 8);
                    self.lf_h(plane, dst + ls, (2 * ls) as usize, 8);
                }
            } else if block_num > 3 {
                self.lf_h(plane, dst, uvls as usize, 8);
            } else {
                self.lf_h(plane, dst, ls as usize, 16);
            }
        }
    }

    fn i_v_loop_filter(&mut self, dest: isize, flags: u32, fieldtx: u8, block_num: usize) {
        if (block_num & 5) == 1 {
            return;
        }
        if flags & TOP_EDGE == 0 || block_num & 2 != 0 {
            let ls = self.linesize as isize;
            let uvls = self.uvlinesize as isize;
            let dst = if block_num > 3 { dest } else { dest + (block_num & 2) as isize * 4 * ls + (block_num & 1) as isize * 8 };
            let plane = if block_num > 3 { block_num - 3 } else { 0 };
            if self.fcm == ILACE_FRAME {
                if block_num > 3 {
                    self.lf_v(plane, dst, (2 * uvls) as usize, 8);
                    self.lf_v(plane, dst + uvls, (2 * uvls) as usize, 8);
                } else if block_num < 2 || fieldtx == 0 {
                    self.lf_v(plane, dst, (2 * ls) as usize, 16);
                    self.lf_v(plane, dst + ls, (2 * ls) as usize, 16);
                }
            } else if block_num > 3 {
                self.lf_v(plane, dst, uvls as usize, 8);
            } else {
                self.lf_v(plane, dst, ls as usize, 16);
            }
        }
    }

    pub fn i_loop_filter(&mut self) {
        let mb_pos = self.mb_x + self.mb_y * self.lay.mb_stride;
        let ms = self.lay.mb_stride;
        let ls = self.linesize as isize;
        let uvls = self.uvlinesize as isize;
        let d = [self.dest[0] as isize, self.dest[1] as isize, self.dest[2] as isize];
        let end_mb_x = self.end_mb_x;
        let sty = self.start_mb_y;
        let ey = self.end_mb_y;
        let fp = |s: &Core, idx: usize| s.fieldtx_plane[idx];
        let cdest = |i: usize| d[i - 3];
        if !self.first_slice_line {
            let mut dest = d[0] - 16 * ls - 16;
            let flags = if self.mb_y == sty + 1 { TOP_EDGE } else { 0 };
            if self.mb_x != 0 {
                let fieldtx = fp(self, mb_pos - ms - 1);
                for i in 0..6 {
                    let dd = if i > 3 { cdest(i) - 8 * uvls - 8 } else { dest };
                    self.i_v_loop_filter(dd, flags, fieldtx, i);
                }
            }
            if self.mb_x == end_mb_x - 1 {
                dest += 16;
                let fieldtx = fp(self, mb_pos - ms);
                for i in 0..6 {
                    let dd = if i > 3 { cdest(i) - 8 * uvls } else { dest };
                    self.i_v_loop_filter(dd, flags, fieldtx, i);
                }
            }
        }
        if self.mb_y == ey - 1 {
            let mut dest = d[0] - 16;
            let flags = if self.first_slice_line { TOP_EDGE | BOTTOM_EDGE } else { BOTTOM_EDGE };
            if self.mb_x != 0 {
                let fieldtx = fp(self, mb_pos - 1);
                for i in 0..6 {
                    let dd = if i > 3 { cdest(i) - 8 } else { dest };
                    self.i_v_loop_filter(dd, flags, fieldtx, i);
                }
            }
            if self.mb_x == end_mb_x - 1 {
                dest += 16;
                let fieldtx = fp(self, mb_pos);
                for i in 0..6 {
                    let dd = if i > 3 { cdest(i) } else { dest };
                    self.i_v_loop_filter(dd, flags, fieldtx, i);
                }
            }
        }
        if self.mb_y >= sty + 2 {
            let mut dest = d[0] - 32 * ls - 16;
            if self.mb_x != 0 {
                let flags = if self.mb_x == 1 { LEFT_EDGE } else { 0 };
                for i in 0..6 {
                    let dd = if i > 3 { cdest(i) - 16 * uvls - 8 } else { dest };
                    self.i_h_loop_filter(dd, flags, i);
                }
            }
            if self.mb_x == end_mb_x - 1 {
                dest += 16;
                let flags = if self.mb_x == 0 { LEFT_EDGE | RIGHT_EDGE } else { RIGHT_EDGE };
                for i in 0..6 {
                    let dd = if i > 3 { cdest(i) - 16 * uvls } else { dest };
                    self.i_h_loop_filter(dd, flags, i);
                }
            }
        }
        if self.mb_y == ey - 1 {
            if self.mb_y > sty {
                let mut dest = d[0] - 16 * ls - 16;
                if self.mb_x != 0 {
                    let flags = if self.mb_x == 1 { LEFT_EDGE } else { 0 };
                    for i in 0..6 {
                        let dd = if i > 3 { cdest(i) - 8 * uvls - 8 } else { dest };
                        self.i_h_loop_filter(dd, flags, i);
                    }
                }
                if self.mb_x == end_mb_x - 1 {
                    let flags = if self.mb_x == 0 { LEFT_EDGE | RIGHT_EDGE } else { RIGHT_EDGE };
                    dest += 16;
                    for i in 0..6 {
                        let dd = if i > 3 { cdest(i) - 8 * uvls } else { dest };
                        self.i_h_loop_filter(dd, flags, i);
                    }
                }
            }
            let mut dest = d[0] - 16;
            if self.mb_x != 0 {
                let flags = if self.mb_x == 1 { LEFT_EDGE } else { 0 };
                for i in 0..6 {
                    let dd = if i > 3 { cdest(i) - 8 } else { dest };
                    self.i_h_loop_filter(dd, flags, i);
                }
            }
            if self.mb_x == end_mb_x - 1 {
                dest += 16;
                let flags = if self.mb_x == 0 { LEFT_EDGE | RIGHT_EDGE } else { RIGHT_EDGE };
                for i in 0..6 {
                    let dd = if i > 3 { cdest(i) } else { dest };
                    self.i_h_loop_filter(dd, flags, i);
                }
            }
        }
    }

    // ------------------------------------------------------------ P-picture loop filter

    /// ring index of macroblock (mb_x - c, mb_y - r) in the 3 row rings
    #[inline]
    fn ring(&self, r: usize, c: usize) -> usize {
        2 * self.lay.mb_stride + self.mb_x - r * self.lay.mb_stride - c
    }

    /// Parameters common to the P filters for block `i` of the macroblock r rows up, c columns left:
    /// (plane, dest offset, luma/chroma mv index base, mv_f index base)
    fn p_geometry(&self, r: usize, c: usize, i: usize) -> (usize, isize, usize) {
        let ls = self.linesize as isize;
        let uvls = self.uvlinesize as isize;
        let d = self.dest;
        if i > 3 {
            let dest = d[i - 3] as isize - 8 * r as isize * uvls - 8 * c as isize;
            (i - 3, dest, self.block_index[i] - r * self.lay.mb_stride - c + self.mb_off)
        } else {
            let dest = d[0] as isize - 16 * r as isize * ls - 16 * c as isize;
            (0, dest, self.block_index[i] - 2 * r * self.lay.b8_stride - 2 * c + self.blocks_off)
        }
    }

    fn p_v_filter(&mut self, r: usize, c: usize, flags: u32, block_num: usize) {
        let (plane, dest, midx) = self.p_geometry(r, c, block_num);
        let ring = self.ring(r, c);
        let ms = self.lay.mb_stride;
        let ls = if block_num > 3 { self.uvlinesize } else { self.linesize };
        let pq = self.pq;
        let dst = if block_num > 3 {
            dest
        } else {
            dest + (block_num & 2) as isize * 4 * self.linesize as isize + (block_num & 1) as isize * 8
        };
        let cbp0 = self.cbp[ring];
        let top_cbp = cbp0 >> (block_num * 4);
        if flags & BOTTOM_EDGE == 0 || block_num < 2 {
            let top_is_intra = self.is_intra[ring] & (1 << block_num) != 0;
            let (bottom_is_intra, bottom_cbp);
            if block_num > 3 {
                bottom_is_intra = self.is_intra[ring + ms] & (1 << block_num) != 0;
                bottom_cbp = self.cbp[ring + ms] >> (block_num * 4);
            } else if block_num < 2 {
                bottom_is_intra = self.is_intra[ring] & (1 << (block_num + 2)) != 0;
                bottom_cbp = cbp0 >> ((block_num + 2) * 4);
            } else {
                bottom_is_intra = self.is_intra[ring + ms] & (1 << (block_num - 2)) != 0;
                bottom_cbp = self.cbp[ring + ms] >> ((block_num - 2) * 4);
            }
            let (m0, m1, f0, f1);
            if block_num > 3 {
                m0 = self.luma_mv[ring];
                m1 = self.luma_mv[ring + ms];
                f0 = self.mv_f[0][midx];
                f1 = self.mv_f[0][midx + ms];
            } else {
                m0 = self.cmv(0, midx);
                m1 = self.cmv(0, midx + self.lay.b8_stride);
                f0 = self.mv_f[0][midx];
                f1 = self.mv_f[0][midx + self.lay.b8_stride];
            }
            let ds = dst + 8 * ls as isize;
            if top_is_intra || bottom_is_intra || m0 != m1 || (self.fcm == ILACE_FIELD && f0 != f1) {
                self.lf_v(plane, ds, ls, 8);
            } else {
                let idx = (top_cbp | (bottom_cbp >> 2)) & 3;
                if idx & 1 != 0 {
                    self.lf_v(plane, ds + 4, ls, 4);
                }
                if idx & 2 != 0 {
                    self.lf_v(plane, ds, ls, 4);
                }
            }
        }
        let tt = (self.ttblk[ring] >> (block_num * 4)) & 0xf;
        if tt == TT_4X4 || tt == TT_8X4 {
            let ds = dst + 4 * ls as isize;
            if top_cbp & 5 != 0 {
                self.lf_v(plane, ds + 4, ls, 4);
            }
            if top_cbp & 10 != 0 {
                self.lf_v(plane, ds, ls, 4);
            }
        }
        let _ = pq;
    }

    fn p_h_filter(&mut self, r: usize, c: usize, flags: u32, block_num: usize) {
        let (plane, dest, midx) = self.p_geometry(r, c, block_num);
        let ring = self.ring(r, c);
        let ls = if block_num > 3 { self.uvlinesize } else { self.linesize };
        let dst = if block_num > 3 {
            dest
        } else {
            dest + (block_num & 2) as isize * 4 * self.linesize as isize + (block_num & 1) as isize * 8
        };
        let left_cbp = self.cbp[ring] >> (block_num * 4);
        if flags & RIGHT_EDGE == 0 || (block_num & 5) == 0 {
            let left_is_intra = self.is_intra[ring] & (1 << block_num) != 0;
            let (right_is_intra, right_cbp);
            if block_num > 3 {
                right_is_intra = self.is_intra[ring + 1] & (1 << block_num) != 0;
                right_cbp = self.cbp[ring + 1] >> (block_num * 4);
            } else if block_num & 1 != 0 {
                right_is_intra = self.is_intra[ring + 1] & (1 << (block_num - 1)) != 0;
                right_cbp = self.cbp[ring + 1] >> ((block_num - 1) * 4);
            } else {
                right_is_intra = self.is_intra[ring] & (1 << (block_num + 1)) != 0;
                right_cbp = self.cbp[ring] >> ((block_num + 1) * 4);
            }
            let (m0, m1);
            if block_num > 3 {
                m0 = self.luma_mv[ring];
                m1 = self.luma_mv[ring + 1];
            } else {
                m0 = self.cmv(0, midx);
                m1 = self.cmv(0, midx + 1);
            }
            let f0 = self.mv_f[0][midx];
            let f1 = self.mv_f[0][midx + 1];
            if left_is_intra || right_is_intra || m0 != m1 || (self.fcm == ILACE_FIELD && f0 != f1) {
                self.lf_h(plane, dst + 8, ls, 8);
            } else {
                let idx = (left_cbp | (right_cbp >> 1)) & 5;
                if idx & 1 != 0 {
                    self.lf_h(plane, dst + 4 * ls as isize + 8, ls, 4);
                }
                if idx & 4 != 0 {
                    self.lf_h(plane, dst + 8, ls, 4);
                }
            }
        }
        let tt = (self.ttblk[ring] >> (block_num * 4)) & 0xf;
        if tt == TT_4X4 || tt == TT_4X8 {
            if left_cbp & 3 != 0 {
                self.lf_h(plane, dst + 4 * ls as isize + 4, ls, 4);
            }
            if left_cbp & 12 != 0 {
                self.lf_h(plane, dst + 4, ls, 4);
            }
        }
    }

    pub fn p_loop_filter(&mut self) {
        let sty = self.start_mb_y;
        let ey = self.end_mb_y;
        let mbx = self.mb_x;
        let last_x = self.mb_width - 1;
        if self.mb_y >= sty + 2 {
            if mbx != 0 {
                let flags = if self.mb_y == sty + 2 { TOP_EDGE } else { 0 };
                for i in 0..6 {
                    self.p_v_filter(2, 1, flags, i);
                }
            }
            if mbx == last_x {
                let flags = if self.mb_y == sty + 2 { TOP_EDGE } else { 0 };
                for i in 0..6 {
                    self.p_v_filter(2, 0, flags, i);
                }
            }
        }
        if self.mb_y == ey - 1 {
            if mbx != 0 {
                if self.mb_y > sty {
                    let flags = if self.mb_y == sty + 1 { TOP_EDGE } else { 0 };
                    for i in 0..6 {
                        self.p_v_filter(1, 1, flags, i);
                    }
                }
                let flags = if self.mb_y == sty { TOP_EDGE | BOTTOM_EDGE } else { BOTTOM_EDGE };
                for i in 0..6 {
                    self.p_v_filter(0, 1, flags, i);
                }
            }
            if mbx == last_x {
                if self.mb_y > sty {
                    let flags = if self.mb_y == sty + 1 { TOP_EDGE } else { 0 };
                    for i in 0..6 {
                        self.p_v_filter(1, 0, flags, i);
                    }
                }
                let flags = if self.mb_y == sty { TOP_EDGE | BOTTOM_EDGE } else { BOTTOM_EDGE };
                for i in 0..6 {
                    self.p_v_filter(0, 0, flags, i);
                }
            }
        }
        if self.mb_y >= sty + 2 {
            if mbx >= 2 {
                let flags = if mbx == 2 { LEFT_EDGE } else { 0 };
                for i in 0..6 {
                    self.p_h_filter(2, 2, flags, i);
                }
            }
            if mbx == last_x {
                if mbx >= 1 {
                    let flags = if mbx == 1 { LEFT_EDGE } else { 0 };
                    for i in 0..6 {
                        self.p_h_filter(2, 1, flags, i);
                    }
                }
                let flags = if mbx != 0 { RIGHT_EDGE } else { LEFT_EDGE | RIGHT_EDGE };
                for i in 0..6 {
                    self.p_h_filter(2, 0, flags, i);
                }
            }
        }
        if self.mb_y == ey - 1 {
            if self.mb_y > sty {
                if mbx >= 2 {
                    let flags = if mbx == 2 { LEFT_EDGE } else { 0 };
                    for i in 0..6 {
                        self.p_h_filter(1, 2, flags, i);
                    }
                }
                if mbx == last_x {
                    if mbx >= 1 {
                        let flags = if mbx == 1 { LEFT_EDGE } else { 0 };
                        for i in 0..6 {
                            self.p_h_filter(1, 1, flags, i);
                        }
                    }
                    let flags = if mbx != 0 { RIGHT_EDGE } else { LEFT_EDGE | RIGHT_EDGE };
                    for i in 0..6 {
                        self.p_h_filter(1, 0, flags, i);
                    }
                }
            }
            if mbx >= 2 {
                let flags = if mbx == 2 { LEFT_EDGE } else { 0 };
                for i in 0..6 {
                    self.p_h_filter(0, 2, flags, i);
                }
            }
            if mbx == last_x {
                if mbx >= 1 {
                    let flags = if mbx == 1 { LEFT_EDGE } else { 0 };
                    for i in 0..6 {
                        self.p_h_filter(0, 1, flags, i);
                    }
                }
                let flags = if mbx != 0 { RIGHT_EDGE } else { LEFT_EDGE | RIGHT_EDGE };
                for i in 0..6 {
                    self.p_h_filter(0, 0, flags, i);
                }
            }
        }
    }
}
