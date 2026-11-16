//! Output renderer: turns decoded audio into the best signal for the actual device.
//!
//! ```text
//! OutputPlan::new(source_labels, device_channels, mode) -> OutputPlan { kind, out_layout, description }
//! plan.build() -> Output;  output.process(planar in) -> planar out
//! ```
//!
//! * [`PlanKind::Identity`]: the source layout fits the device, the planes pass through unchanged
//!   (bit exact; this is the lossless path whenever `device_channels >= source channels`).
//! * [`PlanKind::Mix`]: speaker downmix by the ITU-R BS.775 style rules in [`matrix`]
//!   (7.1.4 / 7.1.2 / 5.1.4 / 5.1.2 -> 7.1 -> 5.1 -> 2.0 -> 1.0), optionally a linked peak limiter.
//! * [`PlanKind::Binaural`]: headphones, every positional channel convolved with the MIT KEMAR
//!   HRTF of its speaker direction ([`binaural`], data and licence in `NOTICE-HRTF`).
//!
//! The limiter is off unless asked for. Nothing here changes the signal silently: the plan's
//! `description` says what is done (layouts, folds, LFE, peak headroom, limiter).

use crate::layout;

mod binaural;
mod fft;
mod hrtf;
mod hrtf_data;
mod limiter;
pub mod matrix;
#[cfg(test)]
mod tests;

pub use binaural::Binaural;
pub use hrtf::Direction;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Loudspeakers: fold to the largest standard layout the device carries.
    Speakers,
    /// Headphones: positional channels are rendered binaurally to two channels.
    Headphones,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlanKind {
    Identity,
    Mix,
    Binaural,
}

#[derive(Clone, Debug, PartialEq)]
pub enum OutputError {
    EmptySource,
    UnknownLabel(String),
    DuplicateLabel(String),
    BadDeviceChannels,
    BadSampleRate(u32),
    BadTarget(String),
    FrameCount { frames: usize, max: usize },
    ChannelCount { expected: usize, got: usize },
}

impl std::fmt::Display for OutputError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptySource => write!(f, "source layout is empty"),
            Self::UnknownLabel(l) => write!(f, "unknown channel label {l:?}"),
            Self::DuplicateLabel(l) => write!(f, "channel label {l:?} appears twice"),
            Self::BadDeviceChannels => write!(f, "device channel count must be 1..=64"),
            Self::BadSampleRate(r) => write!(f, "sample rate {r} outside 8000..=384000"),
            Self::BadTarget(m) => write!(f, "unusable target layout: {m}"),
            Self::FrameCount { frames, max } => write!(f, "{frames} frames exceed the {max} frame limit"),
            Self::ChannelCount { expected, got } => write!(f, "expected {expected} channels, got {got}"),
        }
    }
}

impl std::error::Error for OutputError {}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Options {
    /// Linked peak limiter on the output (off by default, never enabled implicitly).
    pub limiter: bool,
    /// Gain of the LFE channel when it is folded into a stereo / mono mix. `None` picks the mode
    /// default: dropped for speakers (BS.775), 0.5 for headphones (binaural LFE is mixed to both ears).
    pub lfe_gain: Option<f32>,
    /// Headphones: also render plain stereo through the HRTF (speakers at +-30). Default false:
    /// stereo passes through unchanged.
    pub virtualize_stereo: bool,
    pub sample_rate: u32,
    /// Largest frame count of one `process` call.
    pub max_frames: usize,
    /// Convolution hop (power of two). Frame counts that are multiples of it have zero latency.
    pub hop: usize,
}

impl Default for Options {
    fn default() -> Self {
        Self { limiter: false, lfe_gain: None, virtualize_stereo: false, sample_rate: 48000, max_frames: 8192, hop: 256 }
    }
}

/// What will be done for one (source, device, mode) triple.
#[derive(Clone, Debug)]
pub struct OutputPlan {
    /// Canonical input labels, one per input plane.
    pub source: Vec<&'static str>,
    pub device_channels: usize,
    pub mode: Mode,
    pub options: Options,
    pub kind: PlanKind,
    /// Labels of the output planes, in plane order.
    pub out_layout: Vec<&'static str>,
    /// Mix matrix for [`PlanKind::Mix`] (`rows[o]` = `(input, gain)` pairs).
    pub matrix: matrix::Rows,
    pub description: String,
}

