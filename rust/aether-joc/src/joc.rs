//! JOC payload (ETSI TS 103 420 clause 6): bitstream parsing, differential
//! decoding, dequantisation (clauses 6.2 to 6.6.4). Interpolation and the
//! matrix application live in `decoder.rs`.

use crate::bits::BitReader;
use crate::tables_gen::*;

/// Number of subbands of the QMF bank.
pub const NUM_SUBBANDS: usize = 64;
/// QMF timeslots per E-AC-3 frame (1536 / 64).
pub const TIMESLOTS: usize = 24;
pub const MAX_OBJECTS: usize = 16;

pub const NUM_BANDS: [usize; 8] = [1, 3, 5, 7, 9, 12, 15, 23];

/// Table 54 transposed: for every `joc_num_bands_idx`, the parameter band of
/// every QMF subband.
pub fn sb_to_pb_table(num_bands_idx: usize) -> [u8; NUM_SUBBANDS] {
    // Rows are (first subband, last subband) with the band for
    // joc_num_bands = 23, 15, 12, 9, 7, 5, 3, 1.
    const ROWS: [(u8, u8, [u8; 8]); 23] = [
        (0, 0, [0, 0, 0, 0, 0, 0, 0, 0]),
        (1, 1, [1, 1, 1, 1, 1, 1, 0, 0]),
        (2, 2, [2, 2, 2, 2, 2, 1, 0, 0]),
        (3, 3, [3, 3, 3, 3, 2, 2, 1, 0]),
        (4, 4, [4, 4, 4, 3, 3, 2, 1, 0]),
        (5, 5, [5, 5, 4, 4, 3, 2, 1, 0]),
        (6, 6, [6, 6, 5, 4, 3, 2, 1, 0]),
        (7, 7, [7, 7, 5, 5, 3, 2, 1, 0]),
        (8, 8, [8, 8, 6, 5, 4, 2, 1, 0]),
        (9, 9, [9, 9, 6, 6, 4, 3, 1, 0]),
        (10, 10, [10, 9, 6, 6, 4, 3, 1, 0]),
        (11, 11, [11, 10, 7, 6, 4, 3, 1, 0]),
        (12, 13, [12, 10, 7, 6, 4, 3, 1, 0]),
        (14, 15, [13, 11, 8, 7, 5, 3, 2, 0]),
        (16, 17, [14, 11, 8, 7, 5, 3, 2, 0]),
        (18, 19, [15, 12, 9, 7, 5, 3, 2, 0]),
        (20, 22, [16, 12, 9, 7, 5, 3, 2, 0]),
        (23, 25, [17, 13, 10, 8, 6, 4, 2, 0]),
        (26, 29, [18, 13, 10, 8, 6, 4, 2, 0]),
        (30, 34, [19, 13, 10, 8, 6, 4, 2, 0]),
        (35, 40, [20, 14, 11, 8, 6, 4, 2, 0]),
        (41, 47, [21, 14, 11, 8, 6, 4, 2, 0]),
        (48, 63, [22, 14, 11, 8, 6, 4, 2, 0]),
    ];
    // Column of the table for joc_num_bands: 23 -> 0, 15 -> 1, ... 1 -> 7.
    let col = 7 - num_bands_idx;
    let mut t = [0u8; NUM_SUBBANDS];
    for &(a, b, ref v) in ROWS.iter() {
        for sb in a..=b {
            t[sb as usize] = v[col];
        }
    }
    t
}

#[derive(Clone, Debug, Default)]
pub struct JocObject {
    pub present: bool,
    pub num_bands: usize,
    pub num_bands_idx: usize,
    pub sparse: bool,
    pub quant_idx: usize,
    /// `joc_slope_idx`: 0 smooth (linear), 1 steep.
    pub slope_idx: u8,
    pub num_dpoints: usize,
    /// `joc_offset_ts` per data point (only meaningful when steep).
    pub offset_ts: [usize; 2],
    /// Dequantised mixing matrix `[dp][ch][pb]`, flattened as
    /// `(dp * num_channels + ch) * num_bands + pb`.
    pub dq: Vec<f32>,
    /// The quantised values before dequantisation (same layout), for tests.
    pub q: Vec<u16>,
}

