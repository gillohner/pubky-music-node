//! Audio formats, output capabilities and volume (handoff §6.3).

use serde::{Deserialize, Serialize};
use std::fmt;

/// Whether samples are PCM words or a DSD bitstream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SampleKind {
    Pcm,
    Dsd,
}

/// One concrete stream format.
///
/// For PCM, `rate_hz` is the frame rate and `bits` the number of significant
/// bits per sample (16, 24 or 32). For DSD, `rate_hz` is the bit rate
/// (2_822_400 for DSD64) and `bits` is always 1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AudioFormat {
    pub kind: SampleKind,
    pub rate_hz: u32,
    pub bits: u8,
    pub channels: u8,
}

impl AudioFormat {
    pub const fn pcm(rate_hz: u32, bits: u8, channels: u8) -> Self {
        Self { kind: SampleKind::Pcm, rate_hz, bits, channels }
    }

    pub const fn dsd(rate_hz: u32, channels: u8) -> Self {
        Self { kind: SampleKind::Dsd, rate_hz, bits: 1, channels }
    }

    /// DSD64 is 64 × 44.1 kHz.
    pub const DSD64_RATE: u32 = 2_822_400;
    pub const DSD128_RATE: u32 = 5_644_800;
    pub const DSD256_RATE: u32 = 11_289_600;
    pub const DSD512_RATE: u32 = 22_579_200;

    pub fn is_pcm(&self) -> bool {
        self.kind == SampleKind::Pcm
    }

    pub fn is_dsd(&self) -> bool {
        self.kind == SampleKind::Dsd
    }

    /// Frames per second: the PCM rate, or for DSD the equivalent byte rate per
    /// channel (bit rate / 8), which is what buffers and clocks count in.
    pub fn frames_per_second(&self) -> u32 {
        match self.kind {
            SampleKind::Pcm => self.rate_hz,
            SampleKind::Dsd => self.rate_hz / 8,
        }
    }

    /// Multiple of DSD64 (64, 128, 256, 512), if this is DSD.
    pub fn dsd_multiple(&self) -> Option<u32> {
        if self.is_dsd() {
            Some(self.rate_hz / 44_100)
        } else {
            None
        }
    }

    /// Validate the invariants the rest of the node relies on.
    pub fn validate(&self) -> Result<(), FormatError> {
        if self.channels == 0 {
            return Err(FormatError::NoChannels);
        }
        if self.rate_hz == 0 {
            return Err(FormatError::ZeroRate);
        }
        match self.kind {
            SampleKind::Pcm => match self.bits {
                8 | 16 | 20 | 24 | 32 => Ok(()),
                other => Err(FormatError::UnsupportedBits(other)),
            },
            SampleKind::Dsd => {
                if self.bits != 1 {
                    return Err(FormatError::UnsupportedBits(self.bits));
                }
                if self.rate_hz % Self::DSD64_RATE != 0 {
                    return Err(FormatError::NotDsdMultiple(self.rate_hz));
                }
                Ok(())
            }
        }
    }
}

impl fmt::Display for AudioFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.kind {
            SampleKind::Pcm => write!(
                f,
                "PCM {}/{:.1}kHz {}ch",
                self.bits,
                self.rate_hz as f64 / 1000.0,
                self.channels
            ),
            SampleKind::Dsd => write!(
                f,
                "DSD{} {}ch",
                self.dsd_multiple().unwrap_or(0),
                self.channels
            ),
        }
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum FormatError {
    #[error("audio format has zero channels")]
    NoChannels,
    #[error("audio format has zero sample rate")]
    ZeroRate,
    #[error("unsupported bits per sample: {0}")]
    UnsupportedBits(u8),
    #[error("DSD rate {0} is not a multiple of DSD64")]
    NotDsdMultiple(u32),
}

/// How an output accepts DSD.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "lowercase")]
pub enum DsdSupport {
    /// Native DSD frames, up to this bit rate.
    Native { max_rate: u32 },
    /// DSD over PCM (DoP), up to this DSD bit rate. The PCM carrier runs at
    /// `max_rate / 16`.
    DoP { max_rate: u32 },
}

impl DsdSupport {
    pub fn max_rate(&self) -> u32 {
        match self {
            DsdSupport::Native { max_rate } | DsdSupport::DoP { max_rate } => *max_rate,
        }
    }
}

/// The kind of hardware volume control an output exposes.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum VolumeKind {
    /// Decibel scale with a step size.
    Db { min: f32, max: f32, step: f32 },
    /// 0–100 percent.
    Percent,
    /// Only up / down / mute, no absolute level.
    Incremental,
}

/// A volume request against an output.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Volume {
    Db(f32),
    Percent(u8),
    Up,
    Down,
    Mute(bool),
}

