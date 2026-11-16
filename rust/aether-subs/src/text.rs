//! Text tracks: whole-file parsers (SRT, WebVTT, ASS/SSA) and Matroska packet ingestion, all
//! producing [`Cue`]s of the shared model.

use crate::ass::{self, Header};
use crate::markup;
use crate::model::{Cue, TrackInfo};
use std::collections::HashSet;
use std::hash::{Hash, Hasher};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextFormat {
    Srt,
    WebVtt,
    /// ASS and SSA (the header tells them apart).
    Ass,
}

/// Shown when a packet carries no duration.
pub const DEFAULT_CUE_US: i64 = 5_000_000;

pub struct TextTrack {
    pub format: TextFormat,
    header: Header,
    pub cues: Vec<Cue>,
    seen: HashSet<u64>,
}

fn decode(data: &[u8]) -> String {
    let s = String::from_utf8_lossy(data);
    s.trim_start_matches('\u{feff}').to_string()
}

impl TextTrack {
    /// `header` is the container's codec private: the ASS script header for ASS/SSA, ignored
    /// (may be empty) for SRT and WebVTT.
    pub fn new(format: TextFormat, header: &[u8]) -> Self {
        let header = match format {
            TextFormat::Ass => ass::parse_header(&decode(header)),
            _ => Header::default(),
        };
        TextTrack { format, header, cues: Vec::new(), seen: HashSet::new() }
    }

    pub fn info(&self) -> &TrackInfo {
        &self.header.info
    }

    /// Parses a whole file and keeps the cues sorted by start time.
    pub fn parse_file(format: TextFormat, data: &[u8]) -> Self {
        let text = decode(data);
        let mut t = match format {
            TextFormat::Ass => {
                let (header, cues) = ass::parse_file(&text);
                TextTrack { format, header, cues, seen: HashSet::new() }
            }
            TextFormat::Srt => {
                let mut t = TextTrack::new(format, &[]);
                t.cues = parse_srt(&text, &t.header);
                t
            }
            TextFormat::WebVtt => {
                let mut t = TextTrack::new(format, &[]);
                t.cues = parse_vtt(&text, &t.header);
                t
            }
        };
        t.cues.sort_by_key(|c| (c.start_us, c.layer));
        t
    }

    /// One container packet. Returns the new cue, or `None` for an empty or repeated packet
    /// (seeks re-deliver packets).
    pub fn push_packet(&mut self, pts_us: i64, duration_us: i64, data: &[u8]) -> Option<&Cue> {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        (pts_us, data).hash(&mut h);
        if !self.seen.insert(h.finish()) {
            return None;
        }
        let text = decode(data);
        let end = pts_us + if duration_us > 0 { duration_us } else { DEFAULT_CUE_US };
        let cue = match self.format {
            TextFormat::Ass => {
                // ReadOrder,Layer,Style,Name,MarginL,MarginR,MarginV,Effect,Text
                let f: Vec<&str> = text.trim_end_matches(['\r', '\n']).splitn(9, ',').collect();
                let (layer, style, ml, mr, mv, body) = if f.len() == 9 {
                    (f[1], f[2], f[4], f[5], f[6], f[8])
                } else if f.len() == 8 {
                    (f[0], f[1], f[3], f[4], f[5], f[7])
                } else {
                    return None;
                };
                let n = |s: &str| s.trim().parse::<i32>().unwrap_or(0);
                // SSA blocks have Marked here, which is not a layer.
                let layer = if self.header.ssa { 0 } else { n(layer) };
                ass::build_cue(&self.header, pts_us, end, layer, style, n(ml), n(mr), n(mv), body)
            }
            TextFormat::Srt | TextFormat::WebVtt => {
                let vtt = self.format == TextFormat::WebVtt;
                // Matroska WebVTT blocks are `settings \n identifier \n payload` (the first two lines may be empty).
                let (settings, payload) = if vtt { split_vtt_block(&text) } else { (Vec::new(), text.as_str()) };
                let body = payload.trim_matches(['\r', '\n']);
                if body.trim().is_empty() {
                    return None;
                }
                let st = self.header.style("Default").clone();
                let (prefix, ml, mr) = if vtt { vtt_settings(&settings, &self.header) } else { (String::new(), 0, 0) };
                let t = format!("{prefix}{}", markup::to_ass(body, st.size, vtt));
                ass::build_cue(&self.header, pts_us, end, 0, "Default", ml, mr, 0, &t)
            }
        };
        if cue.spans.is_empty() {
            return None;
        }
        self.cues.push(cue);
        self.cues.last()
    }

