//! ASS / SSA: script header, style table, Dialogue events and the override tag subset.
//!
//! Supported tags: `\b \i \u \s \fn \fs \c \1c \3c \4c \alpha \1a \3a \4a \bord \xbord \ybord \shad
//! \xshad \yshad \an \a \pos \move` (start point), `\fad`, `\fade` (the 255/0/255 form), `\r`,
//! `\p` (drawings are skipped), and `\N \n \h \{ \}`. Everything else (`\t`, `\clip`, `\k`, `\fscx`,
//! rotations, blur...) is parsed past and ignored.

use crate::model::{Cue, Rgba, Span, Style, TrackInfo};

#[derive(Clone, Debug)]
pub struct Header {
    pub info: TrackInfo,
    pub styles: Vec<Style>,
    /// `[V4 Styles]` script (SSA): different alignment numbering and Marked instead of Layer.
    pub ssa: bool,
    pub event_format: Vec<String>,
}

impl Default for Header {
    fn default() -> Self {
        Header {
            info: TrackInfo { play_res_x: 384, play_res_y: 288, wrap_style: 0, scaled_border: true },
            styles: vec![Style::default()],
            ssa: false,
            event_format: Vec::new(),
        }
    }
}

impl Header {
    pub fn style(&self, name: &str) -> &Style {
        let name = name.trim().trim_start_matches('*');
        self.styles
            .iter()
            .find(|s| s.name.eq_ignore_ascii_case(name))
            .or_else(|| self.styles.iter().find(|s| s.name.eq_ignore_ascii_case("Default")))
            .or_else(|| self.styles.first())
            .unwrap_or_else(|| default_style())
    }
}

fn default_style() -> &'static Style {
    use std::sync::OnceLock;
    static S: OnceLock<Style> = OnceLock::new();
    S.get_or_init(Style::default)
}

fn parse_num(s: &str) -> Option<f64> {
    s.trim().parse::<f64>().ok()
}

fn parse_int(s: &str) -> Option<i64> {
    let s = s.trim();
    s.parse::<i64>().ok().or_else(|| parse_num(s).map(|f| f as i64))
}

/// `&HAABBGGRR`, `&HBBGGRR&`, `$...` or a decimal word (SSA).
fn parse_color_word(s: &str) -> Option<u32> {
    let s = s.trim();
    let hex = s.strip_prefix("&H").or_else(|| s.strip_prefix("&h")).or_else(|| s.strip_prefix('$')).or_else(|| s.strip_prefix('H'));
    match hex {
        Some(h) => {
            let h = h.trim_end_matches('&');
            u32::from_str_radix(h, 16).ok()
        }
        None => s.parse::<i64>().ok().map(|v| v as u32),
    }
}

/// Parses `Script Info`, the styles section and the events `Format:` line.
pub fn parse_header(text: &str) -> Header {
    let mut h = Header { styles: Vec::new(), ..Header::default() };
    let mut section = String::new();
    let mut style_format: Vec<String> = Vec::new();
    let (mut rx, mut ry) = (0u32, 0u32);
    let mut type_v4plus = false;
    for raw in text.lines() {
        let line = raw.trim_start_matches('\u{feff}').trim();
        if line.is_empty() || line.starts_with(';') {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            section = line.to_ascii_lowercase();
            if section == "[v4 styles]" {
                h.ssa = true;
            } else if section == "[v4+ styles]" {
                h.ssa = false;
                type_v4plus = true;
            }
            continue;
        }
        let Some((key, val)) = line.split_once(':') else { continue };
        let key = key.trim().to_ascii_lowercase();
        let val = val.trim();
        match section.as_str() {
            "[script info]" => match key.as_str() {
                "playresx" => rx = parse_int(val).unwrap_or(0).max(0) as u32,
                "playresy" => ry = parse_int(val).unwrap_or(0).max(0) as u32,
                "wrapstyle" => h.info.wrap_style = parse_int(val).unwrap_or(0).clamp(0, 3) as u8,
                "scaledborderandshadow" => h.info.scaled_border = val.eq_ignore_ascii_case("yes"),
                "scripttype" => {
                    if val.to_ascii_lowercase().contains("v4.00+") {
                        type_v4plus = true;
                    }
                }
                _ => {}
            },
            "[v4+ styles]" | "[v4 styles]" => {
                if key == "format" {
                    style_format = val.split(',').map(|s| s.trim().to_ascii_lowercase()).collect();
                } else if key == "style" {
                    h.styles.push(parse_style(&style_format, val, h.ssa && !type_v4plus));
                }
            }
            "[events]" => {
                if key == "format" {
                    h.event_format = val.split(',').map(|s| s.trim().to_ascii_lowercase()).collect();
                }
            }
            _ => {}
        }
    }
    // libass: a missing PlayRes falls back to 384x288, one missing value is derived at 4:3.
    match (rx, ry) {
        (0, 0) => (rx, ry) = (384, 288),
        (0, y) => rx = y * 4 / 3,
        (x, 0) => ry = x * 3 / 4,
        _ => {}
    }
    h.info.play_res_x = rx;
    h.info.play_res_y = ry;
    if h.styles.is_empty() {
        h.styles.push(Style::default());
    }
    h
}

