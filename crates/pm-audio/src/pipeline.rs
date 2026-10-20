//! Format negotiation (handoff §6.3) and the DSP stage chain (§6.2) with
//! signal-path tracing (§6.1).
//!
//! A [`Plan`] is negotiated once per track from the source format, the
//! output's [`OutputCaps`] and the user's [`PipelineConfig`]; it then
//! processes every block. Rules:
//!
//! * Same rate and bits accepted by the output → **bypass**: blocks pass
//!   through untouched and the path is `BitPerfect`.
//! * Narrower source than the output takes (24 → 32) → `pad`: left shift,
//!   `Lossless`, reversible.
//! * Wider source than the output takes (24 → 16) → `dither`: TPDF dither at
//!   the target LSB, `Lossy`.
//! * Any float stage (`headroom`, `volume`) runs the block through 64-bit
//!   float once and requantizes at the end (`Lossy` for the gain stage).
//! * Sample-rate conversion (M8 first stage) runs only when the output
//!   cannot take the source rate: an FFT sinc resampler in 64-bit float,
//!   shown as a `Lossy` `resample` stage. With `resample = false` the plan
//!   fails with [`PipelineError::ConversionRequired`] instead.
//! * DSD plays native when the output takes it, else as DoP (`Lossless`
//!   packing into 24-bit PCM at 1/16 the DSD rate), else fails.

