//! Public decoder: access unit parsing, reference management and output ordering
//! (FFmpeg vc1dec.c `vc1_decode_frame`).
use crate::bits::BitReader;
use crate::core::*;
use crate::dsp::unescape;
use std::collections::VecDeque;
use std::rc::Rc;

const CODE_ENDOFSEQ: u8 = 0x0A;
const CODE_SLICE: u8 = 0x0B;
const CODE_FIELD: u8 = 0x0C;
const CODE_FRAME: u8 = 0x0D;
const CODE_ENTRYPOINT: u8 = 0x0E;
const CODE_SEQHDR: u8 = 0x0F;

/// Maximum number of decoded frames queued for the host before the oldest is dropped.
pub const MAX_OUTPUT_QUEUE: usize = 8;

/// Position of the next `00 00 01 xx` marker at or after `from` (or `data.len()`).
pub fn find_next_marker(data: &[u8], from: usize) -> usize {
    let n = data.len();
    if n < 4 || from + 4 > n {
        return n;
    }
    let mut i = from;
    while i + 4 <= n {
        if data[i + 2] > 1 {
            i += 3;
        } else if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            return i;
        } else {
            i += 1;
        }
    }
    n
}

/// Split a raw Annex G elementary stream into access units (frame data plus the sequence
/// header / entry point that precede it). Returns byte ranges.
pub fn split_access_units(data: &[u8]) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut pos = find_next_marker(data, 0);
    let mut cur_start = pos;
    let mut has_frame = false;
    let n = data.len();
    while pos < n {
        let code = data[pos + 3];
        match code {
            CODE_SEQHDR | CODE_ENTRYPOINT => {
                if has_frame {
                    out.push((cur_start, pos));
                    cur_start = pos;
                    has_frame = false;
                }
            }
            CODE_FRAME => {
                if has_frame {
                    out.push((cur_start, pos));
                    cur_start = pos;
                }
                has_frame = true;
            }
            _ => {}
        }
        pos = find_next_marker(data, pos + 4);
    }
    if cur_start < n && has_frame {
        out.push((cur_start, n));
    }
    out
}

#[derive(Clone, Debug, Default)]
pub struct StreamInfo {
    pub width: usize,
    pub height: usize,
    pub display_width: usize,
    pub display_height: usize,
    pub profile: u8,
    pub level: u8,
    pub interlace: bool,
    pub broadcast: bool,
    pub aspect_num: u32,
    pub aspect_den: u32,
    pub fps_num: u32,
    pub fps_den: u32,
    pub color_primaries: u8,
    pub transfer_characteristics: u8,
    pub matrix_coefficients: u8,
    pub loop_filter: bool,
    pub overlap: bool,
    pub range_mapy: Option<u8>,
    pub range_mapuv: Option<u8>,
}

/// A decoded picture in display order.
#[derive(Clone)]
pub struct Frame {
    pic: Rc<Pic>,
    mapped: Option<Rc<[Vec<u8>; 3]>>,
    pub width: usize,
    pub height: usize,
    pub pts: i64,
    /// Frame (or field pair) was coded with interlaced tools.
    pub interlaced: bool,
    pub top_field_first: bool,
    /// RFF: repeat the first field (pull-down).
    pub repeat_first_field: bool,
    /// RPTFRM: number of times the frame is repeated (progressive pull-down).
    pub repeat_frames: u8,
    /// Equivalent of FFmpeg `repeat_pict` (in field durations).
    pub repeat_pict: u8,
    /// 'I', 'P', 'B' (BI reported as 'B').
    pub pict_type: char,
    pub keyframe: bool,
    pub coded_as_fields: bool,
}

impl Frame {
    #[cfg(test)]
    fn test_blank() -> Frame {
        use crate::core::Layout;
        Frame {
            pic: Rc::new(Pic::new(&Layout::new(1, 1))),
            mapped: None,
            width: 16,
            height: 16,
            pts: 0,
            interlaced: false,
            top_field_first: true,
            repeat_first_field: false,
            repeat_frames: 0,
            repeat_pict: 0,
            pict_type: 'P',
            keyframe: false,
            coded_as_fields: false,
        }
    }

