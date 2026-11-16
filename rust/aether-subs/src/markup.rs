//! SRT and WebVTT cue text (HTML-like markup) rewritten as ASS dialogue text, the way FFmpeg's
//! text decoders do, so both formats flow through the single ASS override parser and renderer.

fn named_color(name: &str) -> Option<u32> {
    // CSS basic colours plus the WebVTT default cue classes. Returned as 0xRRGGBB.
    Some(match name.to_ascii_lowercase().as_str() {
        "white" => 0xffffff,
        "black" => 0x000000,
        "red" => 0xff0000,
        "lime" => 0x00ff00,
        "green" => 0x008000,
        "blue" => 0x0000ff,
        "yellow" => 0xffff00,
        "cyan" | "aqua" => 0x00ffff,
        "magenta" | "fuchsia" => 0xff00ff,
        "gray" | "grey" => 0x808080,
        "silver" => 0xc0c0c0,
        "orange" => 0xffa500,
        "maroon" => 0x800000,
        "navy" => 0x000080,
        "olive" => 0x808000,
        "purple" => 0x800080,
        "teal" => 0x008080,
        _ => return None,
    })
}

fn parse_css_color(v: &str) -> Option<u32> {
    let v = v.trim();
    if let Some(h) = v.strip_prefix('#') {
        return match h.len() {
            6 => u32::from_str_radix(h, 16).ok(),
            3 => {
                let n = u32::from_str_radix(h, 16).ok()?;
                let (r, g, b) = ((n >> 8) & 15, (n >> 4) & 15, n & 15);
                Some((r * 17) << 16 | (g * 17) << 8 | b * 17)
            }
            _ => None,
        };
    }
    named_color(v)
}

fn ass_color(rgb: u32) -> String {
    let (r, g, b) = ((rgb >> 16) & 255, (rgb >> 8) & 255, rgb & 255);
    format!("{{\\c&H{b:02X}{g:02X}{r:02X}&}}")
}

fn decode_entity(name: &str) -> Option<char> {
    Some(match name {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        "nbsp" => '\u{a0}',
        "lrm" => '\u{200e}',
        "rlm" => '\u{200f}',
        _ => {
            let n = name.strip_prefix('#')?;
            let cp = match n.strip_prefix(['x', 'X']) {
                Some(h) => u32::from_str_radix(h, 16).ok()?,
                None => n.parse().ok()?,
            };
            char::from_u32(cp)?
        }
    })
}

#[derive(Default, Clone)]
struct FontAttrs {
    color: Option<u32>,
    face: Option<String>,
    size: Option<f32>,
}

fn attr<'a>(attrs: &'a str, name: &str) -> Option<&'a str> {
    let lower = attrs.to_ascii_lowercase();
    let mut from = 0;
    while let Some(p) = lower[from..].find(name) {
        let at = from + p;
        let before_ok = at == 0 || !lower.as_bytes()[at - 1].is_ascii_alphanumeric();
        let rest = &attrs[at + name.len()..];
        let rest_trim = rest.trim_start();
        if before_ok && rest_trim.starts_with('=') {
            let v = rest_trim[1..].trim_start();
            return Some(match v.chars().next() {
                Some(q @ ('"' | '\'')) => v[1..].split(q).next().unwrap_or(""),
                _ => v.split(|c: char| c.is_whitespace() || c == '>').next().unwrap_or(""),
            });
        }
        from = at + name.len();
    }
    None
}