/// "7.1.4", "5.1", "2.0": bed channels, LFE channels, height channels.
pub fn layout_name(labels: &[&str]) -> String {
    let heights = labels.iter().filter(|l| matrix::is_height(l)).count();
    let lfe = labels.iter().filter(|l| matrix::is_lfe(l)).count();
    let bed = labels.len() - heights - lfe;
    if heights > 0 { format!("{bed}.{lfe}.{heights}") } else { format!("{bed}.{lfe}") }
}

fn canonical_labels<S: AsRef<str>>(source: &[S]) -> Result<Vec<&'static str>, OutputError> {
    if source.is_empty() {
        return Err(OutputError::EmptySource);
    }
    let mut out: Vec<&'static str> = Vec::with_capacity(source.len());
    for s in source {
        let c = layout::canonical(s.as_ref()).ok_or_else(|| OutputError::UnknownLabel(s.as_ref().to_string()))?;
        if out.contains(&c) {
            return Err(OutputError::DuplicateLabel(c.to_string()));
        }
        out.push(c);
    }
    Ok(out)
}

fn db(x: f32) -> f32 {
    20.0 * x.log10()
}

impl OutputPlan {
    pub fn new<S: AsRef<str>>(source_layout: &[S], device_channels: usize, mode: Mode) -> Result<Self, OutputError> {
        Self::with_options(source_layout, device_channels, mode, Options::default())
    }

    pub fn with_options<S: AsRef<str>>(
        source_layout: &[S],
        device_channels: usize,
        mode: Mode,
        options: Options,
    ) -> Result<Self, OutputError> {
        let source = canonical_labels(source_layout)?;
        if device_channels == 0 || device_channels > 64 {
            return Err(OutputError::BadDeviceChannels);
        }
        if !(8000..=384_000).contains(&options.sample_rate) {
            return Err(OutputError::BadSampleRate(options.sample_rate));
        }
        let headphones = mode == Mode::Headphones && device_channels >= 2;
        let lfe_gain = options.lfe_gain.unwrap_or(if headphones { 0.5 } else { 0.0 });
        let has_heights = source.iter().any(|l| matrix::is_height(l));
        let positional = source.iter().filter(|l| !matrix::is_lfe(l)).count();

        // Mono dual-mono: one channel to a device with two or more.
        if source.len() == 1 && device_channels >= 2 {
            let mut plan = Self::mixed(source, device_channels, mode, options, &matrix::T20, 0.0, "mono duplicated to both channels")?;
            // Dual mono, not a phantom centre: both channels carry the sample unchanged.
            plan.matrix = vec![vec![(0, 1.0)], vec![(0, 1.0)]];
            plan.description = format!(
                "1.0 (1 ch) -> 2.0 (2 ch), mono duplicated to both channels (1.0 each), limiter {}",
                if options.limiter { "on" } else { "off" }
            );
            return Ok(plan);
        }
        if headphones {
            let plain_stereo = source.len() == 2 && source.contains(&"FL") && source.contains(&"FR");
            if (plain_stereo && !options.virtualize_stereo) || positional == 0 {
                return Ok(Self::identity(source, device_channels, mode, options));
            }
            return Ok(Self::binaural(source, device_channels, mode, options, lfe_gain));
        }
        let fits = source.len() <= device_channels && (!has_heights || device_channels >= 10);
        if fits {
            return Ok(Self::identity(source, device_channels, mode, options));
        }
        let target = matrix::select_target(&source, device_channels);
        Self::mixed(source, device_channels, mode, options, &target, lfe_gain, "")
    }

    /// Mix to an explicit target layout (also an upmix: target channels the source lacks stay
    /// silent). `lfe_gain` is used when the target has no LFE channel.
    pub fn with_target<S: AsRef<str>, T: AsRef<str>>(
        source_layout: &[S],
        target_layout: &[T],
        options: Options,
        lfe_gain: f32,
    ) -> Result<Self, OutputError> {
        let source = canonical_labels(source_layout)?;
        let target = canonical_labels(target_layout)?;
        let n = target.len();
        Self::mixed(source, n, Mode::Speakers, options, &target, lfe_gain, "")
    }

    fn identity(source: Vec<&'static str>, device_channels: usize, mode: Mode, options: Options) -> Self {
        let name = layout_name(&source);
        let mut description = format!("{name} ({} ch) passthrough, bit-exact", source.len());
        if options.limiter {
            description.push_str(", limiter on");
        }
        Self {
            out_layout: source.clone(),
            source,
            device_channels,
            mode,
            options,
            kind: PlanKind::Identity,
            matrix: Vec::new(),
            description,
        }
    }

