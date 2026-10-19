//! The in-memory audio block that flows from the decoder through the DSP
//! pipeline to an output.
//!
//! Sample layout rules (every crate relies on these):
//!
//! * **PCM integer** samples are interleaved by channel and stored sign-extended
//!   in an `i32`, right-justified: a 24-bit sample is in `-8_388_608..=8_388_607`,
//!   a 16-bit sample in `-32_768..=32_767`. Bits above `format.bits` are sign
//!   bits, never data. An output packs them into its wire format. Passing a
//!   block through unchanged is bit-perfect by construction.
//! * **PCM float** samples are interleaved `f64` in nominal `[-1.0, 1.0]`. This is
//!   the DSP pipeline's working format. Converting int → f64 → int without a
//!   processing stage in between is exact for ≤ 32-bit integers.
//! * **DSD** is a stream of bytes, interleaved one byte per channel
//!   (`L0 R0 L1 R1 …` for stereo), each byte holding 8 one-bit samples with the
//!   **oldest sample in the most significant bit** (the DFF and ALSA `DSD_U8`
//!   convention). DSF stores LSB-first and in channel blocks; its reader
//!   re-orders into this layout.

use crate::audio::{AudioFormat, SampleKind};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Samples {
    PcmI32(Vec<i32>),
    PcmF64(Vec<f64>),
    Dsd(Vec<u8>),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AudioBlock {
    pub format: AudioFormat,
    pub samples: Samples,
}

impl AudioBlock {
    pub fn pcm_i32(format: AudioFormat, samples: Vec<i32>) -> Self {
        debug_assert_eq!(format.kind, SampleKind::Pcm);
        Self {
            format,
            samples: Samples::PcmI32(samples),
        }
    }

    pub fn pcm_f64(format: AudioFormat, samples: Vec<f64>) -> Self {
        debug_assert_eq!(format.kind, SampleKind::Pcm);
        Self {
            format,
            samples: Samples::PcmF64(samples),
        }
    }

    pub fn dsd(format: AudioFormat, bytes: Vec<u8>) -> Self {
        debug_assert_eq!(format.kind, SampleKind::Dsd);
        Self {
            format,
            samples: Samples::Dsd(bytes),
        }
    }

    /// Number of frames (one sample per channel). For DSD a "frame" is one
    /// byte per channel, i.e. 8 bits of each channel.
    pub fn frames(&self) -> usize {
        let ch = self.format.channels.max(1) as usize;
        match &self.samples {
            Samples::PcmI32(v) => v.len() / ch,
            Samples::PcmF64(v) => v.len() / ch,
            Samples::Dsd(v) => v.len() / ch,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.frames() == 0
    }

    pub fn is_float(&self) -> bool {
        matches!(self.samples, Samples::PcmF64(_))
    }

    /// Full-scale value for the block's integer bit depth: `2^(bits-1)`.
    pub fn full_scale(&self) -> f64 {
        (1u64 << (self.format.bits as u32 - 1)) as f64
    }

    /// Convert integer PCM to `f64` in `[-1, 1)`. Exact and reversible.
    /// Float and DSD blocks are returned unchanged.
    pub fn to_f64(self) -> Self {
        let scale = 1.0 / self.full_scale();
        match self.samples {
            Samples::PcmI32(v) => {
                let out = v.into_iter().map(|s| s as f64 * scale).collect();
                Self {
                    format: self.format,
                    samples: Samples::PcmF64(out),
                }
            }
            _ => self,
        }
    }

    /// Convert float PCM back to integers at `bits`, rounding to nearest and
    /// clamping to the representable range. **Not** dithered: callers that
    /// reduce bit depth must dither first (the `dither` stage does).
    ///
    /// Calling this on a block produced by [`to_f64`](Self::to_f64) with the
    /// same `bits` reproduces the original integers exactly.
    pub fn to_i32(self, bits: u8) -> Self {
        match self.samples {
            Samples::PcmF64(v) => {
                let fs = (1u64 << (bits as u32 - 1)) as f64;
                let max = fs - 1.0;
                let min = -fs;
                let out = v
                    .into_iter()
                    .map(|s| (s * fs).round().clamp(min, max) as i32)
                    .collect();
                let format = AudioFormat { bits, ..self.format };
                Self {
                    format,
                    samples: Samples::PcmI32(out),
                }
            }
            _ => self,
        }
    }

    /// Append another block of the same format.
    pub fn append(&mut self, other: &AudioBlock) -> Result<(), BlockError> {
        if other.format != self.format {
            return Err(BlockError::FormatMismatch {
                expected: self.format,
                found: other.format,
            });
        }
        match (&mut self.samples, &other.samples) {
            (Samples::PcmI32(a), Samples::PcmI32(b)) => a.extend_from_slice(b),
            (Samples::PcmF64(a), Samples::PcmF64(b)) => a.extend_from_slice(b),
            (Samples::Dsd(a), Samples::Dsd(b)) => a.extend_from_slice(b),
            _ => return Err(BlockError::LayoutMismatch),
        }
        Ok(())
    }
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum BlockError {
    #[error("audio format mismatch: expected {expected}, found {found}")]
    FormatMismatch {
        expected: AudioFormat,
        found: AudioFormat,
    },
    #[error("sample layout mismatch (int vs float vs dsd)")]
    LayoutMismatch,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn int_float_int_is_exact_at_24_bits() {
        let f = AudioFormat::pcm(96_000, 24, 2);
        let src: Vec<i32> = vec![0, 1, -1, 8_388_607, -8_388_608, 123_456, -654_321, 42];
        let block = AudioBlock::pcm_i32(f, src.clone());
        let back = block.to_f64().to_i32(24);
        assert_eq!(back.samples, Samples::PcmI32(src));
        assert_eq!(back.format, f);
    }

    #[test]
    fn int_float_int_is_exact_at_16_and_32_bits() {
        let f16 = AudioFormat::pcm(44_100, 16, 2);
        let src16: Vec<i32> = vec![i16::MAX as i32, i16::MIN as i32, 7, -7];
        assert_eq!(
            AudioBlock::pcm_i32(f16, src16.clone())
                .to_f64()
                .to_i32(16)
                .samples,
            Samples::PcmI32(src16)
        );
        let f32b = AudioFormat::pcm(44_100, 32, 2);
        let src32: Vec<i32> = vec![i32::MAX, i32::MIN, 1, -1, 1 << 20];
        assert_eq!(
            AudioBlock::pcm_i32(f32b, src32.clone())
                .to_f64()
                .to_i32(32)
                .samples,
            Samples::PcmI32(src32)
        );
    }

    #[test]
    fn to_i32_clamps() {
        let f = AudioFormat::pcm(44_100, 16, 1);
        let b = AudioBlock::pcm_f64(f, vec![1.5, -1.5, 0.5]);
        assert_eq!(
            b.to_i32(16).samples,
            Samples::PcmI32(vec![32_767, -32_768, 16_384])
        );
    }

    #[test]
    fn frames_and_append() {
        let f = AudioFormat::pcm(44_100, 16, 2);
        let mut a = AudioBlock::pcm_i32(f, vec![1, 2, 3, 4]);
        assert_eq!(a.frames(), 2);
        a.append(&AudioBlock::pcm_i32(f, vec![5, 6])).unwrap();
        assert_eq!(a.frames(), 3);
        let err = a.append(&AudioBlock::pcm_i32(AudioFormat::pcm(48_000, 16, 2), vec![1, 2]));
        assert!(matches!(err, Err(BlockError::FormatMismatch { .. })));
        let d = AudioBlock::dsd(AudioFormat::dsd(AudioFormat::DSD64_RATE, 2), vec![0xAA; 8]);
        assert_eq!(d.frames(), 4);
    }
}
