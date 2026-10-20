//! The audio thread behind a [`crate::zone::Zone`].
//!
//! One `std` thread per zone: sinks block, and the DAC's clock paces the
//! loop through `AudioSink::write` (§6.1). Between blocks it polls the
//! command channel; while stopped or paused it sleeps on it.

use crate::queue::{Queue, QueueItem};
use crate::sink::{AudioSink, Output};
use crate::source::{FrameSource, Processor, SourceError, SourceFactory};
use crate::zone::{NowPlaying, Transport, ZoneEvent, ZoneId, ZoneState};
use pm_model::{AudioFormat, SampleKind};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, watch};

/// How often the position in the published state is refreshed while playing.
const POSITION_INTERVAL: Duration = Duration::from_millis(250);
/// "Previous" within this many ms of the start goes to the previous track;
/// later it restarts the current one.
const PREVIOUS_RESTART_MS: u64 = 3_000;

#[derive(Debug)]
pub(crate) enum Command {
    Play(Option<usize>),
    Pause,
    TogglePause,
    Stop,
    Next,
    Previous,
    Seek(u64),
    QueueChanged,
    Shutdown,
}

struct Current {
    index: usize,
    item: QueueItem,
    source: Box<dyn FrameSource>,
    proc: Box<dyn Processor>,
    /// Position at the last seek/resume, in ms.
    base_ms: u64,
    /// `sink.frames_written()` at the last seek/resume (or track start).
    start_frame: u64,
    paused_at_ms: Option<u64>,
    /// Latest ICY title from the source, for radio.
    stream_title: Option<String>,
}

/// An opened track and its negotiated chain.
type Opened = (Box<dyn FrameSource>, Box<dyn Processor>);

struct Prefetched {
    index: usize,
    item: QueueItem,
    result: Result<Opened, SourceError>,
}

struct Engine {
    id: ZoneId,
    output: Arc<dyn Output>,
    factory: Arc<dyn SourceFactory>,
    queue: Arc<Mutex<Queue>>,
    cmds: mpsc::Receiver<(u64, Command)>,
    state: watch::Sender<ZoneState>,
    events: broadcast::Sender<ZoneEvent>,
    sink: Option<Box<dyn AudioSink>>,
    current: Option<Current>,
    prefetch: Option<Prefetched>,
    transport: Transport,
    error: Option<String>,
    acked: u64,
    last_position_publish: Instant,
    /// A transition happened inside `step` (track change, stop, error): publish before
    /// the next block instead of waiting for the position tick.
    dirty: bool,
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn(
    id: ZoneId,
    output: Arc<dyn Output>,
    factory: Arc<dyn SourceFactory>,
    queue: Arc<Mutex<Queue>>,
    cmds: mpsc::Receiver<(u64, Command)>,
    state: watch::Sender<ZoneState>,
    events: broadcast::Sender<ZoneEvent>,
) {
    let name = format!("zone-{id}");
    let engine = Engine {
        id,
        output,
        factory,
        queue,
        cmds,
        state,
        events,
        sink: None,
        current: None,
        prefetch: None,
        transport: Transport::Stopped,
        error: None,
        acked: 0,
        last_position_publish: Instant::now(),
        dirty: false,
    };
    std::thread::Builder::new()
        .name(name)
        .spawn(move || engine.run())
        .expect("spawn zone thread");
}

/// Whole frames of `fmt` to milliseconds (DSD frames are one byte = 8 bits per channel).
pub(crate) fn frames_to_ms(fmt: AudioFormat, frames: u64) -> u64 {
    let rate = fmt.rate_hz.max(1) as u64;
    match fmt.kind {
        SampleKind::Pcm => frames * 1000 / rate,
        SampleKind::Dsd => frames * 8 * 1000 / rate,
    }
}

enum Flow {
    Continue,
    Exit,
}

impl Engine {
    fn run(mut self) {
        loop {
            let cmd = if self.transport == Transport::Playing {
                match self.cmds.try_recv() {
                    Ok(c) => Some(c),
                    Err(mpsc::TryRecvError::Empty) => None,
                    Err(mpsc::TryRecvError::Disconnected) => break,
                }
            } else {
                match self.cmds.recv() {
                    Ok(c) => Some(c),
                    Err(_) => break,
                }
            };
            if let Some((seq, cmd)) = cmd {
                let flow = self.handle(cmd);
                if seq != u64::MAX {
                    self.acked = self.acked.max(seq);
                }
                self.publish();
                if matches!(flow, Flow::Exit) {
                    break;
                }
                continue;
            }
            if self.transport == Transport::Playing {
                self.step();
                if self.dirty {
                    self.publish();
                }
            }
        }
        self.teardown();
    }

