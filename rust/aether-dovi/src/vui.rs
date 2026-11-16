//! In-place relabelling of the HEVC SPS VUI colour description.
//!
//! Chrome tone maps frames that carry PQ labels and destroys the code values the
//! compositor needs (Dolby Vision reshaping, the FEL merge). Relabelling the stream
//! as BT.709 / sRGB transfer makes `importExternalTexture` return the raw YUV codes. The three colour fields are
//! fixed-width u(8), so the RBSP bit length never changes, but the escaped NAL
//! can: a patched byte may create or dissolve a `00 00 0x` run. The patch is
//! therefore applied to the unescaped RBSP and the NAL is escaped again, and the
//! result reports whether its length changed.

use crate::hevc::Header;

/// Colour description as carried in `vui_parameters()`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Colour {
    pub primaries: u8,
    pub transfer: u8,
    pub matrix: u8,
}

/// Fields to overwrite. `None` keeps the stream value.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ColourPatch {
    pub primaries: Option<u8>,
    pub transfer: Option<u8>,
    pub matrix: Option<u8>,
    /// `video_full_range_flag` to write where the stream has no video signal type at all (a
    /// stream that states its range keeps it). Dolby Vision profile 5 is full range by definition.
    pub full_range: bool,
}

/// BT.709 primaries, sRGB transfer (13), matrix untouched (BT.2020 NCL stays 9).
pub const SDR_CARRIER: ColourPatch = ColourPatch {
    primaries: Some(1),
    transfer: Some(13),
    matrix: None,
    full_range: false,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VuiError {
    NotSps,
    Truncated,
    /// Syntax this reader does not walk (e.g. an inter RPS that references a missing set).
    Unsupported,
    NoVui,
    /// The SPS has no `colour_description` (reading only; a relabel adds one).
    NoColourDescription,
    BadHvcc,
}

/// Patched NAL plus what the stream said before.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Relabelled {
    pub nal: Vec<u8>,
    pub original: Colour,
    pub original_len: usize,
    /// `nal.len() as isize - original_len as isize`.
    pub length_delta: isize,
}

struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
}

impl Bits<'_> {
    fn bit(&mut self) -> Result<u32, VuiError> {
        let byte = *self.data.get(self.pos / 8).ok_or(VuiError::Truncated)?;
        let bit = (byte >> (7 - self.pos % 8)) & 1;
        self.pos += 1;
        Ok(u32::from(bit))
    }
    fn u(&mut self, n: usize) -> Result<u32, VuiError> {
        let mut v = 0u32;
        for _ in 0..n {
            v = (v << 1) | self.bit()?;
        }
        Ok(v)
    }
    fn skip(&mut self, n: usize) -> Result<(), VuiError> {
        if self.pos + n > self.data.len() * 8 {
            return Err(VuiError::Truncated);
        }
        self.pos += n;
        Ok(())
    }
    fn ue(&mut self) -> Result<u32, VuiError> {
        let mut zeros = 0;
        while self.bit()? == 0 {
            zeros += 1;
            if zeros > 31 {
                return Err(VuiError::Unsupported);
            }
        }
        Ok(((1u64 << zeros) - 1 + u64::from(self.u(zeros)?)) as u32)
    }
    fn se(&mut self) -> Result<i32, VuiError> {
        let k = self.ue()?;
        Ok(if k & 1 == 1 { k.div_ceil(2) as i32 } else { -((k / 2) as i32) })
    }
}

/// Removes emulation prevention bytes. `map[i]` is the escaped index of RBSP byte `i`.
fn unescape(nal_payload: &[u8]) -> (Vec<u8>, Vec<usize>) {
    let mut rbsp = Vec::with_capacity(nal_payload.len());
    let mut map = Vec::with_capacity(nal_payload.len());
    let mut zeros = 0;
    for (i, &b) in nal_payload.iter().enumerate() {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        rbsp.push(b);
        map.push(i);
    }
    (rbsp, map)
}