fn parse_style(format: &[String], val: &str, ssa: bool) -> Style {
    let mut s = Style::default();
    // Style lines never contain commas inside a field.
    let fields: Vec<&str> = val.split(',').map(str::trim).collect();
    for (i, key) in format.iter().enumerate() {
        let Some(&v) = fields.get(i) else { break };
        match key.as_str() {
            "name" => s.name = v.trim_start_matches('*').to_string(),
            "fontname" => s.font = v.to_string(),
            "fontsize" => s.size = parse_num(v).unwrap_or(16.0) as f32,
            "primarycolour" | "primarycolor" => s.primary = parse_color_word(v).map(Rgba::from_ass).unwrap_or(Rgba::WHITE),
            "secondarycolour" | "secondarycolor" => s.secondary = parse_color_word(v).map(Rgba::from_ass).unwrap_or(Rgba::WHITE),
            "outlinecolour" | "outlinecolor" | "tertiarycolour" | "tertiarycolor" => {
                s.outline = parse_color_word(v).map(Rgba::from_ass).unwrap_or(Rgba::BLACK)
            }
            "backcolour" | "backcolor" => s.back = parse_color_word(v).map(Rgba::from_ass).unwrap_or(Rgba::BLACK),
            "bold" => s.bold = parse_int(v).unwrap_or(0) != 0,
            "italic" => s.italic = parse_int(v).unwrap_or(0) != 0,
            "underline" => s.underline = parse_int(v).unwrap_or(0) != 0,
            "strikeout" => s.strike = parse_int(v).unwrap_or(0) != 0,
            "borderstyle" => s.border_style = if parse_int(v).unwrap_or(1) == 3 { 3 } else { 1 },
            "outline" => s.outline_w = parse_num(v).unwrap_or(0.0).max(0.0) as f32,
            "shadow" => s.shadow = parse_num(v).unwrap_or(0.0).max(0.0) as f32,
            "alignment" => {
                let a = parse_int(v).unwrap_or(2);
                s.alignment = if ssa { ssa_alignment(a) } else { a.clamp(1, 9) as u8 };
            }
            "marginl" => s.margin_l = parse_int(v).unwrap_or(0) as i32,
            "marginr" => s.margin_r = parse_int(v).unwrap_or(0) as i32,
            "marginv" => s.margin_v = parse_int(v).unwrap_or(0) as i32,
            _ => {}
        }
    }
    s
}

/// SSA alignment (1-3 bottom, 5-7 top, 9-11 middle) to numpad.
fn ssa_alignment(a: i64) -> u8 {
    legacy_alignment(a)
}

/// Same table for the `\a` override.
fn legacy_alignment(a: i64) -> u8 {
    match a {
        1..=3 => a as u8,
        5..=7 => (a + 2) as u8,
        9..=11 => (a - 5) as u8,
        _ => 2,
    }
}

/// `h:mm:ss.cc` to microseconds.
pub fn parse_time(s: &str) -> Option<i64> {
    let mut it = s.trim().split(':');
    let h: i64 = it.next()?.trim().parse().ok()?;
    let m: i64 = it.next()?.trim().parse().ok()?;
    let sec = it.next()?.trim();
    let (whole, frac) = sec.split_once('.').unwrap_or((sec, "0"));
    let sw: i64 = whole.parse().ok()?;
    let mut f = frac.to_string();
    f.truncate(6);
    while f.len() < 6 {
        f.push('0');
    }
    let fu: i64 = f.parse().ok()?;
    Some(((h * 60 + m) * 60 + sw) * 1_000_000 + fu)
}

