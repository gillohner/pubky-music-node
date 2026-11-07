//! The audio thread behind a [`crate::zone::Zone`].
//!
//! One `std` thread per zone: sinks block, and the DAC's clock paces the
//! loop through `AudioSink::write` (§6.1). Between blocks it polls the
//! command channel; while stopped or paused it sleeps on it.

use crate::queue::{Queue, QueueItem};
use crate::sink::{AudioSink, Output};
use crate::source::{FrameSource, Processor, SourceError, SourceFactory, StreamInfo};
use crate::zone::{NowPlaying, Transport, ZoneEvent, ZoneId, ZoneState};
use pm_model::{AudioBlock, AudioFormat, PathStage, Quality, SampleKind, Samples};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
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
    /// The output's level as read back; published in the state (controllers hear of it after
    /// every command, as of every other).
    Volume(Option<pm_model::ZoneVolume>),
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
    /// Fading out into the next track: the signal path shows a `crossfade` stage.
    fading: bool,
    /// What is streaming (codec, bitrate, service), worked out once when the track starts.
    format: Option<StreamInfo>,
    /// Where the track was before a seek that hasn't given a block yet: a stream that took the
    /// seek and then fails to read goes on from there instead of ending.
    seek_from: Option<u64>,
}

/// What a source is streaming, with the service taken from the item's uri when the source
/// leaves it empty. Once per track, never on the audio thread's publish.
fn stream_info_of(source: &dyn FrameSource, item: &QueueItem) -> Option<StreamInfo> {
    source.stream_info().map(|mut f| {
        if f.service.is_empty() {
            f.service = service_of(&item.uri).to_string();
        }
        f
    })
}

/// The next track while it fades in over the end of the current one (crossfade).
struct Incoming {
    index: usize,
    item: QueueItem,
    source: Box<dyn FrameSource>,
    proc: Box<dyn Processor>,
    /// Its processed samples not mixed yet (interleaved, the sink's format).
    buf: VecDeque<i32>,
    /// Frames taken from it so far (mixed or waiting in `buf`).
    pulled: u64,
    /// It ended (or failed) before the fade did.
    ended: bool,
    /// The fade's length and how far it has gone, in frames.
    fade: u64,
    done: u64,
}

/// An opened track and its negotiated chain.
type Opened = (Box<dyn FrameSource>, Box<dyn Processor>);

/// The next item, opened ahead on its own thread so a stream that is slow to answer never
/// holds the audio thread (which keeps serving commands and the sink meanwhile).
struct Prefetched {
    index: usize,
    item: QueueItem,
    result: Prefetch,
}

enum Prefetch {
    /// The thread hands its result over this; a thread that panics drops it.
    Opening(mpsc::Receiver<Result<Opened, SourceError>>),
    Done(Result<Opened, SourceError>),
}

/// How long the crossfade point waits for a next item still opening before it gives up on the
/// crossfade (the item then opens at the ordinary boundary). A local file or a stream that
/// answers is open long before; one that doesn't answer is not waited for.
const PREFETCH_SETTLE: Duration = Duration::from_millis(500);

/// How often a track's end, waiting for the next item still opening, looks for a command.
const PREFETCH_POLL: Duration = Duration::from_millis(50);

impl Prefetched {
    fn abandoned(&self) -> Result<Opened, SourceError> {
        Err(SourceError::Open {
            uri: self.item.uri.clone(),
            reason: "the open ended without an answer".into(),
        })
    }

    /// Take the thread's result, waiting at most `for_` for it; `true` when the open has ended.
    fn settle(&mut self, for_: Duration) -> bool {
        if let Prefetch::Opening(rx) = &self.result {
            let got = match rx.recv_timeout(for_) {
                Ok(r) => r,
                Err(mpsc::RecvTimeoutError::Timeout) => return false,
                Err(mpsc::RecvTimeoutError::Disconnected) => self.abandoned(),
            };
            self.result = Prefetch::Done(got);
        }
        true
    }

    /// The result, waiting for the open to end.
    fn wait(self) -> Result<Opened, SourceError> {
        match self.result {
            Prefetch::Done(r) => r,
            Prefetch::Opening(ref rx) => rx.recv().unwrap_or_else(|_| self.abandoned()),
        }
    }
}