    fn teardown(&mut self) {
        self.current = None;
        self.prefetch = None;
        if let Some(mut s) = self.sink.take() {
            let _ = s.flush();
        }
        self.transport = Transport::Stopped;
        self.publish();
    }

    fn emit(&self, ev: ZoneEvent) {
        let _ = self.events.send(ev);
    }

    // ---- commands ----

    fn handle(&mut self, cmd: Command) -> Flow {
        match cmd {
            Command::Shutdown => return Flow::Exit,
            Command::QueueChanged => {
                // The next item may have changed; prefetch again lazily.
                self.prefetch = None;
                self.emit(ZoneEvent::QueueChanged);
            }
            Command::Play(None) => match self.transport {
                Transport::Paused => self.resume(),
                Transport::Playing => {}
                Transport::Stopped => {
                    let idx = {
                        let q = self.queue.lock().expect("queue poisoned");
                        q.current_index().or((!q.is_empty()).then_some(0))
                    };
                    match idx {
                        Some(i) => self.start_index(i),
                        None => self.error = Some("queue is empty".into()),
                    }
                }
            },
            Command::Play(Some(i)) => self.start_index(i),
            Command::Pause => {
                if self.transport == Transport::Playing {
                    self.pause();
                }
            }
            Command::TogglePause => match self.transport {
                Transport::Playing => self.pause(),
                Transport::Paused => self.resume(),
                Transport::Stopped => return self.handle(Command::Play(None)),
            },
            Command::Stop => self.stop(),
            Command::Next => {
                let next = self.queue.lock().expect("queue poisoned").after_skip();
                match next {
                    Some(i) => self.start_index(i),
                    None => self.stop(),
                }
            }
            Command::Previous => {
                if self.current.is_none() {
                    return Flow::Continue;
                }
                if self.position_ms() > PREVIOUS_RESTART_MS {
                    self.seek(0);
                } else {
                    let prev = self.queue.lock().expect("queue poisoned").before();
                    match prev {
                        Some(i) => self.start_index(i),
                        None => self.seek(0),
                    }
                }
            }
            Command::Seek(ms) => self.seek(ms),
        }
        Flow::Continue
    }

    /// Start the queue item at `index` from the top, skipping unopenable
    /// items (at most one pass over the queue).
    fn start_index(&mut self, index: usize) {
        self.abort_current();
        let len = self.queue.lock().expect("queue poisoned").len();
        let mut index = index;
        for _ in 0..len.max(1) {
            let Some(item) = self.queue.lock().expect("queue poisoned").get(index).cloned() else {
                break;
            };
            match self.open(&item) {
                Ok((source, proc)) => {
                    if self.begin(index, item, source, proc) {
                        return;
                    }
                    // Sink could not open: error is set, stop.
                    self.stop();
                    return;
                }
                Err(e) => {
                    self.emit(ZoneEvent::Skipped {
                        item,
                        reason: e.to_string(),
                    });
                    let next = {
                        let mut q = self.queue.lock().expect("queue poisoned");
                        q.set_current(Some(index));
                        q.after_skip()
                    };
                    match next {
                        Some(i) => index = i,
                        None => break,
                    }
                }
            }
        }
        self.error = Some("nothing in the queue could be played".into());
        self.stop();
    }

