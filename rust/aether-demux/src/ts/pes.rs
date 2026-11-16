//! Transport packet, PSI section and PES header parsing.

/// One transport packet after the (optional) 4 byte BDAV prefix.
pub(super) struct Unit<'a> {
    pub pid: u16,
    pub pusi: bool,
    pub discontinuity: bool,
    pub payload: &'a [u8],
}

pub(super) fn parse_unit(raw: &[u8]) -> Option<Unit<'_>> {
    if raw.len() < 188 || raw[0] != 0x47 {
        return None;
    }
    let pid = (u16::from(raw[1] & 0x1f) << 8) | u16::from(raw[2]);
    let afc = (raw[3] >> 4) & 3;
    let mut at = 4usize;
    let mut discontinuity = false;
    if afc & 2 != 0 {
        let len = usize::from(raw[4]);
        if len > 183 {
            return None;
        }
        discontinuity = len > 0 && raw[5] & 0x80 != 0;
        at = 5 + len;
    }
    let payload = if afc & 1 != 0 { &raw[at.min(188)..188] } else { &[][..] };
    Some(Unit { pid, pusi: raw[1] & 0x40 != 0, discontinuity, payload })
}

/// Finds the stride (188, 192 or 204) and the offset of the first sync byte.
/// Returns (stride, first unit start, sync offset inside the unit).
pub(crate) fn detect(head: &[u8]) -> Option<(usize, usize, usize)> {
    let limit = head.len().min(2048);
    for first in 0..limit {
        if head[first] != 0x47 {
            continue;
        }
        for &stride in &[188usize, 192, 204] {
            let mut n = 1;
            while n < 6 {
                match head.get(first + n * stride) {
                    Some(0x47) => n += 1,
                    Some(_) => break,
                    None => break,
                }
            }
            // Short inputs may hold fewer than six units; two are enough then.
            let enough = n >= 6 || (head.len() < first + 6 * stride && n >= 3 && first + n * stride >= head.len());
            if enough {
                // A 192 byte layout has the sync at offset 4 of each unit.
                let (unit_start, sync_at) = if stride == 192 { (first.saturating_sub(4), first.min(4)) } else { (first, 0) };
                if stride == 192 && first < 4 {
                    // sync byte inside the first 4 bytes: keep the unit aligned to the prefix only if the
                    // prefix layout would also fit; otherwise this is a 188 stream that happens to repeat.
                    continue;
                }
                return Some((stride, unit_start, sync_at));
            }
        }
    }
    None
}

pub(super) fn parse_ts(b: &[u8]) -> Option<i64> {
    if b.len() < 5 {
        return None;
    }
    Some((i64::from(b[0] >> 1 & 7) << 30) | (i64::from(b[1]) << 22) | (i64::from(b[2] >> 1) << 15) | (i64::from(b[3]) << 7) | i64::from(b[4] >> 1))
}

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct PesHeader {
    /// PES_packet_length (0 = unbounded).
    pub packet_len: usize,
    pub pts: Option<i64>,
    pub dts: Option<i64>,
    pub ext_id: Option<u8>,
    /// Bytes of PES header including the 6 byte start.
    pub header_len: usize,
}