#[derive(Clone, Debug, Default)]
pub struct JocFrame {
    pub dmx_config_idx: u8,
    pub num_channels: usize,
    pub num_objects: usize,
    pub ext_config_idx: u8,
    pub clip_gain: f32,
    pub seq_count: u16,
    pub objects: Vec<JocObject>,
    /// Bits consumed by joc() (excluding padding).
    pub bits_used: usize,
    /// Bits in the payload.
    pub bits_total: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JocError {
    Reserved,
    Truncated,
    Huffman,
}

fn huff_decode(br: &mut BitReader<'_>, tree: &[[i16; 2]]) -> Result<u32, JocError> {
    let mut node = 0i32;
    loop {
        let bit = br.bit() as usize;
        if br.overrun {
            return Err(JocError::Truncated);
        }
        node = tree[node as usize][bit] as i32;
        if node <= 0 {
            return Ok((-node - 1) as u32);
        }
        if node as usize >= tree.len() {
            return Err(JocError::Huffman);
        }
    }
}

/// Which offset the non-selected channels of a sparse matrix are set to. The
/// specification (pseudocode 2) prints 50 / 100, which dequantises to +0.4
/// instead of 0; see `SparseOffsetMode` and the notes in README.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SparseOffsetMode {
    /// Offsets exactly as printed in pseudocode 2 (50 and 100).
    AsPrinted,
    /// Offsets equal to the dequantiser zero point (48 and 96).
    ZeroPoint,
}

