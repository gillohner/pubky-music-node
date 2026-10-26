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
    /// The hardware behind it, when the output knows (a phone's USB DAC).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device: Option<OutputDevice>,
}

/// The hardware an output plays through, as it describes itself: a USB DAC on a phone
/// (STEWARD-ANDROID-USB-DAC.md). What the output does with it now is in `caps`; this is what the
/// device itself takes, and how the app may drive it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct OutputDevice {
    /// "FiiO KA17".
    pub name: String,
    /// One line for the picker: "FiiO KA17 · PCM up to 768 kHz / 32 bit · DSD · hardware volume".
    pub summary: String,
    /// PCM rates the device takes, ascending (empty while unknown).
    #[serde(default)]
    pub rates: Vec<u32>,
    #[serde(default)]
    pub max_bits: u8,
    #[serde(default)]
    pub dsd: bool,
    #[serde(default)]
    pub hardware_volume: bool,
    /// How the app can drive it: `shared` (through the system mixer), `bit-perfect` (Android
    /// 14's mixer attributes), `exclusive` (the app's own USB driver).
    #[serde(default)]
    pub modes: Vec<String>,
    /// The mode in use.
    #[serde(default)]
    pub mode: String,
    /// USB ids, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usb: Option<(u16, u16)>,
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
    /// Net frames inserted (positive) or dropped (negative) to keep this
    /// member in time with the leader; 0 when nothing was corrected.
    #[serde(default)]
    pub slipped_frames: i64,
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

    /// Synced group start (Slimproto): from now on (and after every flush) buffer without
    /// playing until `start_at`. Call before the first write. False when the sink cannot; it
    /// then starts as soon as it has audio, as before.
    fn hold(&mut self) -> bool {
        false
    }

    /// A held sink has buffered enough to start on time.
    fn ready(&self) -> bool {
        true
    }

    /// Start a held sink at `at` (the group passes the same instant to every member).
    fn start_at(&mut self, _at: std::time::Instant) -> Result<(), SinkError> {
        Ok(())
    }

    /// Move the playback point now, for drift correction: `frames` > 0 holds it back (the sink
    /// is ahead), < 0 skips ahead. For sinks whose device queues seconds of audio, where frames
    /// slipped into the stream would be heard only after that queue. False when not supported.
    fn nudge(&mut self, _frames: i64) -> bool {
        false
    }
}

/// How closely a zone's speakers stay together (the Zones view says it plainly).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SyncClass {
    /// One device.
    #[default]
    Single,
    /// Sample-accurate: the node's own endpoints (ADR 0009), Slimproto players, a Snapcast group,
    /// a Sonos group, a Cast group (each kept in sync by its own protocol).
    Tight,
    /// Started together, drifting apart within a second or so: devices on different protocols, or
    /// on a protocol without a shared clock (AirPlay 1, DLNA, single Cast devices).
    Loose,
}

/// What a renderer with a screen shows for a track (Cast, DLNA TVs, Sonos).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TrackMeta {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    /// Cover art the device can fetch itself (an `http(s)` URL, no API token).
    pub image: Option<String>,
}

impl TrackMeta {
    /// The queue item's own fields (art only when it is a URL).
    pub fn from_item(item: &crate::QueueItem) -> Self {
        Self {
            title: item.title.clone(),
            artist: item.artist.clone(),
            album: item.album.clone(),
            image: item.image.clone().filter(|u| u.starts_with("http")),
        }
    }
}

/// Turns a queue item into what a renderer shows; the node fills artist, album and art from its
/// library.
pub type Describe = Arc<dyn Fn(&crate::QueueItem) -> TrackMeta + Send + Sync>;

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
            device: self.device(),
        }
    }

    /// The hardware behind this output, when it knows (see [`OutputDevice`]).
    fn device(&self) -> Option<OutputDevice> {
        None
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

    /// How closely this output's speakers stay together.
    fn sync(&self) -> SyncClass {
        SyncClass::Single
    }

    /// The track about to play (before its sink opens, when one opens) or `None` when the zone
    /// stops. Outputs that show metadata (a TV, a Cast screen) keep it; the default ignores it.
    fn now_playing(&self, _item: Option<&crate::QueueItem>) {}
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