    /// Display time in field durations, FFmpeg style: a frame is two fields plus `repeat_pict` (RFF adds one field, RPTFRM n adds 2n).
    pub fn display_fields(&self) -> u32 {
        2 + self.repeat_pict as u32
    }

    /// Display duration in microseconds for a frame rate of `fps_num / fps_den`; 3:2 telecine frames with RFF last 1.5 frame times.
    pub fn display_duration_us(&self, fps_num: u32, fps_den: u32) -> i64 {
        if fps_num == 0 || fps_den == 0 {
            return 0;
        }
        // frame time in us times fields / 2, rounded once (not per field) so 2 + 3 field cadences add up exactly
        (1_000_000i64 * fps_den as i64 * self.display_fields() as i64 + fps_num as i64) / (2 * fps_num as i64)
    }

    /// (plane data, stride in bytes) for 0 = Y, 1 = U, 2 = V. Data starts at the top-left pixel.
    pub fn plane(&self, i: usize) -> (&[u8], usize) {
        match &self.mapped {
            Some(m) => (&m[i], self.pic.stride[i]),
            None => (&self.pic.planes[i], self.pic.stride[i]),
        }
    }
    pub fn plane_width(&self, i: usize) -> usize {
        if i == 0 { self.width } else { (self.width + 1) >> 1 }
    }
    pub fn plane_height(&self, i: usize) -> usize {
        if i == 0 { self.height } else { (self.height + 1) >> 1 }
    }
    /// Append tightly packed planar I420 (Y, U, V) to `out`.
    pub fn write_i420(&self, out: &mut Vec<u8>) {
        for i in 0..3 {
            let (d, s) = self.plane(i);
            let (w, h) = (self.plane_width(i), self.plane_height(i));
            for y in 0..h {
                out.extend_from_slice(&d[y * s..y * s + w]);
            }
        }
    }
    pub fn i420_size(&self) -> usize {
        self.plane_width(0) * self.plane_height(0) + 2 * self.plane_width(1) * self.plane_height(1)
    }
}

struct SliceUnit {
    data: Vec<u8>,
    bit_off: usize,
    mby_start: usize,
}

pub struct Decoder {
    c: Core,
    out: VecDeque<Frame>,
    free: Vec<Box<Pic>>,
    poc: u64,
    pub frames_dropped: u64,
    pub errors: u64,
    /// (unit index, bit position) of the last macroblock layer error: unit 0 is the frame
    /// payload, unit n the n-th field/slice unit of the access unit.
    pub last_error: Option<(usize, usize)>,
    pub apply_range_map: bool,
    seq_seen: bool,
}

impl Default for Decoder {
    fn default() -> Self {
        Self::new()
    }
}

impl Decoder {
    pub fn new() -> Decoder {
        Decoder { c: Core::new(), out: VecDeque::new(), free: Vec::new(), poc: 0, frames_dropped: 0, errors: 0, last_error: None, apply_range_map: true, seq_seen: false }
    }

    pub fn info(&self) -> Option<StreamInfo> {
        let c = &self.c;
        if !c.have_seq || !c.have_ep {
            return None;
        }
        Some(StreamInfo {
            width: c.width,
            height: c.height,
            display_width: c.display_width,
            display_height: c.display_height,
            profile: c.profile,
            level: c.level,
            interlace: c.interlace,
            broadcast: c.broadcast,
            aspect_num: c.aspect_num,
            aspect_den: c.aspect_den,
            fps_num: c.fps_num,
            fps_den: c.fps_den,
            color_primaries: c.color_prim,
            transfer_characteristics: c.transfer_char,
            matrix_coefficients: c.matrix_coef,
            loop_filter: c.loop_filter,
            overlap: c.overlap,
            range_mapy: if c.range_mapy_flag { Some(c.range_mapy) } else { None },
            range_mapuv: if c.range_mapuv_flag { Some(c.range_mapuv) } else { None },
        })
    }

