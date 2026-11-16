//! HDMV Presentation Graphic Stream (PGS, `.sup`, Matroska `S_HDMV/PGS`) decoder.
//!
//! Follows FFmpeg's `pgssubdec.c`: segments PDS (0x14), ODS (0x15), PCS (0x16), WDS (0x17) and
//! END (0x80) are collected per display set, objects and palettes live for a whole epoch (an
//! epoch-start PCS clears them, so a palette-only update display set re-renders the cached
//! objects), the run-length coded objects are expanded to palette indexes and then to RGBA
//! (non-premultiplied, limited range YCbCr with BT.709 coefficients above 720x576 and BT.601
//! at or below, integer arithmetic identical to FFmpeg's).
//!
//! Two input shapes are accepted: Matroska blocks (segments back to back, no header, the pts
//! comes from the container, see [`PgsDecoder::push`]) and `.sup` files (every segment prefixed
//! with `PG`, a 90 kHz pts and dts, see [`parse_sup`]).
//!
//! Windows (WDS) are parsed but not used to clip, exactly like FFmpeg; the composition object
//! position and its optional crop rectangle define what is drawn.

use std::collections::HashMap;

const SEG_PALETTE: u8 = 0x14;
const SEG_OBJECT: u8 = 0x15;
const SEG_PRESENTATION: u8 = 0x16;
const SEG_WINDOW: u8 = 0x17;
const SEG_END: u8 = 0x80;

/// Largest object accepted (the format allows 4095 x 4095, FFmpeg refuses what does not fit).
const MAX_OBJECT_PIXELS: usize = 4096 * 4096;

/// One drawn object of a display set.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PgsBitmap {
    /// Position in video coordinates (`Display::video_width` x `video_height`).
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    /// The composition object carries the forced flag (forced narrative / foreign dialogue).
    pub forced: bool,
    /// `width * height * 4` bytes, straight (non-premultiplied) RGBA.
    pub rgba: Vec<u8>,
}

/// One display set: the screen content from `start_us` until the next display set.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Display {
    pub start_us: i64,
    /// Filled by [`finalize_ends`] or by the caller from the container duration.
    pub end_us: Option<i64>,
    pub video_width: u32,
    pub video_height: u32,
    pub composition_number: u16,
    /// 0x00 normal, 0x40 acquisition point, 0x80 epoch start.
    pub composition_state: u8,
    /// No objects: the previous content is removed at `start_us`.
    pub clear: bool,
    pub bitmaps: Vec<PgsBitmap>,
}

impl Display {
    pub fn forced(&self) -> bool {
        self.bitmaps.iter().any(|b| b.forced)
    }
}

/// Sets every missing `end_us` to the start of the next display set (the last one stays open).
pub fn finalize_ends(displays: &mut [Display]) {
    for i in 0..displays.len().saturating_sub(1) {
        if displays[i].end_us.is_none() {
            displays[i].end_us = Some(displays[i + 1].start_us);
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum PgsError {
    /// A segment header or payload is cut short.
    Truncated,
    /// `.sup` data without the `PG` magic.
    BadMagic,
}

impl std::fmt::Display for PgsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PgsError::Truncated => write!(f, "truncated PGS segment"),
            PgsError::BadMagic => write!(f, "missing PG sync bytes"),
        }
    }
}

impl std::error::Error for PgsError {}

#[derive(Clone)]
struct Object {
    width: usize,
    height: usize,
    /// Run-length data collected so far / expected total (the ODS length field minus the 4 size bytes).
    rle: Vec<u8>,
    expected: usize,
    /// Palette indexes, `width * height`, once the last fragment arrived and the RLE decoded.
    pixels: Option<Vec<u8>>,
}

#[derive(Clone)]
struct CompositionObject {
    object_id: u16,
    #[allow(dead_code)]
    window_id: u8,
    forced: bool,
    x: i32,
    y: i32,
    crop: Option<(u16, u16, u16, u16)>,
}

#[derive(Default)]
struct Presentation {
    video_width: u32,
    video_height: u32,
    composition_number: u16,
    state: u8,
    palette_id: u8,
    objects: Vec<CompositionObject>,
}

/// Stateful decoder; one instance per subtitle track.
pub struct PgsDecoder {
    palettes: HashMap<u8, Box<[[u8; 4]; 256]>>,
    objects: HashMap<u16, Object>,
    windows: HashMap<u8, (u16, u16, u16, u16)>,
    pres: Presentation,
    have_pres: bool,
    video: (u32, u32),
}

impl Default for PgsDecoder {
    fn default() -> Self {
        Self::new()
    }
}

