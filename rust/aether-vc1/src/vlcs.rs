//! Static VLC tables (built once per process from the generated code tables).
use crate::bits::Vlc;
use crate::tables as t;
use std::sync::OnceLock;

pub struct Vlcs {
    pub imode: Vlc,
    pub norm2: Vlc,
    pub norm6: Vlc,
    pub ttmb: Vec<Vlc>,
    pub ttblk: Vec<Vlc>,
    pub subblkpat: Vec<Vlc>,
    pub fourmv_bp: Vec<Vlc>,
    pub cbpcy_p: Vec<Vlc>,
    pub mv_diff: Vec<Vlc>,
    pub intfr_4mv_mbmode: Vec<Vlc>,
    pub intfr_non4mv_mbmode: Vec<Vlc>,
    pub ref1_mvdata: Vec<Vlc>,
    pub twomv_bp: Vec<Vlc>,
    pub ac: Vec<Vlc>,
    pub ref2_mvdata: Vec<Vlc>,
    pub icbpcy: Vec<Vlc>,
    pub if_mmv_mbmode: Vec<Vlc>,
    pub if_1mv_mbmode: Vec<Vlc>,
    pub msmp4_mb_i: Vlc,
    pub msmp4_dc: Vec<Vlc>, // [dc_table_index*2 + chroma]
}

fn mk<C: Copy + Into<u32>, B: Copy + Into<u32>>(bits: u32, codes: &[C], lens: &[B]) -> Vlc {
    let v: Vec<(u32, u8)> = codes.iter().zip(lens).map(|(c, l)| ((*c).into(), (*l).into() as u8)).collect();
    Vlc::build(bits, &v)
}

fn rows<C: Copy + Into<u32>, B: Copy + Into<u32>>(bits: u32, codes: &[C], lens: &[B], n: usize, w: usize) -> Vec<Vlc> {
    (0..n).map(|i| mk(bits, &codes[i * w..(i + 1) * w], &lens[i * w..(i + 1) * w])).collect()
}

static VLCS: OnceLock<Vlcs> = OnceLock::new();