/// Converts markup to ASS text. `base_size` is the style font size in PlayRes units (HTML font
/// sizes 1..7 are scaled from it), `vtt` enables the WebVTT tags (`c`, `v`, `lang`, timestamps).
pub fn to_ass(text: &str, base_size: f32, vtt: bool) -> String {
    let mut out = String::with_capacity(text.len() + 16);
    let mut fonts: Vec<FontAttrs> = Vec::new();
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    let emit_font = |out: &mut String, new: &FontAttrs, old: &FontAttrs| {
        match new.color {
            Some(c) => out.push_str(&ass_color(c)),
            None if old.color.is_some() => out.push_str("{\\c}"),
            None => {}
        }
        match &new.face {
            Some(f) => out.push_str(&format!("{{\\fn{f}}}")),
            None if old.face.is_some() => out.push_str("{\\fn}"),
            None => {}
        }
        match new.size {
            Some(s) => out.push_str(&format!("{{\\fs{s}}}")),
            None if old.size.is_some() => out.push_str("{\\fs}"),
            None => {}
        }
    };
    while i < chars.len() {
        let c = chars[i];
        match c {
            '<' => {
                let Some(rel) = chars[i + 1..].iter().position(|&c| c == '>') else {
                    out.push('<');
                    i += 1;
                    continue;
                };
                let tag: String = chars[i + 1..i + 1 + rel].iter().collect();
                i += rel + 2;
                let closing = tag.starts_with('/');
                let body = tag.trim_start_matches('/').trim();
                let name_end = body.find(|c: char| c.is_whitespace() || c == '.').unwrap_or(body.len());
                let name = body[..name_end].to_ascii_lowercase();
                let rest = &body[name_end..];
                match (name.as_str(), closing) {
                    ("b", c) => out.push_str(if c { "{\\b0}" } else { "{\\b1}" }),
                    ("i", c) => out.push_str(if c { "{\\i0}" } else { "{\\i1}" }),
                    ("u", c) => out.push_str(if c { "{\\u0}" } else { "{\\u1}" }),
                    ("s", c) | ("strike", c) => out.push_str(if c { "{\\s0}" } else { "{\\s1}" }),
                    ("font", false) => {
                        let old = fonts.last().cloned().unwrap_or_default();
                        let mut new = old.clone();
                        if let Some(c) = attr(rest, "color").and_then(parse_css_color) {
                            new.color = Some(c);
                        }
                        if let Some(f) = attr(rest, "face") {
                            if !f.is_empty() {
                                new.face = Some(f.to_string());
                            }
                        }
                        if let Some(s) = attr(rest, "size").and_then(|s| s.trim().parse::<f32>().ok()) {
                            new.size = Some(if s <= 7.0 { base_size * [0.63, 0.63, 0.82, 1.0, 1.13, 1.5, 2.0, 3.0][s.max(0.0) as usize] } else { s });
                        }
                        emit_font(&mut out, &new, &old);
                        fonts.push(new);
                    }
                    ("c", false) if vtt => {
                        // `<c.red>`: first class that names a colour.
                        let old = fonts.last().cloned().unwrap_or_default();
                        let mut new = old.clone();
                        for cls in body[name_end..].split('.').filter(|s| !s.is_empty()) {
                            if let Some(c) = named_color(cls.split_whitespace().next().unwrap_or("")) {
                                new.color = Some(c);
                                break;
                            }
                        }
                        emit_font(&mut out, &new, &old);
                        fonts.push(new);
                    }
                    ("font", true) | ("c", true) => {
                        let old = fonts.pop().unwrap_or_default();
                        let now = fonts.last().cloned().unwrap_or_default();
                        emit_font(&mut out, &now, &old);
                    }
                    _ => {} // v, lang, ruby, rt, timestamps and anything unknown: drop the tag, keep the text
                }
            }
            '&' => {
                if let Some(rel) = chars[i + 1..].iter().take(10).position(|&c| c == ';') {
                    let name: String = chars[i + 1..i + 1 + rel].iter().collect();
                    if let Some(ch) = decode_entity(&name) {
                        out.push(ch);
                        i += rel + 2;
                        continue;
                    }
                }
                out.push('&');
                i += 1;
            }
            '{' => {
                // ASS overrides typed into an SRT file are honoured (FFmpeg does too); other braces are literal.
                if chars.get(i + 1) == Some(&'\\') {
                    if let Some(rel) = chars[i + 1..].iter().position(|&c| c == '}') {
                        out.extend(&chars[i..i + rel + 2]);
                        i += rel + 2;
                        continue;
                    }
                }
                out.push_str("\\{");
                i += 1;
            }
            '\r' => i += 1,
            '\n' => {
                out.push_str("\\N");
                i += 1;
            }
            _ => {
                out.push(c);
                i += 1;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn srt_tags() {
        assert_eq!(to_ass("<b>Hi</b> <i>there</i>\nx", 16.0, false), "{\\b1}Hi{\\b0} {\\i1}there{\\i0}\\Nx");
        assert_eq!(to_ass("<font color=\"#ff0000\">R</font>", 16.0, false), "{\\c&H0000FF&}R{\\c}");
        assert_eq!(to_ass("a &amp; b {\\an8}t { x", 16.0, false), "a & b {\\an8}t \\{ x");
    }

    #[test]
    fn vtt_tags() {
        assert_eq!(to_ass("<v Bob>hi</v> <c.yellow>y</c> <00:00:01.000>z", 16.0, true), "hi {\\c&H00FFFF&}y{\\c} z");
    }
}