struct Engine {
    id: ZoneId,
    output: Arc<dyn Output>,
    factory: Arc<dyn SourceFactory>,
    queue: Arc<Mutex<Queue>>,
    cmds: mpsc::Receiver<(u64, Command)>,
    /// Commands taken off the channel while a pass over unopenable items was cut short: served
    /// before anything else.
    deferred: std::collections::VecDeque<(u64, Command)>,
    state: watch::Sender<ZoneState>,
    events: broadcast::Sender<ZoneEvent>,
    sink: Option<Box<dyn AudioSink>>,
    current: Option<Current>,
    prefetch: Option<Prefetched>,
    /// Crossfade length in ms (0 = off), set from the zone.
    crossfade: Arc<AtomicU64>,
    incoming: Option<Incoming>,
    /// The queue item whose end was already checked for a crossfade (once per track).
    xfade_checked: Option<u64>,
    transport: Transport,
    error: Option<String>,
    acked: u64,
    last_position_publish: Instant,
    /// A transition happened inside `step` (track change, stop, error): publish before
    /// the next block instead of waiting for the position tick.
    dirty: bool,
    /// The runtime of whoever made the zone. `Output::open` runs inside it: outputs use
    /// `spawn_blocking` and timers, which panic on a thread outside a runtime.
    runtime: Option<tokio::runtime::Handle>,
    /// The sequence number of the command being handled (for a panic in it).
    handling: u64,
    /// The sink's device was paused holding its buffer ([`AudioSink::pause`] returned true): it
    /// resumes before anything plays on it, or closes when that buffer is stale.
    sink_paused: bool,
    /// The device went away while the old sink played out at a song boundary (a USB DAC
    /// unplugged): the next song starts paused, on no other device.
    pending_gone: Option<String>,
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
    crossfade: Arc<AtomicU64>,
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
        deferred: std::collections::VecDeque::new(),
        crossfade,
        incoming: None,
        xfade_checked: None,
        transport: Transport::Stopped,
        error: None,
        acked: 0,
        last_position_publish: Instant::now(),
        dirty: false,
        runtime: tokio::runtime::Handle::try_current().ok(),
        handling: 0,
        sink_paused: false,
        pending_gone: None,
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

/// What a caught panic said.
fn panic_text(p: &(dyn std::any::Any + Send)) -> String {
    p.downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| p.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "unknown panic".into())
}