/// Parses a PES header from the first bytes of a PES. `None` when more bytes are needed or it is invalid.
pub(super) fn parse_pes_header(b: &[u8]) -> Result<PesHeader, bool> {
    // Err(true): need more bytes. Err(false): not a PES.
    if b.len() < 6 {
        return Err(true);
    }
    if b[..3] != [0, 0, 1] {
        return Err(false);
    }
    let stream_id = b[3];
    let packet_len = (usize::from(b[4]) << 8) | usize::from(b[5]);
    let no_header = matches!(stream_id, 0xbc | 0xbe | 0xbf | 0xf0 | 0xf1 | 0xff | 0xf2 | 0xf8);
    if no_header {
        return Ok(PesHeader { packet_len, header_len: 6, ..Default::default() });
    }
    if b.len() < 9 {
        return Err(true);
    }
    let flags = b[7];
    let hdr_data = usize::from(b[8]);
    let total = 9 + hdr_data;
    if b.len() < total {
        return Err(true);
    }
    let mut p = 9usize;
    let (mut pts, mut dts, mut ext_id) = (None, None, None);
    if flags & 0x80 != 0 {
        pts = parse_ts(b.get(p..p + 5).ok_or(false)?);
        p += 5;
        if flags & 0x40 != 0 {
            dts = parse_ts(b.get(p..p + 5).ok_or(false)?);
            p += 5;
        } else {
            dts = pts;
        }
    }
    if flags & 0x20 != 0 {
        p += 6; // ESCR
    }
    if flags & 0x10 != 0 {
        p += 3; // ES rate
    }
    if flags & 0x08 != 0 {
        p += 1; // DSM trick mode
    }
    if flags & 0x04 != 0 {
        p += 1; // additional copy info
    }
    if flags & 0x02 != 0 {
        p += 2; // CRC
    }
    if flags & 0x01 != 0 && p < total {
        let ef = b[p];
        p += 1;
        if ef & 0x80 != 0 {
            p += 16;
        }
        if ef & 0x40 != 0 {
            p += 1 + usize::from(*b.get(p).unwrap_or(&0));
        }
        if ef & 0x20 != 0 {
            p += 2;
        }
        if ef & 0x10 != 0 {
            p += 2;
        }
        if ef & 0x01 != 0 && p + 1 < total {
            // PES_extension_flag_2: marker + length, then stream_id_extension_flag + extension id.
            let v = b[p + 1];
            if v & 0x80 == 0 {
                ext_id = Some(v & 0x7f);
            }
        }
    }
    Ok(PesHeader { packet_len, pts, dts, ext_id, header_len: total })
}

/// Events from a [`PesParser`]. Payload bytes borrow from the unit.
pub(super) enum PesEvent<'a> {
    Start(PesHeader),
    Data(&'a [u8]),
    End,
}

/// Reassembles PES packets of one PID.
#[derive(Default)]
pub(super) struct PesParser {
    head: Vec<u8>,
    /// Header parsed, payload bytes still expected (None = until next PUSI).
    active: bool,
    remaining: Option<usize>,
    started: bool,
}

