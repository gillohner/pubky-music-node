use crate::sink::{check_block, AudioSink, BufferLevel, Output, OutputId, SinkError};
use async_trait::async_trait;
use pm_model::{AudioBlock, AudioFormat, OutputCaps};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// Accepts every format and drops the audio. Counts frames so tests and the
/// API can observe progress.
pub struct NullOutput {
    caps: OutputCaps,
    frames: Arc<AtomicU64>,
}

impl Default for NullOutput {
    fn default() -> Self {
        Self::new()
    }
}

impl NullOutput {
    pub fn new() -> Self {
        Self { caps: OutputCaps::unlimited(), frames: Arc::new(AtomicU64::new(0)) }
    }

    /// Total frames written through every sink opened on this output.
    pub fn frames_total(&self) -> u64 {
        self.frames.load(Ordering::Relaxed)
    }
}

#[async_trait]
impl Output for NullOutput {
    fn id(&self) -> OutputId {
        OutputId::new("null", "")
    }

    fn name(&self) -> String {
        "Null output".into()
    }

    fn caps(&self) -> &OutputCaps {
        &self.caps
    }

    async fn open(&self, fmt: AudioFormat) -> Result<Box<dyn AudioSink>, SinkError> {
        fmt.validate().map_err(|e| SinkError::Device(e.to_string()))?;
        if !self.caps.accepts(&fmt) {
            return Err(SinkError::UnsupportedFormat(self.id(), fmt));
        }
        Ok(Box::new(NullSink { fmt, written: 0, total: self.frames.clone() }))
    }
}

struct NullSink {
    fmt: AudioFormat,
    written: u64,
    total: Arc<AtomicU64>,
}

impl AudioSink for NullSink {
    fn format(&self) -> AudioFormat {
        self.fmt
    }

    fn write(&mut self, block: &AudioBlock) -> Result<(), SinkError> {
        check_block(self.fmt, block)?;
        let n = block.frames() as u64;
        self.written += n;
        self.total.fetch_add(n, Ordering::Relaxed);
        Ok(())
    }

    fn buffer_level(&self) -> BufferLevel {
        BufferLevel { queued_frames: 0, capacity_frames: 0 }
    }

    fn flush(&mut self) -> Result<(), SinkError> {
        Ok(())
    }

    fn drain(&mut self) -> Result<(), SinkError> {
        Ok(())
    }

    fn frames_written(&self) -> u64 {
        self.written
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pm_model::Samples;

    #[tokio::test]
    async fn counts_frames_and_rejects_mismatch() {
        let out = NullOutput::new();
        let fmt = AudioFormat::pcm(44_100, 16, 2);
        let mut sink = out.open(fmt).await.unwrap();
        sink.write(&AudioBlock::pcm_i32(fmt, vec![0; 8])).unwrap();
        assert_eq!(sink.frames_written(), 4);
        assert_eq!(out.frames_total(), 4);
        let err = sink.write(&AudioBlock { format: fmt, samples: Samples::PcmF64(vec![0.0; 2]) });
        assert!(matches!(err, Err(SinkError::FloatNotAccepted)));
        let err = sink.write(&AudioBlock::pcm_i32(AudioFormat::pcm(48_000, 16, 2), vec![0; 2]));
        assert!(matches!(err, Err(SinkError::FormatMismatch { .. })));
    }
}