impl Engine {
    /// Serve commands until the zone goes away. A panic (a bug in an output, a source or the
    /// engine) does not leave a dead zone: the zone stops, says why, and takes commands again.
    fn run(mut self) {
        loop {
            let served = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.serve()));
            // The command that panicked counts as handled, so nobody waits for it
            self.acked = self.acked.max(self.handling);
            match served {
                Ok(()) => break,
                Err(p) => self.recover(&panic_text(p.as_ref())),
            }
        }
        self.teardown();
    }

    /// After a panic: drop the track and the sink, stop, and report it as the zone's error.
    fn recover(&mut self, why: &str) {
        tracing::error!(zone = %self.id, "the zone's audio thread panicked; the zone stopped and starts again: {why}");
        // The queue lock may have been held by the code that panicked; its data is still whole
        self.queue.clear_poison();
        let (current, prefetch, incoming, sink) = (
            self.current.take(),
            self.prefetch.take(),
            self.incoming.take(),
            self.sink.take(),
        );
        // A sink or source that panics again while closing must not take the thread down
        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            drop((current, prefetch, incoming, sink));
        }))
        .is_err()
        {
            tracing::warn!(zone = %self.id, "closing the sink after the panic panicked too");
        }
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.output.now_playing(None)));
        let message = format!("the zone stopped on an internal error ({why}); press play to try again");
        self.error = Some(message.clone());
        self.emit(ZoneEvent::Error { message });
        if self.transport != Transport::Stopped {
            self.transport = Transport::Stopped;
            self.emit(ZoneEvent::Stopped);
        }
        self.dirty = true;
        self.publish();
    }

    fn serve(&mut self) {
        loop {
            let cmd = if let Some(c) = self.deferred.pop_front() {
                Some(c)
            } else if self.transport == Transport::Playing {
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
                if seq != u64::MAX {
                    self.handling = seq;
                }
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
    }

    fn teardown(&mut self) {
        self.current = None;
        self.prefetch = None;
        self.incoming = None;
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
                // A fade in progress goes on only while its track is still the one that plays
                // after the current one ("Play next", a removal or a repeat change may differ).
                if let (Some(cur), Some(inc)) = (self.current.as_ref(), self.incoming.as_ref()) {
                    let still_next = {
                        let q = self.queue.lock().expect("queue poisoned");
                        q.after_end(cur.item.id).and_then(|i| q.get(i)).map(|i| i.id) == Some(inc.item.id)
                    };
                    if !still_next {
                        self.cancel_crossfade();
                    }
                }
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
            Command::Volume(level) => self.state.send_modify(|s| s.volume = level),
        }
        Flow::Continue
    }

    /// Whether a command that replaces what plays (Stop, Next, Play of a queue item, Shutdown)
    /// has arrived: a pass over items that are slow to fail (streams that don't answer) ends
    /// there, and the commands are served next. Every other command (Pause, the toggle,
    /// Previous, Play, volume, seek, queue edits) is kept and served after the pass, once a track
    /// plays or the pass has ended, as before.
    fn interrupted(&mut self) -> bool {
        while let Ok(c) = self.cmds.try_recv() {
            self.deferred.push_back(c);
        }
        self.deferred.iter().any(|(_, c)| {
            matches!(
                c,
                Command::Stop | Command::Next | Command::Play(Some(_)) | Command::Shutdown
            )
        })
    }

    /// Start the queue item at `index` from the top, skipping unopenable
    /// items (at most one pass over the queue). A Stop, Next or Play that arrives while an
    /// item is being skipped ends the pass.
    fn start_index(&mut self, index: usize) {
        self.abort_current();
        let len = self.queue.lock().expect("queue poisoned").len();
        let mut index = index;
        let mut last_reason: Option<String> = None;
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
                    last_reason = Some(e.to_string());
                    self.emit(ZoneEvent::Skipped {
                        item,
                        reason: e.to_string(),
                    });
                    let next = {
                        let mut q = self.queue.lock().expect("queue poisoned");
                        q.set_current(Some(index));
                        q.after_skip()
                    };
                    // A command that replaces what plays arrived while that item was failing:
                    // the pass ends here, the queue's current at the item that failed (Next then
                    // goes to the one after it, never past an item nobody tried)
                    if self.interrupted() {
                        return;
                    }
                    match next {
                        Some(i) => index = i,
                        None => break,
                    }
                }
            }
        }
        self.error = Some(match last_reason {
            Some(r) => format!("nothing in the queue could be played; last error: {r}"),
            None => "nothing in the queue could be played".into(),
        });
        self.stop();
    }

    /// The output's device went away mid-track (a USB DAC unplugged from a phone): pause where the
    /// listener was, with the reason, and play on no other device. Play opens the output again
    /// (on the DAC once it is back).
    fn device_gone(&mut self, why: String) {
        let pos = self.position_ms();
        tracing::warn!(zone = %self.id, "device gone, pausing: {why}");
        self.cancel_crossfade();
        // The zone heard it (the sink may forget the loss); its buffer was for a device that is
        // not there: the next play opens a fresh sink
        if let Some(s) = self.sink.as_mut() {
            s.gone_reported();
        }
        self.sink = None;
        self.sink_paused = false;
        if let Some(c) = self.current.as_mut() {
            c.paused_at_ms = Some(pos);
        }
        self.transport = Transport::Paused;
        self.error = Some(format!("{why}: paused"));
        self.dirty = true;
        self.emit(ZoneEvent::Paused { position_ms: pos });
        self.emit(ZoneEvent::Error {
            message: format!("{why}: paused"),
        });
    }

    fn pause(&mut self) {
        let pos = self.position_ms();
        self.cancel_crossfade();
        // The sink silences the device itself where it buffers (UPnP, BluOS): only stopping the
        // writes left a renderer playing its seconds of buffer after the press.
        if let Some(s) = self.sink.as_mut() {
            match s.pause() {
                Ok(held) => self.sink_paused = held,
                // Its device went: paused there with the reason, not stopped
                Err(e) if e.gone_reason().is_some() => {
                    let why = e.gone_reason().unwrap_or_default();
                    self.device_gone(why);
                    return;
                }
                Err(e) => {
                    self.fail(format!("pause: {e}"));
                    return;
                }
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
        let Some(fmt) = self.current.as_ref().map(|c| c.proc.output_format()) else {
            return;
        };
        if !self.unpause_sink(fmt) {
            return;
        }
        // The output takes other formats now (its USB DAC went away during the pause): the track
        // is planned again for it, from where the listener was
        let src = self.current.as_ref().map(|c| c.source.format());
        if self.sink.is_none() && src.is_some_and(|s| !self.output.live_caps_for(s).accepts(&fmt)) {
            if let Some((index, at)) = self
                .current
                .as_ref()
                .map(|c| (c.index, c.paused_at_ms.unwrap_or(0)))
            {
                self.error = None;
                self.start_index(index);
                if at > 0 && self.current.is_some() {
                    self.seek(at);
                }
                return;
            }
        }
        // A seek while paused closed the paused device's sink: a fresh one starts here
        if self.sink.is_none() && !self.open_or_pause(fmt) {
            return;
        }
        let Some(c) = self.current.as_mut() else {
            return;
        };
        let at = c.paused_at_ms.take().unwrap_or(0);
        if let Err(e) = c.source.seek_ms(at) {
            // A live or forward-only stream (internet radio, a controller's HTTP flow): carry on
            // from where it is; only the audio buffered at the pause is skipped.
            tracing::info!(zone = %self.id, "resume without seeking back ({e})");
        }
        c.base_ms = at;
        c.start_frame = self.sink.as_ref().map(|s| s.frames_written()).unwrap_or(0);
        self.transport = Transport::Playing;
        // Playing again: a "device gone" pause is over
        self.error = None;
        self.dirty = true;
        self.emit(ZoneEvent::Resumed);
    }

    /// Resume a paused sink before it plays again. One that cannot carry on (its device
    /// dropped the stream during a long pause) opens again, from what was last heard: the
    /// paused position less what the device held unplayed. False when it could not open: the
    /// error is recorded and the zone stopped.
    fn unpause_sink(&mut self, fmt: AudioFormat) -> bool {
        if !std::mem::take(&mut self.sink_paused) {
            return true;
        }
        let Some(Err(e)) = self.sink.as_mut().map(|s| s.resume()) else {
            return true;
        };
        // Its device went during the pause: paused there, nothing opens elsewhere
        if let Some(why) = e.gone_reason() {
            self.device_gone(why);
            return false;
        }
        tracing::info!(zone = %self.id, "resume: opening the output again ({e})");
        let unheard = self
            .sink
            .as_ref()
            .map(|s| frames_to_ms(fmt, s.buffer_level().queued_frames))
            .unwrap_or(0);
        if let Some(c) = self.current.as_mut() {
            c.paused_at_ms = c.paused_at_ms.map(|p| p.saturating_sub(unheard));
        }
        if let Some(mut s) = self.sink.take() {
            let _ = s.flush();
        }
        self.open_or_pause(fmt)
    }

    /// Before writing `block`: a sink whose fixed-size stream can't take it (its track ran longer
    /// than it said, a resampler's tail, a crossfade's rest) is played out and a fresh one opened
    /// in its place, so nothing written is cut off at the stream's end. False when that could not
    /// open (the zone stopped).
    fn make_room(&mut self, block: &AudioBlock) -> bool {
        let full = self
            .sink
            .as_ref()
            .is_some_and(|s| s.room_frames().is_some_and(|r| r < block.frames() as u64));
        // A sink that may not carry on as it is (a phone's DAC that would go back to full level
        // without consent): a fresh one, planned as things are now
        let reopen = self.sink.as_ref().is_some_and(|s| s.reopen_now());
        !(full || reopen) || self.renew_full_sink(block.format, reopen)
    }

    /// The sink's fixed-size stream is full: play it out and go on on a fresh one from here. The
    /// current track goes on from what was written (the source is not rewound), not from what
    /// a device still holds. False when that could not open (the zone stopped).
    fn renew_full_sink(&mut self, fmt: AudioFormat, discard: bool) -> bool {
        tracing::info!(zone = %self.id, "the output's sink can't go on as it is; going on on a fresh one");
        let written = self.sink.as_ref().map(|s| s.frames_written()).unwrap_or(0);
        let at = self
            .current
            .as_ref()
            .map(|c| c.base_ms + frames_to_ms(c.proc.output_format(), written.saturating_sub(c.start_frame)));
        if let Some(mut s) = self.sink.take() {
            // A sink that may not go on is not played out (its queue is what may not play)
            let r = if discard { s.flush() } else { s.drain() };
            if let Err(e) = r {
                if let Some(why) = e.gone_reason() {
                    s.gone_reported();
                    self.device_gone(why);
                    return false;
                }
                tracing::warn!(zone = %self.id, "closing a full sink: {e}");
            }
        }
        if !self.open_or_pause(fmt) {
            return false;
        }
        let written = self.sink.as_ref().map(|s| s.frames_written()).unwrap_or(0);
        if let (Some(c), Some(at)) = (self.current.as_mut(), at) {
            c.base_ms = at;
            c.start_frame = written;
        }
        true
    }

    /// A sink paused on its device holds audio from before: a new track or a seek while
    /// paused closes it (the device stops and drops that audio), and the next play opens a
    /// fresh one.
    fn close_paused_sink(&mut self) {
        if std::mem::take(&mut self.sink_paused) {
            if let Some(mut s) = self.sink.take() {
                let _ = s.flush();
            }
        }
    }

    fn seek(&mut self, ms: u64) {
        let before = self.position_ms();
        self.cancel_crossfade();
        if self.transport == Transport::Paused {
            self.close_paused_sink();
        }
        let Some(c) = self.current.as_mut() else {
            return;
        };
        let ms = match c.source.duration_ms().or(c.item.duration_ms) {
            Some(d) => ms.min(d),
            None => ms,
        };
        if let Some(s) = self.sink.as_mut() {
            if let Err(e) = s.flush() {
                if let Some(why) = e.gone_reason() {
                    return self.device_gone(why);
                }
                self.fail(format!("flush: {e}"));
                return;
            }
        }
        if let Err(e) = c.source.seek_ms(ms) {
            // A stream that cannot seek keeps playing from where it is; the caller hears why.
            tracing::info!(zone = %self.id, "seek not possible: {e}");
            c.base_ms = before;
            c.start_frame = self.sink.as_ref().map(|s| s.frames_written()).unwrap_or(0);
            self.dirty = true;
            self.emit(ZoneEvent::Error {
                message: format!("this stream cannot seek ({e})"),
            });
            return;
        }
        c.base_ms = ms;
        c.start_frame = self.sink.as_ref().map(|s| s.frames_written()).unwrap_or(0);
        if c.paused_at_ms.is_some() {
            c.paused_at_ms = Some(ms);
        }
        c.seek_from = Some(c.seek_from.unwrap_or(before));
        self.dirty = true;
        self.emit(ZoneEvent::Seeked { position_ms: ms });
    }

    /// The source took a seek, then failed to read (a stream that can't seek but said it could:
    /// TASK-seek-segmented-streams-1104). The track is opened again and goes on from where it
    /// was before the seek, and the caller hears that this stream cannot seek. False when that
    /// isn't possible either (the track then ends as on any read error).
    fn recover_from_seek(&mut self, e: &SourceError) -> bool {
        let Some(c) = self.current.as_mut() else {
            return false;
        };
        let Some(before) = c.seek_from.take() else {
            return false;
        };
        let item = c.item.clone();
        let fresh = match self.factory.open(&item) {
            Ok(s) => s,
            Err(open) => {
                tracing::warn!(zone = %self.id, "reopening after a failed seek: {open}");
                return false;
            }
        };
        let Some(c) = self.current.as_mut() else {
            return false;
        };
        if fresh.format() != c.source.format() {
            return false;
        }
        let fps = u64::from(fresh.format().frames_per_second());
        tracing::info!(zone = %self.id, "the stream failed after a seek ({e}); going on from {before} ms");
        c.source = Box::new(crate::source::SkipFrames {
            inner: fresh,
            left: before * fps / 1000,
        });
        c.base_ms = before;
        c.start_frame = self.sink.as_ref().map(|s| s.frames_written()).unwrap_or(0);
        if c.paused_at_ms.is_some() {
            c.paused_at_ms = Some(before);
        }
        self.dirty = true;
        self.emit(ZoneEvent::Seeked { position_ms: before });
        self.emit(ZoneEvent::Error {
            message: format!("this stream cannot seek ({e})"),
        });
        true
    }

    /// Stop now: discard buffered audio and close the sink.
    fn stop(&mut self) {
        self.abort_current();
        self.sink_paused = false;
        if let Some(mut s) = self.sink.take() {
            let _ = s.flush();
        }
        if self.transport != Transport::Stopped {
            self.transport = Transport::Stopped;
            self.output.now_playing(None);
            self.emit(ZoneEvent::Stopped);
        }
        self.dirty = true;
    }

    /// Record `msg`, stop, and tell listeners.
    fn fail(&mut self, msg: String) {
        tracing::error!(zone = %self.id, "{msg}");
        self.error = Some(msg.clone());
        self.dirty = true;
        self.emit(ZoneEvent::Error { message: msg });
        self.stop();
    }

    fn abort_current(&mut self) {
        self.current = None;
        self.prefetch = None;
        self.incoming = None;
        if let Some(s) = self.sink.as_mut() {
            let _ = s.flush();
        }
    }

    // ---- playback ----

    fn open(&self, item: &QueueItem) -> Result<Opened, SourceError> {
        open_with(self.factory.as_ref(), self.output.as_ref(), item)
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
                // Its device went away meanwhile: nothing opens elsewhere (the phone's speaker)
                if let Some(why) = e.gone_reason() {
                    s.gone_reported();
                    self.sink_paused = false;
                    self.pending_gone = Some(why);
                    return false;
                }
                tracing::warn!(zone = %self.id, "closing sink: {e}");
            }
        }
        self.sink_paused = false;
        let _runtime = self.runtime.as_ref().map(|h| h.enter());
        match block_on(self.output.open(fmt)) {
            Ok(s) => {
                self.sink = Some(s);
                true
            }
            // Its device is gone (or not where the system plays): the same pause as a drain that
            // finds it gone, never a stopped zone
            Err(e) if e.gone_reason().is_some() => {
                self.pending_gone = e.gone_reason();
                false
            }
            Err(e) => {
                self.fail(format!("open {} in {fmt}: {e}", self.output.id()));
                false
            }
        }
    }

    /// [`Self::ensure_sink`] where a track is current: a device gone pauses it there.
    fn open_or_pause(&mut self, fmt: AudioFormat) -> bool {
        if self.ensure_sink(fmt, false) {
            return true;
        }
        if let Some(why) = self.pending_gone.take() {
            self.device_gone(why);
        }
        false
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
        let mut proc = proc;
        // The output's settings changed since its sink opened (a phone's DAC: bit-perfect or
        // shared), or it takes other formats now: this song opens afresh, planned for it now
        let stale = self.sink.as_ref().is_some_and(|s| s.reopen_at_track());
        let caps = self.output.live_caps_for(source.format());
        if self.incoming.is_none() && (stale || !caps.accepts(&proc.output_format())) {
            match self.factory.plan(source.as_ref(), &caps, &self.output.id()) {
                Ok(p) => proc = p,
                Err(e) => tracing::warn!(zone = %self.id, "planning again: {e}"),
            }
            if stale {
                if let Some(mut s) = self.sink.take() {
                    // A paused device never plays its buffer out: discarded, not waited for
                    let r = if self.sink_paused { s.flush() } else { s.drain() };
                    if let Err(e) = r {
                        if let Some(why) = e.gone_reason() {
                            s.gone_reported();
                            self.pending_gone = Some(why);
                        }
                    }
                }
                self.sink_paused = false;
            }
        }
        let fmt = proc.output_format();
        // A track started while paused (next, a queue item) starts on a fresh sink: the paused
        // device's buffer is the old track's
        self.close_paused_sink();
        // A sink of fixed size (a renderer's stream offered as a file) that can't take the whole
        // track is played out first: the track starts on a fresh stream instead of running past
        // the end of this one
        let duration = source.duration_ms().or(item.duration_ms);
        if self
            .sink
            .as_ref()
            .is_some_and(|s| s.format() == fmt && !crate::outputs::fits_in(s.room_frames(), fmt, duration))
        {
            if let Some(mut s) = self.sink.take() {
                if let Err(e) = s.drain() {
                    tracing::warn!(zone = %self.id, "closing a full sink: {e}");
                }
            }
        }
        let gapless = self.sink.as_ref().is_some_and(|s| s.format() == fmt) && self.current.is_some();
        self.output.now_playing(Some(&item));
        // A device gone at this boundary: the song starts paused, on no device
        let gone = match self.pending_gone.take() {
            Some(why) => Some(why),
            None if !self.ensure_sink(fmt, true) => match self.pending_gone.take() {
                Some(why) => Some(why),
                None => return false,
            },
            None => None,
        };
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
        let format = stream_info_of(source.as_ref(), &item);
        self.current = Some(Current {
            index,
            item,
            source,
            proc,
            base_ms: 0,
            start_frame,
            paused_at_ms: None,
            stream_title: None,
            fading: false,
            format,
            seek_from: None,
        });
        self.prefetch = None;
        self.error = None;
        self.transport = Transport::Playing;
        self.dirty = true;
        if let Some(why) = gone {
            self.device_gone(why);
        }
        true
    }

    /// Decode, process and write one block; handle the end of the track.
    fn step(&mut self) {
        let Some(cur) = self.current.as_mut() else {
            self.transport = Transport::Stopped;
            return;
        };
        let read = cur.source.next_block();
        if read.is_ok() {
            cur.seek_from = None;
        }
        match read {
            Err(e) if self.recover_from_seek(&e) => {}
            Ok(Some(block)) => {
                let Some(cur) = self.current.as_mut() else {
                    return;
                };
                let out = match cur.proc.process(block) {
                    Ok(b) => b,
                    Err(e) => return self.fail(format!("process: {e}")),
                };
                let out = if self.incoming.is_some() || self.start_crossfade(&out) {
                    self.mix(out)
                } else {
                    out
                };
                if !out.is_empty() {
                    if !self.make_room(&out) {
                        return;
                    }
                    let Some(sink) = self.sink.as_mut() else {
                        return self.fail("sink closed while playing".into());
                    };
                    if let Err(e) = sink.write(&out) {
                        if let Some(why) = e.gone_reason() {
                            return self.device_gone(why);
                        }
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
                self.emit(ZoneEvent::Error {
                    message: format!("decode: {e}"),
                });
                self.end_of_track();
            }
        }
    }

    /// Open the next item once per track, on its own thread, so the switch at the end is
    /// instant and a stream slow to answer never holds the audio thread.
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
            let (factory, output, it) = (self.factory.clone(), self.output.clone(), item.clone());
            let (tx, rx) = mpsc::sync_channel(1);
            let spawned = std::thread::Builder::new()
                .name(format!("prefetch-{}", self.id))
                .spawn(move || {
                    let _ = tx.send(open_with(factory.as_ref(), output.as_ref(), &it));
                });
            let result = match spawned {
                Ok(_) => Prefetch::Opening(rx),
                Err(_) => Prefetch::Done(self.open(&item)),
            };
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
                let tail = if self.incoming.is_some() {
                    self.mix(tail)
                } else {
                    tail
                };
                if !self.make_room(&tail) {
                    return;
                }
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
        if self.incoming.is_some() {
            self.finish_crossfade();
            return;
        }
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
                Some(mut p) if p.index == index && p.item.id == item.id => {
                    // The next item opened ahead may still be opening (a stream that doesn't
                    // answer): wait for it, but take Stop, Next or a Play of another item meanwhile
                    loop {
                        if p.settle(PREFETCH_POLL) {
                            break p.wait();
                        }
                        if self.interrupted() {
                            // Dropped: its thread ends by itself, its result unused
                            return;
                        }
                    }
                }
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
                    {
                        let mut q = self.queue.lock().expect("queue poisoned");
                        q.set_current(Some(index));
                        next = q.after_skip();
                    }
                    // As in `start_index`: a Stop, Next or Play of another item ends the pass
                    if self.interrupted() {
                        return;
                    }
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
        self.output.now_playing(None);
        self.emit(ZoneEvent::Stopped);
    }

    // ---- crossfade ----

    fn crossfade_ms(&self) -> u64 {
        self.crossfade.load(Ordering::Relaxed)
    }

    /// Drop a fade in progress (pause, seek): the current track goes on alone.
    fn cancel_crossfade(&mut self) {
        if self.incoming.take().is_some() {
            if let Some(c) = self.current.as_mut() {
                c.fading = false;
            }
            self.dirty = true;
        }
    }

    /// Whether the fade into the next track starts with this block of the current one (which
    /// it then is): crossfade on, the track's last seconds reached, the next track open in the
    /// same format, and not the next song of the same album (albums stay gapless).
    fn start_crossfade(&mut self, out: &AudioBlock) -> bool {
        let xf = self.crossfade_ms();
        let Some(cur) = self.current.as_ref() else {
            return false;
        };
        if xf == 0 || self.xfade_checked == Some(cur.item.id) || !matches!(out.samples, Samples::PcmI32(_)) {
            return false;
        }
        let Some(d) = cur.source.duration_ms().or(cur.item.duration_ms) else {
            return false;
        };
        let fmt = cur.proc.output_format();
        let written = self
            .sink
            .as_ref()
            .map(|s| s.frames_written().saturating_sub(cur.start_frame))
            .unwrap_or(0);
        let at = cur.base_ms + frames_to_ms(fmt, written + out.frames() as u64);
        let left = d.saturating_sub(at);
        if left > xf {
            return false;
        }
        // The next item is still opening on its thread: a moment for it, then no crossfade
        let _ = self.prefetch.as_mut().is_some_and(|p| p.settle(PREFETCH_SETTLE));
        let Some(cur) = self.current.as_ref() else {
            return false;
        };
        self.xfade_checked = Some(cur.item.id);
        if d < xf * 2 {
            return false;
        }
        // The output's settings changed since its sink opened: the next song opens afresh at an
        // ordinary boundary (a crossfade would carry it on the old sink)
        if self.sink.as_ref().is_some_and(|s| s.reopen_at_track()) {
            return false;
        }
        let same_album = matches!((&cur.item.album, &self.prefetch), (Some(a), Some(p)) if p.item.album.as_ref() == Some(a));
        let ready = self.prefetch.as_mut().is_some_and(|p| {
            p.settle(Duration::ZERO)
                && matches!(&p.result, Prefetch::Done(Ok((_, proc))) if proc.output_format() == fmt)
        });
        if same_album || !ready {
            return false;
        }
        let Some(Prefetched {
            index,
            item,
            result: Prefetch::Done(Ok((source, proc))),
        }) = self.prefetch.take()
        else {
            return false;
        };
        let fade = (left + frames_to_ms(fmt, out.frames() as u64)) * u64::from(fmt.rate_hz) / 1000;
        self.incoming = Some(Incoming {
            index,
            item,
            source,
            proc,
            buf: VecDeque::new(),
            pulled: 0,
            ended: false,
            fade: fade.max(1),
            done: 0,
        });
        if let Some(c) = self.current.as_mut() {
            c.fading = true;
        }
        self.dirty = true;
        true
    }

    /// Mix the incoming track into `out` (equal-power: the current one fades out as the next
    /// fades in).
    fn mix(&mut self, out: AudioBlock) -> AudioBlock {
        let Some(inc) = self.incoming.as_mut() else {
            return out;
        };
        let Samples::PcmI32(a) = &out.samples else {
            return out;
        };
        let ch = usize::from(out.format.channels.max(1));
        while inc.buf.len() < a.len() && !inc.ended {
            match inc.source.next_block() {
                Ok(Some(b)) => match inc.proc.process(b) {
                    Ok(p) => {
                        if let Samples::PcmI32(v) = p.samples {
                            inc.pulled += (v.len() / ch) as u64;
                            inc.buf.extend(v);
                        }
                    }
                    Err(_) => inc.ended = true,
                },
                _ => inc.ended = true,
            }
        }
        let bits = u32::from(out.format.bits.clamp(2, 32));
        let (lo, hi) = (-(1i64 << (bits - 1)) as f64, ((1i64 << (bits - 1)) - 1) as f64);
        let frames = a.len() / ch;
        let mut mixed = Vec::with_capacity(a.len());
        for f in 0..frames {
            let t = ((inc.done + f as u64) as f64 / inc.fade as f64).min(1.0) * std::f64::consts::FRAC_PI_2;
            let (g_out, g_in) = (t.cos(), t.sin());
            for c in 0..ch {
                let x = f64::from(a[f * ch + c]) * g_out + f64::from(inc.buf.pop_front().unwrap_or(0)) * g_in;
                mixed.push(x.round().clamp(lo, hi) as i32);
            }
        }
        inc.done += frames as u64;
        AudioBlock::pcm_i32(out.format, mixed)
    }

    /// The current track has ended inside a fade: the incoming one carries on from where the
    /// mix left it.
    fn finish_crossfade(&mut self) {
        let Some(inc) = self.incoming.take() else {
            return;
        };
        let fmt = inc.proc.output_format();
        let ch = u64::from(fmt.channels.max(1));
        let waiting = inc.buf.len() as u64 / ch;
        let rest = (waiting > 0).then(|| AudioBlock::pcm_i32(fmt, inc.buf.into_iter().collect()));
        if rest.as_ref().is_some_and(|r| !self.make_room(r)) {
            return;
        }
        let start_frame = self.sink.as_ref().map(|s| s.frames_written()).unwrap_or(0);
        if let Some(rest) = rest {
            if let Some(sink) = self.sink.as_mut() {
                if let Err(e) = sink.write(&rest) {
                    return self.fail(format!("write to {}: {e}", self.output.id()));
                }
            }
        }
        self.output.now_playing(Some(&inc.item));
        // Where the item is now: the queue may have changed since the fade began.
        let index = {
            let mut q = self.queue.lock().expect("queue poisoned");
            let i = q.position_of(inc.item.id).unwrap_or(inc.index);
            q.set_current(Some(i));
            i
        };
        self.emit(ZoneEvent::TrackStarted {
            item: inc.item.clone(),
            index,
            source_format: inc.proc.source_format(),
            output_format: fmt,
            signal_path: inc.proc.signal_path().clone(),
            gapless: false,
        });
        let format = stream_info_of(inc.source.as_ref(), &inc.item);
        self.current = Some(Current {
            index,
            item: inc.item,
            source: inc.source,
            proc: inc.proc,
            base_ms: frames_to_ms(fmt, inc.pulled - waiting),
            start_frame,
            paused_at_ms: None,
            stream_title: None,
            fading: false,
            format,
            seek_from: None,
        });
        self.prefetch = None;
        self.error = None;
        self.transport = Transport::Playing;
        self.dirty = true;
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
        let (queue, queue_index, queue_version, repeat, shuffle, radio) = {
            let q = self.queue.lock().expect("queue poisoned");
            (
                q.items().to_vec(),
                q.current_index(),
                q.version(),
                q.repeat(),
                q.shuffle(),
                q.radio(),
            )
        };
        let now_playing = self.current.as_ref().map(|c| NowPlaying {
            item: c.item.clone(),
            index: c.index,
            position_ms: self.position_ms(),
            // Streams (HLS, radio) rarely know their length; the queue item may.
            duration_ms: c.source.duration_ms().or(c.item.duration_ms),
            source_format: c.proc.source_format(),
            output_format: c.proc.output_format(),
            signal_path: {
                let mut p = c.proc.signal_path().clone();
                if c.fading {
                    let f = c.proc.output_format();
                    p.push(
                        PathStage::new("crossfade", f, f, Quality::Enhanced).with_note(format!(
                            "{:.0} s into the next track",
                            self.crossfade_ms() as f64 / 1000.0
                        )),
                    );
                }
                p
            },
            members: self.sink.as_ref().map(|s| s.members()).unwrap_or_default(),
            stream_title: c.stream_title.clone(),
            format: c.format.clone(),
        });
        self.state.send_modify(|s| {
            s.transport = self.transport;
            s.now_playing = now_playing;
            s.queue = queue;
            s.queue_index = queue_index;
            s.queue_version = queue_version;
            s.repeat = repeat;
            s.shuffle = shuffle;
            s.radio = radio;
            s.error = self.error.clone();
            s.acked = self.acked;
            s.crossfade_ms = self.crossfade_ms();
        });
    }
}

