//! A bit-exact WAV file output.
//!
//! Integer PCM is written at the block's bit depth (16 → `i16`, 24 → 3-byte,
//! 32 → `i32`). Nothing is scaled, so a decode → WavSink run followed by a
//! read-back is the M1 "byte-identical output" check without hardware.
//!
//! DSD is not a WAV payload; opening in a DSD format is refused.

use crate::sink::{check_block, AudioSink, BufferLevel, Output, OutputId, SinkError};
use async_trait::async_trait;
use pm_model::{AudioBlock, AudioFormat, OutputCaps, QualityCeiling, SampleKind, Samples};
use std::fs::File;
use std::io::BufWriter;
use std::path::{Path, PathBuf};

pub struct WavFileOutput {
    path: PathBuf,
    caps: OutputCaps,
}

impl WavFileOutput {
    pub fn new(path: impl AsRef<Path>) -> Self {
        let mut caps = OutputCaps::unlimited();
        caps.dsd = None;
        caps.quality_ceiling = QualityCeiling::BitPerfect;
        Self {
            path: path.as_ref().to_path_buf(),
            caps,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[async_trait]
impl Output for WavFileOutput {
    fn id(&self) -> OutputId {
        OutputId::new("wav", &self.path.to_string_lossy())
    }

    fn name(&self) -> String {
        format!("WAV file {}", self.path.display())
    }

    fn caps(&self) -> &OutputCaps {
        &self.caps
    }

    async fn open(&self, fmt: AudioFormat) -> Result<Box<dyn AudioSink>, SinkError> {
        fmt.validate().map_err(|e| SinkError::Device(e.to_string()))?;
        if fmt.kind != SampleKind::Pcm || !self.caps.accepts(&fmt) || !matches!(fmt.bits, 16 | 24 | 32) {
            return Err(SinkError::UnsupportedFormat(self.id(), fmt));
        }
        WavSink::create(&self.path, fmt).map(|s| Box::new(s) as Box<dyn AudioSink>)
    }
}

/// The open file. Also usable directly from tests and tools.
pub struct WavSink {
    fmt: AudioFormat,
    writer: Option<hound::WavWriter<BufWriter<File>>>,
    written: u64,
}

impl WavSink {
    pub fn create(path: &Path, fmt: AudioFormat) -> Result<Self, SinkError> {
        let spec = hound::WavSpec {
            channels: fmt.channels as u16,
            sample_rate: fmt.rate_hz,
            bits_per_sample: fmt.bits as u16,
            sample_format: hound::SampleFormat::Int,
        };
        let writer = hound::WavWriter::create(path, spec).map_err(|e| SinkError::Device(e.to_string()))?;
        Ok(Self {
            fmt,
            writer: Some(writer),
            written: 0,
        })
    }

    fn finalize(&mut self) -> Result<(), SinkError> {
        if let Some(w) = self.writer.take() {
            w.finalize().map_err(|e| SinkError::Device(e.to_string()))?;
        }
        Ok(())
    }
}

impl AudioSink for WavSink {
    fn format(&self) -> AudioFormat {
        self.fmt
    }

    fn write(&mut self, block: &AudioBlock) -> Result<(), SinkError> {
        check_block(self.fmt, block)?;
        let w = self
            .writer
            .as_mut()
            .ok_or_else(|| SinkError::Device("wav sink already finalized".into()))?;
        let Samples::PcmI32(samples) = &block.samples else {
            return Err(SinkError::Device("wav sink takes integer PCM only".into()));
        };
        let fs = 1i64 << (self.fmt.bits as u32 - 1);
        for &s in samples {
            let s64 = s as i64;
            if s64 >= fs || s64 < -fs {
                return Err(SinkError::Device(format!(
                    "sample {s} exceeds {}-bit range",
                    self.fmt.bits
                )));
            }
            match self.fmt.bits {
                16 => w.write_sample(s as i16),
                _ => w.write_sample(s),
            }
            .map_err(|e| SinkError::Device(e.to_string()))?;
        }
        self.written += block.frames() as u64;
        Ok(())
    }

    fn buffer_level(&self) -> BufferLevel {
        BufferLevel {
            queued_frames: 0,
            capacity_frames: 0,
        }
    }

    fn flush(&mut self) -> Result<(), SinkError> {
        if let Some(w) = self.writer.as_mut() {
            w.flush().map_err(|e| SinkError::Device(e.to_string()))?;
        }
        Ok(())
    }

    fn drain(&mut self) -> Result<(), SinkError> {
        self.finalize()
    }

    fn frames_written(&self) -> u64 {
        self.written
    }
}

impl Drop for WavSink {
    fn drop(&mut self) {
        let _ = self.finalize();
    }
}

/// Read a WAV file back as integer samples (test and tooling helper).
pub fn read_wav(path: &Path) -> Result<AudioBlock, SinkError> {
    let mut r = hound::WavReader::open(path).map_err(|e| SinkError::Device(e.to_string()))?;
    let spec = r.spec();
    if spec.sample_format != hound::SampleFormat::Int {
        return Err(SinkError::Device("float wav not supported".into()));
    }
    let fmt = AudioFormat::pcm(spec.sample_rate, spec.bits_per_sample as u8, spec.channels as u8);
    let samples: Result<Vec<i32>, _> = r.samples::<i32>().collect();
    let samples = samples.map_err(|e| SinkError::Device(e.to_string()))?;
    Ok(AudioBlock::pcm_i32(fmt, samples))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn roundtrip_is_bit_exact_at_16_24_32() {
        for bits in [16u8, 24, 32] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join(format!("out{bits}.wav"));
            let fmt = AudioFormat::pcm(96_000, bits, 2);
            let fs = 1i64 << (bits as u32 - 1);
            let samples: Vec<i32> = vec![0, 1, -1, (fs - 1) as i32, (-fs) as i32, 12_345, -12_345, 7];
            let out = WavFileOutput::new(&path);
            {
                let mut sink = out.open(fmt).await.unwrap();
                sink.write(&AudioBlock::pcm_i32(fmt, samples.clone())).unwrap();
                sink.drain().unwrap();
                assert_eq!(sink.frames_written(), 4);
            }
            let back = read_wav(&path).unwrap();
            assert_eq!(back.format, fmt);
            assert_eq!(back.samples, Samples::PcmI32(samples));
        }
    }

    #[tokio::test]
    async fn refuses_dsd_and_out_of_range() {
        let dir = tempfile::tempdir().unwrap();
        let out = WavFileOutput::new(dir.path().join("x.wav"));
        assert!(matches!(
            out.open(AudioFormat::dsd(AudioFormat::DSD64_RATE, 2)).await,
            Err(SinkError::UnsupportedFormat(..))
        ));
        let fmt = AudioFormat::pcm(44_100, 16, 1);
        let mut sink = out.open(fmt).await.unwrap();
        assert!(sink.write(&AudioBlock::pcm_i32(fmt, vec![40_000])).is_err());
    }
}