/// The honest quality ceiling of a transport (handoff §7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QualityCeiling {
    /// Anything the hardware takes, no transport-imposed limit.
    BitPerfect,
    /// Lossless but capped at a PCM format (e.g. AirPlay: 16/44.1).
    LosslessUpTo { rate_hz: u32, bits: u8 },
    /// Lossy transcoding somewhere in the path.
    Lossy,
}

impl QualityCeiling {
    pub const AIRPLAY: QualityCeiling = QualityCeiling::LosslessUpTo { rate_hz: 44_100, bits: 16 };
    pub const CAST: QualityCeiling = QualityCeiling::LosslessUpTo { rate_hz: 96_000, bits: 24 };
}

/// What an output can do (handoff §6.3).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OutputCaps {
    pub pcm_rates: Vec<u32>,
    pub max_pcm_bits: u8,
    pub max_channels: u8,
    pub dsd: Option<DsdSupport>,
    pub hw_volume: Option<VolumeKind>,
    pub standby: bool,
    pub source_switch: bool,
    pub gapless: bool,
    pub groupable: bool,
    pub fixed_latency_ms: Option<u32>,
    pub quality_ceiling: QualityCeiling,
}

impl OutputCaps {
    /// Does this output accept the format as-is (no conversion)?
    pub fn accepts(&self, fmt: &AudioFormat) -> bool {
        if fmt.channels > self.max_channels {
            return false;
        }
        match fmt.kind {
            SampleKind::Pcm => {
                self.pcm_rates.contains(&fmt.rate_hz)
                    && fmt.bits <= self.max_pcm_bits
                    && match self.quality_ceiling {
                        QualityCeiling::LosslessUpTo { rate_hz, bits } => {
                            fmt.rate_hz <= rate_hz && fmt.bits <= bits
                        }
                        _ => true,
                    }
            }
            SampleKind::Dsd => self
                .dsd
                .map(|d| fmt.rate_hz <= d.max_rate())
                .unwrap_or(false),
        }
    }

    /// Caps describing a sink that takes anything: the null output and file outputs.
    pub fn unlimited() -> Self {
        Self {
            pcm_rates: vec![
                8_000, 11_025, 16_000, 22_050, 32_000, 44_100, 48_000, 88_200, 96_000, 176_400,
                192_000, 352_800, 384_000, 705_600, 768_000,
            ],
            max_pcm_bits: 32,
            max_channels: 8,
            dsd: Some(DsdSupport::Native { max_rate: AudioFormat::DSD512_RATE }),
            hw_volume: None,
            standby: false,
            source_switch: false,
            gapless: true,
            groupable: false,
            fixed_latency_ms: Some(0),
            quality_ceiling: QualityCeiling::BitPerfect,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_and_validate() {
        let f = AudioFormat::pcm(44_100, 16, 2);
        assert_eq!(f.to_string(), "PCM 16/44.1kHz 2ch");
        assert!(f.validate().is_ok());
        let d = AudioFormat::dsd(AudioFormat::DSD256_RATE, 2);
        assert_eq!(d.to_string(), "DSD256 2ch");
        assert_eq!(d.dsd_multiple(), Some(256));
        assert!(d.validate().is_ok());
        assert_eq!(
            AudioFormat::pcm(44_100, 12, 2).validate(),
            Err(FormatError::UnsupportedBits(12))
        );
        assert_eq!(
            AudioFormat::dsd(3_000_000, 2).validate(),
            Err(FormatError::NotDsdMultiple(3_000_000))
        );
    }

    #[test]
    fn caps_accept() {
        let caps = OutputCaps {
            pcm_rates: vec![44_100, 48_000],
            max_pcm_bits: 24,
            max_channels: 2,
            dsd: Some(DsdSupport::DoP { max_rate: AudioFormat::DSD128_RATE }),
            quality_ceiling: QualityCeiling::BitPerfect,
            ..OutputCaps::unlimited()
        };
        assert!(caps.accepts(&AudioFormat::pcm(48_000, 24, 2)));
        assert!(!caps.accepts(&AudioFormat::pcm(96_000, 24, 2)));
        assert!(!caps.accepts(&AudioFormat::pcm(48_000, 32, 2)));
        assert!(caps.accepts(&AudioFormat::dsd(AudioFormat::DSD64_RATE, 2)));
        assert!(!caps.accepts(&AudioFormat::dsd(AudioFormat::DSD256_RATE, 2)));

        let airplay = OutputCaps { quality_ceiling: QualityCeiling::AIRPLAY, ..OutputCaps::unlimited() };
        assert!(airplay.accepts(&AudioFormat::pcm(44_100, 16, 2)));
        assert!(!airplay.accepts(&AudioFormat::pcm(96_000, 24, 2)));
    }

    #[test]
    fn serde_roundtrip() {
        let caps = OutputCaps::unlimited();
        let json = serde_json::to_string(&caps).unwrap();
        let back: OutputCaps = serde_json::from_str(&json).unwrap();
        assert_eq!(caps, back);
    }
}