/// Run a future to completion on this thread. The futures here (`Output::open`) are short;
/// the caller enters the zone's runtime first, for the ones that use it.
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

/// The service a queue item plays from, by its uri: `track:<id>` and paths are the library,
/// `tidal:…` is TIDAL, a bare http(s) URL is a stream.
fn service_of(uri: &str) -> &str {
    if uri.starts_with('/') || uri.starts_with("file:") || uri.starts_with("track:") {
        return "library";
    }
    match uri.split_once(':') {
        Some(("http" | "https", _)) => "stream",
        // Spotify Connect plays through librespot
        Some(("librespot", _)) => "spotify",
        Some((scheme, _)) if !scheme.is_empty() => scheme,
        _ => "library",
    }
}

/// Open `item` through `factory` and plan its chain to `output` (on the audio thread, or on a
/// prefetch thread).
fn open_with(
    factory: &dyn SourceFactory,
    output: &dyn Output,
    item: &QueueItem,
) -> Result<Opened, SourceError> {
    let source = factory.open(item)?;
    let proc = factory.plan(
        source.as_ref(),
        &output.live_caps_for(source.format()),
        &output.id(),
    )?;
    Ok((source, proc))
}

#[cfg(test)]
mod service_tests {
    #[test]
    fn service_of_uris() {
        assert_eq!(super::service_of("track:12"), "library");
        assert_eq!(super::service_of("/music/a.flac"), "library");
        assert_eq!(super::service_of("tidal:track:1"), "tidal");
        assert_eq!(super::service_of("https://x/y.mp3"), "stream");
        assert_eq!(super::service_of("librespot:connect"), "spotify");
    }
}
