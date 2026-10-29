//! Pull outputs (handoff §6.4, last paragraph): some endpoints fetch a URL
//! instead of accepting pushed frames (OpenHome/UPnP renderers, Cast,
//! browsers). A [`PullOutput`] is an `Output` whose sink writes the zone's
//! post-DSP PCM into a channel that an HTTP server (pm-api:
//! `GET /v1/outputs/pull/{name}/stream.wav`) streams to the fetcher, WAV-framed.
//!
//! The listener paces the zone: the sink blocks when the bounded channel is
//! full, so a slow fetcher slows the zone instead of dropping audio. With no
//! listener the sink waits [`LISTENER_TIMEOUT`] and then fails, so a zone
//! never plays silently into nothing. Bit-exact: the stream is exactly the
//! blocks the zone produced.
//!
//! A listener may come back. Renderers built on Linn's OpenHome pipeline read the first few KB
//! of a live stream to recognise it, close the connection and open it again once their pipeline
//! is ready; some TVs probe the same way. So a listener that leaves gives the stream back: the
//! next one gets it, with a fresh header, and first the bytes a listener that left within
//! [`PROBE_TIME`] already had (it heard nothing yet). Nobody back within [`REATTACH_GRACE`]: the
//! sink fails with "listener went away".

use crate::sink::{check_block, AudioSink, BufferLevel, Output, OutputId, SinkError};
use async_trait::async_trait;
use pm_model::{AudioBlock, AudioFormat, OutputCaps, QualityCeiling, SampleKind, Samples};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

/// How long a sink waits for a listener before giving up.
pub const LISTENER_TIMEOUT: Duration = Duration::from_secs(15);
/// A listener that leaves within this time of taking the stream was probing it: the next
/// listener gets what it had again.
pub const PROBE_TIME: Duration = Duration::from_secs(3);
/// How long a stream waits for its listener to come back before the sink fails.
pub const REATTACH_GRACE: Duration = Duration::from_secs(5);
/// Most a probing listener's bytes kept for the next one (a few seconds of hi-res audio).
const PROBE_KEEP_BYTES: usize = 16 * 1024 * 1024;
/// Chunks a listener may lag before the zone blocks (~ a few seconds).
const CHANNEL_CHUNKS: usize = 64;

/// A stream no listener holds: opened by a sink, or given back by a listener that left.
struct Parked {
    format: AudioFormat,
    rx: mpsc::Receiver<Vec<u8>>,
    replay: VecDeque<Vec<u8>>,
    generation: u64,
}

#[derive(Default)]
struct Slot {
    pending: Option<Parked>,
    /// The newest sink's stream; older ones are not given back.
    generation: u64,
    /// When the newest stream's listener left, while nobody has taken it again.
    left: Option<(u64, Instant)>,
}

struct Shared {
    slot: Mutex<Slot>,
    notify: tokio::sync::Notify,
}

/// One live stream as a listener holds it: its format and the PCM (little-endian, the format's
/// byte width, whole frames per chunk; no header). Dropping it gives the stream back for the
/// next listener.
pub struct LiveStream {
    pub format: AudioFormat,
    rx: Option<mpsc::Receiver<Vec<u8>>>,
    /// What an earlier, probing listener had: sent before anything new.
    replay: VecDeque<Vec<u8>>,
    /// What this listener got while it may still be probing.
    kept: Vec<Vec<u8>>,
    kept_bytes: usize,
    keeping: bool,
    taken: Instant,
    /// The sink closed and this listener read everything: nothing to give back.
    finished: bool,
    generation: u64,
    home: Weak<Shared>,
}

impl LiveStream {
    /// The next chunk of PCM; `None` once the sink closed and everything was read.
    pub async fn next_chunk(&mut self) -> Option<Vec<u8>> {
        let chunk = match self.replay.pop_front() {
            Some(c) => c,
            None => match self.rx.as_mut()?.recv().await {
                Some(c) => c,
                None => {
                    self.finished = true;
                    return None;
                }
            },
        };
        if self.keeping {
            if self.taken.elapsed() < PROBE_TIME && self.kept_bytes + chunk.len() <= PROBE_KEEP_BYTES {
                self.kept_bytes += chunk.len();
                self.kept.push(chunk.clone());
            } else {
                self.keeping = false;
                self.kept = Vec::new();
            }
        }
        Some(chunk)
    }
}