    fn pause(&mut self) {
        let pos = self.position_ms();
        if let Some(s) = self.sink.as_mut() {
            if let Err(e) = s.flush() {
                self.fail(format!("flush: {e}"));
                return;
            }
        }
        if let Some(c) = self.current.as_mut() {
            c.paused_at_ms = Some(pos);
        }
        self.transport = Transport::Paused;
        self.dirty = true;
        self.emit(ZoneEvent::Paused { position_ms: pos });
    }

    fn resume(&mut self) {
        let Some(c) = self.current.as_mut() else {
            return;
        };
        let at = c.paused_at_ms.take().unwrap_or(0);
        if let Err(e) = c.source.seek_ms(at) {
            self.fail(format!("resume: {e}"));
            return;
        }
        c.base_ms = at;
        c.start_frame = self.sink.as_ref().map(|s| s.frames_written()).unwrap_or(0);
        self.transport = Transport::Playing;
        self.dirty = true;
        self.emit(ZoneEvent::Resumed);
    }

    fn seek(&mut self, ms: u64) {
        let Some(c) = self.current.as_mut() else {
            return;
        };
        let ms = match c.source.duration_ms() {
            Some(d) => ms.min(d),
            None => ms,
        };
        if let Some(s) = self.sink.as_mut() {
            if let Err(e) = s.flush() {
                self.fail(format!("flush: {e}"));
                return;
            }
        }
        if let Err(e) = c.source.seek_ms(ms) {
            self.fail(format!("seek: {e}"));
            return;
        }
        c.base_ms = ms;
        c.start_frame = self.sink.as_ref().map(|s| s.frames_written()).unwrap_or(0);
        if c.paused_at_ms.is_some() {
            c.paused_at_ms = Some(ms);
        }
        self.dirty = true;
        self.emit(ZoneEvent::Seeked { position_ms: ms });
    }

    /// Stop now: discard buffered audio and close the sink.
    fn stop(&mut self) {
        self.abort_current();
        if let Some(mut s) = self.sink.take() {
            let _ = s.flush();
        }
        if self.transport != Transport::Stopped {
            self.transport = Transport::Stopped;
            self.emit(ZoneEvent::Stopped);
        }
        self.dirty = true;
    }

    /// Record `msg`, stop, and tell listeners.
    fn fail(&mut self, msg: String) {
        tracing::error!(zone = %self.id, "{msg}");
        self.error = Some(msg.clone());
        self.dirty = true;
        self.emit(ZoneEvent::Error(msg));
        self.stop();
    }

    fn abort_current(&mut self) {
        self.current = None;
        self.prefetch = None;
        if let Some(s) = self.sink.as_mut() {
            let _ = s.flush();
        }
    }

    // ---- playback ----

    fn open(&self, item: &QueueItem) -> Result<Opened, SourceError> {
        let source = self.factory.open(item)?;
        let proc = self
            .factory
            .plan(source.as_ref(), self.output.caps(), &self.output.id())?;
        Ok((source, proc))
    }

    /// Make `self.sink` an open sink in `fmt`. `drain` plays out a sink
    /// being replaced (format change at a track boundary); otherwise its
    /// buffer is discarded.
    fn ensure_sink(&mut self, fmt: AudioFormat, drain: bool) -> bool {
        if self.sink.as_ref().is_some_and(|s| s.format() == fmt) {
            return true;
        }
        if let Some(mut s) = self.sink.take() {
            let r = if drain { s.drain() } else { s.flush() };
            if let Err(e) = r {
                tracing::warn!(zone = %self.id, "closing sink: {e}");
            }
        }
        match block_on(self.output.open(fmt)) {
            Ok(s) => {
                self.sink = Some(s);
                true
            }
            Err(e) => {
                self.fail(format!("open {} in {fmt}: {e}", self.output.id()));
                false
            }
        }
    }