pub fn parse_joc(data: &[u8], mode: SparseOffsetMode) -> Result<JocFrame, JocError> {
    let mut br = BitReader::new(data);
    let mut f = JocFrame { bits_total: data.len() * 8, ..Default::default() };
    f.dmx_config_idx = br.bits(3) as u8;
    f.num_objects = br.bits(6) as usize + 1;
    f.ext_config_idx = br.bits(3) as u8;
    f.num_channels = match f.dmx_config_idx {
        0 | 3 => 5,
        1 | 2 | 4 => 7,
        _ => return Err(JocError::Reserved),
    };
    if f.num_objects > MAX_OBJECTS {
        return Err(JocError::Reserved);
    }
    let cx = br.bits(3);
    let cy = br.bits(5);
    f.clip_gain = (1.0 + cy as f32 / 32.0) * (1 << cx) as f32;
    f.seq_count = br.bits(10) as u16;
    let nch = f.num_channels;
    f.objects = vec![JocObject::default(); f.num_objects];
    for o in f.objects.iter_mut() {
        o.present = br.flag();
        if !o.present {
            continue;
        }
        o.num_bands_idx = br.bits(3) as usize;
        o.num_bands = NUM_BANDS[o.num_bands_idx];
        o.sparse = br.flag();
        o.quant_idx = br.bits(1) as usize;
        o.slope_idx = br.bits(1) as u8;
        o.num_dpoints = br.bits(1) as usize + 1;
        if o.slope_idx == 1 {
            for dp in 0..o.num_dpoints {
                o.offset_ts[dp] = br.bits(5) as usize + 1;
            }
        }
    }
    if br.overrun {
        return Err(JocError::Truncated);
    }
    // joc_data()
    for o in f.objects.iter_mut() {
        if !o.present {
            continue;
        }
        let nq: i32 = if o.quant_idx == 0 { 96 } else { 192 };
        let nb = o.num_bands;
        o.q = vec![0u16; o.num_dpoints * nch * nb];
        let zero = nq / 2;
        for dp in 0..o.num_dpoints {
            let base = dp * nch * nb;
            if o.sparse {
                let mut chidx = vec![0u32; nb];
                chidx[0] = br.bits(3);
                let idx_tree: &[[i16; 2]] = if nch == 5 {
                    &JOC_HUFF_CODE_5CH_POS_INDEX_SPARSE
                } else {
                    &JOC_HUFF_CODE_7CH_POS_INDEX_SPARSE
                };
                for pb in 1..nb {
                    chidx[pb] = huff_decode(&mut br, idx_tree)?;
                }
                let vec_tree: &[[i16; 2]] = if o.quant_idx == 0 {
                    &JOC_HUFF_CODE_COARSE_COEFF_SPARSE
                } else {
                    &JOC_HUFF_CODE_FINE_COEFF_SPARSE
                };
                let mut vecv = vec![0u32; nb];
                for pb in 0..nb {
                    vecv[pb] = huff_decode(&mut br, vec_tree)?;
                }
                let printed = if o.quant_idx == 0 { 50 } else { 100 };
                let offset = match mode {
                    SparseOffsetMode::AsPrinted => printed,
                    SparseOffsetMode::ZeroPoint => zero,
                };
                let mut prev_mod = 0usize;
                for pb in 0..nb {
                    let m = if pb == 0 { chidx[0] as usize } else { (prev_mod + chidx[pb] as usize) % nch };
                    prev_mod = m;
                    for ch in 0..nch {
                        let idx = base + ch * nb + pb;
                        if ch == m {
                            o.q[idx] = if pb == 0 {
                                ((offset + vecv[pb] as i32) % nq) as u16
                            } else {
                                ((o.q[base + ch * nb + pb - 1] as i32 + vecv[pb] as i32) % nq) as u16
                            };
                        } else {
                            o.q[idx] = offset as u16;
                        }
                    }
                }
            } else {
                let tree: &[[i16; 2]] = if o.quant_idx == 0 {
                    &JOC_HUFF_CODE_COARSE_GENERIC
                } else {
                    &JOC_HUFF_CODE_FINE_GENERIC
                };
                for ch in 0..nch {
                    for pb in 0..nb {
                        let v = huff_decode(&mut br, tree)? as i32;
                        let idx = base + ch * nb + pb;
                        o.q[idx] = if pb == 0 {
                            ((zero + v) % nq) as u16
                        } else {
                            ((o.q[idx - 1] as i32 + v) % nq) as u16
                        };
                    }
                }
            }
        }
        // Dequantisation (pseudocode 5).
        let scale = 820.0f32 / (4096.0 * (1 + o.quant_idx) as f32);
        o.dq = o.q.iter().map(|&q| (q as i32 - zero) as f32 * scale).collect();
    }
    if br.overrun {
        return Err(JocError::Truncated);
    }
    f.bits_used = br.pos();
    Ok(f)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leaves(tree: &[[i16; 2]], node: usize, depth: u32, out: &mut Vec<(u32, u32)>) {
        for b in 0..2 {
            let c = tree[node][b] as i32;
            if c <= 0 {
                out.push(((-c - 1) as u32, depth + 1));
            } else {
                leaves(tree, c as usize, depth + 1, out);
            }
        }
    }

    /// Code word (bits, length) of `sym` by depth-first search.
    fn code_of(tree: &[[i16; 2]], node: usize, sym: u32, path: u32, depth: u32) -> Option<(u32, u32)> {
        for b in 0..2u32 {
            let c = tree[node][b as usize] as i32;
            let p = (path << 1) | b;
            if c <= 0 {
                if (-c - 1) as u32 == sym {
                    return Some((p, depth + 1));
                }
            } else if let Some(r) = code_of(tree, c as usize, sym, p, depth + 1) {
                return Some(r);
            }
        }
        None
    }

    #[test]
    fn huffman_trees_are_complete_prefix_codes() {
        let trees: [(&[[i16; 2]], u32); 6] = [
            (&JOC_HUFF_CODE_COARSE_GENERIC, 96),
            (&JOC_HUFF_CODE_FINE_GENERIC, 192),
            (&JOC_HUFF_CODE_COARSE_COEFF_SPARSE, 96),
            (&JOC_HUFF_CODE_FINE_COEFF_SPARSE, 192),
            (&JOC_HUFF_CODE_5CH_POS_INDEX_SPARSE, 5),
            (&JOC_HUFF_CODE_7CH_POS_INDEX_SPARSE, 7),
        ];
        for (t, n) in trees {
            let mut l = Vec::new();
            leaves(t, 0, 0, &mut l);
            assert_eq!(l.len() as u32, n);
            let mut syms: Vec<u32> = l.iter().map(|x| x.0).collect();
            syms.sort();
            assert_eq!(syms, (0..n).collect::<Vec<_>>());
            // Kraft sum of a complete prefix code is exactly one.
            let kraft: f64 = l.iter().map(|&(_, d)| 0.5f64.powi(d as i32)).sum();
            assert!((kraft - 1.0).abs() < 1e-12, "kraft {kraft}");
        }
    }

    #[test]
    fn parameter_band_mapping_follows_table_54() {
        // Worked example of clause 6.5: 15 bands, subband 24 -> band 13.
        assert_eq!(sb_to_pb_table(6)[24], 13);
        for (idx, &nb) in NUM_BANDS.iter().enumerate() {
            let t = sb_to_pb_table(idx);
            assert_eq!(t[0], 0);
            assert_eq!(t[63] as usize, nb - 1, "last band for {nb} bands");
            assert!(t.windows(2).all(|w| w[1] >= w[0] && w[1] - w[0] <= 1));
        }
    }

    #[test]
    fn dequantisation_ranges_match_the_note_of_clause_6_6_4() {
        let c = |q: i32, qi: usize| (q - if qi == 0 { 48 } else { 96 }) as f32 * 820.0 / (4096.0 * (1 + qi) as f32);
        assert!((c(0, 0) + 9.6).abs() < 0.02 && (c(95, 0) - 9.4).abs() < 0.02);
        assert!((c(0, 1) + 9.6).abs() < 0.02 && (c(191, 1) - 9.5).abs() < 0.02);
    }

    struct BitWriter(Vec<u8>, usize);
    impl BitWriter {
        fn put(&mut self, v: u32, n: u32) {
            for i in (0..n).rev() {
                if self.1 % 8 == 0 {
                    self.0.push(0);
                }
                let bit = ((v >> i) & 1) as u8;
                let last = self.0.len() - 1;
                self.0[last] |= bit << (7 - self.1 % 8);
                self.1 += 1;
            }
        }
        fn huff(&mut self, tree: &[[i16; 2]], sym: u32) {
            let (c, n) = code_of(tree, 0, sym, 0, 0).unwrap();
            self.put(c, n);
        }
    }

    #[test]
    fn sparse_payload_round_trip_follows_pseudocode_2() {
        let mut w = BitWriter(Vec::new(), 0);
        w.put(0, 3); // 5.X
        w.put(0, 6); // one object
        w.put(0, 3); // no extension
        w.put(0, 3);
        w.put(0, 5); // clip gain
        w.put(5, 10); // seq
        w.put(1, 1); // present
        w.put(1, 3); // 3 bands
        w.put(1, 1); // sparse
        w.put(0, 1); // coarse
        w.put(0, 1); // smooth
        w.put(0, 1); // one data point
        w.put(2, 3); // channel_idx[0]
        w.huff(&JOC_HUFF_CODE_5CH_POS_INDEX_SPARSE, 1);
        w.huff(&JOC_HUFF_CODE_5CH_POS_INDEX_SPARSE, 4);
        for v in [5, 0, 3] {
            w.huff(&JOC_HUFF_CODE_COARSE_COEFF_SPARSE, v);
        }
        let f = parse_joc(&w.0, SparseOffsetMode::ZeroPoint).unwrap();
        assert!(w.1 - f.bits_used == 0 && f.seq_count == 5);
        let o = &f.objects[0];
        let q = |ch: usize, pb: usize| o.q[ch * 3 + pb];
        // Selected channels: pb0 -> 2, pb1 -> (2+1)%5 = 3, pb2 -> (3+4)%5 = 2.
        assert_eq!(q(2, 0), 53);
        assert_eq!(q(3, 1), 48);
        assert_eq!(q(2, 2), (48 + 3) as u16);
        for ch in [0, 1, 4] {
            for pb in 0..3 {
                assert_eq!(q(ch, pb), 48);
            }
        }
    }
}