    /// Container codec private data (Matroska CodecPrivate / ASF extra data): sequence header
    /// and entry point units with start codes.
    pub fn set_extradata(&mut self, data: &[u8]) -> Result<(), Error> {
        let mut start = find_next_marker(data, 0);
        let mut seq = false;
        let mut ep = false;
        while start < data.len() {
            let next = find_next_marker(data, start + 4);
            let code = data[start + 3];
            if next > start + 4 {
                let buf = unescape(&data[start + 4..next]);
                let mut gb = BitReader::new(&buf);
                match code {
                    CODE_SEQHDR => {
                        self.c.parse_sequence_header(&mut gb)?;
                        seq = true;
                    }
                    CODE_ENTRYPOINT => {
                        self.c.parse_entry_point(&mut gb)?;
                        ep = true;
                    }
                    _ => {}
                }
            }
            start = next;
        }
        if !seq || !ep {
            return Err(Error::Invalid("incomplete extradata"));
        }
        self.seq_seen = true;
        Ok(())
    }

    /// Decode a raw Annex G byte stream chunk-wise: splits into access units and decodes each one.
    /// `pts_of(index)` supplies the timestamp of the n-th access unit in this call.
    pub fn decode_annexg<F: FnMut(usize) -> i64>(&mut self, data: &[u8], mut pts_of: F) -> usize {
        let aus = split_access_units(data);
        let mut n = 0;
        for (i, (a, b)) in aus.iter().enumerate() {
            if self.decode(&data[*a..*b], pts_of(i)).is_err() {
                self.errors += 1;
            }
            n += 1;
        }
        n
    }

    /// Drop all references and queued frames (seek).
    pub fn reset(&mut self) {
        self.c.last = None;
        self.c.next = None;
        self.c.cur = None;
        self.out.clear();
        self.c.first_pic_header_flag = true;
    }

    /// Output the delayed reference picture at end of stream.
    pub fn flush(&mut self) {
        if let Some(p) = self.c.next.take() {
            let f = self.make_frame(p);
            self.push_out(f);
        }
    }

    pub fn take_frame(&mut self) -> Option<Frame> {
        self.out.pop_front()
    }

    pub fn pending(&self) -> usize {
        self.out.len()
    }

    fn push_out(&mut self, f: Frame) {
        if self.out.len() >= MAX_OUTPUT_QUEUE {
            self.out.pop_front();
            self.frames_dropped += 1;
        }
        self.out.push_back(f);
    }

    fn make_frame(&self, pic: Rc<Pic>) -> Frame {
        let c = &self.c;
        let mut mapped = None;
        if self.apply_range_map && (c.range_mapy_flag || c.range_mapuv_flag) {
            let mut planes = [pic.planes[0].clone(), pic.planes[1].clone(), pic.planes[2].clone()];
            let apply = |buf: &mut Vec<u8>, m: u8| {
                let k = m as i32 + 9;
                for v in buf.iter_mut() {
                    let x = ((*v as i32 - 128) * k + 4) >> 3;
                    *v = (x + 128).clamp(0, 255) as u8;
                }
            };
            if c.range_mapy_flag {
                apply(&mut planes[0], c.range_mapy);
            }
            if c.range_mapuv_flag {
                apply(&mut planes[1], c.range_mapuv);
                apply(&mut planes[2], c.range_mapuv);
            }
            mapped = Some(Rc::new(planes));
        }
        let rpt = if pic.rff { 1 } else { pic.rptfrm * 2 };
        Frame {
            width: pic.coded_width,
            height: pic.coded_height,
            pts: pic.pts,
            interlaced: pic.interlaced,
            top_field_first: pic.tff,
            repeat_first_field: pic.rff,
            repeat_frames: pic.rptfrm,
            repeat_pict: rpt,
            pict_type: match pic.pict_type {
                PT_I => 'I',
                PT_P => 'P',
                _ => 'B',
            },
            keyframe: pic.key,
            coded_as_fields: pic.field_picture,
            mapped,
            pic,
        }
    }