    /// Switch to `item` as the current track. Returns false if the sink
    /// could not be opened (error already recorded).
    fn begin(
        &mut self,
        index: usize,
        item: QueueItem,
        source: Box<dyn FrameSource>,
        proc: Box<dyn Processor>,
    ) -> bool {
        let fmt = proc.output_format();
        let gapless = self.sink.as_ref().is_some_and(|s| s.format() == fmt) && self.current.is_some();
        if !self.ensure_sink(fmt, true) {
            return false;
        }
        let start_frame = self.sink.as_ref().map(|s| s.frames_written()).unwrap_or(0);
        self.queue
            .lock()
            .expect("queue poisoned")
            .set_current(Some(index));
        self.emit(ZoneEvent::TrackStarted {
            item: item.clone(),
            index,
            source_format: proc.source_format(),
            output_format: fmt,
            signal_path: proc.signal_path().clone(),
            gapless,
        });
        self.current = Some(Current {
            index,
            item,
            source,
            proc,
            base_ms: 0,
            start_frame,
            paused_at_ms: None,
            stream_title: None,
        });
        self.prefetch = None;
        self.error = None;
        self.transport = Transport::Playing;
        self.dirty = true;
        true
    }

    /// Decode, process and write one block; handle the end of the track.
    fn step(&mut self) {
        let Some(cur) = self.current.as_mut() else {
            self.transport = Transport::Stopped;
            return;
        };
        match cur.source.next_block() {
            Ok(Some(block)) => {
                let out = match cur.proc.process(block) {
                    Ok(b) => b,
                    Err(e) => return self.fail(format!("process: {e}")),
                };
                if !out.is_empty() {
                    let Some(sink) = self.sink.as_mut() else {
                        return self.fail("sink closed while playing".into());
                    };
                    if let Err(e) = sink.write(&out) {
                        return self.fail(format!("write to {}: {e}", self.output.id()));
                    }
                }
                self.maybe_prefetch();
                let title = self.current.as_mut().and_then(|c| c.source.stream_title());
                if let Some(t) = title {
                    if let Some(c) = self.current.as_mut() {
                        c.stream_title = Some(t.clone());
                    }
                    self.emit(ZoneEvent::StreamTitle { title: t });
                    self.dirty = true;
                }
                if self.last_position_publish.elapsed() >= POSITION_INTERVAL {
                    self.publish();
                }
            }
            Ok(None) => self.end_of_track(),
            Err(e) => {
                tracing::warn!(zone = %self.id, "decode error, ending track: {e}");
                self.emit(ZoneEvent::Error(format!("decode: {e}")));
                self.end_of_track();
            }
        }
    }

    /// Open the next item once per track so the switch at the end is instant.
    fn maybe_prefetch(&mut self) {
        if self.prefetch.is_some() {
            return;
        }
        let Some(cur) = self.current.as_ref() else {
            return;
        };
        let next = {
            let q = self.queue.lock().expect("queue poisoned");
            q.after_end(cur.item.id)
                .and_then(|i| q.get(i).cloned().map(|it| (i, it)))
        };
        if let Some((index, item)) = next {
            let result = self.open(&item);
            self.prefetch = Some(Prefetched { index, item, result });
        }
    }