impl Drop for LiveStream {
    fn drop(&mut self) {
        let Some(rx) = self.rx.take() else {
            return;
        };
        if self.finished {
            return;
        }
        let Some(home) = self.home.upgrade() else {
            return;
        };
        let mut replay = VecDeque::new();
        if self.keeping && self.taken.elapsed() < PROBE_TIME {
            replay.extend(std::mem::take(&mut self.kept));
        }
        replay.extend(std::mem::take(&mut self.replay));
        if rx.is_closed() && rx.is_empty() && replay.is_empty() {
            return; // the sink closed and this listener had it all
        }
        let mut slot = home.slot.lock().expect("pull lock");
        if slot.generation != self.generation || slot.pending.is_some() {
            return; // a newer stream replaced this one
        }
        slot.pending = Some(Parked {
            format: self.format,
            rx,
            replay,
            generation: self.generation,
        });
        slot.left = Some((self.generation, Instant::now()));
        drop(slot);
        home.notify.notify_waiters();
    }
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
                slot: Mutex::new(Slot::default()),
                notify: tokio::sync::Notify::new(),
            }),
        }
    }

    pub fn stream_name(&self) -> &str {
        &self.name
    }

    /// Take the live stream for an HTTP listener, waiting until a sink opens one (or a listener
    /// that left gives it back).
    pub async fn take_stream(&self, wait: Duration) -> Option<LiveStream> {
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            // Registered before the check, so a stream parked in between wakes us
            let notified = self.shared.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let mut slot = self.shared.slot.lock().expect("pull lock");
                if let Some(p) = slot.pending.take() {
                    if slot.left.is_some_and(|(g, _)| g == p.generation) {
                        slot.left = None;
                    }
                    return Some(LiveStream {
                        format: p.format,
                        rx: Some(p.rx),
                        replay: p.replay,
                        kept: Vec::new(),
                        kept_bytes: 0,
                        keeping: true,
                        taken: Instant::now(),
                        finished: false,
                        generation: p.generation,
                        home: Arc::downgrade(&self.shared),
                    });
                }
            }
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            if left.is_zero() {
                return None;
            }
            let _ = tokio::time::timeout(left, notified).await;
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
        let generation = {
            let mut slot = self.shared.slot.lock().expect("pull lock");
            slot.generation += 1;
            // Replace any stream no listener took; its sink will fail on write.
            slot.pending = Some(Parked {
                format: fmt,
                rx,
                replay: VecDeque::new(),
                generation: slot.generation,
            });
            slot.left = None;
            slot.generation
        };
        self.shared.notify.notify_waiters();
        Ok(Box::new(PullSink {
            id: self.id(),
            fmt,
            tx,
            written: 0,
            generation,
            shared: self.shared.clone(),
        }))
    }
}

struct PullSink {
    id: OutputId,
    fmt: AudioFormat,
    tx: mpsc::Sender<Vec<u8>>,
    written: u64,
    generation: u64,
    shared: Arc<Shared>,
}

/// A WAV header for a stream of unknown length: RIFF and data sizes set to the maximum, as
/// streaming servers do; players ignore them.
pub fn wav_header(fmt: AudioFormat) -> Vec<u8> {
    wav_header_sized(fmt, u32::MAX, u32::MAX)
}

/// A WAV header in OpenHome's form for a continuous stream: RIFF size 0. Linn's pipeline reads
/// sizes literally and rounds an odd chunk size up, so the usual `0xFFFFFFFF` wraps to 0 there and
/// the stream "ends" before its first sample; a RIFF size of 0 is its mark for "live, no end".
pub fn wav_header_continuous(fmt: AudioFormat) -> Vec<u8> {
    wav_header_sized(fmt, 0, 0)
}

