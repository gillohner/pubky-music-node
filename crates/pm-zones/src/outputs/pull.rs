//! Pull outputs (handoff §6.4, last paragraph): some endpoints fetch a URL
//! instead of accepting pushed frames (OpenHome/UPnP renderers, Cast,
//! browsers). A [`PullOutput`] is an `Output` whose sink writes the zone's
//! post-DSP PCM, WAV-framed, into a channel that an HTTP server (pm-api:
//! `GET /v1/outputs/pull/{name}/stream.wav`) streams to the fetcher.
//!
//! The listener paces the zone: the sink blocks when the bounded channel is
//! full, so a slow fetcher slows the zone instead of dropping audio. With no
//! listener the sink waits [`LISTENER_TIMEOUT`] and then fails, so a zone
//! never plays silently into nothing. Bit-exact: the stream is the WAV
//! encoding of exactly the blocks the zone produced.

use crate::sink::{check_block, AudioSink, BufferLevel, Output, OutputId, SinkError};
use async_trait::async_trait;
use pm_model::{AudioBlock, AudioFormat, OutputCaps, QualityCeiling, SampleKind, Samples};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;

/// How long a sink waits for a listener before giving up.
pub const LISTENER_TIMEOUT: Duration = Duration::from_secs(15);
/// Chunks a listener may lag before the zone blocks (~ a few seconds).
const CHANNEL_CHUNKS: usize = 64;

/// One live stream: the format and the channel to the HTTP response.
pub struct LiveStream {
    pub format: AudioFormat,
    pub rx: mpsc::Receiver<Vec<u8>>,
}

struct Shared {
    /// A stream opened by a sink and not yet taken by a listener.
    pending: Mutex<Option<LiveStream>>,
    notify: tokio::sync::Notify,
}

pub struct PullOutput {
    name: String,
    caps: OutputCaps,
    shared: Arc<Shared>,
}

impl PullOutput {
    /// `name` is the URL segment (letters, digits, `-`, `_`).
    pub fn new(name: &str) -> Self {
        let mut caps = OutputCaps::unlimited();
        caps.dsd = None;
        caps.hw_volume = None;
        caps.quality_ceiling = QualityCeiling::BitPerfect;
        caps.pcm_rates.retain(|r| *r <= 384_000);
        caps.pcm_bits = vec![16, 24, 32];
        Self {
            name: name.to_string(),
            caps,
            shared: Arc::new(Shared {
                pending: Mutex::new(None),
                notify: tokio::sync::Notify::new(),
            }),
        }
    }

    pub fn stream_name(&self) -> &str {
        &self.name
    }

    /// Take the live stream for an HTTP listener, waiting until a sink opens one.
    pub async fn take_stream(&self, wait: Duration) -> Option<LiveStream> {
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            if let Some(s) = self.shared.pending.lock().expect("pull lock").take() {
                return Some(s);
            }
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            if left.is_zero() {
                return None;
            }
            let _ = tokio::time::timeout(left, self.shared.notify.notified()).await;
        }
    }
}

#[async_trait]
impl Output for PullOutput {
    fn id(&self) -> OutputId {
        OutputId::new("pull", &self.name)
    }

    fn name(&self) -> String {
        format!("HTTP stream {}", self.name)
    }

    fn caps(&self) -> &OutputCaps {
        &self.caps
    }

    async fn open(&self, fmt: AudioFormat) -> Result<Box<dyn AudioSink>, SinkError> {
        self.open_now(fmt)
    }
}

impl PullOutput {
    /// `open` without the runtime: a sink that restarts its listener's stream (Slimproto after a
    /// flush) opens the next one from its own thread.
    pub fn open_now(&self, fmt: AudioFormat) -> Result<Box<dyn AudioSink>, SinkError> {
        if fmt.kind != SampleKind::Pcm || !self.caps.accepts(&fmt) {
            return Err(SinkError::UnsupportedFormat(self.id(), fmt));
        }
        let (tx, rx) = mpsc::channel(CHANNEL_CHUNKS);
        // Replace any stream no listener took; its sink will fail on write.
        *self.shared.pending.lock().expect("pull lock") = Some(LiveStream { format: fmt, rx });
        self.shared.notify.notify_waiters();
        Ok(Box::new(PullSink {
            id: self.id(),
            fmt,
            tx,
            written: 0,
            sent_header: false,
        }))
    }
}