    /// Cues visible at `t_us`, lowest layer first (draw order).
    pub fn active_at(&self, t_us: i64) -> Vec<&Cue> {
        let mut v: Vec<&Cue> = self.cues.iter().filter(|c| c.start_us <= t_us && t_us < c.end_us).collect();
        v.sort_by_key(|c| c.layer);
        v
    }
}

/// Splits a Matroska WebVTT block into cue settings and payload. A block without the two header
/// lines is all payload.
fn split_vtt_block(text: &str) -> (Vec<&str>, &str) {
    let mut it = text.splitn(3, '\n');
    let (l0, l1, rest) = (it.next().unwrap_or(""), it.next(), it.next());
    let (Some(l1), Some(rest)) = (l1, rest) else { return (Vec::new(), text) };
    let settings_like = l0.trim().is_empty()
        || l0.split_whitespace().all(|t| matches!(t.split_once(':'), Some((k, _)) if matches!(k, "line" | "position" | "size" | "align" | "vertical" | "region")));
    if settings_like && !l1.contains("-->") {
        (l0.split_whitespace().collect(), rest)
    } else {
        (Vec::new(), text)
    }
}

fn parse_clock(s: &str) -> Option<i64> {
    // [hh:]mm:ss[,.]mmm
    let s = s.trim().replace(',', ".");
    let parts: Vec<&str> = s.split(':').collect();
    let (h, m, sec) = match parts.as_slice() {
        [m, s] => ("0", *m, *s),
        [h, m, s] => (*h, *m, *s),
        _ => return None,
    };
    ass::parse_time(&format!("{h}:{m}:{sec}"))
}

fn parse_srt(text: &str, header: &Header) -> Vec<Cue> {
    let mut cues = Vec::new();
    let lines: Vec<&str> = text.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        let Some((a, b)) = lines[i].split_once("-->") else {
            i += 1;
            continue;
        };
        i += 1;
        let (Some(start), Some(end)) = (parse_clock(a), parse_clock(b.split_whitespace().next().unwrap_or(""))) else { continue };
        let mut body: Vec<&str> = Vec::new();
        while i < lines.len() && !lines[i].trim().is_empty() {
            body.push(lines[i]);
            i += 1;
        }
        // A bare cue number glued to the next block (no blank line) is not text.
        if let Some(last) = body.last() {
            if body.len() > 1 && last.trim().chars().all(|c| c.is_ascii_digit()) && lines.get(i).is_some_and(|l| l.contains("-->")) {
                body.pop();
            }
        }
        let t = markup::to_ass(&body.join("\n"), header.style("Default").size, false);
        let cue = ass::build_cue(header, start, end, 0, "Default", 0, 0, 0, &t);
        if !cue.spans.is_empty() {
            cues.push(cue);
        }
    }
    cues
}

fn parse_vtt(text: &str, header: &Header) -> Vec<Cue> {
    let mut cues = Vec::new();
    let lines: Vec<&str> = text.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        // Skip to the next block start; NOTE / STYLE / REGION blocks run to a blank line.
        let l = lines[i].trim();
        if l.is_empty() {
            i += 1;
            continue;
        }
        let is_timing = l.contains("-->");
        let skip_block = !is_timing && (l.starts_with("NOTE") || l.starts_with("STYLE") || l.starts_with("REGION") || l.starts_with("WEBVTT"));
        if skip_block {
            while i < lines.len() && !lines[i].trim().is_empty() {
                i += 1;
            }
            continue;
        }
        if !is_timing {
            // Cue identifier line.
            if lines.get(i + 1).is_some_and(|n| n.contains("-->")) {
                i += 1;
                continue;
            }
            while i < lines.len() && !lines[i].trim().is_empty() {
                i += 1;
            }
            continue;
        }
        let (a, rest) = l.split_once("-->").unwrap();
        let mut rest_it = rest.split_whitespace();
        let end_s = rest_it.next().unwrap_or("");
        let settings: Vec<&str> = rest_it.collect();
        i += 1;
        let mut body: Vec<&str> = Vec::new();
        while i < lines.len() && !lines[i].trim().is_empty() {
            body.push(lines[i]);
            i += 1;
        }
        let (Some(start), Some(end)) = (parse_clock(a), parse_clock(end_s)) else { continue };
        let st = header.style("Default");
        let (prefix, ml, mr) = vtt_settings(&settings, header);
        let t = format!("{prefix}{}", markup::to_ass(&body.join("\n"), st.size, true));
        let cue = ass::build_cue(header, start, end, 0, "Default", ml, mr, 0, &t);
        if !cue.spans.is_empty() {
            cues.push(cue);
        }
    }
    cues
}

