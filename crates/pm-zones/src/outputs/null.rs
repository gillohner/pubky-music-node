use crate::sink::{check_block, AudioSink, BufferLevel, Output, OutputId, SinkError};
use async_trait::async_trait;
use pm_model::{AudioBlock, AudioFormat, OutputCaps};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How far a real-time null sink may run ahead of the wall clock.
const AHEAD: Duration = Duration::from_millis(250);

/// Accepts every format and drops the audio. Counts frames so tests and the
/// API can observe progress. [`NullOutput::realtime`] plays at the speed of
/// the music, like a DAC would (soak tests, mock rooms); the default drops
/// audio as fast as it is decoded.
pub struct NullOutput {
    caps: OutputCaps,
    frames: Arc<AtomicU64>,
    opens: AtomicU64,
    realtime: bool,
}

impl Default for NullOutput {
    fn default() -> Self {
        Self::new()
    }
}

impl NullOutput {
    pub fn new() -> Self {
        Self {
            caps: OutputCaps::unlimited(),
            frames: Arc::new(AtomicU64::new(0)),
            opens: AtomicU64::new(0),
            realtime: false,
        }
    }

    /// A null output paced by the wall clock.
    pub fn realtime() -> Self {
        Self {
            realtime: true,
            ..Self::new()
        }
    }

    /// The real-time variant when `PM_NULL_OUTPUT=realtime` is set (tests
    /// that need playback to take as long as the music).
    pub fn from_env() -> Self {
        match std::env::var("PM_NULL_OUTPUT").as_deref() {
            Ok("realtime") => Self::realtime(),
            _ => Self::new(),
        }
    }

    /// Restrict what the output accepts (tests: a "16/44.1-only" device).
    pub fn with_caps(caps: OutputCaps) -> Self {
        Self { caps, ..Self::new() }
    }

    /// How many sinks were opened on this output (gapless = one per format run).
    pub fn opens_total(&self) -> u64 {
        self.opens.load(Ordering::Relaxed)
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
        self.opens.fetch_add(1, Ordering::Relaxed);
        Ok(Box::new(NullSink {
            fmt,
            written: 0,
            total: self.frames.clone(),
            clock: self.realtime.then(|| (Instant::now(), 0)),
        }))
    }
}

struct NullSink {
    fmt: AudioFormat,
    written: u64,
    total: Arc<AtomicU64>,
    /// Real-time pacing: the instant and frame count playback is measured from.
    clock: Option<(Instant, u64)>,
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
        if let Some((start, base)) = self.clock {
            let rate = f64::from(self.fmt.rate_hz.max(1));
            let played = Duration::from_secs_f64((self.written - base) as f64 / rate);
            let elapsed = start.elapsed();
            if elapsed > played + AHEAD {
                // Writing resumed after a pause: measure from now.
                self.clock = Some((Instant::now(), self.written));
            } else if played > elapsed + AHEAD {
                std::thread::sleep(played - elapsed - AHEAD);
            }
        }
        Ok(())
    }

    fn buffer_level(&self) -> BufferLevel {
        BufferLevel {
            queued_frames: 0,
            capacity_frames: 0,
        }
    }

    fn flush(&mut self) -> Result<(), SinkError> {
        // After a seek the clock starts again from here.
        if self.clock.is_some() {
            self.clock = Some((Instant::now(), self.written));
        }
        Ok(())
    }

    fn drain(&mut self) -> Result<(), SinkError> {
        // A real-time sink is done when the music would have finished playing.
        if let Some((start, base)) = self.clock {
            let rate = f64::from(self.fmt.rate_hz.max(1));
            let played = Duration::from_secs_f64((self.written - base) as f64 / rate);
            std::thread::sleep(played.saturating_sub(start.elapsed()));
        }
        Ok(())
    }

    fn frames_written(&self) -> u64 {
        self.written
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_realtime_variant_keeps_pace_with_the_music() {
        let out = NullOutput::realtime();
        let fmt = AudioFormat::pcm(8000, 16, 1);
        let mut sink = out.open(fmt).await.unwrap();
        let block = AudioBlock::pcm_i32(fmt, vec![0; 800]); // 100 ms
        let t = Instant::now();
        for _ in 0..6 {
            sink.write(&block).unwrap();
        }
        // 600 ms of audio, up to 250 ms ahead: at least ~350 ms of wall time.
        assert!(t.elapsed() >= Duration::from_millis(300), "{:?}", t.elapsed());
    }

    use pm_model::Samples;

    #[tokio::test]
    async fn counts_frames_and_rejects_mismatch() {
        let out = NullOutput::new();
        let fmt = AudioFormat::pcm(44_100, 16, 2);
        let mut sink = out.open(fmt).await.unwrap();
        sink.write(&AudioBlock::pcm_i32(fmt, vec![0; 8])).unwrap();
        assert_eq!(sink.frames_written(), 4);
        assert_eq!(out.frames_total(), 4);
        let err = sink.write(&AudioBlock {
            format: fmt,
            samples: Samples::PcmF64(vec![0.0; 2]),
        });
        assert!(matches!(err, Err(SinkError::FloatNotAccepted)));
        let err = sink.write(&AudioBlock::pcm_i32(AudioFormat::pcm(48_000, 16, 2), vec![0; 2]));
        assert!(matches!(err, Err(SinkError::FormatMismatch { .. })));
    }
}