pub fn get() -> &'static Vlcs {
    VLCS.get_or_init(|| {
        let mut ac = Vec::new();
        for i in 0..8 {
            let size = t::AC_SIZES[i] as usize;
            let mut v = Vec::new();
            for k in 0..size {
                let e = &t::AC_TABLES[(i * 186 + k) * 2..(i * 186 + k) * 2 + 2];
                v.push((e[0], e[1] as u8));
            }
            ac.push(Vlc::build(9, &v));
        }
        let mut dc = Vec::new();
        for i in 0..2 {
            for j in 0..2 {
                let mut v = Vec::new();
                for k in 0..120 {
                    let b = ((i * 2 + j) * 120 + k) * 2;
                    v.push((t::MSMP4_DC_TABLES[b], t::MSMP4_DC_TABLES[b + 1] as u8));
                }
                dc.push(Vlc::build(9, &v));
            }
        }
        Vlcs {
            imode: mk(4, &t::IMODE_CODES, &t::IMODE_BITS),
            norm2: mk(3, &t::NORM2_CODES, &t::NORM2_BITS),
            norm6: mk(9, &t::NORM6_CODES, &t::NORM6_BITS),
            ttmb: rows(9, &t::TTMB_CODES, &t::TTMB_BITS, 3, 16),
            ttblk: rows(5, &t::TTBLK_CODES, &t::TTBLK_BITS, 3, 8),
            subblkpat: rows(6, &t::SUBBLKPAT_CODES, &t::SUBBLKPAT_BITS, 3, 15),
            fourmv_bp: rows(6, &t::FOURMV_BP_CODES, &t::FOURMV_BP_BITS, 4, 16),
            cbpcy_p: rows(9, &t::CBPCY_P_CODES, &t::CBPCY_P_BITS, 4, 64),
            mv_diff: rows(9, &t::MV_DIFF_CODES, &t::MV_DIFF_BITS, 4, 73),
            intfr_4mv_mbmode: rows(9, &t::INTFR_4MV_MBMODE_CODES, &t::INTFR_4MV_MBMODE_BITS, 4, 15),
            intfr_non4mv_mbmode: rows(6, &t::INTFR_NON4MV_MBMODE_CODES, &t::INTFR_NON4MV_MBMODE_BITS, 4, 9),
            ref1_mvdata: rows(9, &t::REF1_MVDATA_CODES, &t::REF1_MVDATA_BITS, 4, 72),
            twomv_bp: rows(3, &t::TWOMV_BP_CODES, &t::TWOMV_BP_BITS, 4, 4),
            ac,
            ref2_mvdata: rows(9, &t::REF2_MVDATA_CODES, &t::REF2_MVDATA_BITS, 8, 126),
            icbpcy: rows(9, &t::ICBPCY_P_CODES, &t::ICBPCY_P_BITS, 8, 63),
            if_mmv_mbmode: rows(5, &t::IF_MMV_MBMODE_CODES, &t::IF_MMV_MBMODE_BITS, 8, 8),
            if_1mv_mbmode: rows(5, &t::IF_1MV_MBMODE_CODES, &t::IF_1MV_MBMODE_BITS, 8, 6),
            msmp4_mb_i: {
                let v: Vec<(u32, u8)> =
                    (0..64).map(|k| (t::MSMP4_MB_I_TABLE[k * 2] as u32, t::MSMP4_MB_I_TABLE[k * 2 + 1] as u8)).collect();
                Vlc::build(9, &v)
            },
            msmp4_dc: dc,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bits::BitReader;

    /// Every code of every table must decode back to its own symbol and consume exactly its length.
    fn roundtrip(v: &crate::bits::Vlc, codes: &[(u32, u8)]) {
        for (sym, &(code, len)) in codes.iter().enumerate() {
            if len == 0 {
                continue;
            }
            // pad with a pattern of ones/zeros after the code
            for tail in [0u64, 0xffff_ffff] {
                let total = len as u32 + 32;
                let bits: u64 = ((code as u64) << 32) | tail;
                let mut bytes = Vec::new();
                let shifted = bits << (64 - total);
                for i in 0..8 {
                    bytes.push((shifted >> (56 - 8 * i)) as u8);
                }
                let mut br = BitReader::new(&bytes);
                assert_eq!(br.get_vlc(v), sym as i32, "code {code:#x}/{len}");
                assert_eq!(br.pos(), len as usize);
            }
        }
    }

    #[test]
    fn all_tables_roundtrip() {
        let t = super::get();
        for i in 0..8 {
            let size = crate::tables::AC_SIZES[i] as usize;
            let codes: Vec<(u32, u8)> = (0..size).map(|k| (crate::tables::AC_TABLES[(i * 186 + k) * 2], crate::tables::AC_TABLES[(i * 186 + k) * 2 + 1] as u8)).collect();
            roundtrip(&t.ac[i], &codes);
        }
        for i in 0..4 {
            let codes: Vec<(u32, u8)> = (0..73).map(|k| (crate::tables::MV_DIFF_CODES[i * 73 + k] as u32, crate::tables::MV_DIFF_BITS[i * 73 + k])).collect();
            roundtrip(&t.mv_diff[i], &codes);
            let codes: Vec<(u32, u8)> = (0..64).map(|k| (crate::tables::CBPCY_P_CODES[i * 64 + k] as u32, crate::tables::CBPCY_P_BITS[i * 64 + k])).collect();
            roundtrip(&t.cbpcy_p[i], &codes);
        }
        for i in 0..8 {
            let codes: Vec<(u32, u8)> = (0..126).map(|k| (crate::tables::REF2_MVDATA_CODES[i * 126 + k], crate::tables::REF2_MVDATA_BITS[i * 126 + k])).collect();
            roundtrip(&t.ref2_mvdata[i], &codes);
        }
        for k in 0..4 {
            let codes: Vec<(u32, u8)> = (0..120).map(|s| (crate::tables::MSMP4_DC_TABLES[(k * 120 + s) * 2], crate::tables::MSMP4_DC_TABLES[(k * 120 + s) * 2 + 1] as u8)).collect();
            roundtrip(&t.msmp4_dc[k], &codes);
        }
        let codes: Vec<(u32, u8)> = (0..64).map(|k| (crate::tables::MSMP4_MB_I_TABLE[k * 2] as u32, crate::tables::MSMP4_MB_I_TABLE[k * 2 + 1] as u8)).collect();
        roundtrip(&t.msmp4_mb_i, &codes);
    }

    #[test]
    fn bitreader_past_end_reads_zero_and_never_panics() {
        let data = [0xffu8, 0x0f];
        let mut br = BitReader::new(&data);
        assert_eq!(br.get(12), 0xff0);
        assert_eq!(br.get(32), 0xf000_0000);
        assert!(br.bits_left() < 0);
        let _ = br.get_vlc(&super::get().imode);
        assert_eq!(br.unary(0, 5), 0);
    }
}