fn escape(rbsp: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(rbsp.len() + rbsp.len() / 64);
    let mut zeros = 0;
    for &b in rbsp {
        if zeros >= 2 && b <= 3 {
            out.push(3);
            zeros = 0;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        out.push(b);
    }
    out
}

fn profile_tier_level(r: &mut Bits, max_sub_layers_minus1: u32) -> Result<(), VuiError> {
    r.skip(96)?;
    let mut profile = [false; 8];
    let mut level = [false; 8];
    for i in 0..max_sub_layers_minus1 as usize {
        profile[i] = r.bit()? == 1;
        level[i] = r.bit()? == 1;
    }
    if max_sub_layers_minus1 > 0 {
        r.skip(2 * (8 - max_sub_layers_minus1 as usize))?;
    }
    for i in 0..max_sub_layers_minus1 as usize {
        if profile[i] {
            r.skip(88)?;
        }
        if level[i] {
            r.skip(8)?;
        }
    }
    Ok(())
}

fn scaling_list_data(r: &mut Bits) -> Result<(), VuiError> {
    for size_id in 0..4 {
        let step = if size_id == 3 { 3 } else { 1 };
        let mut matrix_id = 0;
        while matrix_id < 6 {
            if r.bit()? == 0 {
                r.ue()?;
            } else {
                let coefs = 64.min(1 << (4 + (size_id << 1)));
                if size_id > 1 {
                    r.se()?;
                }
                for _ in 0..coefs {
                    r.se()?;
                }
            }
            matrix_id += step;
        }
    }
    Ok(())
}

#[derive(Clone, Default)]
struct Rps {
    s0: Vec<i32>,
    s1: Vec<i32>,
}

impl Rps {
    fn num_delta(&self) -> usize {
        self.s0.len() + self.s1.len()
    }
}

fn st_ref_pic_set(r: &mut Bits, idx: usize, count: usize, sets: &[Rps]) -> Result<Rps, VuiError> {
    let inter = idx != 0 && r.bit()? == 1;
    if !inter {
        let neg = r.ue()? as usize;
        let pos = r.ue()? as usize;
        if neg > 16 || pos > 16 {
            return Err(VuiError::Unsupported);
        }
        let (mut s0, mut s1) = (Vec::new(), Vec::new());
        let mut poc = 0;
        for _ in 0..neg {
            poc -= r.ue()? as i32 + 1;
            r.bit()?;
            s0.push(poc);
        }
        poc = 0;
        for _ in 0..pos {
            poc += r.ue()? as i32 + 1;
            r.bit()?;
            s1.push(poc);
        }
        return Ok(Rps { s0, s1 });
    }
    let delta_idx = if idx == count { r.ue()? as usize + 1 } else { 1 };
    let reference = sets
        .get(idx.checked_sub(delta_idx).ok_or(VuiError::Unsupported)?)
        .ok_or(VuiError::Unsupported)?;
    let sign = r.bit()?;
    let abs = r.ue()? as i32 + 1;
    let delta = if sign == 1 { -abs } else { abs };
    let n = reference.num_delta();
    let mut use_delta = vec![true; n + 1];
    for flag in use_delta.iter_mut() {
        if r.bit()? == 0 {
            *flag = r.bit()? == 1;
        }
    }
    let (neg_ref, pos_ref) = (reference.s0.len(), reference.s1.len());
    let (mut s0, mut s1) = (Vec::new(), Vec::new());
    for j in (0..pos_ref).rev() {
        let d = reference.s1[j] + delta;
        if d < 0 && use_delta[neg_ref + j] {
            s0.push(d);
        }
    }
    if delta < 0 && use_delta[n] {
        s0.push(delta);
    }
    for j in 0..neg_ref {
        let d = reference.s0[j] + delta;
        if d < 0 && use_delta[j] {
            s0.push(d);
        }
    }
    for j in (0..neg_ref).rev() {
        let d = reference.s0[j] + delta;
        if d > 0 && use_delta[j] {
            s1.push(d);
        }
    }
    if delta > 0 && use_delta[n] {
        s1.push(delta);
    }
    for j in 0..pos_ref {
        let d = reference.s1[j] + delta;
        if d > 0 && use_delta[neg_ref + j] {
            s1.push(d);
        }
    }
    Ok(Rps { s0, s1 })
}

/// Where the colour description is, or the bit offset of the flag that says it is absent.
#[derive(Clone, Copy)]
enum Spot {
    Colour(usize),
    NoDescription(usize),
    NoSignalType(usize),
    NoVui(usize),
}

/// Bit offset of `colour_primaries` inside the RBSP (after the 2 byte NAL header), or what is missing.
fn colour_spot(rbsp: &[u8]) -> Result<Spot, VuiError> {
    let mut r = Bits { data: rbsp, pos: 0 };
    r.skip(4)?;
    let max_sub_layers_minus1 = r.u(3)?;
    r.skip(1)?;
    profile_tier_level(&mut r, max_sub_layers_minus1)?;
    r.ue()?;
    if r.ue()? == 3 {
        r.skip(1)?;
    }
    r.ue()?;
    r.ue()?;
    if r.bit()? == 1 {
        for _ in 0..4 {
            r.ue()?;
        }
    }
    r.ue()?;
    r.ue()?;
    let log2_poc_lsb = r.ue()? + 4;
    let ordering_present = r.bit()? == 1;
    let first = if ordering_present { 0 } else { max_sub_layers_minus1 };
    for _ in first..=max_sub_layers_minus1 {
        for _ in 0..3 {
            r.ue()?;
        }
    }
    for _ in 0..6 {
        r.ue()?;
    }
    if r.bit()? == 1 && r.bit()? == 1 {
        scaling_list_data(&mut r)?;
    }
    r.skip(2)?;
    if r.bit()? == 1 {
        r.skip(8)?;
        r.ue()?;
        r.ue()?;
        r.skip(1)?;
    }
    let count = r.ue()? as usize;
    if count > 64 {
        return Err(VuiError::Unsupported);
    }
    let mut sets: Vec<Rps> = Vec::with_capacity(count);
    for i in 0..count {
        let set = st_ref_pic_set(&mut r, i, count, &sets)?;
        sets.push(set);
    }
    if r.bit()? == 1 {
        let n = r.ue()?;
        if n > 32 {
            return Err(VuiError::Unsupported);
        }
        for _ in 0..n {
            r.skip(log2_poc_lsb as usize + 1)?;
        }
    }
    r.skip(2)?;
    if r.bit()? == 0 {
        return Ok(Spot::NoVui(r.pos - 1));
    }
    if r.bit()? == 1 && r.u(8)? == 255 {
        r.skip(32)?;
    }
    if r.bit()? == 1 {
        r.skip(1)?;
    }
    if r.bit()? == 0 {
        return Ok(Spot::NoSignalType(r.pos - 1));
    }
    r.skip(4)?;
    if r.bit()? == 0 {
        return Ok(Spot::NoDescription(r.pos - 1));
    }
    Ok(Spot::Colour(r.pos))
}

/// Gives an SPS without a colour description one, filled with "unspecified" (2), and returns the
/// bit offset of its `colour_primaries`. Streaming profile 5 encodes leave the description out
/// (measured on a live title: the relabel failed and the title left the engine), and a decoder
/// then guesses the colour; the carrier has to be stated. The flag at `flag` becomes 1 and the
/// missing syntax goes in behind it, so every later bit moves and the trailing bits are rebuilt.
fn add_colour_description(rbsp: &mut Vec<u8>, spot: Spot, full_range: bool) -> Result<usize, VuiError> {
    let range = u8::from(full_range);
    let (flag, lead): (usize, Vec<u8>) = match spot {
        Spot::Colour(bit) => return Ok(bit),
        // colour_description_present_flag was 0: only the three fields are missing, the stream's range flag stands
        Spot::NoDescription(flag) => (flag, vec![]),
        // video_signal_type_present_flag was 0: video_format 5 (unspecified), the range asked for, description present
        Spot::NoSignalType(flag) => (flag, vec![1, 0, 1, range, 1]),
        // vui_parameters_present_flag was 0: no aspect ratio, no overscan, then the signal type as above
        Spot::NoVui(flag) => (flag, vec![0, 0, 1, 1, 0, 1, range, 1]),
    };
    let mut bits: Vec<u8> = rbsp.iter().flat_map(|&b| (0..8).rev().map(move |i| (b >> i) & 1)).collect();
    // rbsp_trailing_bits: the last 1 is the stop bit, everything behind it is alignment
    let stop = bits.iter().rposition(|&b| b == 1).ok_or(VuiError::Truncated)?;
    if stop <= flag {
        return Err(VuiError::Truncated);
    }
    bits.truncate(stop);
    bits[flag] = 1;
    let mut insert = lead;
    let colour_bit = flag + 1 + insert.len();
    for _ in 0..3 {
        insert.extend_from_slice(&[0, 0, 0, 0, 0, 0, 1, 0]);
    }
    if matches!(spot, Spot::NoVui(_)) {
        // the rest of vui_parameters(): chroma location, neutral chroma, field sequence, frame field info,
        // default display window, timing info, bitstream restriction, all absent
        insert.extend_from_slice(&[0; 7]);
    }
    bits.splice(flag + 1..flag + 1, insert);
    bits.push(1);
    while bits.len() % 8 != 0 {
        bits.push(0);
    }
    *rbsp = bits.chunks(8).map(|c| c.iter().fold(0, |a, &x| a << 1 | x)).collect();
    Ok(colour_bit)
}

/// `add`: an SPS without a colour description gets one (all "unspecified", with this range flag), see `add_colour_description`.
fn sps_parts(nal: &[u8], add: Option<bool>) -> Result<(Vec<u8>, Vec<usize>, usize), VuiError> {
    if nal.len() < 3 {
        return Err(VuiError::Truncated);
    }
    let header = Header::parse(nal).map_err(|_| VuiError::NotSps)?;
    if header.nal_type != 33 {
        return Err(VuiError::NotSps);
    }
    let (mut rbsp, map) = unescape(&nal[2..]);
    let spot = colour_spot(&rbsp)?;
    let bit = if let Some(full_range) = add { add_colour_description(&mut rbsp, spot, full_range)? } else {
        match spot {
            Spot::Colour(bit) => bit,
            Spot::NoVui(_) => return Err(VuiError::NoVui),
            _ => return Err(VuiError::NoColourDescription),
        }
    };
    if bit % 8 == 0 && bit / 8 + 3 > rbsp.len() || bit + 24 > rbsp.len() * 8 {
        return Err(VuiError::Truncated);
    }
    Ok((rbsp, map, bit))
}

fn read_colour(rbsp: &[u8], bit: usize) -> Colour {
    let mut r = Bits { data: rbsp, pos: bit };
    let mut next = || r.u(8).unwrap_or(0) as u8;
    Colour { primaries: next(), transfer: next(), matrix: next() }
}

/// Reads the colour description of one complete SPS NAL (with its 2 byte header).
pub fn read_sps_colour(nal: &[u8]) -> Result<Colour, VuiError> {
    let (rbsp, _, bit) = sps_parts(nal, None)?;
    Ok(read_colour(&rbsp, bit))
}

/// Rewrites the VUI colour fields of one SPS NAL. See the module comment for why
/// the result may differ in length from the input. An SPS that states no colour gets the
/// description added and reports its original as unspecified (2, 2, 2).
pub fn relabel_sps(nal: &[u8], patch: ColourPatch) -> Result<Relabelled, VuiError> {
    let (mut rbsp, _, bit) = sps_parts(nal, Some(patch.full_range))?;
    let original = read_colour(&rbsp, bit);
    let new = [
        patch.primaries.unwrap_or(original.primaries),
        patch.transfer.unwrap_or(original.transfer),
        patch.matrix.unwrap_or(original.matrix),
    ];
    for (k, value) in new.into_iter().enumerate() {
        for b in 0..8 {
            let p = bit + k * 8 + b;
            let mask = 0x80u8 >> (p % 8);
            if value & (0x80 >> b) != 0 {
                rbsp[p / 8] |= mask;
            } else {
                rbsp[p / 8] &= !mask;
            }
        }
    }
    let mut out = Vec::with_capacity(nal.len() + 2);
    out.extend_from_slice(&nal[..2]);
    out.extend(escape(&rbsp));
    Ok(Relabelled {
        length_delta: out.len() as isize - nal.len() as isize,
        original_len: nal.len(),
        original,
        nal: out,
    })
}

/// Rewrites every SPS in an Annex B buffer (3 or 4 byte start codes). Returns the
/// new buffer and the original colour of each SPS found. Other NALs are copied.
pub fn relabel_annexb(stream: &[u8], patch: ColourPatch) -> Result<(Vec<u8>, Vec<Colour>), VuiError> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= stream.len() {
        if stream[i] == 0 && stream[i + 1] == 0 && stream[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut out = Vec::with_capacity(stream.len());
    let mut originals = Vec::new();
    let mut cursor = 0;
    for (n, &s) in starts.iter().enumerate() {
        let mut end = starts.get(n + 1).map_or(stream.len(), |&next| next - 3);
        // A 4 byte start code leaves one extra zero at the tail of the previous NAL.
        while end > s && stream[end - 1] == 0 && n + 1 < starts.len() {
            end -= 1;
        }
        let nal = &stream[s..end];
        out.extend_from_slice(&stream[cursor..s]);
        if nal.len() > 2 && (nal[0] >> 1) & 63 == 33 {
            let r = relabel_sps(nal, patch)?;
            originals.push(r.original);
            out.extend_from_slice(&r.nal);
        } else {
            out.extend_from_slice(nal);
        }
        cursor = end;
    }
    out.extend_from_slice(&stream[cursor..]);
    Ok((out, originals))
}

/// Rewrites the SPS entries of an `hvcC` record (the WebCodecs `description`).
/// Array and NAL length fields are updated when an SPS changes size.
pub fn relabel_hvcc(hvcc: &[u8], patch: ColourPatch) -> Result<(Vec<u8>, Vec<Colour>), VuiError> {
    if hvcc.len() < 23 {
        return Err(VuiError::BadHvcc);
    }
    let mut out = hvcc[..23].to_vec();
    let mut originals = Vec::new();
    let mut p = 23;
    for _ in 0..hvcc[22] {
        let head = hvcc.get(p..p + 3).ok_or(VuiError::BadHvcc)?;
        let nal_type = head[0] & 0x3f;
        let count = u16::from_be_bytes([head[1], head[2]]);
        out.extend_from_slice(head);
        p += 3;
        for _ in 0..count {
            let len = hvcc.get(p..p + 2).ok_or(VuiError::BadHvcc)?;
            let len = usize::from(u16::from_be_bytes([len[0], len[1]]));
            let nal = hvcc.get(p + 2..p + 2 + len).ok_or(VuiError::BadHvcc)?;
            p += 2 + len;
            if nal_type == 33 {
                let r = relabel_sps(nal, patch)?;
                originals.push(r.original);
                out.extend_from_slice(&(r.nal.len() as u16).to_be_bytes());
                out.extend_from_slice(&r.nal);
            } else {
                out.extend_from_slice(&(len as u16).to_be_bytes());
                out.extend_from_slice(nal);
            }
        }
    }
    Ok((out, originals))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_round_trip_and_hazards() {
        let rbsp = [1, 0, 0, 1, 0, 0, 0, 2, 0, 0, 4, 5];
        let esc = escape(&rbsp);
        assert_eq!(esc, [1, 0, 0, 3, 1, 0, 0, 3, 0, 2, 0, 0, 4, 5]);
        assert_eq!(unescape(&esc).0, rbsp);
    }

    /// Hand built Main10 SPS: 1 sub layer, 64x64, VUI with colour description 9/16/9.
    fn tiny_sps(colour: [u8; 3]) -> Vec<u8> {
        let put_ue = |v: u32, b: &mut Vec<u8>| {
            let x = v + 1;
            let n = 2 * (32 - x.leading_zeros() as usize) - 1;
            (0..n).rev().for_each(|i| b.push(((x >> i) & 1) as u8));
        };
        let mut b: Vec<u8> = Vec::new();
        let p = |v: u32, n: usize, b: &mut Vec<u8>| (0..n).rev().for_each(|i| b.push(((v >> i) & 1) as u8));
        p(0, 4, &mut b);
        p(0, 3, &mut b);
        p(1, 1, &mut b);
        p(2, 8, &mut b); // profile space, tier, profile idc 2
        p(0x2000_0000, 32, &mut b);
        p(0b1001, 4, &mut b);
        p(0, 32, &mut b);
        p(0, 12, &mut b); // 43 reserved bits + 1 flag
        p(93, 8, &mut b);
        put_ue(0, &mut b);
        put_ue(1, &mut b);
        put_ue(64, &mut b);
        put_ue(64, &mut b);
        p(0, 1, &mut b);
        put_ue(2, &mut b);
        put_ue(2, &mut b);
        put_ue(4, &mut b);
        p(1, 1, &mut b);
        for v in [4, 4, 4] {
            put_ue(v, &mut b);
        }
        for v in [0, 3, 0, 3, 1, 1] {
            put_ue(v, &mut b);
        }
        p(0, 1, &mut b);
        p(0, 3, &mut b);
        put_ue(0, &mut b);
        p(0, 1, &mut b);
        p(0, 2, &mut b);
        p(1, 1, &mut b); // vui present
        p(0, 1, &mut b);
        p(0, 1, &mut b);
        p(1, 1, &mut b);
        p(5, 3, &mut b);
        p(0, 1, &mut b);
        p(1, 1, &mut b);
        for c in colour {
            p(u32::from(c), 8, &mut b);
        }
        p(1, 1, &mut b);
        while b.len() % 8 != 0 {
            b.push(0);
        }
        let mut nal = vec![0x42, 0x01];
        nal.extend(escape(&b.chunks(8).map(|c| c.iter().fold(0, |a, &x| a << 1 | x)).collect::<Vec<u8>>()));
        nal
    }

    /// The tiny SPS with its colour description cut out at one of the three levels.
    fn tiny_sps_without(level: u8) -> Vec<u8> {
        let full = tiny_sps([9, 16, 9]);
        let (rbsp, _) = unescape(&full[2..]);
        let Spot::Colour(bit) = colour_spot(&rbsp).unwrap() else { panic!() };
        let mut bits: Vec<u8> = rbsp.iter().flat_map(|&b| (0..8).rev().map(move |i| (b >> i) & 1)).collect();
        let stop = bits.iter().rposition(|&b| b == 1).unwrap();
        bits.truncate(stop);
        // layout before `bit`: vui(1) aspect(1) overscan(1) signal(1) format(3) range(1) description(1)
        let (flag, cut_from) = match level {
            0 => (bit - 1, bit),
            1 => (bit - 6, bit - 5),
            _ => (bit - 9, bit - 8),
        };
        bits[flag] = 0;
        bits.drain(cut_from..bit + 24);
        // a marker behind the VUI, to see that later syntax survives the insert
        bits.extend_from_slice(&[1, 0, 1, 1, 0]);
        bits.push(1);
        while bits.len() % 8 != 0 {
            bits.push(0);
        }
        let mut nal = vec![0x42, 0x01];
        nal.extend(escape(&bits.chunks(8).map(|c| c.iter().fold(0, |a, &x| a << 1 | x)).collect::<Vec<u8>>()));
        nal
    }

    #[test]
    fn adds_a_colour_description_where_the_stream_states_none() {
        for level in 0..3 {
            let sps = tiny_sps_without(level);
            assert!(read_sps_colour(&sps).is_err(), "level {level}");
            let r = relabel_sps(&sps, ColourPatch { primaries: Some(1), transfer: Some(13), matrix: Some(9), full_range: level == 1 }).unwrap();
            assert_eq!(r.original, Colour { primaries: 2, transfer: 2, matrix: 2 }, "level {level}");
            assert_eq!(read_sps_colour(&r.nal).unwrap(), Colour { primaries: 1, transfer: 13, matrix: 9 }, "level {level}");
            // the range flag sits two bits in front of the colour: written as asked where the stream had no signal type
            let (rb, _) = unescape(&r.nal[2..]);
            let Spot::Colour(at) = colour_spot(&rb).unwrap() else { panic!() };
            assert_eq!((rb[(at - 2) / 8] >> (7 - (at - 2) % 8)) & 1, u8::from(level == 1), "level {level}");
            // what followed the VUI is still there, bit for bit, in front of the stop bit
            let (rbsp, _) = unescape(&r.nal[2..]);
            let bits: Vec<u8> = rbsp.iter().flat_map(|&b| (0..8).rev().map(move |i| (b >> i) & 1)).collect();
            let stop = bits.iter().rposition(|&b| b == 1).unwrap();
            assert_eq!(&bits[stop - 5..stop], &[1, 0, 1, 1, 0], "level {level}");
            if level == 2 {
                // a new VUI ends in its seven absent parts
                assert_eq!(&bits[stop - 12..stop - 5], &[0; 7], "level {level}");
            }
            // a second pass finds the description and changes nothing
            assert_eq!(relabel_sps(&r.nal, ColourPatch { primaries: Some(1), transfer: Some(13), matrix: Some(9), full_range: level == 1 }).unwrap().nal, r.nal);
        }
    }

    #[test]
    fn reads_and_patches_colour() {
        let sps = tiny_sps([9, 16, 9]);
        assert_eq!(read_sps_colour(&sps).unwrap(), Colour { primaries: 9, transfer: 16, matrix: 9 });
        let r = relabel_sps(&sps, SDR_CARRIER).unwrap();
        assert_eq!(r.original, Colour { primaries: 9, transfer: 16, matrix: 9 });
        assert_eq!(read_sps_colour(&r.nal).unwrap(), Colour { primaries: 1, transfer: 13, matrix: 9 });
        assert_eq!(r.nal.len() as isize, sps.len() as isize + r.length_delta);
    }

    #[test]
    fn patch_that_creates_emulation_sequence_is_re_escaped() {
        // transfer 0 next to primaries 0 makes `00 00` runs; patching to 1/13 later and
        // back must still unescape to the same RBSP.
        let sps = tiny_sps([0, 0, 0]);
        let r = relabel_sps(&sps, ColourPatch { primaries: Some(1), transfer: Some(1), matrix: Some(0), full_range: false }).unwrap();
        assert_eq!(read_sps_colour(&r.nal).unwrap().primaries, 1);
        let back = relabel_sps(&r.nal, ColourPatch { primaries: Some(0), transfer: Some(0), matrix: Some(0), full_range: false }).unwrap();
        assert_eq!(read_sps_colour(&back.nal).unwrap(), Colour { primaries: 0, transfer: 0, matrix: 0 });
        let (a, _) = unescape(&sps[2..]);
        let (b, _) = unescape(&back.nal[2..]);
        assert_eq!(a, b);
    }

    #[test]
    fn rejects_non_sps() {
        assert_eq!(read_sps_colour(&[0x40, 0x01, 0, 0]), Err(VuiError::NotSps));
    }

    #[test]
    fn relabels_hvcc_sps_and_fixes_lengths() {
        let sps = tiny_sps([9, 16, 9]);
        let mut hvcc = vec![1; 22];
        hvcc.push(1);
        hvcc.extend([0x80 | 33, 0, 1]);
        hvcc.extend((sps.len() as u16).to_be_bytes());
        hvcc.extend(&sps);
        let (out, originals) = relabel_hvcc(&hvcc, SDR_CARRIER).unwrap();
        assert_eq!(originals[0].transfer, 16);
        let len = u16::from_be_bytes([out[26], out[27]]) as usize;
        assert_eq!(out.len(), 28 + len);
        assert_eq!(read_sps_colour(&out[28..]).unwrap().transfer, 13);
    }
}
