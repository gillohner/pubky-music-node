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
//! [`PROBE_TIME`] of its first bytes already had (it heard nothing yet), up to
//! [`PROBE_KEEP_BYTES`]. The time runs from the first bytes, not the connection: a Linn holds a
//! stream 5 s waiting for them. Nobody back within [`REATTACH_GRACE`]: the sink fails with
//! "listener went away".

use crate::sink::{check_block, AudioSink, BufferLevel, Output, OutputId, SinkError};
use async_trait::async_trait;
use pm_model::{AudioBlock, AudioFormat, OutputCaps, QualityCeiling, SampleKind, Samples};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

/// How long a sink waits for a listener before giving up.
pub const LISTENER_TIMEOUT: Duration = Duration::from_secs(15);
/// A listener that leaves within this time of its first bytes was probing: the next listener
/// gets what it had again. Longer, and it was playing: that is not heard twice.
pub const PROBE_TIME: Duration = Duration::from_secs(3);
/// How long a stream waits for its listener to come back before the sink fails.
pub const REATTACH_GRACE: Duration = Duration::from_secs(5);
/// A listener that took more than this was playing, not probing (a Linn reads about 72 KB):
/// what it had is not sent again.
pub const PROBE_KEEP_BYTES: usize = 1024 * 1024;
/// Chunks a listener may lag before the zone blocks (~ a few seconds).
const CHANNEL_CHUNKS: usize = 64;
/// How far ahead of real time a sink may write. A renderer that buffers fast (a BluOS player
/// takes 40 s in a second) would otherwise have the whole queue before it played any of it,
/// and the zone would stop it at "the end".
pub const PACE_AHEAD: Duration = Duration::from_secs(5);
/// What a renderer still plays after the last byte reached it, at most (its own buffer).
const PLAYOUT_MARGIN: Duration = Duration::from_secs(2);

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
    /// A stream that cannot play (the renderer refused Play): its sink fails with this.
    failed: Option<(u64, String)>,
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
    /// When this listener had its first bytes.
    first: Option<Instant>,
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
        let first = *self.first.get_or_insert_with(Instant::now);
        if self.keeping {
            if first.elapsed() < PROBE_TIME && self.kept_bytes + chunk.len() <= PROBE_KEEP_BYTES {
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
        if self.keeping && self.first.is_none_or(|t| t.elapsed() < PROBE_TIME) {
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
    /// Sinks keep to real time ([`PACE_AHEAD`]); off for players paced by their own buffer.
    paced: bool,
    /// The node tells the fetcher when to fetch ([`PullOutput::controlled`]).
    controlled: bool,
}

/// Where a pull output's stream generations start: milliseconds of the clock, so they don't
/// repeat across restarts of the node.
fn first_generation() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Why [`PullOutput::take_stream_for`] gave no stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Missed {
    /// Nothing played into it within the wait.
    Timeout,
    /// A newer stream opened while this request was waiting: the request is from before (a
    /// stopped renderer polling its old URI); the fetch the node asks for next gets it.
    Superseded,
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
                // Generations start from the clock, so a URL naming one from before a restart
                // never names one after it
                slot: Mutex::new(Slot {
                    generation: first_generation(),
                    ..Slot::default()
                }),
                notify: tokio::sync::Notify::new(),
            }),
            paced: true,
            controlled: false,
        }
    }

    /// A pull output whose fetcher the node tells when to fetch (a UPnP renderer after
    /// SetAVTransportURI; Cast, MPD and BluOS likewise). A stopped Linn keeps polling its old URI
    /// every 15 s, and such a request, waiting when the next stream opens, took that stream and
    /// dropped it (NUC, 2026-10-30): here it is answered instead ([`Missed::Superseded`]).
    pub fn controlled(name: &str) -> Self {
        Self {
            controlled: true,
            ..Self::new(name)
        }
    }

    /// A pull output whose fetcher paces the zone by itself (a Squeezebox player reads only as
    /// its buffer frees, and synced groups rely on that buffer).
    pub fn unpaced(name: &str) -> Self {
        Self {
            paced: false,
            ..Self::new(name)
        }
    }

    pub fn stream_name(&self) -> &str {
        &self.name
    }

    /// The newest sink's stream, to name it in [`Self::abort`].
    pub fn generation(&self) -> u64 {
        self.shared.slot.lock().expect("pull lock").generation
    }

    /// Make the sink of stream `generation` fail with `why` at its next write (a renderer that
    /// refused Play will never fetch it). A newer stream is not touched.
    pub fn abort(&self, generation: u64, why: String) {
        let mut slot = self.shared.slot.lock().expect("pull lock");
        if slot.generation == generation {
            slot.failed = Some((generation, why));
        }
    }

    /// Take the live stream for an HTTP listener, waiting until a sink opens one (or a listener
    /// that left gives it back).
    pub async fn take_stream(&self, wait: Duration) -> Option<LiveStream> {
        self.take_stream_for(wait, None).await.ok()
    }

    /// [`Self::take_stream`], saying why there is none. `generation`: the stream the request is
    /// for (the node puts it in the URL it gives a renderer, [`Self::generation`]); no other is
    /// for it, whenever the request comes (a renderer polling its old URI, also from before a
    /// restart). Without one,
    /// on a [`Self::controlled`] output, a stream opened after the request began waiting is not
    /// for it.
    pub async fn take_stream_for(
        &self,
        wait: Duration,
        generation: Option<u64>,
    ) -> Result<LiveStream, Missed> {
        let deadline = tokio::time::Instant::now() + wait;
        let since = generation.unwrap_or_else(|| self.shared.slot.lock().expect("pull lock").generation);
        let checked = self.controlled || generation.is_some();
        loop {
            // Registered before the check, so a stream parked in between wakes us
            let notified = self.shared.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let mut slot = self.shared.slot.lock().expect("pull lock");
                // A request naming its stream gets that one only (older, newer, or from before a
                // restart: answered); one that doesn't, none opened after it began
                let other = match generation {
                    Some(g) => slot.generation != g,
                    None => slot.generation > since,
                };
                if checked && other {
                    return Err(Missed::Superseded);
                }
                // A stream whose sink closed with nothing left to hear is no stream
                if slot
                    .pending
                    .as_ref()
                    .is_some_and(|p| p.rx.is_closed() && p.rx.is_empty() && p.replay.is_empty())
                {
                    slot.pending = None;
                }
                if let Some(p) = slot.pending.take() {
                    if slot.left.is_some_and(|(g, _)| g == p.generation) {
                        slot.left = None;
                    }
                    return Ok(LiveStream {
                        format: p.format,
                        rx: Some(p.rx),
                        replay: p.replay,
                        kept: Vec::new(),
                        kept_bytes: 0,
                        keeping: true,
                        first: None,
                        finished: false,
                        generation: p.generation,
                        home: Arc::downgrade(&self.shared),
                    });
                }
            }
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            if left.is_zero() {
                return Err(Missed::Timeout);
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
            slot.failed = None;
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
            clock: None,
            paced: self.paced,
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
    /// When the current run started (first write after open or a flush), and `written` then.
    clock: Option<(Instant, u64)>,
    paced: bool,
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

    /// Why this stream cannot play, when someone said so ([`PullOutput::abort`]).
    fn aborted(&self) -> Option<SinkError> {
        let slot = self.shared.slot.lock().expect("pull lock");
        slot.failed
            .as_ref()
            .filter(|(g, _)| *g == self.generation)
            .map(|(_, why)| SinkError::Device(why.clone()))
    }

    /// How far the audio sent is ahead of real time since the run started.
    fn ahead(&self) -> Duration {
        let Some((at, from)) = self.clock else {
            return Duration::ZERO;
        };
        let sent = Duration::from_secs_f64((self.written - from) as f64 / f64::from(self.fmt.rate_hz.max(1)));
        sent.saturating_sub(at.elapsed())
    }

    /// The listener left (and nobody has taken the stream since).
    fn listener_left(&self) -> bool {
        let slot = self.shared.slot.lock().expect("pull lock");
        slot.left.is_some_and(|(g, _)| g == self.generation)
    }

    fn send(&mut self, bytes: Vec<u8>) -> Result<(), SinkError> {
        let deadline = std::time::Instant::now() + LISTENER_TIMEOUT;
        let mut payload = Some(bytes);
        loop {
            match self.tx.try_send(payload.take().expect("payload")) {
                Ok(()) => return Ok(()),
                Err(mpsc::error::TrySendError::Full(b)) => {
                    payload = Some(b);
                    if let Some(e) = self.aborted() {
                        return Err(e);
                    }
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
        if let Some(e) = self.aborted() {
            return Err(e);
        }
        self.send(pack_pcm(block))?;
        self.clock.get_or_insert((Instant::now(), self.written));
        self.written += block.frames() as u64;
        // Never more than PACE_AHEAD ahead of the clock (the listener's own pace when slower)
        let ahead = self.ahead();
        if self.paced && ahead > PACE_AHEAD {
            std::thread::sleep(ahead - PACE_AHEAD);
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
        self.clock = None;
        Ok(())
    }

    fn drain(&mut self) -> Result<(), SinkError> {
        // Wait for the listener to catch up with what was sent.
        let deadline = std::time::Instant::now() + LISTENER_TIMEOUT;
        while self.tx.capacity() < CHANNEL_CHUNKS
            && std::time::Instant::now() < deadline
            && !self.tx.is_closed()
            && !self.listener_left()
            && self.aborted().is_none()
        {
            std::thread::sleep(Duration::from_millis(5));
        }
        // The renderer has the audio; give it the time to play it before the zone moves on (or
        // stops it at the end of the queue)
        if self.paced && !self.tx.is_closed() && !self.listener_left() && self.aborted().is_none() {
            std::thread::sleep((self.ahead() + PLAYOUT_MARGIN).min(PACE_AHEAD + PLAYOUT_MARGIN));
        }
        self.clock = None;
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

    /// A Linn holds a stream ~5 s waiting for its first bytes, gives up and asks again: the
    /// next listener still gets the stream from the first byte (the time runs from the first
    /// bytes, not the connection), not after a gap the pacing would leave (NUC, 2026-10-30).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_listener_that_waited_long_for_its_first_bytes_gives_them_all_back() {
        let fmt = AudioFormat::pcm(44_100, 16, 2);
        let out = PullOutput::new("held");
        let mut first = out.take_stream(Duration::from_secs(2)).await;
        let mut sink = out.open_now(fmt).unwrap();
        if first.is_none() {
            first = out.take_stream(Duration::from_secs(2)).await;
        }
        let mut first = first.unwrap();
        tokio::time::sleep(Duration::from_millis(3500)).await;
        let writer = std::thread::spawn(move || {
            for i in 0..8 {
                sink.write(&block(fmt, i * 4)).unwrap();
            }
            sink
        });
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
    }

    /// A renderer that played a few seconds and then drops (a network blip) doesn't hear them
    /// again: past [`PROBE_TIME`] from its first bytes, the next listener goes on from there.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_listener_that_played_a_while_is_not_given_it_again() {
        let fmt = AudioFormat::pcm(44_100, 16, 2);
        let out = PullOutput::new("played");
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
        tokio::time::sleep(Duration::from_millis(3500)).await;
        drop(first);
        let mut second = out.take_stream(Duration::from_secs(2)).await.expect("given back");
        assert_eq!(
            second.next_chunk().await.unwrap(),
            pack_pcm(&block(fmt, 8)),
            "goes on, no repeat"
        );
        drop(writer.join().unwrap());
    }

    /// A stopped renderer polls its old URI: a request waiting when the next stream opens is
    /// answered (superseded) and the fresh fetch gets the stream from its first byte; a listener
    /// that reconnects to the same stream still gets it. An uncontrolled output (the Stream
    /// zone) lets a waiting player have the stream it waited for.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_request_from_before_the_stream_does_not_take_it() {
        let fmt = AudioFormat::pcm(44_100, 16, 2);
        let out = Arc::new(PullOutput::controlled("rend"));
        // The last session: played and stopped
        let mut sink = out.open_now(fmt).unwrap();
        let w = std::thread::spawn(move || {
            sink.write(&block(fmt, 0)).unwrap();
            sink
        });
        let mut live = out.take_stream(Duration::from_secs(2)).await.unwrap();
        assert!(live.next_chunk().await.is_some());
        drop(w.join().unwrap());
        assert_eq!(live.next_chunk().await, None);
        drop(live);
        // The renderer polls while stopped; then the node opens the next stream
        let o = out.clone();
        let stale =
            tokio::spawn(async move { o.take_stream_for(Duration::from_secs(5), None).await.map(|_| ()) });
        tokio::time::sleep(Duration::from_millis(100)).await;
        let mut sink = out.open_now(fmt).unwrap();
        let w = std::thread::spawn(move || {
            sink.write(&block(fmt, 40)).unwrap();
            sink
        });
        let missed = tokio::time::timeout(Duration::from_secs(1), stale)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(missed.err(), Some(Missed::Superseded));
        let mut fresh = out.take_stream_for(Duration::from_secs(2), None).await.unwrap();
        assert_eq!(fresh.next_chunk().await.unwrap(), pack_pcm(&block(fmt, 40)));
        drop(w.join().unwrap());
        // The Stream zone: a player that waited gets the stream it waited for
        let open = Arc::new(PullOutput::new("open"));
        let o = open.clone();
        let waiting =
            tokio::spawn(async move { o.take_stream_for(Duration::from_secs(5), None).await.is_ok() });
        tokio::time::sleep(Duration::from_millis(100)).await;
        let sink = open.open_now(fmt).unwrap();
        assert!(waiting.await.unwrap(), "the waiting player gets it");
        drop(sink);
    }

    /// A renderer's URL names its stream: a request for another one, older or from before a
    /// restart (a higher number than any now), is answered and never takes the stream; the
    /// one it names, it gets (review of #181).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_request_naming_another_stream_never_takes_this_one() {
        let fmt = AudioFormat::pcm(44_100, 16, 2);
        let out = PullOutput::controlled("named");
        let sink = out.open_now(fmt).unwrap();
        let g = out.generation();
        for other in [g - 1, g + 1_000_000, 5] {
            assert_eq!(
                out.take_stream_for(Duration::from_secs(1), Some(other))
                    .await
                    .err(),
                Some(Missed::Superseded),
                "g={other}"
            );
        }
        assert!(out.take_stream_for(Duration::from_secs(1), Some(g)).await.is_ok());
        drop(sink);
        // A new node (a restart) does not start where the old one did
        assert_ne!(PullOutput::controlled("named").generation(), 0);
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
        assert!(
            took >= REATTACH_GRACE - Duration::from_millis(50) && took < LISTENER_TIMEOUT,
            "{took:?}"
        );
    }

    /// A stream nobody will fetch (the renderer refused Play) fails its sink at once.
    #[test]
    fn an_aborted_stream_fails_its_sink_with_the_reason() {
        let fmt = AudioFormat::pcm(44_100, 16, 2);
        let out = PullOutput::new("a");
        let mut sink = out.open_now(fmt).unwrap();
        let g = out.generation();
        out.abort(g + 1, "not this one".into());
        sink.write(&block(fmt, 0)).unwrap();
        out.abort(g, "the renderer refused Play".into());
        let started = std::time::Instant::now();
        let e = sink.write(&block(fmt, 4)).unwrap_err();
        assert!(e.to_string().contains("refused Play"), "{e}");
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    /// A listener that takes everything at once still gets it at about real time (5 s ahead at
    /// most), and drain waits until it could have played it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_fast_listener_is_paced_to_real_time() {
        let fmt = AudioFormat::pcm(44_100, 16, 1); // 44 100 frames = 1 s
        let out = PullOutput::new("fast");
        let mut sink = out.open_now(fmt).unwrap();
        let mut live = out.take_stream(Duration::from_secs(2)).await.unwrap();
        let reader = tokio::spawn(async move { while live.next_chunk().await.is_some() {} });
        let started = std::time::Instant::now();
        let writer = std::thread::spawn(move || {
            for _ in 0..7 {
                sink.write(&AudioBlock::pcm_i32(fmt, vec![0; 44_100])).unwrap();
            }
            let written = started.elapsed();
            sink.drain().unwrap();
            (written, started.elapsed())
        });
        let (written, drained) = writer.join().unwrap();
        // 7 s of audio: the writes may run 5 s ahead, so they take about 2 s
        assert!(
            written >= Duration::from_millis(1_800) && written < Duration::from_secs(4),
            "{written:?}"
        );
        // drain: until the 7 s could have played, plus the margin
        assert!(drained >= Duration::from_millis(6_800), "{drained:?}");
        reader.abort();
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
