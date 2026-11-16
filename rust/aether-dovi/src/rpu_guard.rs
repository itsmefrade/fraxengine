//! Conservative pre-parser for known dolby_vision 3.4.0 panic/allocation paths.
//! Based on upstream DoviRpuGuard.swift at ac357d4c. Unlike the upstream guard,
//! truncated/unsupported scans are rejected here rather than delegated.
//! This is not a proof that every possible parser panic is excluded.

struct Bits<'a> {
    bytes: &'a [u8],
    pos: usize,
}
impl Bits<'_> {
    fn end(&self) -> usize {
        self.bytes.len() * 8
    }
    fn bit(&mut self) -> Option<u64> {
        self.bits(1)
    }
    fn bits(&mut self, n: usize) -> Option<u64> {
        if n > 64 || self.pos.checked_add(n)? > self.end() {
            return None;
        }
        let mut value = 0;
        for _ in 0..n {
            value = (value << 1) | u64::from((self.bytes[self.pos / 8] >> (7 - self.pos % 8)) & 1);
            self.pos += 1;
        }
        Some(value)
    }
    fn ue(&mut self) -> Option<u64> {
        let mut zeros = 0;
        while self.bit()? == 0 {
            zeros += 1;
            if zeros >= 32 {
                return None;
            }
        }
        Some(((1u64 << zeros) - 1) + self.bits(zeros)?)
    }
    fn skip_signed(&mut self) -> Option<()> {
        self.ue()?;
        Some(())
    }
}

pub(crate) fn accepts(bytes: &[u8]) -> bool {
    let mut reader = Bits { bytes, pos: 24 };
    scan(&mut reader).is_some()
}
fn scan(r: &mut Bits<'_>) -> Option<()> {
    if r.bits(6)? != 2 {
        return None;
    }
    let format = r.bits(11)?;
    r.bits(4)?;
    r.bits(4)?;
    if r.bit()? != 1 {
        return None;
    }
    r.bit()?;
    let data_type = r.bits(2)?;
    let denom = if data_type == 0 { r.ue()? } else { 0 };
    r.bits(2)?;
    r.bit()?;
    let (bl_depth, el_depth, reserved, disable_residual) = if format & 0x700 == 0 {
        let bl = r.ue()?;
        let el = r.ue()? & 255;
        let vdr = r.ue()?;
        if vdr > 8 {
            return None;
        }
        r.bit()?;
        let reserved = r.bits(3)?;
        r.bit()?;
        let disabled = r.bit()? == 1;
        (bl, el, reserved, disabled)
    } else {
        (0, 0, 0, false)
    };
    let coef_len = match data_type {
        0 if denom <= 23 => denom as usize,
        1 => 32,
        _ => return None,
    };
    if bl_depth != 2 {
        return None;
    }
    let dm_present = r.bit()? == 1;
    let use_previous = r.bit()? == 1;
    if use_previous {
        r.ue()?;
        return if dm_present {
            scan_dm(r, reserved == 1)
        } else {
            Some(())
        };
    }
    r.ue()?;
    r.ue()?;
    r.ue()?;
    let depth = (bl_depth + 8) as usize;
    let mut pieces = [0usize; 3];
    for piece_count in &mut pieces {
        let n = r.ue()?;
        if n > (r.end() - r.pos) as u64 / depth as u64 {
            return None;
        }
        *piece_count = n as usize + 1;
        for _ in 0..n + 2 {
            r.bits(depth)?;
        }
    }
    if format & 0x700 == 0 && !disable_residual {
        if r.bits(3)? != 0 {
            return None;
        }
        r.bits(depth)?;
        r.bits(depth)?;
    }
    r.ue()?;
    r.ue()?;
    for count in pieces {
        let mut curve_idc = None;
        for _ in 0..count {
            let idc = r.ue()?;
            if curve_idc.is_some_and(|old| old != idc) {
                return None;
            }
            curve_idc = Some(idc);
            match idc {
                0 => {
                    let order = r.ue()?;
                    if order > 1 {
                        return None;
                    }
                    if order == 0 && r.bit()? == 1 {
                        return None;
                    }
                    for _ in 0..order + 2 {
                        if data_type == 0 {
                            r.skip_signed()?;
                        }
                        r.bits(coef_len)?;
                    }
                }
                1 => {
                    let order = r.bits(2)?;
                    if order > 2 {
                        return None;
                    }
                    if data_type == 0 {
                        r.skip_signed()?;
                    }
                    r.bits(coef_len)?;
                    for _ in 0..=order {
                        for _ in 0..7 {
                            if data_type == 0 {
                                r.skip_signed()?;
                            }
                            r.bits(coef_len)?;
                        }
                    }
                }
                _ => return None,
            }
        }
    }
    if format & 0x700 == 0 && !disable_residual {
        if el_depth != 2 {
            return None;
        }
        for _ in 0..3 {
            r.bits(el_depth as usize + 8)?;
            if data_type == 0 {
                r.skip_signed()?;
            }
            r.bits(coef_len)?;
            for _ in 0..2 {
                if data_type == 0 {
                    r.skip_signed()?;
                }
                r.bits(coef_len)?;
            }
        }
    }
    if dm_present {
        scan_dm(r, reserved == 1)
    } else {
        Some(())
    }
}
fn scan_dm(r: &mut Bits<'_>, compressed: bool) -> Option<()> {
    r.ue()?;
    r.ue()?;
    r.ue()?;
    if !compressed {
        if r.pos + 509 > r.end() {
            return None;
        }
        r.pos += 509;
    }
    for section in 0..2 {
        if section == 1 && r.end() - r.pos < 56 {
            return Some(());
        }
        let count = r.ue()?;
        if count > (r.end() - r.pos) as u64 / 9 {
            return None;
        }
        while r.pos & 7 != 0 {
            r.bit()?;
        }
        for _ in 0..count {
            let length = r.ue()?;
            let level = r.bits(8)?;
            if section == 1 {
                let accepted = match level {
                    8 => [10, 12, 13, 19, 25].contains(&length),
                    9 => [1, 17].contains(&length),
                    10 => [5, 21].contains(&length),
                    3 | 11 | 254 => true,
                    _ => false,
                };
                if !accepted {
                    return None;
                }
            } else if ![1, 2, 4, 5, 6, 255].contains(&level) {
                return None;
            }
            if length >= 1 << 20 || length > (r.end() - r.pos) as u64 / 8 {
                return None;
            }
            r.pos += length as usize * 8;
        }
    }
    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exp_golomb_limit_and_truncation_are_rejected() {
        let mut r = Bits {
            bytes: &[0, 0, 0, 0, 0x80],
            pos: 0,
        };
        assert_eq!(r.ue(), None);
        let mut r = Bits {
            bytes: &[0],
            pos: 0,
        };
        assert_eq!(r.ue(), None);
        let mut r = Bits {
            bytes: &[0x80],
            pos: 0,
        };
        assert_eq!(r.ue(), Some(0));
    }
    #[test]
    fn scan_rejects_incomplete_header() {
        assert!(!accepts(&[0x7c, 1, 0x19]));
    }
}