    fn mixed(
        source: Vec<&'static str>,
        device_channels: usize,
        mode: Mode,
        options: Options,
        target: &[&'static str],
        lfe_gain: f32,
        note: &str,
    ) -> Result<Self, OutputError> {
        let rows = matrix::build(&source, target, lfe_gain)?;
        let peak = matrix::peak_gain(&rows);
        let from = layout_name(&source);
        let to = layout_name(target);
        let mut d = format!("{from} ({} ch) -> {to} ({} ch)", source.len(), target.len());
        if !note.is_empty() {
            d.push_str(&format!(", {note}"));
        } else {
            let heights = source.iter().filter(|l| matrix::is_height(l)).count()
                > target.iter().filter(|l| matrix::is_height(l)).count();
            d.push_str(" downmix (ITU-R BS.775 style");
            if heights {
                d.push_str(", heights folded at -3 dB");
            }
            d.push(')');
            if source.contains(&"LFE") && !target.contains(&"LFE") {
                if lfe_gain > 0.0 {
                    d.push_str(&format!(", LFE mixed at {:.1} dB", db(lfe_gain)));
                } else {
                    d.push_str(", LFE dropped");
                }
            }
        }
        d.push_str(&format!(", worst-case peak {:+.1} dB", db(peak)));
        d.push_str(if options.limiter { ", limiter on" } else { ", limiter off" });
        Ok(Self {
            out_layout: target.to_vec(),
            source,
            device_channels,
            mode,
            options,
            kind: PlanKind::Mix,
            matrix: rows,
            description: d,
        })
    }

    fn binaural(source: Vec<&'static str>, device_channels: usize, mode: Mode, options: Options, lfe_gain: f32) -> Self {
        let description = format!(
            "{} ({} ch) -> binaural stereo (MIT KEMAR diffuse-field HRTF, {} Hz, LFE {}, limiter {})",
            layout_name(&source),
            source.len(),
            options.sample_rate,
            if lfe_gain > 0.0 { format!("{:.1} dB", db(lfe_gain)) } else { "dropped".to_string() },
            if options.limiter { "on" } else { "off" }
        );
        Self {
            out_layout: vec!["FL", "FR"],
            source,
            device_channels,
            mode,
            options,
            kind: PlanKind::Binaural,
            matrix: Vec::new(),
            description,
        }
    }

    /// Dense `outputs x inputs` matrix (row-major) of a Mix plan, empty for the other kinds.
    pub fn matrix_dense(&self) -> Vec<f32> {
        let n = self.source.len();
        let mut m = vec![0.0; self.matrix.len() * n];
        for (o, row) in self.matrix.iter().enumerate() {
            for &(i, g) in row {
                m[o * n + i] = g;
            }
        }
        m
    }

    /// Worst-case output level for full-scale aligned input (Mix plans), 1.0 otherwise.
    pub fn peak_gain(&self) -> f32 {
        if self.kind == PlanKind::Mix { matrix::peak_gain(&self.matrix) } else { 1.0 }
    }

    /// The response pairs (left ear, right ear) of the binaural plan, in input plane order, at the
    /// plan's sample rate, calibrated so that the front left speaker has unit total energy.
    pub fn binaural_responses(&self) -> Vec<(Vec<f32>, Vec<f32>)> {
        let rate = self.options.sample_rate;
        let has_side = self.source.iter().any(|l| matches!(*l, "SL" | "SR"));
        let has_back = self.source.iter().any(|l| matches!(*l, "BL" | "BR"));
        let fl = hrtf::direction("FL", true, true).expect("FL has a direction");
        let (l, r) = hrtf::pair_at(fl, rate);
        let energy: f32 = l.iter().chain(r.iter()).map(|x| x * x).sum();
        let scale = 1.0 / energy.sqrt();
        let lfe_gain = self.options.lfe_gain.unwrap_or(0.5);
        let lfe_delay = ((0.0005 * f64::from(rate)).round() as usize).max(1);
        self.source
            .iter()
            .map(|label| match hrtf::direction(label, has_side, has_back) {
                Some(dir) => {
                    let (l, r) = hrtf::pair_at(dir, rate);
                    (l.iter().map(|x| x * scale).collect(), r.iter().map(|x| x * scale).collect())
                }
                None => {
                    let mut d = vec![0.0f32; lfe_delay + 1];
                    d[lfe_delay] = lfe_gain;
                    (d.clone(), d)
                }
            })
            .collect()
    }

    pub fn build(&self) -> Result<Output, OutputError> {
        Output::new(self.clone())
    }
}

enum Engine {
    Identity,
    Mix,
    Binaural(Box<Binaural>),
}

/// A running output stage. `process` never allocates.
pub struct Output {
    plan: OutputPlan,
    engine: Engine,
    limiter: Option<limiter::Limiter>,
}

impl Output {
    pub fn new(plan: OutputPlan) -> Result<Self, OutputError> {
        let engine = match plan.kind {
            PlanKind::Identity => Engine::Identity,
            PlanKind::Mix => Engine::Mix,
            PlanKind::Binaural => {
                Engine::Binaural(Box::new(Binaural::new(&plan.binaural_responses(), plan.options.hop, plan.options.max_frames)))
            }
        };
        let limiter = plan.options.limiter.then(|| limiter::Limiter::new(plan.options.sample_rate));
        Ok(Self { plan, engine, limiter })
    }

