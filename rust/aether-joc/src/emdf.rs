//! Minimal EMDF locator and payload extractor (ETSI TS 102 366 Annex H).
//!
//! The container may sit in a skip field or in the auxiliary data of a
//! syncframe, at any bit offset, so every bit position of the frame is tried
//! for the sync word 0x5838 and a candidate is only accepted if its payload
//! list and protection field fit exactly into the declared container length.

use crate::bits::BitReader;

pub const PAYLOAD_ID_OAMD: u32 = 11;
pub const PAYLOAD_ID_JOC: u32 = 14;

#[derive(Clone, Debug, Default)]
pub struct EmdfPayload {
    pub id: u32,
    pub data: Vec<u8>,
}

/// Payloads of one E-AC-3 access unit that matter for object decoding.
#[derive(Clone, Debug, Default)]
pub struct EmdfPayloads {
    pub joc: Option<Vec<u8>>,
    pub oamd: Option<Vec<u8>>,
    /// Every payload found (id, size in bytes), for diagnostics.
    pub all: Vec<(u32, usize)>,
}

/// Splits `au` into syncframes and returns the JOC and OAMD payloads found in
/// the last syncframe that carries any (the spec puts the container into the
/// last dependent substream).
pub fn extract_from_access_unit(au: &[u8]) -> EmdfPayloads {
    let mut out = EmdfPayloads::default();
    let mut pos = 0usize;
    while pos + 7 <= au.len() {
        if au[pos] != 0x0B || au[pos + 1] != 0x77 {
            pos += 1;
            continue;
        }
        let Ok(info) = aether_ac3::parse_frame_info(&au[pos..]) else {
            pos += 1;
            continue;
        };
        let end = (pos + info.frame_size).min(au.len());
        if info.frame_size < 8 {
            break;
        }
        let payloads = scan_frame(&au[pos..end]);
        if !payloads.is_empty() {
            let mut r = EmdfPayloads::default();
            for p in payloads {
                r.all.push((p.id, p.data.len()));
                match p.id {
                    PAYLOAD_ID_JOC => r.joc = Some(p.data),
                    PAYLOAD_ID_OAMD => r.oamd = Some(p.data),
                    _ => {}
                }
            }
            if r.joc.is_some() || r.oamd.is_some() || out.all.is_empty() {
                out = r;
            }
        }
        pos = end;
    }
    out
}

/// Finds the first plausible EMDF container in one syncframe.
pub fn scan_frame(frame: &[u8]) -> Vec<EmdfPayload> {
    let total_bits = frame.len() * 8;
    let mut win = 0u32;
    // The CRC at the end of the frame is not part of any container.
    let limit = total_bits.saturating_sub(16);
    for bitpos in 0..limit {
        let byte = frame[bitpos >> 3];
        win = (win << 1) | ((byte >> (7 - (bitpos & 7))) & 1) as u32;
        if bitpos >= 15 && (win & 0xFFFF) == 0x5838 {
            let start = bitpos + 1;
            if let Some(p) = parse_container(frame, start, limit) {
                return p;
            }
        }
    }
    Vec::new()
}

fn parse_container(frame: &[u8], start: usize, limit: usize) -> Option<Vec<EmdfPayload>> {
    let mut br = BitReader::with_len(frame, limit);
    br.seek(start);
    let len_bytes = br.bits(16) as usize;
    let body_start = br.pos();
    let end = body_start + len_bytes * 8;
    if len_bytes == 0 || end > limit {
        return None;
    }
    let mut br = BitReader::with_len(frame, end);
    br.seek(body_start);
    let mut version = br.bits(2);
    if version == 3 {
        version += br.variable_bits(2);
    }
    let mut key_id = br.bits(3);
    if key_id == 7 {
        key_id += br.variable_bits(3);
    }
    let _ = (version, key_id);
    let mut payloads = Vec::new();
    loop {
        let mut id = br.bits(5);
        if id == 0 {
            break;
        }
        if id == 31 {
            id += br.variable_bits(5);
        }
        // emdf_payload_config()
        let smploffste = br.flag();
        if smploffste {
            br.skip(12);
        }
        if br.flag() {
            br.variable_bits(11);
        }
        if br.flag() {
            br.variable_bits(2);
        }
        if br.flag() {
            br.skip(8);
        }
        let discard_unknown = br.flag();
        if !discard_unknown {
            let mut aligned = false;
            if !smploffste {
                aligned = br.flag();
                if aligned {
                    br.skip(2);
                }
            }
            if smploffste || aligned {
                br.skip(7);
            }
        }
        let size = br.variable_bits(8) as usize;
        if br.overrun || br.pos() + size * 8 > end {
            return None;
        }
        let mut data = Vec::with_capacity(size);
        for _ in 0..size {
            data.push(br.bits(8) as u8);
        }
        payloads.push(EmdfPayload { id, data });
    }
    // emdf_protection()
    let plp = br.bits(2);
    let pls = br.bits(2);
    let prim = match plp {
        1 => 8,
        2 => 32,
        3 => 128,
        _ => return None,
    };
    let sec = [0, 8, 32, 128][pls as usize];
    br.skip(prim + sec);
    if br.overrun || br.pos() > end || end - br.pos() >= 8 {
        return None;
    }
    if payloads.is_empty() {
        return None;
    }
    Some(payloads)
}