fn be16(b: &[u8], o: usize) -> u16 {
    u16::from_be_bytes([b[o], b[o + 1]])
}

impl PgsDecoder {
    pub fn new() -> Self {
        Self {
            palettes: HashMap::new(),
            objects: HashMap::new(),
            windows: HashMap::new(),
            pres: Presentation::default(),
            have_pres: false,
            video: (0, 0),
        }
    }

    /// Forgets all cached objects and palettes (seek, track switch). The next display set that is
    /// not an epoch start may reference missing objects and then draws nothing for them.
    pub fn reset(&mut self) {
        *self = Self::new();
    }

    /// Feeds one Matroska block (or any run of headerless segments) displayed from `pts_us`.
    /// Returns the display sets completed by it (normally one).
    pub fn push(&mut self, pts_us: i64, data: &[u8]) -> Vec<Display> {
        let mut out = Vec::new();
        let mut pos = 0usize;
        while pos + 3 <= data.len() {
            let kind = data[pos];
            let len = be16(data, pos + 1) as usize;
            pos += 3;
            // A cut-off payload is decoded with what is there (FFmpeg does the same).
            let end = (pos + len).min(data.len());
            let body = &data[pos..end];
            pos = end;
            if let Some(d) = self.segment(pts_us, kind, body) {
                out.push(d);
            }
        }
        out
    }

    fn segment(&mut self, pts_us: i64, kind: u8, b: &[u8]) -> Option<Display> {
        match kind {
            SEG_PALETTE => self.palette(b),
            SEG_OBJECT => self.object(b),
            SEG_PRESENTATION => self.presentation(b),
            SEG_WINDOW => self.window(b),
            SEG_END => return self.end(pts_us),
            _ => {}
        }
        None
    }

    fn palette(&mut self, b: &[u8]) {
        if b.len() < 2 {
            return;
        }
        let id = b[0];
        // b[1] is the palette version; entries update in place so partial palettes work.
        let pal = self.palettes.entry(id).or_insert_with(|| Box::new([[0u8; 4]; 256]));
        let (vw, vh) = if self.have_pres { (self.pres.video_width, self.pres.video_height) } else { self.video };
        let ycc = vw > 720 || vh > 576;
        for e in b[2..].chunks_exact(5) {
            let (idx, y, cr, cb, a) = (e[0] as usize, e[1] as i32, e[2] as i32, e[3] as i32, e[4]);
            let (r, g, bl) = ycbcr_to_rgb(y, cb, cr, ycc);
            pal[idx] = [r, g, bl, a];
        }
    }

    fn window(&mut self, b: &[u8]) {
        if b.is_empty() {
            return;
        }
        let n = b[0] as usize;
        for i in 0..n {
            let o = 1 + i * 9;
            if o + 9 > b.len() {
                break;
            }
            self.windows.insert(b[o], (be16(b, o + 1), be16(b, o + 3), be16(b, o + 5), be16(b, o + 7)));
        }
    }

    fn object(&mut self, b: &[u8]) {
        if b.len() < 4 {
            return;
        }
        let id = be16(b, 0);
        let seq = b[3];
        let first = seq & 0x80 != 0;
        let last = seq & 0x40 != 0;
        let mut payload = &b[4..];
        if first {
            if payload.len() < 7 {
                return;
            }
            let total = ((payload[0] as usize) << 16) | ((payload[1] as usize) << 8) | payload[2] as usize;
            let w = be16(payload, 3) as usize;
            let h = be16(payload, 5) as usize;
            payload = &payload[7..];
            if w == 0 || h == 0 || w * h > MAX_OBJECT_PIXELS {
                self.objects.remove(&id);
                return;
            }
            self.objects.insert(id, Object { width: w, height: h, rle: Vec::new(), expected: total.saturating_sub(4), pixels: None });
        }
        let Some(obj) = self.objects.get_mut(&id) else { return };
        if obj.pixels.is_some() && !first {
            return;
        }
        let take = if obj.expected == 0 { payload.len() } else { payload.len().min(obj.expected.saturating_sub(obj.rle.len())) };
        obj.rle.extend_from_slice(&payload[..take]);
        if last {
            obj.pixels = Some(decode_rle(&obj.rle, obj.width, obj.height));
            obj.rle = Vec::new();
        }
    }