struct PullSink {
    id: OutputId,
    fmt: AudioFormat,
    tx: mpsc::Sender<Vec<u8>>,
    written: u64,
    sent_header: bool,
}

/// A WAV header for a stream of unknown length (sizes set to the maximum,
/// as streaming servers do; players ignore them).
pub fn wav_header(fmt: AudioFormat) -> Vec<u8> {
    let bytes_per_sample = (fmt.bits as u32).div_ceil(8);
    let block_align = bytes_per_sample as u16 * fmt.channels as u16;
    let byte_rate = fmt.rate_hz * block_align as u32;
    let mut h = Vec::with_capacity(44);
    h.extend_from_slice(b"RIFF");
    h.extend_from_slice(&u32::MAX.to_le_bytes());
    h.extend_from_slice(b"WAVE");
    h.extend_from_slice(b"fmt ");
    h.extend_from_slice(&16u32.to_le_bytes());
    h.extend_from_slice(&1u16.to_le_bytes()); // PCM
    h.extend_from_slice(&(fmt.channels as u16).to_le_bytes());
    h.extend_from_slice(&fmt.rate_hz.to_le_bytes());
    h.extend_from_slice(&byte_rate.to_le_bytes());
    h.extend_from_slice(&block_align.to_le_bytes());
    h.extend_from_slice(&(fmt.bits as u16).to_le_bytes());
    h.extend_from_slice(b"data");
    h.extend_from_slice(&u32::MAX.to_le_bytes());
    h
}

/// Pack a block as little-endian PCM at the format's byte width.
pub fn pack_pcm(block: &AudioBlock) -> Vec<u8> {
    let bits = block.format.bits as u32;
    let width = bits.div_ceil(8) as usize;
    match &block.samples {
        Samples::PcmI32(s) => {
            let mut v = Vec::with_capacity(s.len() * width);
            for x in s {
                v.extend_from_slice(&x.to_le_bytes()[..width]);
            }
            v
        }
        _ => Vec::new(),
    }
}

impl PullSink {
    fn send(&mut self, bytes: Vec<u8>) -> Result<(), SinkError> {
        let deadline = std::time::Instant::now() + LISTENER_TIMEOUT;
        let mut payload = Some(bytes);
        loop {
            match self.tx.try_send(payload.take().expect("payload")) {
                Ok(()) => return Ok(()),
                Err(mpsc::error::TrySendError::Full(b)) => {
                    payload = Some(b);
                    if std::time::Instant::now() > deadline {
                        return Err(SinkError::Device(format!("{}: listener stalled", self.id)));
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    return Err(SinkError::Device(format!("{}: listener went away", self.id)));
                }
            }
        }
    }
}

impl AudioSink for PullSink {
    fn format(&self) -> AudioFormat {
        self.fmt
    }

    fn write(&mut self, block: &AudioBlock) -> Result<(), SinkError> {
        check_block(self.fmt, block)?;
        if !self.sent_header {
            self.send(wav_header(self.fmt))?;
            self.sent_header = true;
        }
        self.send(pack_pcm(block))?;
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
        Ok(())
    }

    fn drain(&mut self) -> Result<(), SinkError> {
        // Wait for the listener to catch up with what was sent.
        let deadline = std::time::Instant::now() + LISTENER_TIMEOUT;
        while self.tx.capacity() < CHANNEL_CHUNKS
            && std::time::Instant::now() < deadline
            && !self.tx.is_closed()
        {
            std::thread::sleep(Duration::from_millis(5));
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

    #[test]
    fn header_and_packing() {
        let fmt = AudioFormat::pcm(44_100, 24, 2);
        let h = wav_header(fmt);
        assert_eq!(h.len(), 44);
        assert_eq!(&h[0..4], b"RIFF");
        assert_eq!(u16::from_le_bytes([h[22], h[23]]), 2);
        assert_eq!(u32::from_le_bytes([h[24], h[25], h[26], h[27]]), 44_100);
        assert_eq!(u16::from_le_bytes([h[32], h[33]]), 6);
        assert_eq!(u16::from_le_bytes([h[34], h[35]]), 24);
        let b = AudioBlock::pcm_i32(fmt, vec![0x123456, -1]);
        assert_eq!(pack_pcm(&b), vec![0x56, 0x34, 0x12, 0xff, 0xff, 0xff]);
        let b16 = AudioBlock::pcm_i32(AudioFormat::pcm(44_100, 16, 1), vec![-2]);
        assert_eq!(pack_pcm(&b16), vec![0xfe, 0xff]);
    }
}