    fn end_of_track(&mut self) {
        let Some(mut cur) = self.current.take() else {
            return;
        };
        // Stages with delay (resampler) hand back their tail now.
        match cur.proc.finish() {
            Ok(Some(tail)) if !tail.is_empty() => {
                if let Some(sink) = self.sink.as_mut() {
                    if let Err(e) = sink.write(&tail) {
                        tracing::warn!(zone = %self.id, "writing tail: {e}");
                    }
                }
            }
            Ok(_) => {}
            Err(e) => tracing::warn!(zone = %self.id, "finish: {e}"),
        }
        let verify_ok = cur.source.finalize();
        self.emit(ZoneEvent::TrackEnded {
            item: cur.item.clone(),
            verify_ok,
        });
        // Keep `current` set while the next track begins so `begin` knows the join is gapless.
        self.current = Some(cur);
        let played_id = self.current.as_ref().map(|c| c.item.id).unwrap_or(0);
        let mut next = self.queue.lock().expect("queue poisoned").after_end(played_id);
        let len = self.queue.lock().expect("queue poisoned").len();
        for _ in 0..len.max(1) {
            let Some(index) = next else { break };
            let Some(item) = self.queue.lock().expect("queue poisoned").get(index).cloned() else {
                break;
            };
            let opened = match self.prefetch.take() {
                Some(p) if p.index == index && p.item.id == item.id => p.result,
                _ => self.open(&item),
            };
            match opened {
                Ok((source, proc)) => {
                    // `begin` drains the sink only when the format changes.
                    if !self.begin(index, item, source, proc) {
                        self.stop();
                    }
                    return;
                }
                Err(e) => {
                    self.emit(ZoneEvent::Skipped {
                        item,
                        reason: e.to_string(),
                    });
                    let mut q = self.queue.lock().expect("queue poisoned");
                    q.set_current(Some(index));
                    next = q.after_skip();
                }
            }
        }
        // End of the queue: let the tail play out, then close.
        self.current = None;
        self.prefetch = None;
        if let Some(mut s) = self.sink.take() {
            if let Err(e) = s.drain() {
                tracing::warn!(zone = %self.id, "drain: {e}");
            }
        }
        self.queue.lock().expect("queue poisoned").set_current(None);
        self.transport = Transport::Stopped;
        self.dirty = true;
        self.emit(ZoneEvent::Stopped);
    }

    // ---- state ----

    fn position_ms(&self) -> u64 {
        let Some(cur) = &self.current else {
            return 0;
        };
        if let Some(p) = cur.paused_at_ms {
            return p;
        }
        let Some(sink) = &self.sink else {
            return cur.base_ms;
        };
        let written = sink.frames_written().saturating_sub(cur.start_frame);
        let queued = sink.buffer_level().queued_frames;
        cur.base_ms + frames_to_ms(cur.proc.output_format(), written.saturating_sub(queued))
    }

    fn publish(&mut self) {
        self.last_position_publish = Instant::now();
        self.dirty = false;
        let (queue, queue_index, queue_version, repeat, radio) = {
            let q = self.queue.lock().expect("queue poisoned");
            (
                q.items().to_vec(),
                q.current_index(),
                q.version(),
                q.repeat(),
                q.radio(),
            )
        };
        let now_playing = self.current.as_ref().map(|c| NowPlaying {
            item: c.item.clone(),
            index: c.index,
            position_ms: self.position_ms(),
            duration_ms: c.source.duration_ms(),
            source_format: c.proc.source_format(),
            output_format: c.proc.output_format(),
            signal_path: c.proc.signal_path().clone(),
            members: self.sink.as_ref().map(|s| s.members()).unwrap_or_default(),
            stream_title: c.stream_title.clone(),
        });
        self.state.send_modify(|s| {
            s.transport = self.transport;
            s.now_playing = now_playing;
            s.queue = queue;
            s.queue_index = queue_index;
            s.queue_version = queue_version;
            s.repeat = repeat;
            s.radio = radio;
            s.error = self.error.clone();
            s.acked = self.acked;
        });
    }
}

/// Run a future to completion on this thread. The futures here (`Output::open`)
/// are short and do their blocking work synchronously, so no runtime is needed.
fn block_on<F: std::future::Future>(fut: F) -> F::Output {
    use std::pin::pin;
    use std::sync::Arc as StdArc;
    use std::task::{Context, Poll, Wake, Waker};

    struct ThreadWaker(std::thread::Thread);
    impl Wake for ThreadWaker {
        fn wake(self: StdArc<Self>) {
            self.0.unpark();
        }
    }
    let waker = Waker::from(StdArc::new(ThreadWaker(std::thread::current())));
    let mut cx = Context::from_waker(&waker);
    let mut fut = pin!(fut);
    loop {
        match fut.as_mut().poll(&mut cx) {
            Poll::Ready(v) => return v,
            Poll::Pending => std::thread::park(),
        }
    }
}
