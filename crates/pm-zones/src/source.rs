//! What a zone pulls audio from.
//!
//! `pm-zones` owns the queue and transport but knows nothing about codecs:
//! `pm-audio` implements [`FrameSource`] (one decoded track), [`Processor`]
//! (the negotiated chain from source format to output format, with its
//! signal path) and [`SourceFactory`] (opens a [`QueueItem`] and plans a
//! chain against an output's caps). The engine in [`crate::zone`] drives
//! them from its audio thread.

use crate::queue::QueueItem;
use pm_model::{AudioBlock, AudioFormat, OutputCaps, SignalPath};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SourceError {
    #[error("cannot open {uri}: {reason}")]
    Open { uri: String, reason: String },
    #[error("decode error: {0}")]
    Decode(String),
    #[error("seek failed: {0}")]
    Seek(String),
    #[error("no path from {format} to this output: {reason}")]
    Negotiate { format: AudioFormat, reason: String },
}

/// ReplayGain values from the track's tags, in dB (applied by the
/// pipeline's `loudness` stage when enabled).
#[derive(Debug, Clone, Copy, PartialEq, Default, serde::Serialize, serde::Deserialize)]
pub struct ReplayGain {
    pub track_db: Option<f32>,
    pub album_db: Option<f32>,
}

/// What is actually arriving for the playing track, for the player's quality label
/// (STEWARD-UX-PLAYER-SOURCES.md §2): the codec as people know it, and what the source knows
/// about its stream. The decoded rate and bit depth are `NowPlaying::source_format`.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct StreamInfo {
    /// `FLAC`, `ALAC`, `MP3`, `AAC`, `Vorbis`, `Opus`, `WAV`, `DSD`…
    pub codec: String,
    /// The stream's bitrate, for lossy codecs (average for a file).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bitrate_kbps: Option<u32>,
    pub lossless: bool,
    /// Where it comes from: `library`, `tidal`, `qobuz`, `deezer`, `radio`… (the engine fills
    /// it from the queue item when the source leaves it empty).
    #[serde(default)]
    pub service: String,
    /// The service's quality tier, when it has tiers (`max`, `high`, `hires`, `cd`…).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier: Option<String>,
    /// A short preview, not the whole track (Deezer without a partner agreement).
    #[serde(default)]
    pub preview: bool,
}

/// A source with ReplayGain attached (the opener knows the tags, the
/// decoder does not).
pub struct WithReplayGain {
    pub inner: Box<dyn FrameSource>,
    pub gain: ReplayGain,
}

impl FrameSource for WithReplayGain {
    fn format(&self) -> AudioFormat {
        self.inner.format()
    }
    fn duration_ms(&self) -> Option<u64> {
        self.inner.duration_ms()
    }
    fn next_block(&mut self) -> Result<Option<AudioBlock>, SourceError> {
        self.inner.next_block()
    }
    fn seek_ms(&mut self, ms: u64) -> Result<(), SourceError> {
        self.inner.seek_ms(ms)
    }
    fn finalize(&mut self) -> Option<bool> {
        self.inner.finalize()
    }
    fn replay_gain(&self) -> Option<ReplayGain> {
        Some(self.gain)
    }
    fn stream_info(&self) -> Option<StreamInfo> {
        self.inner.stream_info()
    }
    fn lossy_source(&self) -> bool {
        self.inner.lossy_source()
    }
    fn stream_title(&mut self) -> Option<String> {
        self.inner.stream_title()
    }
}

/// One decoded track. Methods block (file I/O); the zone calls them from its
/// audio thread.
pub trait FrameSource: Send {
    /// The source's native format; every block has it.
    fn format(&self) -> AudioFormat;
    fn duration_ms(&self) -> Option<u64>;
    /// Next block, `None` at the end of the track.
    fn next_block(&mut self) -> Result<Option<AudioBlock>, SourceError>;
    /// Accurate seek: the next block starts at `ms`.
    fn seek_ms(&mut self, ms: u64) -> Result<(), SourceError>;
    /// Codec verification once the end was reached (FLAC MD5); `None` when
    /// the codec has none.
    fn finalize(&mut self) -> Option<bool> {
        None
    }
    /// ReplayGain from the track's tags, when known.
    fn replay_gain(&self) -> Option<ReplayGain> {
        None
    }
    /// The audio went through a lossy codec; the signal path says so.
    fn lossy_source(&self) -> bool {
        false
    }
    /// A new in-stream title (internet radio's ICY `StreamTitle`) since the
    /// last call; `None` when nothing changed. Polled once per block.
    fn stream_title(&mut self) -> Option<String> {
        None
    }
    /// What is arriving (codec, bitrate, service tier); `None` when the source can't say.
    fn stream_info(&self) -> Option<StreamInfo> {
        None
    }
}

/// The negotiated chain from a source format to the output format the sink
/// is opened in. Stateful (dither, volume ramps), one per track.
pub trait Processor: Send {
    fn source_format(&self) -> AudioFormat;
    fn output_format(&self) -> AudioFormat;
    fn signal_path(&self) -> &SignalPath;
    fn process(&mut self, block: AudioBlock) -> Result<AudioBlock, SourceError>;
    /// Flush stages with internal delay at the end of the track.
    fn finish(&mut self) -> Result<Option<AudioBlock>, SourceError> {
        Ok(None)
    }
}

/// Opens queue items and plans chains. Shared by every zone of a node.
pub trait SourceFactory: Send + Sync {
    fn open(&self, item: &QueueItem) -> Result<Box<dyn FrameSource>, SourceError>;
    /// Plan the chain from `source` to `output` (per-zone DSP keys on the
    /// output; the source supplies its format and ReplayGain).
    fn plan(
        &self,
        source: &dyn FrameSource,
        caps: &OutputCaps,
        output: &crate::sink::OutputId,
    ) -> Result<Box<dyn Processor>, SourceError>;
}

#[cfg(test)]
mod stream_info_tests {
    use super::StreamInfo;

    /// Older nodes and sources leave members out: they read as empty, and what is unknown is
    /// left out of the JSON again.
    #[test]
    fn stream_info_defaults() {
        let f: StreamInfo = serde_json::from_str(r#"{"codec":"FLAC","lossless":true}"#).unwrap();
        assert_eq!(
            f,
            StreamInfo {
                codec: "FLAC".into(),
                bitrate_kbps: None,
                lossless: true,
                service: String::new(),
                tier: None,
                preview: false,
            }
        );
        let json = serde_json::to_value(&f).unwrap();
        assert_eq!(
            json,
            serde_json::json!({"codec": "FLAC", "lossless": true, "service": "", "preview": false})
        );
    }
}