    fn new_pic(&mut self) -> Box<Pic> {
        let lay = self.c.lay;
        while let Some(p) = self.free.pop() {
            // pictures allocated for a previous frame size must not be reused
            if p.planes[0].len() == lay.mb_width * 16 * lay.mbh_a * 16 && p.motion_val[0].len() == lay.lsize + 2 * PAD {
                return p;
            }
        }
        Box::new(Pic::new(&lay))
    }

    fn recycle(&mut self, rc: Option<Rc<Pic>>) {
        if let Some(rc) = rc {
            if let Ok(p) = Rc::try_unwrap(rc) {
                if self.free.len() < 4 {
                    self.free.push(Box::new(p));
                }
            }
        }
    }

    fn gray_pic(&mut self) -> Rc<Pic> {
        let mut p = self.new_pic();
        for pl in p.planes.iter_mut() {
            for v in pl.iter_mut() {
                *v = 128;
            }
        }
        for d in 0..2 {
            for v in p.motion_val[d].iter_mut() {
                *v = [0, 0];
            }
        }
        for v in p.mb_type.iter_mut() {
            *v = MBT_INTRA;
        }
        p.coded_width = self.c.width;
        p.coded_height = self.c.height;
        Rc::from(p)
    }

    /// Decode one access unit. Frames become available through `take_frame`.
    pub fn decode(&mut self, buf: &[u8], pts: i64) -> Result<(), Error> {
        if buf.is_empty() || (buf.len() == 4 && buf[0] == 0 && buf[1] == 0 && buf[2] == 1 && buf[3] == CODE_ENDOFSEQ) {
            return Ok(());
        }
        let is_marker = buf.len() >= 4 && buf[0] == 0 && buf[1] == 0 && buf[2] == 1;
        let mut frame_buf: Option<Vec<u8>> = None;
        let mut slices: Vec<SliceUnit> = Vec::new();
        let mut n_slices1: isize = -1;
        if is_marker {
            let end = buf.len();
            let mut start = 0usize;
            let mut next = 0usize;
            while next < end {
                next = find_next_marker(buf, start + 4);
                let size = next as isize - start as isize - 4;
                if size > 0 {
                    let body = &buf[start + 4..next];
                    match buf[start + 3] {
                        CODE_FRAME => frame_buf = Some(unescape(body)),
                        CODE_FIELD => {
                            slices.push(SliceUnit { data: unescape(body), bit_off: 0, mby_start: (self.c.height + 31) >> 5 });
                            n_slices1 = slices.len() as isize - 2;
                        }
                        CODE_ENTRYPOINT => {
                            let b = unescape(body);
                            let mut gb = BitReader::new(&b);
                            self.c.parse_entry_point(&mut gb)?;
                        }
                        CODE_SEQHDR => {
                            let b = unescape(body);
                            let mut gb = BitReader::new(&b);
                            self.c.parse_sequence_header(&mut gb)?;
                            self.seq_seen = true;
                        }
                        CODE_SLICE => {
                            let d = unescape(body);
                            let mut gb = BitReader::new(&d);
                            let mby = gb.get(9) as usize;
                            slices.push(SliceUnit { data: d, bit_off: 9, mby_start: mby });
                        }
                        _ => {}
                    }
                }
                start = next;
            }
        } else if self.c.interlace && (buf[0] & 0xC0) == 0xC0 {
            // WVC1 interlaced: both fields in one packet divided by a field marker
            let divider = find_next_marker(buf, 0);
            if divider >= buf.len() || buf[divider + 3] != CODE_FIELD {
                return inv("bad WVC1 interlaced frame");
            }
            let second = unescape(&buf[divider + 4..]);
            slices.push(SliceUnit { data: second, bit_off: 0, mby_start: (self.c.mb_height + 1) >> 1 });
            n_slices1 = -1;
            frame_buf = Some(unescape(&buf[..divider]));
        } else {
            frame_buf = Some(unescape(buf));
        }
        let frame_buf = match frame_buf {
            Some(f) => f,
            None => return Ok(()),
        };
        if !self.c.have_seq || !self.c.have_ep {
            return Err(Error::NoSequenceHeader);
        }
        self.decode_picture(frame_buf, slices, n_slices1, pts)
    }