    fn presentation(&mut self, b: &[u8]) {
        if b.len() < 11 {
            return;
        }
        let state = b[7];
        let mut p = Presentation {
            video_width: be16(b, 0) as u32,
            video_height: be16(b, 2) as u32,
            composition_number: be16(b, 5),
            state,
            palette_id: b[9],
            objects: Vec::new(),
        };
        // b[4] frame rate, b[8] palette update flag: a palette-only update simply re-renders the cache.
        if state == 0x80 {
            self.objects.clear();
            self.palettes.clear();
            self.windows.clear();
        }
        let count = b[10] as usize;
        let mut o = 11;
        for _ in 0..count {
            if o + 8 > b.len() {
                break;
            }
            let flags = b[o + 3];
            let mut c = CompositionObject {
                object_id: be16(b, o),
                window_id: b[o + 2],
                forced: flags & 0x40 != 0,
                x: be16(b, o + 4) as i32,
                y: be16(b, o + 6) as i32,
                crop: None,
            };
            o += 8;
            if flags & 0x80 != 0 {
                if o + 8 > b.len() {
                    break;
                }
                c.crop = Some((be16(b, o), be16(b, o + 2), be16(b, o + 4), be16(b, o + 6)));
                o += 8;
            }
            p.objects.push(c);
        }
        self.pres = p;
        self.have_pres = true;
    }

    fn end(&mut self, pts_us: i64) -> Option<Display> {
        if !self.have_pres {
            return None;
        }
        self.have_pres = false;
        let p = std::mem::take(&mut self.pres);
        self.video = (p.video_width, p.video_height);
        let mut d = Display {
            start_us: pts_us,
            end_us: None,
            video_width: p.video_width,
            video_height: p.video_height,
            composition_number: p.composition_number,
            composition_state: p.state,
            clear: p.objects.is_empty(),
            bitmaps: Vec::new(),
        };
        let palette = self.palettes.get(&p.palette_id);
        for c in &p.objects {
            let (Some(obj), Some(pal)) = (self.objects.get(&c.object_id), palette) else { continue };
            let Some(px) = obj.pixels.as_ref() else { continue };
            let (cx, cy, cw, ch) = match c.crop {
                Some((x, y, w, h)) => {
                    let x = (x as usize).min(obj.width);
                    let y = (y as usize).min(obj.height);
                    (x, y, (w as usize).min(obj.width - x), (h as usize).min(obj.height - y))
                }
                None => (0, 0, obj.width, obj.height),
            };
            if cw == 0 || ch == 0 {
                continue;
            }
            let mut rgba = Vec::with_capacity(cw * ch * 4);
            for row in 0..ch {
                let line = &px[(cy + row) * obj.width + cx..][..cw];
                for &i in line {
                    rgba.extend_from_slice(&pal[i as usize]);
                }
            }
            d.bitmaps.push(PgsBitmap { x: c.x, y: c.y, width: cw as u32, height: ch as u32, forced: c.forced, rgba });
        }
        d.clear = d.bitmaps.is_empty() && p.objects.is_empty();
        Some(d)
    }
}

/// PGS run-length decoding. Lines end with the `00 00` marker; a missing or early marker leaves
/// the rest of the line index 0 (transparent in every sane palette).
fn decode_rle(data: &[u8], width: usize, height: usize) -> Vec<u8> {
    let mut out = vec![0u8; width * height];
    let (mut x, mut y, mut i) = (0usize, 0usize, 0usize);
    while i < data.len() && y < height {
        let c = data[i];
        i += 1;
        if c != 0 {
            if x < width {
                out[y * width + x] = c;
                x += 1;
            }
            continue;
        }
        let Some(&f) = data.get(i) else { break };
        i += 1;
        if f == 0 {
            // End of line.
            x = 0;
            y += 1;
            continue;
        }
        let mut run = (f & 0x3f) as usize;
        if f & 0x40 != 0 {
            let Some(&lo) = data.get(i) else { break };
            i += 1;
            run = (run << 8) | lo as usize;
        }
        let color = if f & 0x80 != 0 {
            let Some(&col) = data.get(i) else { break };
            i += 1;
            col
        } else {
            0
        };
        let n = run.min(width.saturating_sub(x));
        if color != 0 && n > 0 {
            out[y * width + x..y * width + x + n].fill(color);
        }
        x += n;
    }
    out
}

const SCALEBITS: i32 = 10;
const ONE_HALF: i32 = 1 << (SCALEBITS - 1);

fn fix(x: f64) -> i32 {
    (x * (1 << SCALEBITS) as f64 + 0.5) as i32
}

