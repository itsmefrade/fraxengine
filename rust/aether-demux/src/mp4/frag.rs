//! Movie fragments (`moof`), segment index (`sidx`) and random access (`mfra`).

use super::{TrackState, R, boxes, find, to_ns, u16, u32, u64};
use crate::{FragmentEntry, PacketRef};

const MAX_TRUN_SAMPLES_PER_MOOF: usize = 4_000_000;

pub(super) fn parse_moof(
    body: &[u8],
    moof_start: u64,
    src_len: u64,
    tracks: &mut [TrackState],
    out: &mut Vec<PacketRef>,
    left: &mut usize,
) -> R<()> {
    let mut budget = 200_000u32;
    let kids = boxes(body, &mut budget)?;
    let mut prev_end: Option<u64> = None;
    let mut moof_samples = 0usize;
    for traf in kids.iter().filter(|b| &b.kind == b"traf") {
        let tk = boxes(traf.body, &mut budget)?;
        let tfhd = find(&tk, b"tfhd").ok_or("traf without tfhd")?;
        let b = tfhd.body;
        let flags = u32(b, 0)? & 0x00ff_ffff;
        let id = u64::from(u32(b, 4)?);
        let mut p = 8usize;
        let mut take = |bit: u32, wide: bool| -> R<Option<u64>> {
            if flags & bit == 0 {
                return Ok(None);
            }
            let v = if wide { u64(b, p)? } else { u64::from(u32(b, p)?) };
            p += if wide { 8 } else { 4 };
            Ok(Some(v))
        };
        let base_off = take(0x1, true)?;
        take(0x2, false)?;
        let def_dur = take(0x8, false)?.map(|v| v as u32);
        let def_size = take(0x10, false)?.map(|v| v as u32);
        let def_flags = take(0x20, false)?.map(|v| v as u32);
        let Some(t) = tracks.iter_mut().find(|t| t.info.id == id) else { continue };
        let base = base_off.unwrap_or(if flags & 0x2_0000 != 0 { moof_start } else { prev_end.unwrap_or(moof_start) });
        if let Some(d) = find(&tk, b"tfdt") {
            let v = if d.body.first() == Some(&1) { u64(d.body, 4)? } else { u64::from(u32(d.body, 4)?) };
            t.next_dts = i64::try_from(v).map_err(|_| "tfdt out of range")?;
        }
        let (dd, ds, df) = (def_dur.unwrap_or(t.trex.duration), def_size.unwrap_or(t.trex.size), def_flags.unwrap_or(t.trex.flags));
        let mut cursor = base;
        for trun in tk.iter().filter(|b| &b.kind == b"trun") {
            let r = trun.body;
            let tf = u32(r, 0)?;
            let (version, f) = ((tf >> 24) as u8, tf & 0x00ff_ffff);
            let _ = version;
            let count = u32(r, 4)? as usize;
            let mut q = 8usize;
            let data_offset = if f & 1 != 0 {
                q += 4;
                Some(i64::from(u32(r, 8)? as i32))
            } else {
                None
            };
            let first_flags = if f & 4 != 0 {
                let v = u32(r, q)?;
                q += 4;
                Some(v)
            } else {
                None
            };
            let row = 4 * [0x100, 0x200, 0x400, 0x800].iter().filter(|&&bit| f & bit != 0).count();
            if count.checked_mul(row).and_then(|v| v.checked_add(q)).is_none_or(|v| v > r.len()) {
                return Err("trun larger than its box".into());
            }
            moof_samples = moof_samples.saturating_add(count);
            if moof_samples > MAX_TRUN_SAMPLES_PER_MOOF || count > *left {
                return Err("packet count limit".into());
            }
            let mut at: u64 = match data_offset {
                Some(o) => u64::try_from(i128::from(base) + i128::from(o)).map_err(|_| "negative data offset")?,
                None => cursor,
            };
            let mut dts = t.next_dts;
            for i in 0..count {
                let mut rp = q + i * row;
                let mut field = |bit: u32| -> Option<u32> {
                    if f & bit == 0 {
                        return None;
                    }
                    let v = u32(r, rp).ok();
                    rp += 4;
                    v
                };
                let dur = field(0x100).unwrap_or(dd);
                let size = field(0x200).unwrap_or(ds);
                let sflags = field(0x400);
                let cts = field(0x800).map_or(0, |v| i64::from(v as i32));
                let sflags = if i == 0 && first_flags.is_some() { first_flags.unwrap_or(0) } else { sflags.unwrap_or(df) };
                if size > super::MAX_SAMPLE_BYTES {
                    return Err("sample too large".into());
                }
                let end = at.checked_add(u64::from(size)).ok_or("sample extent overflow")?;
                if end <= src_len {
                    *left -= 1;
                    out.push(PacketRef {
                        track: id,
                        pts: dts.saturating_add(cts),
                        dts,
                        duration: i64::from(dur),
                        keyframe: sflags & 0x1_0000 == 0 && (sflags >> 24) & 3 != 1,
                        presentation: true,
                        offset: at,
                        size,
                        inner: 0,
                    });
                }
                at = end;
                dts = dts.checked_add(i64::from(dur)).ok_or("decode time overflow")?;
            }
            t.next_dts = dts;
            cursor = at;
        }
        prev_end = Some(cursor);
    }
    Ok(())
}