use pm_model::{
    AudioBlock, AudioFormat, DsdSupport, OutputCaps, PathStage, Quality, QualityCeiling, SampleKind, Samples,
    SignalPath,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DsdMode {
    /// Native if the output takes it, else DoP, else fail.
    #[default]
    Auto,
    Native,
    DoP,
    /// DSD → PCM conversion. Not implemented yet.
    ToPcm,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct PipelineConfig {
    /// Pre-DSP attenuation in dB (negative), applied in float.
    pub headroom_db: Option<f32>,
    /// Software volume in dB, only for outputs without hardware volume.
    pub volume_db: Option<f32>,
    /// TPDF dither whenever the bit depth is reduced. Default on.
    pub dither: bool,
    pub dsd_mode: DsdMode,
    /// Resample (64-bit float FFT sinc, shown as a `Lossy` stage) when the
    /// output cannot take the source rate. Off → `ConversionRequired`.
    pub resample: bool,
    /// Parametric EQ bands (M8); empty keeps the path bit-perfect.
    pub eq: Vec<crate::dsp::EqBand>,
    /// ReplayGain application, when the track carries values.
    pub loudness: Loudness,
    /// Room correction: convolve with an impulse response (M8).
    pub convolution: Option<ConvolutionConfig>,
}

/// An impulse response file (WAV at the stream's rate; mono or one channel
/// per stream channel) and a make-up gain.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ConvolutionConfig {
    pub path: String,
    #[serde(default)]
    pub gain_db: f32,
    #[serde(default = "yes")]
    pub enabled: bool,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Loudness {
    #[default]
    Off,
    /// Track gain; album gain when the track has none.
    Track,
    /// Album gain; track gain when the album has none.
    Album,
}

impl Loudness {
    /// The gain to apply for this mode, if the tags provide one.
    pub fn gain_db(self, rg: Option<pm_zones::ReplayGain>) -> Option<f32> {
        let rg = rg?;
        match self {
            Loudness::Off => None,
            Loudness::Track => rg.track_db.or(rg.album_db),
            Loudness::Album => rg.album_db.or(rg.track_db),
        }
    }
}

impl Default for PipelineConfig {
    fn default() -> Self {
        Self {
            headroom_db: None,
            volume_db: None,
            dither: true,
            dsd_mode: DsdMode::Auto,
            resample: true,
            eq: Vec::new(),
            loudness: Loudness::Off,
            convolution: None,
        }
    }
}

impl PipelineConfig {
    fn wants_float(&self) -> bool {
        self.headroom_db.is_some_and(|d| d != 0.0)
            || self.volume_db.is_some_and(|d| d != 0.0)
            || self.eq.iter().any(|b| b.enabled)
            || self.convolution.as_ref().is_some_and(|c| c.enabled)
    }
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum PipelineError {
    #[error("conversion from {from} to {to} is required but not implemented yet: {reason}")]
    ConversionRequired {
        from: AudioFormat,
        to: AudioFormat,
        reason: String,
    },
    #[error("output takes no DSD and DSD to PCM conversion is not implemented yet")]
    NoDsdPath,
    #[error("DSD to PCM conversion is not implemented yet")]
    DsdToPcmUnsupported,
    #[error("DSD cannot be processed in float (headroom/volume); use hardware volume or DSD to PCM")]
    DsdNotProcessable,
    #[error("invalid source format: {0}")]
    InvalidFormat(String),
    #[error("output accepts no usable PCM bit depth")]
    NoBitDepth,
}

enum Stage {
    Resample(Box<crate::resample::Resampler>),
    Eq(Box<crate::dsp::Equalizer>),
    Convolve(Box<crate::convolve::Convolver>),
    Gain {
        gain: f64,
    },
    Pad {
        shift: u32,
    },
    Dither {
        from_bits: u8,
        to_bits: u8,
        rng: XorShift,
    },
    /// Float → integer at `bits`, with optional TPDF dither.
    Requantize {
        bits: u8,
        dither: Option<XorShift>,
    },
    DoP {
        channels: usize,
        pending: Vec<u8>,
        marker_high: bool,
    },
}

/// The negotiated chain for one source → output pair.
pub struct Plan {
    source: AudioFormat,
    output: AudioFormat,
    stages: Vec<Stage>,
    path: SignalPath,
}

impl std::fmt::Debug for Plan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Plan")
            .field("source", &self.source)
            .field("output", &self.output)
            .field("path", &self.path)
            .finish()
    }
}

impl Plan {
    pub fn negotiate(
        source: AudioFormat,
        caps: &OutputCaps,
        cfg: &PipelineConfig,
    ) -> Result<Plan, PipelineError> {
        Self::negotiate_for(source, caps, cfg, None)
    }

    /// Like [`negotiate`](Self::negotiate), with the track's ReplayGain for
    /// the `loudness` stage.
    pub fn negotiate_for(
        source: AudioFormat,
        caps: &OutputCaps,
        cfg: &PipelineConfig,
        replay_gain: Option<pm_zones::ReplayGain>,
    ) -> Result<Plan, PipelineError> {
        source
            .validate()
            .map_err(|e| PipelineError::InvalidFormat(e.to_string()))?;
        let mut path = SignalPath::new();
        path.push(PathStage::passthrough("decode", source));
        let loudness_db = cfg.loudness.gain_db(replay_gain).filter(|g| *g != 0.0);
        match source.kind {
            SampleKind::Pcm => Self::negotiate_pcm(source, caps, cfg, loudness_db, path),
            SampleKind::Dsd => Self::negotiate_dsd(source, caps, cfg, path),
        }
    }

    fn negotiate_pcm(
        source: AudioFormat,
        caps: &OutputCaps,
        cfg: &PipelineConfig,
        loudness_db: Option<f32>,
        mut path: SignalPath,
    ) -> Result<Plan, PipelineError> {
        let (ceil_rate, ceil_bits) = match caps.quality_ceiling {
            QualityCeiling::LosslessUpTo { rate_hz, bits } => (rate_hz, bits),
            _ => (u32::MAX, u8::MAX),
        };
        let max_bits = caps.max_pcm_bits.min(ceil_bits);
        let mut rates: Vec<u32> = caps
            .pcm_rates
            .iter()
            .copied()
            .filter(|r| *r <= ceil_rate)
            .collect();
        rates.sort_unstable();
        if source.channels > caps.max_channels {
            let to = AudioFormat {
                channels: caps.max_channels,
                ..source
            };
            return Err(PipelineError::ConversionRequired {
                from: source,
                to,
                reason: "channel downmix".into(),
            });
        }
        let mut resample_to: Option<u32> = None;
        if !rates.contains(&source.rate_hz) {
            let to_rate = rates
                .iter()
                .rev()
                .find(|r| **r <= source.rate_hz)
                .or_else(|| rates.last())
                .copied()
                .ok_or(PipelineError::NoBitDepth)?;
            if !cfg.resample {
                let to = AudioFormat {
                    rate_hz: to_rate,
                    bits: source.bits.min(max_bits),
                    ..source
                };
                return Err(PipelineError::ConversionRequired {
                    from: source,
                    to,
                    reason: "sample-rate conversion is disabled (resample = false)".into(),
                });
            }
            resample_to = Some(to_rate);
        }
        let candidates: Vec<u8> = [16u8, 24, 32]
            .into_iter()
            .filter(|b| *b <= max_bits && caps.takes_pcm_bits(*b))
            .collect();
        if candidates.is_empty() {
            return Err(PipelineError::NoBitDepth);
        }
        let out_bits = match candidates.iter().find(|b| **b >= source.bits) {
            Some(b) => *b,
            None => *candidates.last().unwrap(),
        };
        let output = AudioFormat {
            bits: out_bits,
            rate_hz: resample_to.unwrap_or(source.rate_hz),
            ..source
        };
        let mut stages = Vec::new();

        if cfg.wants_float() || resample_to.is_some() || loudness_db.is_some() {
            let mut cur = source;
            let fbits = AudioFormat {
                bits: 32,
                rate_hz: resample_to.unwrap_or(source.rate_hz),
                ..source
            };
            if let Some(to) = resample_to {
                stages.push(Stage::Resample(Box::new(crate::resample::Resampler::new(
                    source.rate_hz,
                    to,
                    source.channels as usize,
                )?)));
                path.push(
                    PathStage::new("resample", cur, fbits, Quality::Lossy).with_note(format!(
                        "{} → {} Hz, FFT sinc in 64-bit float",
                        source.rate_hz, to
                    )),
                );
                cur = fbits;
            }
            if let Some(db) = cfg.headroom_db.filter(|d| *d != 0.0) {
                stages.push(Stage::Gain { gain: db_to_gain(db) });
                path.push(
                    PathStage::new("headroom", cur, fbits, Quality::Lossy)
                        .with_note(format!("{db:+.1} dB in 64-bit float")),
                );
                cur = fbits;
            }
            if let Some(db) = loudness_db {
                let db = db.clamp(-30.0, 12.0);
                stages.push(Stage::Gain { gain: db_to_gain(db) });
                path.push(
                    PathStage::new("loudness", cur, fbits, Quality::Enhanced).with_note(format!(
                        "ReplayGain {} {db:+.1} dB in 64-bit float",
                        match cfg.loudness {
                            Loudness::Album => "album",
                            _ => "track",
                        }
                    )),
                );
                cur = fbits;
            }
            if cfg.eq.iter().any(|b| b.enabled) {
                let eq = crate::dsp::Equalizer::new(&cfg.eq, fbits.rate_hz, source.channels as usize)
                    .map_err(PipelineError::InvalidFormat)?;
                let note = format!("{} in 64-bit float", eq.describe());
                stages.push(Stage::Eq(Box::new(eq)));
                path.push(PathStage::new("eq", cur, fbits, Quality::Enhanced).with_note(note));
                cur = fbits;
            }
            if let Some(conv) = cfg.convolution.as_ref().filter(|c| c.enabled) {
                let ir = crate::convolve::ImpulseResponse::from_wav(std::path::Path::new(&conv.path))?;
                let c = crate::convolve::Convolver::new(
                    &ir,
                    fbits.rate_hz,
                    source.channels as usize,
                    conv.gain_db,
                )?;
                let note = format!("{} in 64-bit float", c.describe());
                stages.push(Stage::Convolve(Box::new(c)));
                path.push(PathStage::new("convolution", cur, fbits, Quality::Enhanced).with_note(note));
                cur = fbits;
            }
            if let Some(db) = cfg.volume_db.filter(|d| *d != 0.0) {
                stages.push(Stage::Gain { gain: db_to_gain(db) });
                path.push(
                    PathStage::new("volume", cur, fbits, Quality::Lossy)
                        .with_note(format!("software volume {db:+.1} dB in 64-bit float")),
                );
                cur = fbits;
            }
            let dither = cfg.dither.then(XorShift::seeded);
            let note = if dither.is_some() {
                format!("float to {out_bits}-bit, TPDF dither")
            } else {
                format!("float to {out_bits}-bit, rounded")
            };
            let q = if out_bits >= source.bits {
                Quality::Lossless
            } else {
                Quality::Lossy
            };
            stages.push(Stage::Requantize {
                bits: out_bits,
                dither,
            });
            path.push(PathStage::new("requantize", cur, output, q).with_note(note));
        } else if out_bits > source.bits {
            stages.push(Stage::Pad {
                shift: (out_bits - source.bits) as u32,
            });
            path.push(
                PathStage::new("pad", source, output, Quality::Lossless)
                    .with_note(format!("{}-bit samples in {out_bits}-bit words", source.bits)),
            );
        } else if out_bits < source.bits {
            let rng = XorShift::seeded();
            stages.push(Stage::Dither {
                from_bits: source.bits,
                to_bits: out_bits,
                rng,
            });
            let note = if cfg.dither {
                format!("{} to {out_bits}-bit, TPDF dither", source.bits)
            } else {
                format!("{} to {out_bits}-bit, rounded", source.bits)
            };
            if !cfg.dither {
                if let Some(Stage::Dither { .. }) = stages.last() {
                    stages.pop();
                    stages.push(Stage::Requantize {
                        bits: out_bits,
                        dither: None,
                    });
                }
            }
            path.push(PathStage::new("dither", source, output, Quality::Lossy).with_note(note));
        }
        path.push(PathStage::passthrough("output", output));
        Ok(Plan {
            source,
            output,
            stages,
            path,
        })
    }

    fn negotiate_dsd(
        source: AudioFormat,
        caps: &OutputCaps,
        cfg: &PipelineConfig,
        mut path: SignalPath,
    ) -> Result<Plan, PipelineError> {
        if cfg.wants_float() {
            return Err(PipelineError::DsdNotProcessable);
        }
        if cfg.dsd_mode == DsdMode::ToPcm {
            return Err(PipelineError::DsdToPcmUnsupported);
        }
        if source.channels > caps.max_channels {
            let to = AudioFormat {
                channels: caps.max_channels,
                ..source
            };
            return Err(PipelineError::ConversionRequired {
                from: source,
                to,
                reason: "channel downmix".into(),
            });
        }
        let native_ok =
            matches!(caps.dsd, Some(DsdSupport::Native { max_rate }) if source.rate_hz <= max_rate);
        let dop_pcm = AudioFormat::pcm(source.rate_hz / 16, 24, source.channels);
        let dop_ok = matches!(caps.dsd, Some(DsdSupport::DoP { max_rate }) if source.rate_hz <= max_rate)
            && caps.accepts(&dop_pcm);
        let use_native = match cfg.dsd_mode {
            DsdMode::Auto => native_ok,
            DsdMode::Native => {
                if !native_ok {
                    return Err(PipelineError::NoDsdPath);
                }
                true
            }
            DsdMode::DoP => false,
            DsdMode::ToPcm => unreachable!(),
        };
        if use_native {
            path.push(PathStage::passthrough("output", source));
            return Ok(Plan {
                source,
                output: source,
                stages: Vec::new(),
                path,
            });
        }
        if !dop_ok {
            return Err(PipelineError::NoDsdPath);
        }
        let ch = source.channels as usize;
        let stages = vec![Stage::DoP {
            channels: ch,
            pending: Vec::with_capacity(ch),
            marker_high: false,
        }];
        path.push(
            PathStage::new("dop", source, dop_pcm, Quality::Lossless).with_note(format!(
                "DSD{} packed as DoP in {} PCM",
                source.dsd_multiple().unwrap_or(0),
                dop_pcm
            )),
        );
        path.push(PathStage::passthrough("output", dop_pcm));
        Ok(Plan {
            source,
            output: dop_pcm,
            stages,
            path,
        })
    }

    pub fn source_format(&self) -> AudioFormat {
        self.source
    }

    pub fn output_format(&self) -> AudioFormat {
        self.output
    }

    pub fn signal_path(&self) -> &SignalPath {
        &self.path
    }

    pub fn is_bit_perfect(&self) -> bool {
        self.path.is_bit_perfect()
    }

    /// Run one block through the chain.
    pub fn process(&mut self, block: AudioBlock) -> Result<AudioBlock, PipelineError> {
        if block.format != self.source {
            return Err(PipelineError::InvalidFormat(format!(
                "block is {}, plan source is {}",
                block.format, self.source
            )));
        }
        self.run_from(0, block)
    }

    /// Flush stages with internal delay (resampler, convolver) at the end of
    /// a track, in order, each tail running through the stages after it;
    /// `None` when there is nothing to flush.
    pub fn finish(&mut self) -> Result<Option<AudioBlock>, PipelineError> {
        let mut out: Option<AudioBlock> = None;
        for idx in 0..self.stages.len() {
            let tail = match &mut self.stages[idx] {
                Stage::Resample(r) => {
                    let rate = r.out_rate();
                    let v = r.finish()?;
                    (!v.is_empty()).then_some((rate, v))
                }
                Stage::Convolve(c) => {
                    let v = c.finish()?;
                    (!v.is_empty()).then(|| (self.current_rate_before(idx), v))
                }
                _ => None,
            };
            let Some((rate, samples)) = tail else {
                continue;
            };
            let fmt = AudioFormat {
                rate_hz: rate,
                bits: 32,
                ..self.source
            };
            let block = self.run_from(
                idx + 1,
                AudioBlock {
                    format: fmt,
                    samples: Samples::PcmF64(samples),
                },
            )?;
            if block.is_empty() {
                continue;
            }
            match &mut out {
                None => out = Some(block),
                Some(acc) => acc
                    .append(&block)
                    .map_err(|e| PipelineError::InvalidFormat(e.to_string()))?,
            }
        }
        Ok(out)
    }

    /// The sample rate of the stream entering stage `idx` (after any
    /// resampler before it).
    fn current_rate_before(&self, idx: usize) -> u32 {
        self.stages[..idx]
            .iter()
            .rev()
            .find_map(|s| match s {
                Stage::Resample(r) => Some(r.out_rate()),
                _ => None,
            })
            .unwrap_or(self.source.rate_hz)
    }

    fn run_from(&mut self, start: usize, block: AudioBlock) -> Result<AudioBlock, PipelineError> {
        let mut block = block;
        for stage in self.stages.iter_mut().skip(start) {
            block = match stage {
                Stage::Resample(r) => {
                    let b = block.to_f64();
                    match b.samples {
                        Samples::PcmF64(v) => {
                            let out = r.push(&v)?;
                            AudioBlock {
                                format: AudioFormat {
                                    rate_hz: r.out_rate(),
                                    ..b.format
                                },
                                samples: Samples::PcmF64(out),
                            }
                        }
                        _ => return Err(PipelineError::InvalidFormat("resample expects PCM".into())),
                    }
                }
                Stage::Convolve(c) => {
                    let b = block.to_f64();
                    match b.samples {
                        Samples::PcmF64(v) => AudioBlock {
                            format: b.format,
                            samples: Samples::PcmF64(c.push(&v)?),
                        },
                        _ => return Err(PipelineError::InvalidFormat("convolution expects PCM".into())),
                    }
                }
                Stage::Eq(eq) => {
                    let mut b = block.to_f64();
                    if let Samples::PcmF64(v) = &mut b.samples {
                        eq.process(v);
                    }
                    b
                }
                Stage::Gain { gain } => {
                    let mut b = block.to_f64();
                    if let Samples::PcmF64(v) = &mut b.samples {
                        for s in v.iter_mut() {
                            *s *= *gain;
                        }
                    }
                    b
                }
                Stage::Pad { shift } => {
                    let shift = *shift;
                    match block.samples {
                        Samples::PcmI32(mut v) => {
                            for s in &mut v {
                                *s <<= shift;
                            }
                            AudioBlock::pcm_i32(
                                AudioFormat {
                                    bits: block.format.bits + shift as u8,
                                    ..block.format
                                },
                                v,
                            )
                        }
                        _ => return Err(PipelineError::InvalidFormat("pad expects integer PCM".into())),
                    }
                }
                Stage::Dither {
                    from_bits,
                    to_bits,
                    rng,
                } => {
                    let d = (*from_bits - *to_bits) as u32;
                    let div = (1u64 << d) as f64;
                    let fs = 1i64 << (*to_bits as u32 - 1);
                    match block.samples {
                        Samples::PcmI32(mut v) => {
                            for s in &mut v {
                                let x = *s as f64 / div + rng.tpdf();
                                *s = (x.round() as i64).clamp(-fs, fs - 1) as i32;
                            }
                            AudioBlock::pcm_i32(
                                AudioFormat {
                                    bits: *to_bits,
                                    ..block.format
                                },
                                v,
                            )
                        }
                        _ => return Err(PipelineError::InvalidFormat("dither expects integer PCM".into())),
                    }
                }
                Stage::Requantize { bits, dither } => {
                    let b = block.to_f64();
                    let fs = (1u64 << (*bits as u32 - 1)) as f64;
                    let max = fs - 1.0;
                    match b.samples {
                        Samples::PcmF64(v) => {
                            let out: Vec<i32> = v
                                .into_iter()
                                .map(|s| {
                                    let x = s * fs + dither.as_mut().map(|r| r.tpdf()).unwrap_or(0.0);
                                    x.round().clamp(-fs, max) as i32
                                })
                                .collect();
                            AudioBlock::pcm_i32(
                                AudioFormat {
                                    bits: *bits,
                                    ..b.format
                                },
                                out,
                            )
                        }
                        _ => return Err(PipelineError::InvalidFormat("requantize expects float".into())),
                    }
                }
                Stage::DoP {
                    channels,
                    pending,
                    marker_high,
                } => {
                    let Samples::Dsd(bytes) = &block.samples else {
                        return Err(PipelineError::InvalidFormat("dop expects DSD".into()));
                    };
                    let ch = *channels;
                    let mut frames: Vec<u8> = Vec::with_capacity(pending.len() + bytes.len());
                    frames.extend_from_slice(pending);
                    frames.extend_from_slice(bytes);
                    let n_frames = frames.len() / ch;
                    let pcm_frames = n_frames / 2;
                    let mut out = Vec::with_capacity(pcm_frames * ch);
                    for f in 0..pcm_frames {
                        let marker: u32 = if *marker_high { 0xFA } else { 0x05 };
                        *marker_high = !*marker_high;
                        for c in 0..ch {
                            let b0 = frames[(2 * f) * ch + c] as u32;
                            let b1 = frames[(2 * f + 1) * ch + c] as u32;
                            let w = (marker << 16) | (b0 << 8) | b1;
                            out.push(((w << 8) as i32) >> 8);
                        }
                    }
                    pending.clear();
                    pending.extend_from_slice(&frames[pcm_frames * 2 * ch..]);
                    AudioBlock::pcm_i32(self.output, out)
                }
            };
        }
        Ok(block)
    }
}

fn db_to_gain(db: f32) -> f64 {
    10f64.powf(db as f64 / 20.0)
}

/// Small deterministic PRNG for dither; reproducible output in tests.
struct XorShift(u64);

impl XorShift {
    fn seeded() -> Self {
        XorShift(0x9E37_79B9_7F4A_7C15)
    }

    fn next_f64(&mut self) -> f64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        (x >> 11) as f64 / (1u64 << 53) as f64
    }

    /// Triangular PDF noise in (-1, 1) LSB.
    fn tpdf(&mut self) -> f64 {
        self.next_f64() - self.next_f64()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps(rates: &[u32], bits: u8, dsd: Option<DsdSupport>) -> OutputCaps {
        OutputCaps {
            pcm_rates: rates.to_vec(),
            max_pcm_bits: bits,
            dsd,
            ..OutputCaps::unlimited()
        }
    }

    fn random_block(fmt: AudioFormat, frames: usize) -> AudioBlock {
        let mut rng = XorShift(42);
        let fs = 1i64 << (fmt.bits as u32 - 1);
        let v: Vec<i32> = (0..frames * fmt.channels as usize)
            .map(|i| match i {
                0 => (fs - 1) as i32,
                1 => (-fs) as i32,
                _ => ((rng.next_f64() * 2.0 - 1.0) * fs as f64)
                    .floor()
                    .clamp(-(fs as f64), (fs - 1) as f64) as i32,
            })
            .collect();
        AudioBlock::pcm_i32(fmt, v)
    }

    #[test]
    fn bypass_is_bit_exact() {
        for bits in [16u8, 24, 32] {
            let fmt = AudioFormat::pcm(96_000, bits, 2);
            let mut plan =
                Plan::negotiate(fmt, &OutputCaps::unlimited(), &PipelineConfig::default()).unwrap();
            assert!(plan.is_bit_perfect());
            assert_eq!(plan.output_format(), fmt);
            assert_eq!(plan.signal_path().stages.len(), 2);
            let b = random_block(fmt, 1000);
            let out = plan.process(b.clone()).unwrap();
            assert_eq!(out, b);
        }
    }

    #[test]
    fn pad_24_to_32_is_reversible() {
        let fmt = AudioFormat::pcm(44_100, 24, 2);
        let c = OutputCaps {
            pcm_rates: vec![44_100],
            max_pcm_bits: 32,
            ..OutputCaps::unlimited()
        };
        // Force "32 only" by making the sink reject 24: emulate with a ceiling-free caps where
        // 24 is accepted too, so instead test the explicit Pad stage via a 20-bit source.
        let mut plan =
            Plan::negotiate(AudioFormat::pcm(44_100, 20, 2), &c, &PipelineConfig::default()).unwrap();
        assert_eq!(plan.output_format().bits, 24);
        assert_eq!(plan.signal_path().headline(), Quality::Lossless);
        assert_eq!(plan.signal_path().stages[1].name, "pad");
        let b = random_block(AudioFormat::pcm(44_100, 20, 2), 500);
        let out = plan.process(b.clone()).unwrap();
        if let (Samples::PcmI32(o), Samples::PcmI32(i)) = (&out.samples, &b.samples) {
            assert!(o.iter().zip(i).all(|(o, i)| *o == (*i << 4)));
        } else {
            panic!();
        }
        let _ = fmt;
    }

    #[test]
    fn pad_24_into_32_bit_only_device() {
        // FiiO KA17: S16_LE and S32_LE, no 24-bit format.
        let c = OutputCaps {
            pcm_rates: vec![44_100, 96_000],
            pcm_bits: vec![16, 32],
            ..OutputCaps::unlimited()
        };
        let src = AudioFormat::pcm(96_000, 24, 2);
        let mut plan = Plan::negotiate(src, &c, &PipelineConfig::default()).unwrap();
        assert_eq!(plan.output_format().bits, 32);
        assert_eq!(plan.signal_path().headline(), Quality::Lossless);
        let names: Vec<_> = plan
            .signal_path()
            .stages
            .iter()
            .map(|s| s.name.as_str())
            .collect();
        assert_eq!(names, ["decode", "pad", "output"]);
        let b = random_block(src, 500);
        let out = plan.process(b.clone()).unwrap();
        if let (Samples::PcmI32(o), Samples::PcmI32(i)) = (&out.samples, &b.samples) {
            assert!(o.iter().zip(i).all(|(o, i)| *o == (*i << 8)));
        } else {
            panic!();
        }

        // 16-bit stays bit-perfect on the same device.
        let plan = Plan::negotiate(AudioFormat::pcm(44_100, 16, 2), &c, &PipelineConfig::default()).unwrap();
        assert!(plan.is_bit_perfect());
    }

    #[test]
    fn resamples_when_the_output_cannot_take_the_rate() {
        let c = OutputCaps {
            pcm_rates: vec![44_100, 48_000],
            ..OutputCaps::unlimited()
        };
        let src = AudioFormat::pcm(96_000, 24, 2);
        let mut plan = Plan::negotiate(src, &c, &PipelineConfig::default()).unwrap();
        assert_eq!(plan.output_format(), AudioFormat::pcm(48_000, 24, 2));
        assert_eq!(plan.signal_path().headline(), Quality::Lossy);
        let names: Vec<_> = plan
            .signal_path()
            .stages
            .iter()
            .map(|s| s.name.as_str())
            .collect();
        assert_eq!(names, ["decode", "resample", "requantize", "output"]);
        let mut frames_out = 0;
        for _ in 0..24 {
            let out = plan.process(random_block(src, 4000)).unwrap();
            assert!(out.is_empty() || out.format == AudioFormat::pcm(48_000, 24, 2));
            frames_out += out.frames();
        }
        if let Some(tail) = plan.finish().unwrap() {
            assert_eq!(tail.format, AudioFormat::pcm(48_000, 24, 2));
            frames_out += tail.frames();
        }
        assert_eq!(frames_out, 24 * 4000 / 2);

        let off = PipelineConfig {
            resample: false,
            ..Default::default()
        };
        assert!(matches!(
            Plan::negotiate(src, &c, &off),
            Err(PipelineError::ConversionRequired { .. })
        ));
        // A supported rate never resamples.
        let plan = Plan::negotiate(AudioFormat::pcm(48_000, 24, 2), &c, &PipelineConfig::default()).unwrap();
        assert!(plan.is_bit_perfect());
    }

    #[test]
    fn eq_is_an_enhanced_float_stage_and_absent_by_default() {
        let src = AudioFormat::pcm(48_000, 24, 2);
        let cfg = PipelineConfig {
            eq: vec![crate::dsp::EqBand::peaking(100.0, -3.0, 1.0)],
            ..Default::default()
        };
        let mut plan = Plan::negotiate(src, &OutputCaps::unlimited(), &cfg).unwrap();
        let names: Vec<_> = plan
            .signal_path()
            .stages
            .iter()
            .map(|s| s.name.as_str())
            .collect();
        assert_eq!(names, ["decode", "eq", "requantize", "output"]);
        let eq_stage = &plan.signal_path().stages[1];
        assert_eq!(eq_stage.quality, Quality::Enhanced);
        assert!(eq_stage.note.as_deref().unwrap().contains("peak 100 Hz -3.0 dB"));
        assert!(!plan.is_bit_perfect());
        let out = plan.process(random_block(src, 1000)).unwrap();
        assert_eq!(out.format, src);
        // A disabled band changes nothing.
        let mut off = crate::dsp::EqBand::peaking(100.0, -3.0, 1.0);
        off.enabled = false;
        let plan = Plan::negotiate(
            src,
            &OutputCaps::unlimited(),
            &PipelineConfig {
                eq: vec![off],
                ..Default::default()
            },
        )
        .unwrap();
        assert!(plan.is_bit_perfect());
        // Invalid bands are refused at negotiation.
        let bad = PipelineConfig {
            eq: vec![crate::dsp::EqBand::peaking(40_000.0, 1.0, 1.0)],
            ..Default::default()
        };
        assert!(Plan::negotiate(src, &OutputCaps::unlimited(), &bad).is_err());
    }

    #[test]
    fn loudness_applies_replaygain_only_when_asked() {
        let src = AudioFormat::pcm(44_100, 16, 2);
        let rg = pm_zones::ReplayGain {
            track_db: Some(-7.5),
            album_db: Some(-6.0),
        };
        let plan = Plan::negotiate_for(
            src,
            &OutputCaps::unlimited(),
            &PipelineConfig::default(),
            Some(rg),
        )
        .unwrap();
        assert!(plan.is_bit_perfect(), "off by default");
        let cfg = PipelineConfig {
            loudness: Loudness::Album,
            ..Default::default()
        };
        let mut plan = Plan::negotiate_for(src, &OutputCaps::unlimited(), &cfg, Some(rg)).unwrap();
        let st = &plan.signal_path().stages[1];
        assert_eq!(st.name, "loudness");
        assert_eq!(st.quality, Quality::Enhanced);
        assert!(st.note.as_deref().unwrap().contains("album -6.0 dB"));
        // A full-scale sample comes out ~6 dB lower.
        let out = plan
            .process(AudioBlock::pcm_i32(src, vec![32767, 32767]))
            .unwrap();
        if let Samples::PcmI32(v) = out.samples {
            assert!((v[0] as f64 / 32767.0 - 0.501).abs() < 0.01, "{v:?}");
        } else {
            panic!();
        }
        // No tags → no stage, still bit-perfect.
        let plan = Plan::negotiate_for(src, &OutputCaps::unlimited(), &cfg, None).unwrap();
        assert!(plan.is_bit_perfect());
    }

    #[test]
    fn convolution_with_a_delta_is_transparent_and_flushes_its_tail() {
        let dir = tempfile::tempdir().unwrap();
        let ir_path = dir.path().join("delta.wav");
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 48_000,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Int,
        };
        let mut w = hound::WavWriter::create(&ir_path, spec).unwrap();
        w.write_sample(i32::MAX).unwrap();
        w.write_sample(0).unwrap();
        w.write_sample(0).unwrap();
        w.finalize().unwrap();
        let src = AudioFormat::pcm(48_000, 24, 2);
        let cfg = PipelineConfig {
            convolution: Some(ConvolutionConfig {
                path: ir_path.to_string_lossy().into_owned(),
                gain_db: 0.0,
                enabled: true,
            }),
            dither: false,
            ..Default::default()
        };
        let mut plan = Plan::negotiate(src, &OutputCaps::unlimited(), &cfg).unwrap();
        let names: Vec<_> = plan
            .signal_path()
            .stages
            .iter()
            .map(|s| s.name.as_str())
            .collect();
        assert_eq!(names, ["decode", "convolution", "requantize", "output"]);
        assert_eq!(plan.signal_path().stages[1].quality, Quality::Enhanced);
        let input = random_block(src, 10_000);
        let mut out = plan.process(input.clone()).unwrap();
        if let Some(tail) = plan.finish().unwrap() {
            out.append(&tail).unwrap();
        }
        // Delta IR of 3 taps: same samples back, plus 2 frames of (near) silence.
        assert_eq!(out.frames(), 10_002);
        if let (Samples::PcmI32(o), Samples::PcmI32(i)) = (&out.samples, &input.samples) {
            let mismatches = o.iter().zip(i).filter(|(a, b)| (**a - **b).abs() > 1).count();
            assert_eq!(mismatches, 0, "delta convolution must be transparent to the LSB");
        } else {
            panic!();
        }
        // A missing IR file fails negotiation, not playback.
        let bad = PipelineConfig {
            convolution: Some(ConvolutionConfig {
                path: dir.path().join("nope.wav").to_string_lossy().into_owned(),
                gain_db: 0.0,
                enabled: true,
            }),
            ..Default::default()
        };
        assert!(Plan::negotiate(src, &OutputCaps::unlimited(), &bad).is_err());
    }

    #[test]
    fn dither_down_to_16() {
        let fmt = AudioFormat::pcm(44_100, 24, 2);
        let c = caps(&[44_100], 16, None);
        let mut plan = Plan::negotiate(fmt, &c, &PipelineConfig::default()).unwrap();
        assert_eq!(plan.output_format(), AudioFormat::pcm(44_100, 16, 2));
        assert_eq!(plan.signal_path().headline(), Quality::Lossy);
        assert_eq!(plan.signal_path().stages[1].name, "dither");
        // Silence stays within ±1 LSB.
        let out = plan.process(AudioBlock::pcm_i32(fmt, vec![0; 2000])).unwrap();
        if let Samples::PcmI32(v) = &out.samples {
            assert!(v.iter().all(|s| (-1..=1).contains(s)));
            assert!(v.iter().any(|s| *s != 0), "dither should add noise");
        }
        // Full scale never exceeds range.
        let out = plan.process(random_block(fmt, 2000)).unwrap();
        if let Samples::PcmI32(v) = &out.samples {
            assert!(v.iter().all(|s| (-32_768..=32_767).contains(s)));
        }
        assert!(plan.signal_path().is_continuous());
    }

    #[test]
    fn volume_runs_in_float_and_is_lossy() {
        let fmt = AudioFormat::pcm(48_000, 24, 2);
        let cfg = PipelineConfig {
            volume_db: Some(-6.0),
            ..Default::default()
        };
        let mut plan = Plan::negotiate(fmt, &OutputCaps::unlimited(), &cfg).unwrap();
        assert_eq!(plan.signal_path().headline(), Quality::Lossy);
        let names: Vec<_> = plan
            .signal_path()
            .stages
            .iter()
            .map(|s| s.name.as_str())
            .collect();
        assert_eq!(names, ["decode", "volume", "requantize", "output"]);
        let out = plan
            .process(AudioBlock::pcm_i32(fmt, vec![1_000_000, -1_000_000]))
            .unwrap();
        if let Samples::PcmI32(v) = &out.samples {
            let g = db_to_gain(-6.0);
            assert!((v[0] as f64 - 1_000_000.0 * g).abs() <= 1.5);
            assert!((v[1] as f64 + 1_000_000.0 * g).abs() <= 1.5);
        }
        assert_eq!(out.format, fmt);
        assert!(plan.signal_path().is_continuous());
    }

    #[test]
    fn conversion_required_for_unsupported_rate() {
        let fmt = AudioFormat::pcm(96_000, 24, 2);
        let err = Plan::negotiate(
            fmt,
            &caps(&[44_100], 24, None),
            &PipelineConfig {
                resample: false,
                ..Default::default()
            },
        )
        .unwrap_err();
        assert_eq!(
            err,
            PipelineError::ConversionRequired {
                from: fmt,
                to: AudioFormat::pcm(44_100, 24, 2),
                reason: "sample-rate conversion is disabled (resample = false)".into()
            }
        );
        let airplay = OutputCaps {
            quality_ceiling: QualityCeiling::AIRPLAY,
            ..OutputCaps::unlimited()
        };
        assert!(matches!(
            Plan::negotiate(
                fmt,
                &airplay,
                &PipelineConfig {
                    resample: false,
                    ..Default::default()
                }
            ),
            Err(PipelineError::ConversionRequired { .. })
        ));
        let ok = Plan::negotiate(
            AudioFormat::pcm(44_100, 24, 2),
            &airplay,
            &PipelineConfig {
                resample: false,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(ok.output_format().bits, 16);
    }

    #[test]
    fn dsd_native_dop_and_failure() {
        let dsd = AudioFormat::dsd(AudioFormat::DSD128_RATE, 2);
        let native = caps(
            &[44_100, 352_800],
            32,
            Some(DsdSupport::Native {
                max_rate: AudioFormat::DSD256_RATE,
            }),
        );
        let plan = Plan::negotiate(dsd, &native, &PipelineConfig::default()).unwrap();
        assert!(plan.is_bit_perfect());
        assert_eq!(plan.output_format(), dsd);

        let dop = caps(
            &[352_800],
            24,
            Some(DsdSupport::DoP {
                max_rate: AudioFormat::DSD128_RATE,
            }),
        );
        let mut plan = Plan::negotiate(dsd, &dop, &PipelineConfig::default()).unwrap();
        assert_eq!(plan.output_format(), AudioFormat::pcm(352_800, 24, 2));
        assert_eq!(plan.signal_path().headline(), Quality::Lossless);
        // Hand-computed packing: frames (L,R) = (0x12,0x34),(0x56,0x78) → L = 05 12 56, R = 05 34 78,
        // then next PCM frame marker FA.
        let block = AudioBlock::dsd(dsd, vec![0x12, 0x34, 0x56, 0x78, 0xAB, 0xCD]);
        let out = plan.process(block).unwrap();
        assert_eq!(out.samples, Samples::PcmI32(vec![0x05_1256, 0x05_3478]));
        // The odd frame (0xAB,0xCD) is pending; feed one more frame.
        let out = plan.process(AudioBlock::dsd(dsd, vec![0xEF, 0x01])).unwrap();
        let expect_l = ((0xFA_ABEFu32 << 8) as i32) >> 8;
        let expect_r = ((0xFA_CD01u32 << 8) as i32) >> 8;
        assert_eq!(out.samples, Samples::PcmI32(vec![expect_l, expect_r]));
        assert!(expect_l < 0, "0xFA marker words are negative 24-bit values");

        let none = caps(&[44_100], 24, None);
        assert_eq!(
            Plan::negotiate(dsd, &none, &PipelineConfig::default()).unwrap_err(),
            PipelineError::NoDsdPath
        );
        let cfg = PipelineConfig {
            dsd_mode: DsdMode::ToPcm,
            ..Default::default()
        };
        assert_eq!(
            Plan::negotiate(dsd, &native, &cfg).unwrap_err(),
            PipelineError::DsdToPcmUnsupported
        );
        let cfg = PipelineConfig {
            volume_db: Some(-3.0),
            ..Default::default()
        };
        assert_eq!(
            Plan::negotiate(dsd, &native, &cfg).unwrap_err(),
            PipelineError::DsdNotProcessable
        );
        let cfg = PipelineConfig {
            dsd_mode: DsdMode::Native,
            ..Default::default()
        };
        assert_eq!(
            Plan::negotiate(dsd, &dop, &cfg).unwrap_err(),
            PipelineError::NoDsdPath
        );
    }
}
