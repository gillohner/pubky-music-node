//! Decode → negotiate → sink, blocking.
//!
//! [`play_to_sink`] is the whole audio path of M1 in one function and is the
//! basis of the "byte-identical output against the source PCM" check: a
//! source decoded through it into a [`pm_zones::outputs::WavFileOutput`] and
//! read back must equal the source samples exactly, with a `BitPerfect`
//! signal path. The test in this module does exactly that without hardware.
//! [`play_file`] adds the async `Output::open` step.

use crate::decode::Decoder;
use crate::dsd::DsdReader;
use crate::error::AudioError;
use crate::pipeline::{PipelineConfig, Plan};
use crate::probe::is_dsd_file;
use pm_model::{AudioBlock, AudioFormat, SignalPath};
use pm_zones::{AudioSink, Output};
use std::path::Path;

/// One decoded source, PCM or DSD.
pub enum Source {
    Pcm(Decoder),
    Dsd(DsdReader),
}

impl Source {
    pub fn open(path: &Path) -> Result<Self, AudioError> {
        if is_dsd_file(path)? {
            Ok(Source::Dsd(DsdReader::open(path)?))
        } else {
            Ok(Source::Pcm(Decoder::open(path)?))
        }
    }

    /// A PCM source over any media source (streams); see [`Decoder::from_media_source`].
    pub fn from_media_source(
        source: Box<dyn symphonia::core::io::MediaSource>,
        ext: Option<&str>,
        name: &str,
    ) -> Result<Self, AudioError> {
        Ok(Source::Pcm(Decoder::from_media_source(source, ext, name)?))
    }

    pub fn format(&self) -> AudioFormat {
        match self {
            Source::Pcm(d) => d.format(),
            Source::Dsd(d) => d.format(),
        }
    }

    pub fn duration_ms(&self) -> Option<u64> {
        match self {
            Source::Pcm(d) => d.duration_ms(),
            Source::Dsd(d) => d.duration_ms(),
        }
    }

    /// Lossy codec behind this source (MP3, AAC, Vorbis).
    pub fn is_lossy(&self) -> bool {
        match self {
            Source::Pcm(d) => d.is_lossy(),
            Source::Dsd(_) => false,
        }
    }

    /// The codec as people know it (`FLAC`, `MP3`, `DSD`…), for the player's quality label.
    pub fn codec_name(&self) -> String {
        match self {
            Source::Dsd(_) => "DSD".into(),
            Source::Pcm(d) => match d.container() {
                "flac" | "alac" | "mp3" | "aac" | "wav" => d.container().to_uppercase(),
                "aiff" => "AIFF".into(),
                "vorbis" => "Vorbis".into(),
                "opus" => "Opus".into(),
                other => other.to_uppercase(),
            },
        }
    }

    /// What this decoder knows of its stream (no service: the opener or engine adds it).
    pub fn stream_info(&self) -> pm_zones::StreamInfo {
        pm_zones::StreamInfo {
            codec: self.codec_name(),
            lossless: !self.is_lossy(),
            ..Default::default()
        }
    }

    pub fn next_block(&mut self) -> Result<Option<AudioBlock>, AudioError> {
        match self {
            Source::Pcm(d) => d.next_block(),
            Source::Dsd(d) => d.next_block(),
        }
    }

    /// Accurate seek: the next block starts at `ms`.
    pub fn seek_ms(&mut self, ms: u64) -> Result<(), AudioError> {
        match self {
            Source::Pcm(d) => d.seek_ms(ms),
            Source::Dsd(d) => d.seek_ms(ms),
        }
    }