pub(super) fn parse_sidx(body: &[u8], box_end: u64, tracks: &[TrackState], out: &mut Vec<FragmentEntry>) {
    let _ = (|| -> R<()> {
        let version = *body.first().ok_or("short sidx")?;
        let reference = u64::from(u32(body, 4)?);
        let scale = u32(body, 8)?;
        if scale == 0 {
            return Err("zero sidx timescale".into());
        }
        let (mut time, first_offset, p) = if version == 1 {
            (u64(body, 12)?, u64(body, 20)?, 28)
        } else {
            (u64::from(u32(body, 12)?), u64::from(u32(body, 16)?), 20)
        };
        let n = usize::from(u16(body, p + 2)?);
        if p + 4 + n * 12 > body.len() {
            return Err("sidx larger than its box".into());
        }
        let mut anchor = box_end.checked_add(first_offset).ok_or("sidx offset overflow")?;
        // Prefer the track id when the reference id matches one; it is a stream id otherwise.
        let _ = tracks;
        for i in 0..n {
            let word = u32(body, p + 4 + i * 12)?;
            let dur = u64::from(u32(body, p + 8 + i * 12)?);
            if word >> 31 == 0 && out.len() < 1_000_000 {
                out.push(FragmentEntry {
                    track: reference,
                    time_ns: to_ns(i128::from(time), scale).clamp(0, i128::from(i64::MAX)) as i64,
                    offset: anchor,
                    from_sidx: true,
                });
            }
            anchor = anchor.saturating_add(u64::from(word & 0x7fff_ffff));
            time = time.saturating_add(dur);
        }
        Ok(())
    })();
}

pub(super) fn parse_mfra(body: &[u8], tracks: &[TrackState], out: &mut Vec<FragmentEntry>) {
    let mut budget = 10_000u32;
    let Ok(kids) = boxes(body, &mut budget) else { return };
    for tfra in kids.iter().filter(|b| &b.kind == b"tfra") {
        let _ = (|| -> R<()> {
            let b = tfra.body;
            let wide = b.first() == Some(&1);
            let id = u64::from(u32(b, 4)?);
            let lens = u32(b, 8)?;
            let (lt, lr, ls) = (((lens >> 4) & 3) as usize + 1, ((lens >> 2) & 3) as usize + 1, (lens & 3) as usize + 1);
            let n = u32(b, 12)? as usize;
            let row = if wide { 16 } else { 8 } + lt + lr + ls;
            if n.checked_mul(row).and_then(|v| v.checked_add(16)).is_none_or(|v| v > b.len()) {
                return Err("tfra larger than its box".into());
            }
            let scale = tracks.iter().find(|t| t.info.id == id).map_or(1, |t| t.info.timescale);
            for i in 0..n {
                let p = 16 + i * row;
                let (time, off) = if wide { (u64(b, p)?, u64(b, p + 8)?) } else { (u64::from(u32(b, p)?), u64::from(u32(b, p + 4)?)) };
                if out.len() < 1_000_000 {
                    out.push(FragmentEntry {
                        track: id,
                        time_ns: to_ns(i128::from(time), scale).clamp(0, i128::from(i64::MAX)) as i64,
                        offset: off,
                        from_sidx: false,
                    });
                }
            }
            Ok(())
        })();
    }
}
