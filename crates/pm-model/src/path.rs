//! Signal-path tracing (handoff §6.1 and constraint 5).
//!
//! Every stage between the file and the DAC appends a [`PathStage`]. The UI
//! renders the chain; the lowest [`Quality`] in the chain is the headline.

use crate::audio::AudioFormat;
use serde::{Deserialize, Serialize};

/// How faithful one stage is to its input.
///
/// Ordered from best to worst so `min()` over a chain gives the headline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Quality {
    /// Bits in equal bits out.
    BitPerfect,
    /// Enhanced: a user-chosen DSP stage (EQ, convolution, upsampling) applied
    /// in 64-bit float. Not the source bits, but nothing lost by accident.
    Enhanced,
    /// Lossless: a transparent conversion the device required (e.g. 24-bit
    /// padded to 32-bit words, DSD packed as DoP).
    Lossless,
    /// Lossy: information was discarded (sample-rate conversion down,
    /// bit-depth reduction, lossy codec, volume scaling).
    Lossy,
}

/// One stage in the chain.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PathStage {
    /// Short machine name: `decode`, `headroom`, `loudness`, `peq`,
    /// `convolution`, `crossfeed`, `src`, `pcm_to_dsd`, `volume`, `dither`,
    /// `dop`, `output`.
    pub name: String,
    pub in_format: AudioFormat,
    pub out_format: AudioFormat,
    pub quality: Quality,
    /// Human-readable detail: the filter chosen, the reason a conversion was
    /// forced, the device name.
    pub note: Option<String>,
}

impl PathStage {
    pub fn new(
        name: impl Into<String>,
        in_format: AudioFormat,
        out_format: AudioFormat,
        quality: Quality,
    ) -> Self {
        Self {
            name: name.into(),
            in_format,
            out_format,
            quality,
            note: None,
        }
    }

    pub fn with_note(mut self, note: impl Into<String>) -> Self {
        self.note = Some(note.into());
        self
    }

    /// A stage that passes its input through untouched.
    pub fn passthrough(name: impl Into<String>, format: AudioFormat) -> Self {
        Self::new(name, format, format, Quality::BitPerfect)
    }
}

/// The whole chain for one zone.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SignalPath {
    pub stages: Vec<PathStage>,
}

impl SignalPath {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, stage: PathStage) -> &mut Self {
        self.stages.push(stage);
        self
    }

    /// The worst quality in the chain, or `BitPerfect` for an empty chain.
    pub fn headline(&self) -> Quality {
        self.stages
            .iter()
            .map(|s| s.quality)
            .max()
            .unwrap_or(Quality::BitPerfect)
    }

    pub fn is_bit_perfect(&self) -> bool {
        self.headline() == Quality::BitPerfect
    }

    /// The format leaving the last stage.
    pub fn output_format(&self) -> Option<AudioFormat> {
        self.stages.last().map(|s| s.out_format)
    }

    /// Check that each stage's input matches the previous stage's output.
    pub fn is_continuous(&self) -> bool {
        self.stages.windows(2).all(|w| w[0].out_format == w[1].in_format)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headline_is_worst_stage() {
        let f = AudioFormat::pcm(44_100, 16, 2);
        let g = AudioFormat::pcm(96_000, 24, 2);
        let mut p = SignalPath::new();
        p.push(PathStage::passthrough("decode", f));
        assert_eq!(p.headline(), Quality::BitPerfect);
        p.push(PathStage::new("src", f, g, Quality::Enhanced).with_note("sinc, linear phase"));
        assert_eq!(p.headline(), Quality::Enhanced);
        assert!(p.is_continuous());
        p.push(PathStage::new("volume", g, g, Quality::Lossy));
        assert_eq!(p.headline(), Quality::Lossy);
        assert!(!p.is_bit_perfect());
        assert_eq!(p.output_format(), Some(g));
    }

    #[test]
    fn discontinuity_detected() {
        let f = AudioFormat::pcm(44_100, 16, 2);
        let g = AudioFormat::pcm(96_000, 24, 2);
        let mut p = SignalPath::new();
        p.push(PathStage::passthrough("decode", f));
        p.push(PathStage::passthrough("output", g));
        assert!(!p.is_continuous());
    }

    #[test]
    fn quality_order() {
        assert!(Quality::BitPerfect < Quality::Enhanced);
        assert!(Quality::Enhanced < Quality::Lossless);
        assert!(Quality::Lossless < Quality::Lossy);
    }
}
