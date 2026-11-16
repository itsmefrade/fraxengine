//! Minimal JSON writer for the cue model (the wasm boundary hands JSON to the JS renderer).

use crate::model::{Cue, Rgba, TrackInfo};

fn esc(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

fn rgba(out: &mut String, c: Rgba) {
    out.push_str(&format!("[{},{},{},{}]", c.r, c.g, c.b, c.a));
}

fn num(out: &mut String, v: f32) {
    if v.fract() == 0.0 {
        out.push_str(&format!("{}", v as i64));
    } else {
        out.push_str(&format!("{v}"));
    }
}

pub fn track_info(t: &TrackInfo) -> String {
    format!(
        "{{\"playResX\":{},\"playResY\":{},\"wrapStyle\":{},\"scaledBorder\":{}}}",
        t.play_res_x, t.play_res_y, t.wrap_style, t.scaled_border
    )
}

pub fn cue(c: &Cue) -> String {
    let mut o = String::new();
    o.push_str(&format!(
        "{{\"s\":{},\"e\":{},\"layer\":{},\"an\":{},\"ml\":{},\"mr\":{},\"mv\":{},\"bs\":{},",
        c.start_us, c.end_us, c.layer, c.alignment, c.margin_l, c.margin_r, c.margin_v, c.border_style
    ));
    match c.pos {
        Some((x, y)) => {
            o.push_str("\"pos\":[");
            num(&mut o, x);
            o.push(',');
            num(&mut o, y);
            o.push_str("],");
        }
        None => o.push_str("\"pos\":null,"),
    }
    match c.fade {
        Some((a, b)) => o.push_str(&format!("\"fad\":[{a},{b}],")),
        None => o.push_str("\"fad\":null,"),
    }
    o.push_str("\"spans\":[");
    for (i, s) in c.spans.iter().enumerate() {
        if i > 0 {
            o.push(',');
        }
        o.push_str("{\"t\":");
        esc(&mut o, &s.text);
        o.push_str(",\"fn\":");
        esc(&mut o, &s.font);
        o.push_str(",\"fs\":");
        num(&mut o, s.size);
        o.push_str(&format!(
            ",\"b\":{},\"i\":{},\"u\":{},\"st\":{},\"c\":",
            s.bold as u8, s.italic as u8, s.underline as u8, s.strike as u8
        ));
        rgba(&mut o, s.primary);
        o.push_str(",\"oc\":");
        rgba(&mut o, s.outline);
        o.push_str(",\"bc\":");
        rgba(&mut o, s.back);
        o.push_str(",\"bord\":");
        num(&mut o, s.border);
        o.push_str(",\"shad\":");
        num(&mut o, s.shadow);
        o.push('}');
    }
    o.push_str("]}");
    o
}
