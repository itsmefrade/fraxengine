use aether_dtsuhd::ace::*;
use aether_dtsuhd::aceframe::*;
use aether_dtsuhd::bitreader::BitReader;
use aether_dtsuhd::ftoc::*;
use aether_dtsuhd::lfe::*;
use std::collections::BTreeMap;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let pk = std::fs::read(&args[1]).unwrap();
    let idx: Vec<usize> = std::fs::read_to_string(format!("{}.idx", args[1]))
        .unwrap()
        .trim_matches(|c| c == '[' || c == ']' || c == '\n')
        .split(',')
        .map(|s| s.trim().parse().unwrap())
        .collect();
    let qv: u32 = std::env::var("Q")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let max_frames: usize = args
        .get(2)
        .map(|s| s.parse().unwrap())
        .unwrap_or(usize::MAX);
    let mut off = 0;
    let mut fs = FtocState::new();
    let mut hs = AceHeaderState::default();
    let mut mono: Vec<AceStream> = vec![];
    let mut stereo: Vec<AceStream> = vec![];
    let mut stats: BTreeMap<String, usize> = BTreeMap::new();
    let mut leftovers: BTreeMap<String, usize> = BTreeMap::new();
    for (fi, &sz) in idx.iter().enumerate() {
        if fi >= max_frames {
            break;
        }
        let data = &pk[off..off + sz];
        off += sz;
        let ft = parse_ftoc(data, &mut fs).unwrap();
        let md_total: usize = ft.md_chunks.iter().map(|c| c.size as usize).sum();
        let ac_start = ft.ftoc_bytes + md_total;
        let ac = &data[ac_start..ac_start + ft.audio_chunks[0].size as usize];
        let h = parse_ace_frame_header(ac, &mut hs).unwrap();
        let sh = &h.streamsets[0];
        if h.is_sync {
            mono = (0..sh.num_mono_streams)
                .map(|_| AceStream::new(false))
                .collect();
            stereo = (0..sh.num_stereo_streams)
                .map(|_| AceStream::new(true))
                .collect();
            for m in mono.iter_mut().chain(stereo.iter_mut()) {
                apply_q(&mut m.quirks, qv);
            }
        }
        let mut p = h.header_bytes;
        // LFE
        for &l in &sh.lfe_sizes {
            p += l;
        }
        let ctx = |bw: usize| FrameCtx {
            sampling_rate: h.sampling_rate,
            is_predictive: sh.is_predictive,
            enable_deemphasis: h.enable_deemphasis,
            bandwidth_mode: bw,
        };
        for (i, &m) in sh.mono_sizes.iter().enumerate() {
            let payload = &ac[p..p + m];
            p += m;
            match mono[i].decode_frame(payload, &ctx(sh.mono_bandwidth_mode[i])) {
                Ok(t) => {
                    *stats
                        .entry(format!(
                            "mono cm={} short={} model={} lts={}",
                            t.coding_mode, t.is_short, t.alloc_model, t.lts_enabled
                        ))
                        .or_default() += 1;
                    if std::env::var("LIST").is_ok()
                        && t.coding_mode != 1
                        && (t.alloc_model == 1 || t.alloc_model == 4 || t.alloc_model == 5)
                    {
                        println!(
                            "MONO f={fi} model={} short={} hi={} lts={} coded={} left={} size={}",
                            t.alloc_model,
                            t.is_short,
                            t.is_highres,
                            t.lts_enabled,
                            t.num_coded_bands,
                            t.bits_left_at_end,
                            m
                        );
                    }
                    if t.coding_mode != 1 {
                        *leftovers
                            .entry(format!(
                                "mono model={} short={} lts={} hi={} coded={} left={}",
                                t.alloc_model,
                                t.is_short,
                                t.lts_enabled,
                                t.is_highres,
                                t.num_coded_bands,
                                if t.bits_left_at_end >= 3 {
                                    3
                                } else {
                                    t.bits_left_at_end.max(-3)
                                }
                            ))
                            .or_default() += 1;
                    }
                }
                Err(_e) => {
                    let t = &mono[i].last_trace;
                    *stats
                        .entry(format!(
                            "mono ERR cm={} short={} model={} left={}",
                            t.coding_mode,
                            t.is_short,
                            t.alloc_model,
                            t.bits_left_at_end.min(1)
                        ))
                        .or_default() += 1;
                }
            }
        }
        for (i, &m) in sh.stereo_sizes.iter().enumerate() {
            let payload = &ac[p..p + m];
            p += m;
            match stereo[i].decode_frame(payload, &ctx(sh.stereo_bandwidth_mode[i])) {
                Ok(t) => {
                    *stats
                        .entry(format!(
                            "stereo cm={} short={} model={} lts={} ms_mono={}",
                            t.coding_mode,
                            t.is_short,
                            t.alloc_model,
                            t.lts_enabled,
                            t.num_ms_mono_bands
                        ))
                        .or_default() += 1;
                    if t.coding_mode != 1 {
                        *leftovers
                            .entry(format!(
                                "stereo model={} short={} cm={} left={}",
                                t.alloc_model,
                                t.is_short,
                                t.coding_mode,
                                t.bits_left_at_end.clamp(-3, 3)
                            ))
                            .or_default() += 1;
                    }
                }
                Err(_e) => {
                    let t = &stereo[i].last_trace;
                    *stats
                        .entry(format!(
                            "stereo ERR cm={} short={} model={}",
                            t.coding_mode, t.is_short, t.alloc_model
                        ))
                        .or_default() += 1;
                }
            }
        }
        if std::env::var("PERFRAME").is_ok() {
            let mut line = format!("F{fi}{} ", if h.is_sync { "S" } else { "" });
            for st in mono.iter().chain(stereo.iter()) {
                let t = &st.last_trace;
                line += &format!(
                    "[c{}m{}{}{}] ",
                    t.coding_mode,
                    t.alloc_model,
                    if t.is_short { "s" } else { "l" },
                    if t.num_coded_bands == 0 { "0" } else { "" }
                );
            }
            println!("{line}");
        }
        let _ = (BitReader::new(&[]), LfeChannel::new());
    }
    for (k, v) in &stats {
        println!("{v:6} {k}");
    }
    for (k, v) in &leftovers {
        println!("LEFT {v:6} {k}");
    }
}

fn apply_q(q: &mut Quirks, v: u32) {
    q.lts_flag_bit = v & 1 == 0;
    q.refine_subtract_nch = (v >> 1) & 1 == 0;
    q.adjust_variant = ((v >> 2) & 3) as u8;
    q.beta_shift = if (v >> 4) & 1 == 0 { 11 } else { 15 };
    q.ms_diff_includes_beta = (v >> 5) & 1 == 1;
    q.pred_residual_chain = (v >> 6) & 1 == 0;
}