/// Everything a Dialogue line carries besides its text.
pub struct Event<'a> {
    pub start_us: i64,
    pub end_us: i64,
    pub layer: i32,
    pub style: &'a str,
    pub margin_l: i32,
    pub margin_r: i32,
    pub margin_v: i32,
    pub text: &'a str,
}

/// Parses a `Dialogue:` payload (after the colon) using the header's Format line.
pub fn parse_dialogue<'a>(h: &Header, payload: &'a str) -> Option<Event<'a>> {
    let default_fmt: Vec<String>;
    let fmt: &[String] = if h.event_format.is_empty() {
        default_fmt = if h.ssa {
            "marked,start,end,style,name,marginl,marginr,marginv,effect,text"
        } else {
            "layer,start,end,style,name,marginl,marginr,marginv,effect,text"
        }
        .split(',')
        .map(String::from)
        .collect();
        &default_fmt
    } else {
        &h.event_format
    };
    let parts: Vec<&str> = payload.splitn(fmt.len(), ',').collect();
    let mut ev = Event { start_us: 0, end_us: 0, layer: 0, style: "Default", margin_l: 0, margin_r: 0, margin_v: 0, text: "" };
    let mut got_time = (false, false);
    for (i, key) in fmt.iter().enumerate() {
        let Some(&v) = parts.get(i) else { break };
        match key.as_str() {
            "layer" => ev.layer = parse_int(v).unwrap_or(0) as i32,
            "start" => {
                ev.start_us = parse_time(v)?;
                got_time.0 = true;
            }
            "end" => {
                ev.end_us = parse_time(v)?;
                got_time.1 = true;
            }
            "style" => ev.style = v.trim(),
            "marginl" => ev.margin_l = parse_int(v).unwrap_or(0) as i32,
            "marginr" => ev.margin_r = parse_int(v).unwrap_or(0) as i32,
            "marginv" => ev.margin_v = parse_int(v).unwrap_or(0) as i32,
            "text" => ev.text = v,
            _ => {}
        }
    }
    if !(got_time.0 && got_time.1) {
        return None;
    }
    Some(ev)
}

/// Builds a cue from a style name, margins (0 = style default) and the raw ASS text.
#[allow(clippy::too_many_arguments)]
pub fn build_cue(h: &Header, start_us: i64, end_us: i64, layer: i32, style: &str, ml: i32, mr: i32, mv: i32, text: &str) -> Cue {
    let st = h.style(style);
    let mut o = Overrides::default();
    let spans = parse_text(h, st, text, &mut o);
    let nz = |v: i32, d: i32| if v != 0 { v } else { d };
    Cue {
        start_us,
        end_us,
        layer,
        alignment: o.alignment.unwrap_or(st.alignment),
        margin_l: nz(ml, st.margin_l),
        margin_r: nz(mr, st.margin_r),
        margin_v: nz(mv, st.margin_v),
        pos: o.pos,
        fade: o.fade,
        border_style: st.border_style,
        spans,
    }
}

#[derive(Default)]
struct Overrides {
    alignment: Option<u8>,
    pos: Option<(f32, f32)>,
    fade: Option<(i32, i32)>,
}

fn span_from(style: &Style) -> Span {
    Span {
        text: String::new(),
        font: style.font.clone(),
        size: style.size,
        bold: style.bold,
        italic: style.italic,
        underline: style.underline,
        strike: style.strike,
        primary: style.primary,
        outline: style.outline,
        back: style.back,
        border: style.outline_w,
        shadow: style.shadow,
    }
}

/// Splits `text` into styled spans, applying override blocks; fills the cue-level overrides.
fn parse_text(h: &Header, style: &Style, text: &str, o: &mut Overrides) -> Vec<Span> {
    let mut spans: Vec<Span> = Vec::new();
    let mut cur = span_from(style);
    let mut drawing = false;
    let mut buf = String::new();
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    let flush = |buf: &mut String, cur: &Span, spans: &mut Vec<Span>| {
        if !buf.is_empty() {
            let mut s = cur.clone();
            s.text = std::mem::take(buf);
            spans.push(s);
        }
    };
    while i < chars.len() {
        let c = chars[i];
        if c == '{' {
            // Override block or comment up to the closing brace.
            if let Some(rel) = chars[i + 1..].iter().position(|&c| c == '}') {
                let block: String = chars[i + 1..i + 1 + rel].iter().collect();
                flush(&mut buf, &cur, &mut spans);
                apply_block(h, style, &block, &mut cur, &mut drawing, o);
                i += rel + 2;
                continue;
            }
        }
        if c == '\\' {
            match chars.get(i + 1) {
                Some('N') => {
                    if !drawing {
                        buf.push('\n');
                    }
                    i += 2;
                    continue;
                }
                Some('n') => {
                    if !drawing {
                        buf.push(if h.info.wrap_style == 2 { '\n' } else { ' ' });
                    }
                    i += 2;
                    continue;
                }
                Some('h') => {
                    if !drawing {
                        buf.push('\u{a0}');
                    }
                    i += 2;
                    continue;
                }
                Some('{') | Some('}') => {
                    if !drawing {
                        buf.push(chars[i + 1]);
                    }
                    i += 2;
                    continue;
                }
                _ => {}
            }
        }
        if !drawing {
            buf.push(c);
        }
        i += 1;
    }
    flush(&mut buf, &cur, &mut spans);
    spans
}