    /// Codec verification result, if any (FLAC MD5).
    pub fn finalize(&mut self) -> Option<bool> {
        match self {
            Source::Pcm(d) => d.finalize(),
            Source::Dsd(_) => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct PlayReport {
    /// Frames pushed to the sink, in the sink's format.
    pub frames: u64,
    pub source_format: AudioFormat,
    pub output_format: AudioFormat,
    pub signal_path: SignalPath,
    /// FLAC MD5 verification, when the codec provides one.
    pub verify_ok: Option<bool>,
}

/// Negotiate a plan for `path` against `caps` without opening anything.
pub fn plan_for(
    path: &Path,
    caps: &pm_model::OutputCaps,
    cfg: &PipelineConfig,
) -> Result<(Source, Plan), AudioError> {
    let src = Source::open(path)?;
    let plan = Plan::negotiate(src.format(), caps, cfg)?;
    Ok((src, plan))
}

/// Push every block of `source` through `plan` into `sink`, then drain.
pub fn play_to_sink(
    mut source: Source,
    mut plan: Plan,
    sink: &mut dyn AudioSink,
) -> Result<PlayReport, AudioError> {
    if sink.format() != plan.output_format() {
        return Err(AudioError::UnsupportedFormat(sink.format()));
    }
    let mut frames = 0u64;
    while let Some(block) = source.next_block()? {
        let out = plan.process(block)?;
        if out.is_empty() {
            continue;
        }
        frames += out.frames() as u64;
        sink.write(&out)?;
    }
    if let Some(tail) = plan.finish()? {
        frames += tail.frames() as u64;
        sink.write(&tail)?;
    }
    sink.drain()?;
    let verify_ok = source.finalize();
    Ok(PlayReport {
        frames,
        source_format: plan.source_format(),
        output_format: plan.output_format(),
        signal_path: plan.signal_path().clone(),
        verify_ok,
    })
}

/// Open `output` in the negotiated format and play `path` into it.
/// Decoding blocks the current task; call from a blocking context.
pub async fn play_file(
    path: &Path,
    output: &dyn Output,
    cfg: &PipelineConfig,
) -> Result<PlayReport, AudioError> {
    let (source, plan) = plan_for(path, output.caps(), cfg)?;
    let mut sink = output.open(plan.output_format()).await?;
    play_to_sink(source, plan, sink.as_mut())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pm_model::Samples;
    use pm_zones::outputs::{read_wav, NullOutput, WavFileOutput};

    fn write_pattern_wav(path: &Path, bits: u16, rate: u32, frames: usize) -> Vec<i32> {
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: rate,
            bits_per_sample: bits,
            sample_format: hound::SampleFormat::Int,
        };
        let mut w = hound::WavWriter::create(path, spec).unwrap();
        let fs = 1i64 << (bits - 1);
        let mut v = Vec::new();
        for i in 0..frames * 2 {
            let s = match i {
                0 => fs - 1,
                1 => -fs,
                _ => ((i as i64 * 7919) % (2 * fs)) - fs,
            } as i32;
            v.push(s);
            if bits == 16 {
                w.write_sample(s as i16).unwrap();
            } else {
                w.write_sample(s).unwrap();
            }
        }
        w.finalize().unwrap();
        v
    }

    #[tokio::test]
    async fn wav_to_wav_is_byte_identical() {
        for bits in [16u16, 24, 32] {
            let dir = tempfile::tempdir().unwrap();
            let src = dir.path().join("src.wav");
            let dst = dir.path().join("dst.wav");
            let expect = write_pattern_wav(&src, bits, 96_000, 3000);
            let out = WavFileOutput::new(&dst);
            let report = play_file(&src, &out, &PipelineConfig::default()).await.unwrap();
            assert!(
                report.signal_path.is_bit_perfect(),
                "{bits}: {:?}",
                report.signal_path
            );
            assert_eq!(report.frames, 3000);
            assert_eq!(report.source_format, AudioFormat::pcm(96_000, bits as u8, 2));
            let back = read_wav(&dst).unwrap();
            assert_eq!(back.format, AudioFormat::pcm(96_000, bits as u8, 2));
            assert_eq!(back.samples, Samples::PcmI32(expect));
            // And the raw files carry identical PCM payloads.
            let a = std::fs::read(&src).unwrap();
            let b = std::fs::read(&dst).unwrap();
            let payload = |f: &[u8]| {
                let i = f.windows(4).position(|w| w == b"data").unwrap();
                f[i + 8..].to_vec()
            };
            assert_eq!(payload(&a), payload(&b));
        }
    }

    #[tokio::test]
    async fn wav_to_null_counts_frames() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.wav");
        write_pattern_wav(&src, 24, 44_100, 1234);
        let out = NullOutput::new();
        let report = play_file(&src, &out, &PipelineConfig::default()).await.unwrap();
        assert_eq!(report.frames, 1234);
        assert_eq!(out.frames_total(), 1234);
        assert!(report.signal_path.is_bit_perfect());
    }

    #[tokio::test]
    async fn conversion_required_is_an_error_not_a_silent_convert() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.wav");
        write_pattern_wav(&src, 24, 96_000, 100);
        let caps = pm_model::OutputCaps {
            pcm_rates: vec![44_100],
            ..pm_model::OutputCaps::unlimited()
        };
        let cfg = PipelineConfig {
            resample: false,
            ..Default::default()
        };
        let err = plan_for(&src, &caps, &cfg).err().unwrap();
        assert!(
            matches!(
                err,
                AudioError::Pipeline(crate::pipeline::PipelineError::ConversionRequired { .. })
            ),
            "{err}"
        );
    }
}
