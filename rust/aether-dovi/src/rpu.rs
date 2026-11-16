//! RPU CRC preflight ported from AetherEngine's DoviRpuGuard.
//!
//! A valid CRC is not full RPU validation. This module calls no RPU parser; `semantic` does, after this check.

/// Removes valid emulation-prevention bytes and checks payload prefix, trailing
/// marker and MPEG-2 CRC32. Accepts a type-62 NAL including its two-byte header.
pub fn crc_is_valid(nal: &[u8]) -> bool {
    verified_unescaped(nal).is_some()
}

pub(crate) fn verified_unescaped(nal: &[u8]) -> Option<Vec<u8>> {
    if nal.len() < 8 || crate::hevc::Header::parse(nal).is_err() || (nal[0] >> 1) & 63 != 62 {
        return None;
    }
    let mut unescaped = Vec::with_capacity(nal.len());
    let mut zeros = 0;
    for (i, &byte) in nal.iter().enumerate() {
        if zeros >= 2 && byte == 3 {
            if i + 1 >= nal.len() || nal[i + 1] > 3 {
                return None;
            }
            zeros = 0;
            continue;
        }
        unescaped.push(byte);
        zeros = if byte == 0 { zeros + 1 } else { 0 };
    }
    while unescaped.last() == Some(&0) {
        unescaped.pop();
    }
    if unescaped.len() < 8 || unescaped[2] != 0x19 || unescaped.last() != Some(&0x80) {
        return None;
    }
    let crc_offset = unescaped.len() - 5;
    let stored = u32::from_be_bytes(unescaped[crc_offset..crc_offset + 4].try_into().unwrap());
    if mpeg2_crc32(&unescaped[3..crc_offset]) == stored {
        Some(unescaped)
    } else {
        None
    }
}

pub fn mpeg2_crc32(bytes: &[u8]) -> u32 {
    let mut crc = u32::MAX;
    for &byte in bytes {
        crc ^= u32::from(byte) << 24;
        for _ in 0..8 {
            crc = if crc & 0x8000_0000 != 0 {
                (crc << 1) ^ 0x04c1_1db7
            } else {
                crc << 1
            };
        }
    }
    crc
}
