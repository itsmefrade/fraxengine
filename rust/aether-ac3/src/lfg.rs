//! FFmpeg's additive lagged Fibonacci generator (`libavutil/lfg.h`), used for
//! dither and spectral-extension noise. The sequence has to match FFmpeg
//! exactly for the decoded output to match.

use crate::tables::LFG_SEED0_STATE;

#[derive(Clone)]
pub(crate) struct Lfg {
    state: [u32; 64],
    index: u32,
}

impl Lfg {
    /// Equivalent of `av_lfg_init(&lfg, 0)` on a zero-initialised context.
    pub fn new() -> Self {
        Lfg {
            state: LFG_SEED0_STATE,
            index: 0,
        }
    }

    #[inline(always)]
    pub fn get(&mut self) -> u32 {
        let i = self.index;
        let a = self.state[(i.wrapping_sub(24) & 63) as usize]
            .wrapping_add(self.state[(i.wrapping_sub(55) & 63) as usize]);
        self.state[(i & 63) as usize] = a;
        self.index = i.wrapping_add(1);
        a
    }
}