    pub fn plan(&self) -> &OutputPlan {
        &self.plan
    }

    pub fn input_channels(&self) -> usize {
        self.plan.source.len()
    }

    pub fn output_channels(&self) -> usize {
        self.plan.out_layout.len()
    }

    pub fn max_frames(&self) -> usize {
        self.plan.options.max_frames
    }

    /// Forgets all filter and limiter state (after a seek).
    pub fn reset(&mut self) {
        if let Engine::Binaural(b) = &mut self.engine {
            b.reset();
        }
        if let Some(l) = &mut self.limiter {
            l.reset();
        }
    }

    /// Frames the limiter reduced and the deepest reduction in dB since the previous call.
    pub fn limiter_stats(&mut self) -> (u64, f32) {
        self.limiter.as_mut().map_or((0, 0.0), limiter::Limiter::take_stats)
    }

    /// Planar flat buffers: input plane `c` is `input[c * in_stride..][..frames]`, output plane
    /// `o` is `out[o * out_stride..][..frames]`.
    pub fn process_planar(
        &mut self,
        input: &[f32],
        in_stride: usize,
        frames: usize,
        out: &mut [f32],
        out_stride: usize,
    ) -> Result<(), OutputError> {
        let max = self.plan.options.max_frames;
        if frames > max || frames > in_stride || frames > out_stride {
            return Err(OutputError::FrameCount { frames, max: max.min(in_stride).min(out_stride) });
        }
        let nin = self.plan.source.len();
        let nout = self.plan.out_layout.len();
        match &mut self.engine {
            Engine::Identity => {
                for c in 0..nin {
                    out[c * out_stride..c * out_stride + frames]
                        .copy_from_slice(&input[c * in_stride..c * in_stride + frames]);
                }
            }
            Engine::Mix => {
                for (o, row) in self.plan.matrix.iter().enumerate() {
                    let dst = &mut out[o * out_stride..o * out_stride + frames];
                    match row.as_slice() {
                        [] => dst.fill(0.0),
                        [(i, g)] if *g == 1.0 => dst.copy_from_slice(&input[i * in_stride..i * in_stride + frames]),
                        [(i, g), rest @ ..] => {
                            let src = &input[i * in_stride..i * in_stride + frames];
                            for (d, s) in dst.iter_mut().zip(src) {
                                *d = g * s;
                            }
                            for (i, g) in rest {
                                let src = &input[i * in_stride..i * in_stride + frames];
                                for (d, s) in dst.iter_mut().zip(src) {
                                    *d += g * s;
                                }
                            }
                        }
                    }
                }
            }
            Engine::Binaural(b) => b.process(input, in_stride, frames, out, out_stride),
        }
        if let Some(l) = &mut self.limiter {
            l.process(out, out_stride, nout, frames);
        }
        Ok(())
    }

    /// Convenience over `Vec` planes (allocates the result). All input planes need the same length.
    pub fn process(&mut self, input: &[Vec<f32>]) -> Result<Vec<Vec<f32>>, OutputError> {
        if input.len() != self.input_channels() {
            return Err(OutputError::ChannelCount { expected: self.input_channels(), got: input.len() });
        }
        let frames = input.first().map_or(0, Vec::len);
        let mut flat_in = vec![0.0f32; input.len() * frames.max(1)];
        for (c, p) in input.iter().enumerate() {
            flat_in[c * frames..c * frames + p.len().min(frames)].copy_from_slice(&p[..p.len().min(frames)]);
        }
        let mut flat_out = vec![0.0f32; self.output_channels() * frames.max(1)];
        self.process_planar(&flat_in, frames, frames, &mut flat_out, frames)?;
        Ok(flat_out.chunks(frames.max(1)).map(|c| c[..frames].to_vec()).take(self.output_channels()).collect())
    }
}