fn wav_header_sized(fmt: AudioFormat, riff: u32, data: u32) -> Vec<u8> {
    let bytes_per_sample = (fmt.bits as u32).div_ceil(8);
    let block_align = bytes_per_sample as u16 * fmt.channels as u16;
    let byte_rate = fmt.rate_hz * block_align as u32;
    let mut h = Vec::with_capacity(44);
    h.extend_from_slice(b"RIFF");
    h.extend_from_slice(&riff.to_le_bytes());
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
    h.extend_from_slice(&data.to_le_bytes());
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
    /// The listener left and did not come back in time.
    fn listener_gone(&self) -> bool {
        let slot = self.shared.slot.lock().expect("pull lock");
        slot.left
            .is_some_and(|(g, at)| g == self.generation && at.elapsed() > REATTACH_GRACE)
    }

    fn send(&mut self, bytes: Vec<u8>) -> Result<(), SinkError> {
        let deadline = std::time::Instant::now() + LISTENER_TIMEOUT;
        let mut payload = Some(bytes);
        loop {
            match self.tx.try_send(payload.take().expect("payload")) {
                Ok(()) => return Ok(()),
                Err(mpsc::error::TrySendError::Full(b)) => {
                    payload = Some(b);
                    if self.listener_gone() {
                        return Err(SinkError::Device(format!("{}: listener went away", self.id)));
                    }
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
            && !self.listener_gone()
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
        // OpenHome's continuous form differs only in the sizes
        let c = wav_header_continuous(fmt);
        assert_eq!(&c[4..8], &[0, 0, 0, 0]);
        assert_eq!(&c[40..44], &[0, 0, 0, 0]);
        assert_eq!(&c[8..40], &h[8..40]);
    }

    fn block(fmt: AudioFormat, from: i32) -> AudioBlock {
        AudioBlock::pcm_i32(fmt, (from..from + 4).collect())
    }

    /// A listener that reads a little and leaves (Linn's pipeline does, to recognise the
    /// stream) gives it back: the next one starts from the same first byte, then goes on.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_probing_listener_gives_the_stream_back_from_the_start() {
        let fmt = AudioFormat::pcm(44_100, 16, 2);
        let out = PullOutput::new("p");
        let mut sink = out.open_now(fmt).unwrap();
        let writer = std::thread::spawn(move || {
            for i in 0..8 {
                sink.write(&block(fmt, i * 4)).unwrap();
            }
            sink
        });
        let mut first = out.take_stream(Duration::from_secs(2)).await.unwrap();
        assert_eq!(first.next_chunk().await.unwrap(), pack_pcm(&block(fmt, 0)));
        assert_eq!(first.next_chunk().await.unwrap(), pack_pcm(&block(fmt, 4)));
        drop(first);
        let mut second = out.take_stream(Duration::from_secs(2)).await.expect("given back");
        for i in 0..8 {
            assert_eq!(
                second.next_chunk().await.unwrap(),
                pack_pcm(&block(fmt, i * 4)),
                "chunk {i}"
            );
        }
        drop(writer.join().unwrap());
        assert_eq!(second.next_chunk().await, None, "the sink closed");
        drop(second);
        // Everything was read: nothing is given back to a third listener
        assert!(out.take_stream(Duration::from_millis(50)).await.is_none());
    }

    /// Nobody comes back: the sink fails soon after the listener left, not after the full
    /// listener timeout.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_listener_that_never_comes_back_fails_the_sink() {
        let fmt = AudioFormat::pcm(44_100, 16, 2);
        let out = PullOutput::new("q");
        let mut sink = out.open_now(fmt).unwrap();
        let live = out.take_stream(Duration::from_secs(2)).await.unwrap();
        drop(live);
        let started = std::time::Instant::now();
        let err = std::thread::spawn(move || loop {
            if let Err(e) = sink.write(&block(fmt, 0)) {
                break e;
            }
        })
        .join()
        .unwrap();
        assert!(err.to_string().contains("listener went away"), "{err}");
        let took = started.elapsed();
        assert!(took >= REATTACH_GRACE && took < LISTENER_TIMEOUT, "{took:?}");
    }

    /// A newer sink's stream wins over an old one a listener gives back.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_old_stream_is_not_given_back_over_a_new_one() {
        let fmt = AudioFormat::pcm(44_100, 16, 2);
        let out = PullOutput::new("r");
        let _old = out.open_now(fmt).unwrap();
        let old_live = out.take_stream(Duration::from_secs(2)).await.unwrap();
        let fmt2 = AudioFormat::pcm(48_000, 24, 2);
        let _new = out.open_now(fmt2).unwrap();
        drop(old_live);
        let live = out.take_stream(Duration::from_secs(2)).await.unwrap();
        assert_eq!(live.format, fmt2);
    }
}