    fn decode_picture(&mut self, frame_buf: Vec<u8>, slices: Vec<SliceUnit>, n_slices1: isize, pts: i64) -> Result<(), Error> {
        let n_slices = slices.len();
        self.last_error = None;
        {
            let c = &mut self.c;
            c.second_field = false;
            c.pic_header_flag = false;
            c.first_pic_header_flag = true;
        }
        let mut gb = BitReader::new(&frame_buf);
        if let Err(e) = self.c.parse_frame_header_adv(&mut gb) {
            self.last_error = Some((0, gb.pos()));
            return Err(e);
        }
        self.c.first_pic_header_flag = false;
        if (self.c.mb_height >> self.c.field_mode as usize) == 0 {
            return inv("image too short");
        }
        let pt = self.c.pict_type;
        if self.c.last.is_none() && pt == PT_B {
            // B picture without reference frames is skipped
            return Ok(());
        }

        // ---- ff_mpv_frame_start
        let mut cur = self.new_pic();
        cur.pts = pts;
        cur.coded_width = self.c.width;
        cur.coded_height = self.c.height;
        cur.pict_type = pt;
        cur.key = pt == PT_I;
        cur.field_picture = self.c.field_mode;
        cur.interlaced = self.c.fcm != PROGRESSIVE;
        cur.tff = self.c.tff;
        cur.rff = self.c.rff;
        cur.rptfrm = self.c.rptfrm;
        if pt != PT_B {
            let old_last = self.c.last.take();
            self.c.last = self.c.next.take();
            self.recycle(old_last);
        }
        if self.c.last.is_none() && pt != PT_I {
            let g = self.gray_pic();
            self.c.last = Some(g);
        }
        if pt == PT_B && self.c.next.is_none() {
            let g = self.gray_pic();
            self.c.next = Some(g);
        }
        self.c.last_interlaced = self.c.last.as_ref().map(|p| p.interlaced).unwrap_or(false);
        self.c.next_interlaced = if pt == PT_B { self.c.next.as_ref().map(|p| p.interlaced).unwrap_or(false) } else { cur.interlaced };
        self.c.cur = Some(cur);
        let field_mode = self.c.field_mode;
        {
            let c = &mut self.c;
            c.end_mb_x = c.mb_width;
            let st = c.cur.as_ref().unwrap().stride;
            c.linesize = st[0] << field_mode as usize;
            c.uvlinesize = st[1] << field_mode as usize;
        }
        let mb_height = self.c.mb_height >> field_mode as usize;

        // ---- slices
        let mut header_ret_ok = true;
        let mut cur_gb_slice: Option<usize> = None; // None: frame gb
        for i in 0..=n_slices {
            if i > 0 && slices[i - 1].mby_start >= mb_height {
                if !field_mode {
                    continue;
                }
                self.c.second_field = true;
                let bs = self.c.lay.b8_stride;
                self.c.blocks_off = bs * (self.c.mb_height & !1);
                self.c.mb_off = self.c.lay.mb_stride * self.c.mb_height >> 1;
            } else {
                self.c.second_field = false;
                self.c.blocks_off = 0;
                self.c.mb_off = 0;
            }
            // pick the reader for this iteration
            let mut tmp_gb;
            let gbr: &mut BitReader = if i == 0 {
                &mut gb
            } else {
                let s = &slices[i - 1];
                tmp_gb = BitReader::new(&s.data);
                tmp_gb.skip(s.bit_off);
                cur_gb_slice = Some(i - 1);
                &mut tmp_gb
            };
            let _ = cur_gb_slice;
            if i > 0 {
                self.c.pic_header_flag = false;
                if field_mode && i as isize == n_slices1 + 2 {
                    if self.c.parse_frame_header_adv(gbr).is_err() {
                        header_ret_ok = false;
                        self.errors += 1;
                        self.last_error = Some((i, gbr.pos()));
                        continue;
                    }
                    header_ret_ok = true;
                } else if gbr.get1() != 0 {
                    self.c.pic_header_flag = true;
                    if self.c.parse_frame_header_adv(gbr).is_err() {
                        header_ret_ok = false;
                        self.errors += 1;
                        continue;
                    }
                    header_ret_ok = true;
                }
            }
            if !header_ret_ok {
                continue;
            }
            self.c.start_mb_y = if i == 0 { 0 } else { slices[i - 1].mby_start % mb_height };
            if !field_mode || self.c.second_field {
                self.c.end_mb_y = if i == n_slices { mb_height } else { mb_height.min(slices[i].mby_start % mb_height) };
            } else {
                if i >= n_slices {
                    continue;
                }
                self.c.end_mb_y = if i as isize == n_slices1 + 1 { mb_height } else { mb_height.min(slices[i].mby_start % mb_height) };
            }
            if self.c.end_mb_y <= self.c.start_mb_y {
                continue;
            }
            if self.c.decode_blocks(gbr).is_err() {
                self.errors += 1;
                self.last_error = Some((i, gbr.pos()));
            }
        }
        if field_mode {
            self.c.second_field = false;
            if self.c.pict_type != PT_B {
                let c = &mut self.c;
                std::mem::swap(&mut c.mv_f_next[0], &mut c.mv_f[0]);
                std::mem::swap(&mut c.mv_f_next[1], &mut c.mv_f[1]);
            }
        }

        // ---- output / reference update
        let cur: Rc<Pic> = Rc::from(self.c.cur.take().unwrap());
        self.poc += 1;
        if pt == PT_B {
            let f = self.make_frame(cur);
            self.push_out(f);
        } else {
            let prev_anchor = self.c.last.clone();
            self.c.next = Some(cur);
            if let Some(p) = prev_anchor {
                let f = self.make_frame(p);
                self.push_out(f);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod duration_tests {
    use super::*;

    fn frame(rff: bool, rptfrm: u8) -> Frame {
        // same derivation as the decoder: FFmpeg repeat_pict = rff ? 1 : rptfrm * 2
        let mut f = Frame::test_blank();
        f.repeat_first_field = rff;
        f.repeat_frames = rptfrm;
        f.repeat_pict = if rff { 1 } else { rptfrm * 2 };
        f
    }

    #[test]
    fn repeat_pict_semantics() {
        assert_eq!(frame(false, 0).display_fields(), 2);
        assert_eq!(frame(true, 0).display_fields(), 3); // 1.5 frames
        assert_eq!(frame(false, 1).display_fields(), 4); // doubled frame
        assert_eq!(frame(false, 3).display_fields(), 8);
    }

    #[test]
    fn telecine_cadence_adds_up() {
        // 24000/1001 film with 2-3 pull-down at 30000/1001: RFF on every second frame, 5 fields per two frames
        let (n, d) = (24000, 1001);
        let total: i64 = (0..4).map(|i| frame(i % 2 == 1, 0).display_duration_us(n, d)).sum();
        // four film frames cover 10 fields = 5 frame times of 41708.33 us
        assert!((total - 5 * 1_001_000_000 / 24000).abs() <= 2, "{total}");
        assert_eq!(frame(false, 0).display_duration_us(n, d), 41708);
        assert_eq!(frame(true, 0).display_duration_us(n, d), 62563);
        assert_eq!(frame(false, 1).display_duration_us(n, d), 83417);
        assert_eq!(frame(false, 0).display_duration_us(0, 1), 0);
    }
}
