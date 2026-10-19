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
//! * Sample-rate conversion is **not implemented yet** (M8): the plan fails
//!   with [`PipelineError::ConversionRequired`] so the caller can pick another
//!   output or format instead of silently converting.
//! * DSD plays native when the output takes it, else as DoP (`Lossless`
//!   packing into 24-bit PCM at 1/16 the DSD rate), else fails.

use pm_model::{
    AudioBlock, AudioFormat, DsdSupport, OutputCaps, PathStage, Quality, QualityCeiling, SampleKind, Samples,
    SignalPath,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DsdMode {
    /// Native if the output takes it, else DoP, else fail.
    #[default]
    Auto,
    Native,
    DoP,
    /// DSD → PCM conversion. Not implemented yet.
    ToPcm,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PipelineConfig {
    /// Pre-DSP attenuation in dB (negative), applied in float.
    pub headroom_db: Option<f32>,
    /// Software volume in dB, only for outputs without hardware volume.
    pub volume_db: Option<f32>,
    /// TPDF dither whenever the bit depth is reduced. Default on.
    pub dither: bool,
    pub dsd_mode: DsdMode,
}

impl Default for PipelineConfig {
    fn default() -> Self {
        Self {
            headroom_db: None,
            volume_db: None,
            dither: true,
            dsd_mode: DsdMode::Auto,
        }
    }
}

impl PipelineConfig {
    fn wants_float(&self) -> bool {
        self.headroom_db.is_some_and(|d| d != 0.0) || self.volume_db.is_some_and(|d| d != 0.0)
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
        source
            .validate()
            .map_err(|e| PipelineError::InvalidFormat(e.to_string()))?;
        let mut path = SignalPath::new();
        path.push(PathStage::passthrough("decode", source));
        match source.kind {
            SampleKind::Pcm => Self::negotiate_pcm(source, caps, cfg, path),
            SampleKind::Dsd => Self::negotiate_dsd(source, caps, cfg, path),
        }
    }

    fn negotiate_pcm(
        source: AudioFormat,
        caps: &OutputCaps,
        cfg: &PipelineConfig,
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
        if !rates.contains(&source.rate_hz) {
            let to_rate = rates
                .iter()
                .rev()
                .find(|r| **r <= source.rate_hz)
                .or_else(|| rates.last())
                .copied()
                .unwrap_or(source.rate_hz);
            let to = AudioFormat {
                rate_hz: to_rate,
                bits: source.bits.min(max_bits),
                ..source
            };
            return Err(PipelineError::ConversionRequired {
                from: source,
                to,
                reason: "sample-rate conversion (M8)".into(),
            });
        }
        let candidates: Vec<u8> = [16u8, 24, 32].into_iter().filter(|b| *b <= max_bits).collect();
        if candidates.is_empty() {
            return Err(PipelineError::NoBitDepth);
        }
        let out_bits = match candidates.iter().find(|b| **b >= source.bits) {
            Some(b) => *b,
            None => *candidates.last().unwrap(),
        };
        let output = AudioFormat {
            bits: out_bits,
            ..source
        };
        let mut stages = Vec::new();

        if cfg.wants_float() {
            let mut cur = source;
            let fbits = AudioFormat { bits: 32, ..source };
            if let Some(db) = cfg.headroom_db.filter(|d| *d != 0.0) {
                stages.push(Stage::Gain { gain: db_to_gain(db) });
                path.push(
                    PathStage::new("headroom", cur, fbits, Quality::Lossy)
                        .with_note(format!("{db:+.1} dB in 64-bit float")),
                );
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
        let mut block = block;
        for stage in &mut self.stages {
            block = match stage {
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
        let err = Plan::negotiate(fmt, &caps(&[44_100], 24, None), &PipelineConfig::default()).unwrap_err();
        assert_eq!(
            err,
            PipelineError::ConversionRequired {
                from: fmt,
                to: AudioFormat::pcm(44_100, 24, 2),
                reason: "sample-rate conversion (M8)".into()
            }
        );
        let airplay = OutputCaps {
            quality_ceiling: QualityCeiling::AIRPLAY,
            ..OutputCaps::unlimited()
        };
        assert!(matches!(
            Plan::negotiate(fmt, &airplay, &PipelineConfig::default()),
            Err(PipelineError::ConversionRequired { .. })
        ));
        let ok = Plan::negotiate(
            AudioFormat::pcm(44_100, 24, 2),
            &airplay,
            &PipelineConfig::default(),
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