/// Maps cue settings to an alignment override and side margins. `line` picks the top, middle or
/// bottom row (percentages below 33 / above 66, line numbers 0..2 count as top), `align` the
/// column, `size` narrows the box symmetrically. `position` and regions are not honoured.
fn vtt_settings(settings: &[&str], header: &Header) -> (String, i32, i32) {
    let mut row = 2u8; // bottom
    let mut col = 1u8; // centre
    let mut size = 100.0f32;
    for s in settings {
        let Some((k, v)) = s.split_once(':') else { continue };
        match k {
            "line" => {
                let v = v.split(',').next().unwrap_or("");
                if let Some(p) = v.strip_suffix('%') {
                    if let Ok(p) = p.parse::<f32>() {
                        row = if p < 33.0 { 0 } else if p <= 66.0 { 1 } else { 2 };
                    }
                } else if let Ok(n) = v.parse::<i32>() {
                    row = if (0..3).contains(&n) { 0 } else { 2 };
                }
            }
            "align" => {
                col = match v {
                    "left" | "start" => 0,
                    "right" | "end" => 2,
                    _ => 1,
                }
            }
            "size" => size = v.trim_end_matches('%').parse().unwrap_or(100.0),
            _ => {}
        }
    }
    let an = match row {
        0 => 7,
        1 => 4,
        _ => 1,
    } + col;
    let prefix = if an == 2 { String::new() } else { format!("{{\\an{an}}}") };
    let side = if size < 100.0 { (header.info.play_res_x as f32 * (100.0 - size) / 200.0).round() as i32 } else { 0 };
    (prefix, side, side)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn srt_basic() {
        let t = TextTrack::parse_file(TextFormat::Srt, b"1\n00:00:01,000 --> 00:00:02,500\n<i>Hello</i>\nworld\n\n2\n00:00:03,000 --> 00:00:04,000\nBye\n");
        assert_eq!(t.cues.len(), 2);
        assert_eq!(t.cues[0].start_us, 1_000_000);
        assert_eq!(t.cues[0].end_us, 2_500_000);
        assert_eq!(t.cues[0].plain_text(), "Hello\nworld");
        assert!(t.cues[0].spans[0].italic);
    }

    #[test]
    fn vtt_basic() {
        let t = TextTrack::parse_file(
            TextFormat::WebVtt,
            b"WEBVTT\n\nNOTE x\n\nintro\n00:01.000 --> 00:02.000 line:10% align:left\nTop <b>left</b>\n\n00:00:03.000 --> 00:00:04.000\nBottom\n",
        );
        assert_eq!(t.cues.len(), 2);
        assert_eq!(t.cues[0].alignment, 7);
        assert_eq!(t.cues[1].alignment, 2);
        assert_eq!(t.cues[0].plain_text(), "Top left");
    }

    #[test]
    fn vtt_matroska_block() {
        let mut t = TextTrack::new(TextFormat::WebVtt, b"");
        let c = t.push_packet(0, 1_000_000, b"align:left line:0\nid1\n<b>Up</b>\nleft").unwrap().clone();
        assert_eq!((c.alignment, c.plain_text().as_str()), (7, "Up\nleft"));
        let c = t.push_packet(2_000_000, 1_000_000, b"\n\nplain").unwrap().clone();
        assert_eq!((c.alignment, c.plain_text().as_str()), (2, "plain"));
        let c = t.push_packet(4_000_000, 1_000_000, b"two\nlines").unwrap().clone();
        assert_eq!(c.plain_text(), "two\nlines");
    }

    #[test]
    fn ass_packet_dedupe() {
        let hdr = b"[Script Info]\nPlayResX: 640\nPlayResY: 360\n[V4+ Styles]\nFormat: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, BackColour, Bold, Italic, Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, Encoding\nStyle: Default,Arial,20,&H00FFFFFF,&H000000FF,&H00000000,&H00000000,-1,0,0,0,100,100,0,0,1,2,1,2,10,10,20,1\n";
        let mut t = TextTrack::new(TextFormat::Ass, hdr);
        assert_eq!(t.info().play_res_x, 640);
        let pkt = b"0,0,Default,,0,0,0,,{\\an8}Hi, there";
        let c = t.push_packet(1_000_000, 2_000_000, pkt).unwrap().clone();
        assert_eq!(c.plain_text(), "Hi, there");
        assert_eq!(c.alignment, 8);
        assert!(c.spans[0].bold);
        assert_eq!(c.margin_v, 20);
        assert!(t.push_packet(1_000_000, 2_000_000, pkt).is_none());
        assert_eq!(t.active_at(2_000_000).len(), 1);
        assert_eq!(t.active_at(3_000_000).len(), 0);
    }
}