fn apply_block(h: &Header, style: &Style, block: &str, cur: &mut Span, drawing: &mut bool, o: &mut Overrides) {
    let b = block.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] != b'\\' {
            i += 1;
            continue;
        }
        let start = i + 1;
        let mut depth = 0i32;
        let mut j = start;
        while j < b.len() {
            match b[j] {
                b'(' => depth += 1,
                b')' => depth -= 1,
                b'\\' if depth <= 0 => break,
                _ => {}
            }
            j += 1;
        }
        apply_tag(h, style, block[start..j].trim(), cur, drawing, o);
        i = j;
    }
}

const TAGS: &[&str] = &[
    "fscx", "fscy", "fsp", "fade", "fad", "fs", "fn", "alpha", "bord", "xbord", "ybord", "shad", "xshad", "yshad", "move", "pos", "clip",
    "iclip", "1c", "2c", "3c", "4c", "1a", "2a", "3a", "4a", "an", "be", "blur", "fr", "fax", "fay", "org", "rnd", "kf", "ko", "t", "c", "a",
    "b", "i", "u", "s", "r", "p", "q", "k", "K",
];

fn apply_tag(h: &Header, style: &Style, tag: &str, cur: &mut Span, drawing: &mut bool, o: &mut Overrides) {
    // Longest known name that prefixes the tag, so `fscx` is never read as `fs`+`cx`.
    let mut name = "";
    for t in TAGS {
        if tag.starts_with(t) && t.len() > name.len() {
            name = t;
        }
    }
    let arg = tag[name.len()..].trim();
    let flag = |a: &str, base: bool| match a.parse::<i64>() {
        Ok(0) => false,
        Ok(_) => true,
        Err(_) => base,
    };
    let color = |a: &str| parse_color_word(a).map(|v| Rgba::from_ass(v & 0x00ff_ffff));
    let set_rgb = |dst: &mut Rgba, src: Option<Rgba>, fallback: Rgba| {
        let c = src.unwrap_or(fallback);
        *dst = Rgba { r: c.r, g: c.g, b: c.b, a: dst.a };
    };
    let alpha_of = |a: &str, fallback: u8| parse_color_word(a).map(|v| 255 - (v & 0xff) as u8).unwrap_or(fallback);
    let num = |a: &str| parse_num(a);
    let args = |a: &str| -> Vec<f64> {
        a.trim().trim_start_matches('(').trim_end_matches(')').split(',').filter_map(|s| s.trim().parse::<f64>().ok()).collect()
    };
    match name {
        "b" => cur.bold = match arg.parse::<i64>() {
            Ok(w) if w > 1 => w >= 600,
            _ => flag(arg, style.bold),
        },
        "i" => cur.italic = flag(arg, style.italic),
        "u" => cur.underline = flag(arg, style.underline),
        "s" => cur.strike = flag(arg, style.strike),
        "fn" => cur.font = if arg.is_empty() { style.font.clone() } else { arg.to_string() },
        "fs" => {
            cur.size = match num(arg) {
                Some(v) if v > 0.0 => v as f32,
                _ => style.size,
            }
        }
        "c" | "1c" => set_rgb(&mut cur.primary, color(arg), style.primary),
        "3c" => set_rgb(&mut cur.outline, color(arg), style.outline),
        "4c" => set_rgb(&mut cur.back, color(arg), style.back),
        "alpha" => {
            cur.primary.a = alpha_of(arg, style.primary.a);
            cur.outline.a = alpha_of(arg, style.outline.a);
            cur.back.a = alpha_of(arg, style.back.a);
        }
        "1a" => cur.primary.a = alpha_of(arg, style.primary.a),
        "3a" => cur.outline.a = alpha_of(arg, style.outline.a),
        "4a" => cur.back.a = alpha_of(arg, style.back.a),
        "bord" | "xbord" | "ybord" => cur.border = num(arg).map(|v| v.max(0.0) as f32).unwrap_or(style.outline_w),
        "shad" | "xshad" | "yshad" => cur.shadow = num(arg).map(|v| v.abs() as f32).unwrap_or(style.shadow),
        "an" => {
            if o.alignment.is_none() {
                if let Ok(a) = arg.parse::<i64>() {
                    if (1..=9).contains(&a) {
                        o.alignment = Some(a as u8);
                    }
                }
            }
        }
        "a" => {
            if o.alignment.is_none() {
                if let Ok(a) = arg.parse::<i64>() {
                    o.alignment = Some(legacy_alignment(a));
                }
            }
        }
        "pos" | "move" => {
            let v = args(arg);
            if o.pos.is_none() && v.len() >= 2 {
                o.pos = Some((v[0] as f32, v[1] as f32));
            }
        }
        "fad" => {
            let v = args(arg);
            if v.len() == 2 {
                o.fade = Some((v[0].max(0.0) as i32, v[1].max(0.0) as i32));
            }
        }
        "fade" => {
            let v = args(arg);
            if v.len() == 7 && v[0] == 255.0 && v[1] == 0.0 && v[2] == 255.0 {
                o.fade = Some(((v[4] - v[3]).max(0.0) as i32, (v[6] - v[5]).max(0.0) as i32));
            }
        }
        "r" => {
            let st = if arg.is_empty() { style } else { h.style(arg) };
            let text = std::mem::take(&mut cur.text);
            *cur = span_from(st);
            cur.text = text;
        }
        "p" => *drawing = arg.parse::<i64>().map(|v| v > 0).unwrap_or(false),
        _ => {}
    }
}