impl PesParser {
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    pub fn flush(&mut self, f: &mut impl FnMut(PesEvent<'_>)) {
        if self.active {
            f(PesEvent::End);
        }
        self.reset();
    }

    pub fn push(&mut self, pusi: bool, payload: &[u8], f: &mut impl FnMut(PesEvent<'_>)) {
        let mut payload = payload;
        if pusi {
            if self.active {
                f(PesEvent::End);
            }
            self.reset();
            self.started = true;
        } else if !self.started {
            return; // joined mid PES
        }
        if !self.active {
            self.head.extend_from_slice(payload);
            match parse_pes_header(&self.head) {
                Ok(h) => {
                    let consumed_before = self.head.len() - payload.len();
                    let body_start = h.header_len.saturating_sub(consumed_before).min(payload.len());
                    self.active = true;
                    self.remaining = if h.packet_len == 0 { None } else { Some(h.packet_len.saturating_sub(h.header_len - 6)) };
                    self.head = Vec::new();
                    f(PesEvent::Start(h));
                    payload = &payload[body_start..];
                }
                Err(true) if self.head.len() < 300 => return,
                Err(_) => {
                    self.reset();
                    return;
                }
            }
        }
        if let Some(rem) = self.remaining {
            let take = rem.min(payload.len());
            if take > 0 {
                f(PesEvent::Data(&payload[..take]));
            }
            self.remaining = Some(rem - take);
            if rem == take {
                f(PesEvent::End);
                self.reset();
            }
        } else if !payload.is_empty() {
            f(PesEvent::Data(payload));
        }
    }
}

/// PSI section assembler for one PID (PAT or PMT). Yields complete sections.
#[derive(Default)]
pub(super) struct SectionParser {
    buf: Vec<u8>,
}

impl SectionParser {
    pub fn push(&mut self, pusi: bool, payload: &[u8]) -> Option<Vec<u8>> {
        let mut p = payload;
        if pusi {
            let pointer = usize::from(*p.first()?);
            p = p.get(1 + pointer..)?;
            self.buf.clear();
        } else if self.buf.is_empty() {
            return None;
        }
        self.buf.extend_from_slice(p);
        if self.buf.len() >= 3 {
            let len = (usize::from(self.buf[1] & 0x0f) << 8) | usize::from(self.buf[2]);
            if self.buf.len() >= 3 + len {
                let out = self.buf[..3 + len].to_vec();
                self.buf.clear();
                return Some(out);
            }
        }
        None
    }
}

#[derive(Debug, Clone)]
pub(super) struct PmtStream {
    pub stream_type: u8,
    pub pid: u16,
    pub descriptors: Vec<(u8, Vec<u8>)>,
}

pub(super) fn parse_pat(section: &[u8]) -> Vec<(u16, u16)> {
    let mut out = Vec::new();
    if section.len() < 12 || section[0] != 0 {
        return out;
    }
    let end = section.len().saturating_sub(4);
    let mut p = 8;
    while p + 4 <= end {
        let program = (u16::from(section[p]) << 8) | u16::from(section[p + 1]);
        let pid = (u16::from(section[p + 2] & 0x1f) << 8) | u16::from(section[p + 3]);
        if program != 0 {
            out.push((program, pid));
        }
        p += 4;
    }
    out
}

pub(super) fn parse_pmt(section: &[u8]) -> Option<(u16, Vec<PmtStream>)> {
    if section.len() < 16 || section[0] != 2 {
        return None;
    }
    let program = (u16::from(section[3]) << 8) | u16::from(section[4]);
    let end = section.len().saturating_sub(4);
    let info_len = (usize::from(section[10] & 0x0f) << 8) | usize::from(section[11]);
    let mut p = 12 + info_len;
    let mut streams = Vec::new();
    while p + 5 <= end {
        let stream_type = section[p];
        let pid = (u16::from(section[p + 1] & 0x1f) << 8) | u16::from(section[p + 2]);
        let es_len = (usize::from(section[p + 3] & 0x0f) << 8) | usize::from(section[p + 4]);
        let d_end = (p + 5 + es_len).min(end);
        let mut d = p + 5;
        let mut descriptors = Vec::new();
        while d + 2 <= d_end {
            let tag = section[d];
            let len = usize::from(section[d + 1]);
            let body = section.get(d + 2..(d + 2 + len).min(d_end)).unwrap_or(&[]);
            descriptors.push((tag, body.to_vec()));
            d += 2 + len;
        }
        streams.push(PmtStream { stream_type, pid, descriptors });
        p = d_end;
    }
    Some((program, streams))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pts_roundtrip() {
        // 90000 * 600 = 54000000
        let v: i64 = 54_000_000;
        let b = [
            0x21 | ((v >> 29) & 0x0e) as u8,
            (v >> 22) as u8,
            ((v >> 14) & 0xfe) as u8 | 1,
            (v >> 7) as u8,
            ((v << 1) & 0xfe) as u8 | 1,
        ];
        assert_eq!(parse_ts(&b), Some(v));
    }

    #[test]
    fn detects_188_and_192() {
        let mut a = vec![0u8; 188 * 8];
        for i in 0..8 {
            a[i * 188] = 0x47;
        }
        assert_eq!(detect(&a), Some((188, 0, 0)));
        let mut b = vec![0u8; 192 * 8];
        for i in 0..8 {
            b[i * 192 + 4] = 0x47;
        }
        assert_eq!(detect(&b), Some((192, 0, 4)));
        assert_eq!(detect(&[0u8; 2000]), None);
    }
}
