//! Decode plan for a task that carries its predecessor GOP as a prefix (open GOP: the CRA's leading pictures predict from the
//! GOP before it). Decoding the whole prefix doubles the work per delivered picture, but only the pictures the leading
//! pictures transitively predict from are needed. The plan reads headers only (`Decoder::planner`), keeps the closure of the
//! reference sets of the target GOP backwards through the prefix, and says which NALs to push and how many output pictures of
//! the pruned prefix to drop. Every picture that is still decoded sees exactly the references it would see with the full
//! prefix, so its samples are unchanged.
use rust_h265::{Decoder, PicPlan, parse_nal};

pub struct Plan {
    /// Per input NAL: push it to the decoder.
    pub keep: Vec<bool>,
    /// Output pictures of the kept prefix (to drop before the target GOP's first picture).
    pub skip: u32,
}

/// Longest NAL prefix the planner needs of a slice NAL (its header; the CABAC payload is not read).
pub const SLICE_HEAD: usize = 1024;

fn nal_type(n: &[u8]) -> u8 {
    (n[0] >> 1) & 63
}

/// Runs the planner over NALs (slice NALs may be cut after `SLICE_HEAD` bytes); returns the planned pictures and, per NAL,
/// the index of its picture (`usize::MAX` for NALs without one).
fn run(nals: &[&[u8]]) -> Option<(Vec<PicPlan>, Vec<usize>)> {
    let mut d = Decoder::planner();
    let mut pic_of = Vec::with_capacity(nals.len());
    let mut count = 0usize;
    for &n in nals {
        if n.len() < 2 || n[0] & 0x80 != 0 {
            return None;
        }
        let t = nal_type(n);
        let nal = parse_nal(n)?;
        let c = d.plan_nal(&nal).ok()?;
        if t < 32 && (n[1] >> 3) | ((n[0] & 1) << 5) == 0 {
            if c > count {
                count = c;
                pic_of.push(c - 1);
            } else if count > 0 {
                pic_of.push(count - 1);
            } else {
                return None;
            }
        } else {
            pic_of.push(usize::MAX);
        }
    }
    Some((d.planned().to_vec(), pic_of))
}

/// `None`: no pruning is possible or safe (fewer than two IRAP pictures, long-term references, an output order the skip
/// count cannot express, a header the planner could not read): decode everything as before.
pub fn plan(nals: &[&[u8]]) -> Option<Plan> {
    let (pics, pic_of) = run(nals)?;
    let live: Vec<usize> = (0..pics.len()).filter(|&i| !pics[i].skipped).collect();
    let k2 = live.iter().copied().filter(|&i| pics[i].irap).nth(1)?;
    if pics.iter().any(|p| p.long_term) || !pics[live[0]].irap {
        return None;
    }
    let mut need = vec![false; pics.len()];
    for &i in live.iter().filter(|&&i| i >= k2) {
        need[i] = true;
    }
    need[live[0]] = true;
    for &i in live.iter().rev() {
        if !need[i] {
            continue;
        }
        for &r in &pics[i].refs {
            // the picture with POC r that precedes this one in decoding order
            let j = live.iter().copied().rev().find(|&j| j < i && pics[j].poc == r)?;
            need[j] = true;
        }
    }
    let prefix_out = |i: usize| i < k2 && need[i] && pics[i].output;
    let max_prefix = live.iter().copied().filter(|&i| prefix_out(i)).map(|i| pics[i].poc).max();
    let min_main = live.iter().copied().filter(|&i| i >= k2 && pics[i].output).map(|i| pics[i].poc).min();
    if let (Some(a), Some(b)) = (max_prefix, min_main) {
        if a >= b {
            return None;
        }
    }
    let skip = live.iter().filter(|&&i| prefix_out(i)).count() as u32;
    let keep: Vec<bool> = pic_of.iter().map(|&p| p == usize::MAX || (!pics[p].skipped && need[p])).collect();
    // The POC derivation follows the previous TemporalId 0 picture: replan the kept NALs and require the same POCs.
    let kept: Vec<&[u8]> = nals.iter().zip(&keep).filter(|(_, k)| **k).map(|(n, _)| *n).collect();
    let (pics2, _) = run(&kept)?;
    let a: Vec<i32> = (0..pics.len()).filter(|&i| !pics[i].skipped && need[i]).map(|i| pics[i].poc).collect();
    let b: Vec<i32> = pics2.iter().filter(|p| !p.skipped).map(|p| p.poc).collect();
    if a != b {
        return None;
    }
    Some(Plan { keep, skip })
}