/// FFmpeg's `YUV_TO_RGB1_CCIR` / `_BT709` + `YUV_TO_RGB2_CCIR` (limited range in, full range out).
pub fn ycbcr_to_rgb(y1: i32, cb1: i32, cr1: i32, bt709: bool) -> (u8, u8, u8) {
    let cb = cb1 - 128;
    let cr = cr1 - 128;
    let (r_add, g_add, b_add) = if bt709 {
        (
            fix(1.5748 * 255.0 / 224.0) * cr + ONE_HALF,
            -fix(0.1873 * 255.0 / 224.0) * cb - fix(0.4681 * 255.0 / 224.0) * cr + ONE_HALF,
            fix(1.8556 * 255.0 / 224.0) * cb + ONE_HALF,
        )
    } else {
        (
            fix(1.40200 * 255.0 / 224.0) * cr + ONE_HALF,
            -fix(0.34414 * 255.0 / 224.0) * cb - fix(0.71414 * 255.0 / 224.0) * cr + ONE_HALF,
            fix(1.77200 * 255.0 / 224.0) * cb + ONE_HALF,
        )
    };
    let y = (y1 - 16) * fix(255.0 / 219.0);
    let clamp = |v: i32| (v >> SCALEBITS).clamp(0, 255) as u8;
    (clamp(y + r_add), clamp(y + g_add), clamp(y + b_add))
}

/// Parses a whole `.sup` file (or any `PG`-prefixed stream) into display sets with their ends
/// resolved.
pub fn parse_sup(data: &[u8]) -> Result<Vec<Display>, PgsError> {
    let mut dec = PgsDecoder::new();
    let mut out = Vec::new();
    let mut pos = 0usize;
    while pos < data.len() {
        if data.len() - pos < 13 {
            return if out.is_empty() { Err(PgsError::Truncated) } else { break };
        }
        if &data[pos..pos + 2] != b"PG" {
            return Err(PgsError::BadMagic);
        }
        let pts = u32::from_be_bytes([data[pos + 2], data[pos + 3], data[pos + 4], data[pos + 5]]) as i64;
        let kind = data[pos + 10];
        let len = be16(data, pos + 11) as usize;
        let end = (pos + 13 + len).min(data.len());
        let body = &data[pos + 13..end];
        pos = end;
        // 90 kHz ticks to microseconds, rounded.
        let us = (pts * 100 + 4) / 9;
        if let Some(d) = dec.segment(us, kind, body) {
            out.push(d);
        }
    }
    finalize_ends(&mut out);
    Ok(out)
}

/// Strips the `PG` headers of a `.sup` file into Matroska-style blocks: `(pts_us, segments)` per
/// display set (split after every END segment). Used by tests and tools.
pub fn sup_to_blocks(data: &[u8]) -> Result<Vec<(i64, Vec<u8>)>, PgsError> {
    let mut blocks = Vec::new();
    let mut cur: Vec<u8> = Vec::new();
    let mut cur_pts = 0i64;
    let mut pos = 0usize;
    while pos + 13 <= data.len() {
        if &data[pos..pos + 2] != b"PG" {
            return Err(PgsError::BadMagic);
        }
        let pts = u32::from_be_bytes([data[pos + 2], data[pos + 3], data[pos + 4], data[pos + 5]]) as i64;
        let kind = data[pos + 10];
        let len = be16(data, pos + 11) as usize;
        let end = (pos + 13 + len).min(data.len());
        if cur.is_empty() {
            cur_pts = (pts * 100 + 4) / 9;
        }
        cur.extend_from_slice(&data[pos + 10..end]);
        pos = end;
        if kind == SEG_END {
            blocks.push((cur_pts, std::mem::take(&mut cur)));
        }
    }
    Ok(blocks)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rle_runs_and_lines() {
        // Row 0: 2 x color 5 (short run with color), 1 x color 9 literal; row 1: 3 transparent then 7.
        let data = [0x00, 0x82, 5, 9, 0x00, 0x00, 0x00, 0x03, 7, 0x00, 0x00];
        let px = decode_rle(&data, 3, 2);
        assert_eq!(px, vec![5, 5, 9, 0, 0, 0]);
        // Long run: 0x40 flag, 14 bit length 300 > width is clipped.
        let data = [0x00, 0xc1, 0x2c, 6, 0x00, 0x00];
        assert_eq!(decode_rle(&data, 4, 1), vec![6, 6, 6, 6]);
    }

    #[test]
    fn colour_matches_ffmpeg_corners() {
        assert_eq!(ycbcr_to_rgb(16, 128, 128, true), (0, 0, 0));
        assert_eq!(ycbcr_to_rgb(235, 128, 128, true), (255, 255, 255));
        assert_eq!(ycbcr_to_rgb(235, 128, 128, false), (255, 255, 255));
    }
}