/// A whole `.ass` / `.ssa` file: `(header, cues)`.
pub fn parse_file(text: &str) -> (Header, Vec<Cue>) {
    let h = parse_header(text);
    let mut cues = Vec::new();
    let mut in_events = false;
    for raw in text.lines() {
        let line = raw.trim_start_matches('\u{feff}').trim_end_matches('\r');
        let t = line.trim_start();
        if t.starts_with('[') {
            in_events = t.eq_ignore_ascii_case("[events]");
            continue;
        }
        if !in_events {
            continue;
        }
        let Some(rest) = t.strip_prefix("Dialogue:").or_else(|| t.strip_prefix("dialogue:")) else { continue };
        if let Some(ev) = parse_dialogue(&h, rest.trim_start()) {
            cues.push(build_cue(&h, ev.start_us, ev.end_us, ev.layer, ev.style, ev.margin_l, ev.margin_r, ev.margin_v, ev.text));
        }
    }
    (h, cues)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tags_and_breaks() {
        let h = Header::default();
        let c = build_cue(&h, 0, 1, 0, "Default", 0, 0, 0, r"{\an8\b1\c&H0000FF&\fs30}Hi{\r} there\Nline2\h!");
        assert_eq!(c.alignment, 8);
        assert_eq!(c.spans[0].text, "Hi");
        assert!(c.spans[0].bold);
        assert_eq!(c.spans[0].primary, Rgba { r: 255, g: 0, b: 0, a: 255 });
        assert_eq!(c.spans[0].size, 30.0);
        assert!(!c.spans[1].bold);
        assert_eq!(c.plain_text(), "Hi there\nline2\u{a0}!");
    }

    #[test]
    fn nested_parens_and_drawing() {
        let h = Header::default();
        let c = build_cue(&h, 0, 1, 0, "Default", 0, 0, 0, r"{\pos(10,20)\t(0,100,\fs50\c&H00FF00&)\fad(200,300)}A{\p1}m 0 0 l 5 5{\p0}B");
        assert_eq!(c.pos, Some((10.0, 20.0)));
        assert_eq!(c.fade, Some((200, 300)));
        assert_eq!(c.plain_text(), "AB");
        assert_eq!(c.spans[0].size, 16.0);
    }

    #[test]
    fn time_parse() {
        assert_eq!(parse_time("0:01:02.50"), Some(62_500_000));
    }
}
