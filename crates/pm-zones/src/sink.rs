use async_trait::async_trait;
use pm_model::{AudioBlock, AudioFormat, OutputCaps, Volume};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

/// Stable identifier for an output: `<protocol>:<device>`, e.g.
/// `alsa:hw:0,0`, `openhome:uuid:…`, `wav:/tmp/out.wav`, `null`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct OutputId(pub String);

impl OutputId {
    pub fn new(protocol: &str, device: &str) -> Self {
        if device.is_empty() {
            OutputId(protocol.to_string())
        } else {
            OutputId(format!("{protocol}:{device}"))
        }
    }

    pub fn protocol(&self) -> &str {
        self.0.split(':').next().unwrap_or(&self.0)
    }
}

impl fmt::Display for OutputId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// What a client sees when listing outputs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OutputInfo {
    pub id: OutputId,
    pub name: String,
    pub protocol: String,
    pub caps: OutputCaps,
}

#[derive(Debug, thiserror::Error)]
pub enum SinkError {
    #[error("output {0} does not accept format {1}")]
    UnsupportedFormat(OutputId, AudioFormat),
    #[error("block format {found} does not match the open format {expected}")]
    FormatMismatch {
        expected: AudioFormat,
        found: AudioFormat,
    },
    #[error("float blocks must be converted to integer PCM before an output")]
    FloatNotAccepted,
    #[error("output {0} is not available: {1}")]
    Unavailable(OutputId, String),
    #[error("output {0} has no volume control")]
    NoVolume(OutputId),
    #[error("output {0} has no standby control")]
    NoStandby(OutputId),
    #[error("device error: {0}")]
    Device(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// How full the sink's buffer is; the zone paces itself on this.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BufferLevel {
    pub queued_frames: u64,
    pub capacity_frames: u64,
}

impl BufferLevel {
    pub fn fill(&self) -> f64 {
        if self.capacity_frames == 0 {
            0.0
        } else {
            self.queued_frames as f64 / self.capacity_frames as f64
        }
    }
}

/// A clock an output exposes, for grouping and drift correction (§6.5).
pub trait ClockSource: Send + Sync {
    /// Frames this clock has consumed since the sink opened.
    fn position_frames(&self) -> u64;
    /// The clock's rate in frames per second as the device reports it.
    fn rate_hz(&self) -> u32;
    /// Fixed latency between a frame being accepted and being audible.
    fn latency(&self) -> Duration;
}

/// One member of a grouped sink, as reported to the zone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemberStatus {
    pub output: OutputId,
    /// Audible position relative to the group's leader; positive = ahead.
    pub drift_ms: i64,
    pub level: BufferLevel,
}

/// An open output. Methods block; call them from the zone's audio thread.
pub trait AudioSink: Send {
    /// The negotiated format. Every block written must match it exactly.
    fn format(&self) -> AudioFormat;

    /// Push one block. Blocks until the sink has room for it.
    fn write(&mut self, block: &AudioBlock) -> Result<(), SinkError>;

    fn buffer_level(&self) -> BufferLevel;

    /// Discard queued audio (stop / seek).
    fn flush(&mut self) -> Result<(), SinkError>;

    /// Play out queued audio and return when it is audible (end of track
    /// without gapless, or clean stop).
    fn drain(&mut self) -> Result<(), SinkError>;

    /// Frames written so far, for position reporting.
    fn frames_written(&self) -> u64;

    /// Members of a grouped sink and their drift; empty for a single output.
    fn members(&self) -> Vec<MemberStatus> {
        Vec::new()
    }
}

/// One place audio can go (handoff §6.4).
#[async_trait]
pub trait Output: Send + Sync {
    fn id(&self) -> OutputId;
    fn name(&self) -> String;
    fn caps(&self) -> &OutputCaps;

    fn info(&self) -> OutputInfo {
        OutputInfo {
            id: self.id(),
            name: self.name(),
            protocol: self.id().protocol().to_string(),
            caps: self.caps().clone(),
        }
    }

    /// Open the output in exactly `fmt`. Implementations must not convert:
    /// if the device cannot take `fmt` as-is, return
    /// [`SinkError::UnsupportedFormat`] so the pipeline negotiates and records
    /// the conversion in the signal path.
    async fn open(&self, fmt: AudioFormat) -> Result<Box<dyn AudioSink>, SinkError>;

    async fn set_volume(&self, _v: Volume) -> Result<(), SinkError> {
        Err(SinkError::NoVolume(self.id()))
    }

    async fn standby(&self, _on: bool) -> Result<(), SinkError> {
        Err(SinkError::NoStandby(self.id()))
    }

    /// The output's clock, if it exposes one.
    fn clock(&self) -> Option<Arc<dyn ClockSource>> {
        None
    }
}

/// Reject a block the sink cannot take: wrong format or still float.
pub fn check_block(expected: AudioFormat, block: &AudioBlock) -> Result<(), SinkError> {
    if block.is_float() {
        return Err(SinkError::FloatNotAccepted);
    }
    if block.format != expected {
        return Err(SinkError::FormatMismatch {
            expected,
            found: block.format,
        });
    }
    Ok(())
}
