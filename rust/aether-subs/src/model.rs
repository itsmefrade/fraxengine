//! The one styled-cue model every text format is converted into.
//!
//! Colours are straight RGBA with `a` = opacity (255 opaque), i.e. ASS alpha inverted. Lengths
//! (font size, outline, shadow, margins, positions) are in the track's PlayRes units; a renderer
//! scales them by `video height / play_res_y`.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rgba {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Rgba {
    pub const WHITE: Rgba = Rgba { r: 255, g: 255, b: 255, a: 255 };
    pub const BLACK: Rgba = Rgba { r: 0, g: 0, b: 0, a: 255 };

    /// ASS `&HAABBGGRR` word (alpha 00 = opaque).
    pub fn from_ass(v: u32) -> Rgba {
        Rgba { r: v as u8, g: (v >> 8) as u8, b: (v >> 16) as u8, a: 255 - (v >> 24) as u8 }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Style {
    pub name: String,
    pub font: String,
    pub size: f32,
    pub primary: Rgba,
    pub secondary: Rgba,
    pub outline: Rgba,
    pub back: Rgba,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub strike: bool,
    /// 1 outline + shadow, 3 opaque box.
    pub border_style: u8,
    pub outline_w: f32,
    pub shadow: f32,
    /// Numpad alignment 1..9.
    pub alignment: u8,
    pub margin_l: i32,
    pub margin_r: i32,
    pub margin_v: i32,
}

impl Default for Style {
    /// FFmpeg's default subtitle style (`ff_ass_subtitle_header_default`), used for SRT and WebVTT.
    fn default() -> Self {
        Style {
            name: "Default".into(),
            font: "Arial".into(),
            size: 16.0,
            primary: Rgba::WHITE,
            secondary: Rgba::WHITE,
            outline: Rgba::BLACK,
            back: Rgba::BLACK,
            bold: false,
            italic: false,
            underline: false,
            strike: false,
            border_style: 1,
            outline_w: 1.0,
            shadow: 0.0,
            alignment: 2,
            margin_l: 10,
            margin_r: 10,
            margin_v: 15,
        }
    }
}

/// A run of text with its resolved style (override tags already applied). `text` may hold
/// `\n` for hard line breaks and U+00A0 for hard spaces.
#[derive(Clone, Debug, PartialEq)]
pub struct Span {
    pub text: String,
    pub font: String,
    pub size: f32,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub strike: bool,
    pub primary: Rgba,
    pub outline: Rgba,
    pub back: Rgba,
    pub border: f32,
    pub shadow: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Cue {
    pub start_us: i64,
    pub end_us: i64,
    pub layer: i32,
    pub alignment: u8,
    pub margin_l: i32,
    pub margin_r: i32,
    pub margin_v: i32,
    /// `\pos` or the start point of `\move` (anchor of the alignment box).
    pub pos: Option<(f32, f32)>,
    /// `\fad(in, out)` in milliseconds.
    pub fade: Option<(i32, i32)>,
    pub border_style: u8,
    pub spans: Vec<Span>,
}

impl Cue {
    /// The text with no styling, lines joined by `\n`.
    pub fn plain_text(&self) -> String {
        self.spans.iter().map(|s| s.text.as_str()).collect()
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct TrackInfo {
    pub play_res_x: u32,
    pub play_res_y: u32,
    /// ASS WrapStyle 0..3 (2 = only `\N` breaks).
    pub wrap_style: u8,
    pub scaled_border: bool,
}
