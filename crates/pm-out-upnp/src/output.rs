//! A UPnP renderer as an `Output`: open a pull stream, point the renderer
//! at its URL with the track's metadata, Play; the renderer's fetching paces
//! the zone. Stop on close.
//!
//! TV quirks handled here (see `dlna`): the stream is offered in a format the
//! renderer listed (WAV, or DLNA LPCM for TVs that take nothing else), with
//! full DLNA protocolInfo; a Stop goes first, because some TVs refuse a new
//! URI while they hold one; Play is retried while the TV still loads (UPnP
//! error 701, "transition not available").

use crate::dlna::StreamKind;
use crate::soap::{self, didl};
use crate::{Renderer, UpnpError, AV_TRANSPORT, RENDERING_CONTROL};
use async_trait::async_trait;
use pm_model::{AudioBlock, AudioFormat, OutputCaps, QualityCeiling, SampleKind, Volume};
use pm_zones::outputs::PullOutput;
use pm_zones::{AudioSink, BufferLevel, Describe, Output, OutputId, QueueItem, SinkError, TrackMeta};
use std::sync::{Arc, Mutex};

pub struct UpnpOutput {
    renderer: Renderer,
    pull: Arc<PullOutput>,
    /// The URL the renderer fetches, as it can reach the node.
    stream_url: String,
    caps: OutputCaps,
    kind: StreamKind,
    /// An OpenHome renderer (Linn): WAV with OpenHome's continuous-stream header (`live.wav`).
    openhome: bool,
    describe: Option<Describe>,
    meta: Mutex<Option<TrackMeta>>,
    /// The renderer answers Pause on the node's live stream but keeps playing (a Linn does):
    /// it is stopped for a pause instead, and started again on resume. Learned on the first try.
    stops_for_pause: Arc<std::sync::atomic::AtomicBool>,
}

impl UpnpOutput {
    /// `pull` must be registered with the node's HTTP server so that
    /// `stream_url` serves it.
    pub fn new(renderer: Renderer, pull: Arc<PullOutput>, stream_url: String) -> Self {
        let mut caps = OutputCaps::unlimited();
        // LPCM over HTTP: what renderers accept as WAV. Hi-res beyond 192 kHz
        // and DSD would need FLAC/DoP transport; left to a device-db entry.
        caps.pcm_rates = vec![44_100, 48_000, 88_200, 96_000, 176_400, 192_000];
        caps.max_pcm_bits = 24;
        caps.pcm_bits = vec![16, 24];
        caps.max_channels = 2;
        caps.dsd = None;
        caps.hw_volume = renderer
            .rendering_control
            .as_ref()
            .map(|_| pm_model::VolumeKind::Percent);
        caps.standby = false;
        caps.gapless = true;
        caps.groupable = false;
        caps.fixed_latency_ms = Some(500);
        caps.quality_ceiling = QualityCeiling::BitPerfect;
        if crate::sonos::is_sonos(&renderer) {
            // Sonos plays WAV at 16 bits up to 48 kHz.
            caps.pcm_rates = vec![44_100, 48_000];
            caps.pcm_bits = vec![16];
            caps.max_pcm_bits = 16;
            caps.quality_ceiling = QualityCeiling::LosslessUpTo {
                rate_hz: 48_000,
                bits: 16,
            };
        }
        let openhome = is_openhome(&renderer);
        Self {
            renderer,
            pull,
            stream_url,
            caps,
            kind: StreamKind::Wav {
                mime: "audio/wav".into(),
            },
            openhome,
            describe: None,
            meta: Mutex::new(None),
            stops_for_pause: Arc::default(),
        }
    }

    /// Offer the stream as `kind` (from `dlna::choose`). DLNA LPCM is 16-bit at 44.1 or 48 kHz,
    /// so the zone converts to it and the signal path says so.
    pub fn with_stream_kind(mut self, kind: StreamKind) -> Self {
        if kind == StreamKind::L16 {
            self.caps.pcm_rates = vec![44_100, 48_000];
            self.caps.pcm_bits = vec![16];
            self.caps.max_pcm_bits = 16;
            self.caps.quality_ceiling = QualityCeiling::LosslessUpTo {
                rate_hz: 48_000,
                bits: 16,
            };
        }
        self.kind = kind;
        self
    }

    /// Show artist, album and art from the node's library.
    pub fn with_describe(mut self, describe: Describe) -> Self {
        self.describe = Some(describe);
        self
    }

    pub fn stream_kind(&self) -> &StreamKind {
        &self.kind
    }

    /// The URL for this renderer's stream kind (`…/stream.wav` → `…/stream.l16`, or
    /// `…/live.wav` for an OpenHome renderer).
    fn url(&self) -> String {
        let file = match self.kind {
            StreamKind::Wav { .. } if self.openhome => "live.wav",
            ref k => k.file(),
        };
        self.stream_url.replacen("/stream.wav", &format!("/{file}"), 1)
    }

    pub fn renderer(&self) -> &Renderer {
        &self.renderer
    }

    pub fn pull(&self) -> &Arc<PullOutput> {
        &self.pull
    }
}

/// `url` naming stream `generation` of its pull output (`g`, read by the pull route).
fn with_generation(url: &str, generation: u64) -> String {
    let sep = if url.contains('?') { '&' } else { '?' };
    format!("{url}{sep}g={generation}")
}

/// A renderer on Linn's OpenHome pipeline (Linn's own players). Its WAV codec reads the header's
/// sizes literally, so it needs OpenHome's continuous-stream header.
pub fn is_openhome(r: &Renderer) -> bool {
    r.manufacturer
        .as_deref()
        .is_some_and(|m| m.to_ascii_lowercase().starts_with("linn"))
}

fn soap_err(e: UpnpError) -> SinkError {
    SinkError::Device(e.to_string())
}

/// Run blocking control calls off the executor when there is one (API
/// handlers), directly otherwise (the zone's audio thread).
async fn offload<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    if tokio::runtime::Handle::try_current().is_ok() {
        tokio::task::spawn_blocking(f)
            .await
            .expect("control task panicked")
    } else {
        f()
    }
}

#[async_trait]
impl Output for UpnpOutput {
    fn id(&self) -> OutputId {
        OutputId::new("upnp", self.renderer.udn.trim_start_matches("uuid:"))
    }

    fn name(&self) -> String {
        self.renderer.friendly_name.clone()
    }

    /// Maker and model from the device description: `Linn Klimax DSM`, `Sonos One` (the maker
    /// once, without its company suffix).
    fn model(&self) -> Option<String> {
        let maker = self.renderer.manufacturer.as_deref().and_then(|m| {
            let m = m.trim().trim_end_matches('.');
            let m = ["Inc", "Ltd", "GmbH", "LLC", "Corporation", "Corp", "Co"]
                .iter()
                .fold(m, |m, s| {
                    m.trim_end_matches(s).trim().trim_end_matches(',').trim()
                });
            (!m.is_empty()).then(|| m.to_string())
        });
        let model = self
            .renderer
            .model
            .as_deref()
            .map(str::trim)
            .filter(|m| !m.is_empty())
            .map(str::to_string);
        match (maker, model) {
            (Some(a), Some(b)) if b.to_lowercase().contains(&a.to_lowercase()) => Some(b),
            (Some(a), Some(b)) => Some(format!("{a} {b}")),
            (a, b) => a.or(b),
        }
    }

    fn caps(&self) -> &OutputCaps {
        &self.caps
    }

    fn now_playing(&self, item: Option<&QueueItem>) {
        let meta = item.map(|it| match &self.describe {
            Some(d) => d(it),
            None => TrackMeta::from_item(it),
        });
        *self.meta.lock().expect("meta") = meta;
    }

    async fn open(&self, fmt: AudioFormat) -> Result<Box<dyn AudioSink>, SinkError> {
        if fmt.kind != SampleKind::Pcm || !self.caps.accepts(&fmt) {
            return Err(SinkError::UnsupportedFormat(self.id(), fmt));
        }
        let inner = self.pull.open(fmt).await?;
        // The URL names this stream: the renderer's polls of an older one (a stopped Linn asks
        // for its last URI every 15 s) are answered, never given this stream
        let url = with_generation(&self.url(), self.pull.generation());
        let protocol_info = self.kind.protocol_info(fmt.rate_hz, fmt.channels);
        let meta = didl(
            &self.meta.lock().expect("meta").clone().unwrap_or_default(),
            &url,
            &protocol_info,
        );
        let renderer = self.renderer.clone();
        // Stop, SetAVTransportURI and Play go out in order on their own thread, and the sink is
        // writable at once: a Linn fetches the URI during SetAVTransportURI (and again during
        // Play) and answers only once it has read the start of the stream, which has no bytes
        // until the zone writes into the sink. Waiting for either answer here stalled both for
        // the Linn's 5 s, twice (NUC, 2026-10-30). A refused URI or Play shows up on the next
        // write.
        let (played_tx, played) = std::sync::mpsc::channel();
        let (control, controls) = std::sync::mpsc::channel::<Control>();
        let target = Arc::new(Mutex::new(None::<String>));
        let target_set = target.clone();
        let (pull, generation) = (self.pull.clone(), self.pull.generation());
        let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop_retrying = cancelled.clone();
        let refill = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let emptied = refill.clone();
        let (paused_tx, pause_results) = std::sync::mpsc::channel::<Paused>();
        let stops_for_pause = self.stops_for_pause.clone();
        std::thread::Builder::new()
            .name("upnp-play".into())
            .spawn(move || {
                let result = start(&renderer, &url, &meta, &target_set, &stop_retrying);
                if let Err(e) = &result {
                    // Nobody will fetch the stream: the zone hears why now, not after 15 s
                    pull.abort(generation, e.clone());
                }
                let started = result.is_ok();
                let _ = played_tx.send(result);
                // Then the zone's pauses and resumes, in the order it gave them
                for c in controls {
                    if stop_retrying.load(std::sync::atomic::Ordering::SeqCst) {
                        break;
                    }
                    let av = target_set.lock().expect("target").clone();
                    match c {
                        Control::Pause => {
                            let how = match av.as_deref() {
                                Some(av) if started => pause_renderer(av, &stops_for_pause, &stop_retrying),
                                _ => Paused::Refused,
                            };
                            if how == Paused::Refused {
                                // It played its buffer out: the stream fills it again from now
                                emptied.store(true, std::sync::atomic::Ordering::SeqCst);
                            }
                            let _ = paused_tx.send(how);
                        }
                        Control::Play => {
                            let r = match av {
                                Some(av) if started => {
                                    play_after_pause(&av, &renderer, &url, &meta, &target_set, &stop_retrying)
                                        .map(|restarted| {
                                            // Set again, it starts with an empty buffer
                                            if restarted {
                                                emptied.store(true, std::sync::atomic::Ordering::SeqCst);
                                            }
                                        })
                                }
                                // Nothing started: a Play has nothing to go on with
                                _ => Err("the renderer never started the stream".into()),
                            };
                            if let Err(e) = &r {
                                // The zone hears it at once, not after the listener timeout
                                pull.abort(generation, e.clone());
                            }
                            let _ = played_tx.send(r);
                        }
                    }
                }
            })
            .map_err(|e| SinkError::Device(format!("cannot start the Play call: {e}")))?;
        Ok(Box::new(UpnpSink {
            inner,
            renderer: self.renderer.av_transport.clone(),
            av_transport: target,
            answers: played,
            pending: 1,
            cancelled,
            control: Some(control),
            paused: false,
            refill,
            pause_results,
            pauses_pending: 0,
            pause_outcome: None,
        }))
    }

    async fn set_volume(&self, v: Volume) -> Result<(), SinkError> {
        let Some(rc) = self.renderer.rendering_control.clone() else {
            return Err(SinkError::NoVolume(self.id()));
        };
        offload(move || set_volume_blocking(&rc, v))
            .await
            .map_err(soap_err)
    }
}

/// Point the renderer at the stream and play it: Stop, SetAVTransportURI, Play, in that order
/// (on the sink's own thread, see [`UpnpOutput::open`]). The AVTransport it used goes into
/// `target`, for the sink's Stop. Err is the reason, in the zone's words.
fn start(
    renderer: &Renderer,
    url: &str,
    meta: &str,
    target: &Mutex<Option<String>>,
    cancelled: &std::sync::atomic::AtomicBool,
) -> Result<(), String> {
    // A Sonos player in a Sonos group plays through its coordinator.
    let av = if crate::sonos::is_sonos(renderer) {
        crate::sonos::transport_target(renderer)
    } else {
        renderer.av_transport.clone()
    };
    *target.lock().expect("target") = Some(av.clone());
    // Some TVs refuse a new URI while they hold one; a renderer with nothing loaded may answer
    // Stop with an error, which is fine.
    let _ = soap::call(&av, AV_TRANSPORT, "Stop", &[("InstanceID", "0")]);
    // The sink closed meanwhile (a slow start): nothing more, so nothing plays after its Stop
    let closed = || cancelled.load(std::sync::atomic::Ordering::SeqCst);
    if closed() {
        return Ok(());
    }
    soap::call(
        &av,
        AV_TRANSPORT,
        "SetAVTransportURI",
        &[
            ("InstanceID", "0"),
            ("CurrentURI", url),
            ("CurrentURIMetaData", meta),
        ],
    )
    .map_err(|e| format!("the renderer refused the stream: {e}"))?;
    if closed() {
        return Ok(());
    }
    // TVs answer 701 while they still open the URI: try again for a moment.
    let mut tries = 0;
    loop {
        match soap::call(&av, AV_TRANSPORT, "Play", &[("InstanceID", "0"), ("Speed", "1")]) {
            Ok(_) => return Ok(()),
            // The sink closed meanwhile: no Play after its Stop
            Err(e) if tries < 4 && !closed() => {
                tries += 1;
                tracing::debug!(tries, "renderer Play refused, retrying: {e}");
                std::thread::sleep(std::time::Duration::from_millis(400));
            }
            Err(e) => return Err(format!("the renderer refused Play: {e}")),
        }
    }
}

/// A transport change the zone asks for while its stream is open.
enum Control {
    /// The renderer's own Pause: it stops at once and keeps what it buffered. Only stopping
    /// the stream left it playing seconds of buffer after the press (NUC, 2026-10-30).
    Pause,
    /// Play again after a Pause; its answer goes to the sink like the start's.
    Play,
}

/// How a renderer took a pause.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Paused {
    /// Paused, holding its buffer: Play carries on.
    Held,
    /// Stopped instead (it ignores Pause on a live stream): the resume starts it again.
    Stopped,
    /// It refused both Pause and Stop (or never started): it plays its buffer out, as before.
    Refused,
}

/// How long a renderer may take to report the pause before it counts as ignoring it.
const PAUSE_CONFIRM: std::time::Duration = std::time::Duration::from_millis(450);
/// How long a resume waits for its pause's outcome (the confirmation and a Stop's answer).
const PAUSE_OUTCOME_WAIT: std::time::Duration = std::time::Duration::from_secs(3);

/// The renderer's own Pause (on the sink's control thread). A Linn answers Pause on the
/// node's live WAV but plays on (NUC, 2026-10-30): a renderer that still reports a playing
/// state soon after is stopped instead, and `stops` remembers it so later pauses stop it at
/// once. A sink closed meanwhile (`cancelled`: a new track or a seek while paused) gets
/// nothing more: its own Stop follows, and a late one could stop the next stream.
fn pause_renderer(
    av: &str,
    stops: &std::sync::atomic::AtomicBool,
    cancelled: &std::sync::atomic::AtomicBool,
) -> Paused {
    let closed = || cancelled.load(std::sync::atomic::Ordering::SeqCst);
    let stop = || match soap::call(av, AV_TRANSPORT, "Stop", &[("InstanceID", "0")]) {
        Ok(_) => Paused::Stopped,
        Err(e) => {
            tracing::warn!("renderer Stop for the pause refused, it plays out its buffer: {e}");
            Paused::Refused
        }
    };
    if stops.load(std::sync::atomic::Ordering::SeqCst) {
        return stop();
    }
    if let Err(e) = soap::call(av, AV_TRANSPORT, "Pause", &[("InstanceID", "0")]) {
        tracing::info!("renderer Pause refused ({e}); stopping it for the pause");
        return if closed() { Paused::Held } else { stop() };
    }
    let deadline = std::time::Instant::now() + PAUSE_CONFIRM;
    let mut last: Option<String>;
    loop {
        if closed() {
            return Paused::Held;
        }
        let state = soap::call(av, AV_TRANSPORT, "GetTransportInfo", &[("InstanceID", "0")])
            .ok()
            .and_then(|x| soap::element_text(&x, "CurrentTransportState"));
        match state.as_deref() {
            Some("PAUSED_PLAYBACK") => return Paused::Held,
            Some("STOPPED") => return Paused::Stopped,
            _ => {}
        }
        last = state;
        if std::time::Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    // Only a renderer last seen playing on is learned; one that didn't say may well have paused
    if !matches!(last.as_deref(), Some("PLAYING" | "TRANSITIONING")) || closed() {
        return Paused::Held;
    }
    tracing::info!("the renderer plays on after Pause; stopping it for pauses from now on");
    stops.store(true, std::sync::atomic::Ordering::SeqCst);
    stop()
}

/// Play after a pause. A renderer that dropped the stream meanwhile (stopped, "no media")
/// refuses: it is pointed at the same stream again, as at the start (Ok(true)).
fn play_after_pause(
    av: &str,
    renderer: &Renderer,
    url: &str,
    meta: &str,
    target: &Mutex<Option<String>>,
    cancelled: &std::sync::atomic::AtomicBool,
) -> Result<bool, String> {
    match soap::call(av, AV_TRANSPORT, "Play", &[("InstanceID", "0"), ("Speed", "1")]) {
        Ok(_) => Ok(false),
        Err(e) => {
            tracing::info!("renderer refused Play after the pause ({e}); setting the stream again");
            start(renderer, url, meta, target, cancelled).map(|()| true)
        }
    }
}

fn set_volume_blocking(rc: &str, v: Volume) -> Result<(), UpnpError> {
    let current = || -> Result<u8, UpnpError> {
        let xml = soap::call(
            rc,
            RENDERING_CONTROL,
            "GetVolume",
            &[("InstanceID", "0"), ("Channel", "Master")],
        )?;
        Ok(soap::element_text(&xml, "CurrentVolume")
            .and_then(|t| t.parse().ok())
            .unwrap_or(50))
    };
    let target: u8 = match v {
        Volume::Percent(p) => p.min(100),
        Volume::Db(db) => ((db.clamp(-60.0, 0.0) + 60.0) / 60.0 * 100.0).round() as u8,
        Volume::Up => current()?.saturating_add(3).min(100),
        Volume::Down => current()?.saturating_sub(3),
        Volume::Mute(m) => {
            soap::call(
                rc,
                RENDERING_CONTROL,
                "SetMute",
                &[
                    ("InstanceID", "0"),
                    ("Channel", "Master"),
                    ("DesiredMute", if m { "1" } else { "0" }),
                ],
            )?;
            return Ok(());
        }
    };
    soap::call(
        rc,
        RENDERING_CONTROL,
        "SetVolume",
        &[
            ("InstanceID", "0"),
            ("Channel", "Master"),
            ("DesiredVolume", &target.to_string()),
        ],
    )?;
    Ok(())
}

struct UpnpSink {
    inner: Box<dyn AudioSink>,
    /// The renderer's own AVTransport, for Stop before [`start`] chose one.
    renderer: String,
    /// The AVTransport [`start`] used (a Sonos group's coordinator), once it chose it.
    av_transport: Arc<Mutex<Option<String>>>,
    /// The answers to the start (SetAVTransportURI and Play) and to each Play after a pause.
    answers: std::sync::mpsc::Receiver<Result<(), String>>,
    /// Answers still to come.
    pending: usize,
    /// Set on close: the Play thread stops retrying.
    cancelled: Arc<std::sync::atomic::AtomicBool>,
    /// Pauses and resumes for the Play thread, after its start.
    control: Option<std::sync::mpsc::Sender<Control>>,
    /// The renderer was told to pause.
    paused: bool,
    /// The renderer's buffer emptied (it refused both Pause and Stop and played it out, or was
    /// set to the stream again): the next write lets the stream fill it again from now.
    refill: Arc<std::sync::atomic::AtomicBool>,
    /// How each pause went, from the control thread: a renderer stopped for it is started
    /// again by the zone on resume.
    pause_results: std::sync::mpsc::Receiver<Paused>,
    /// Pauses whose outcome hasn't been read.
    pauses_pending: usize,
    /// How the last pause went, once read (see [`UpnpSink::read_pause_outcomes`]).
    pause_outcome: Option<Paused>,
}

impl UpnpSink {
    /// Play's answers that came: a refusal is this sink's error from now on.
    fn play_refused(&mut self) -> Option<SinkError> {
        while self.pending > 0 {
            match self.answers.try_recv() {
                Ok(Ok(())) => self.pending -= 1,
                Ok(Err(e)) => {
                    self.pending -= 1;
                    return Some(SinkError::Device(e));
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => self.pending = 0,
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
            }
        }
        None
    }
}

impl AudioSink for UpnpSink {
    fn format(&self) -> AudioFormat {
        self.inner.format()
    }
    fn write(&mut self, block: &AudioBlock) -> Result<(), SinkError> {
        if let Some(e) = self.play_refused() {
            return Err(e);
        }
        if self.refill.swap(false, std::sync::atomic::Ordering::SeqCst) {
            self.inner.flush()?;
        }
        // A renderer that refused Play never fetches: say that, not "listener stalled"
        self.inner
            .write(block)
            .map_err(|e| self.play_refused().unwrap_or(e))
    }
    fn buffer_level(&self) -> BufferLevel {
        self.inner.buffer_level()
    }
    fn flush(&mut self) -> Result<(), SinkError> {
        self.inner.flush()
    }
    fn drain(&mut self) -> Result<(), SinkError> {
        self.inner.drain()
    }
    /// The renderer's own Pause, in order after its start (on the Play thread, so a renderer
    /// slow to answer does not hold the zone).
    fn pause(&mut self) -> Result<bool, SinkError> {
        if let Some(c) = &self.control {
            if c.send(Control::Pause).is_ok() {
                self.paused = true;
                self.pauses_pending += 1;
                self.pause_outcome = None;
                self.inner.pause()?;
                return Ok(true);
            }
        }
        self.inner.flush().map(|()| false)
    }
    /// Whether the renderer held its buffer (a group asks before any member resumes): one stopped
    /// for the pause can't carry on, nor one that refused Pause and Stop and played its buffer
    /// out, which would play out of step with members that held theirs. (On its own it carries
    /// on: the stream goes on from where it was.)
    fn ready_to_resume(&mut self) -> Result<(), SinkError> {
        self.read_pause_outcomes();
        match self.pause_outcome.filter(|_| self.paused) {
            Some(Paused::Stopped) => Err(stopped_for_pause()),
            Some(Paused::Refused) => Err(SinkError::Device(
                "the renderer refused Pause and Stop and played its buffer out; starting it again".into(),
            )),
            _ => Ok(()),
        }
    }
    /// Play again; a refusal shows on the next write, as a refused start does.
    fn resume(&mut self) -> Result<(), SinkError> {
        // Stopped for the pause: the renderer dropped its buffer, so the zone opens the output
        // again from what was last heard
        self.read_pause_outcomes();
        if self.pause_outcome.take() == Some(Paused::Stopped) {
            self.paused = false;
            return Err(stopped_for_pause());
        }
        if std::mem::take(&mut self.paused) {
            let sent = self
                .control
                .as_ref()
                .is_some_and(|c| c.send(Control::Play).is_ok());
            if !sent {
                return Err(SinkError::Device("the renderer's control thread ended".into()));
            }
            self.pending += 1;
        }
        self.inner.resume()
    }
    fn frames_written(&self) -> u64 {
        self.inner.frames_written()
    }
}

impl UpnpSink {
    /// How the pauses went, from the control thread (a quick resume waits for it, briefly); the
    /// last one is kept in `pause_outcome`.
    fn read_pause_outcomes(&mut self) {
        let deadline = std::time::Instant::now() + PAUSE_OUTCOME_WAIT;
        while self.pauses_pending > 0 {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            match self.pause_results.recv_timeout(left) {
                Ok(how) => {
                    self.pauses_pending -= 1;
                    self.pause_outcome = Some(how);
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    self.pauses_pending = 0;
                    break;
                }
                // Still coming: the next resume reads it first
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => break,
            }
        }
    }
}

fn stopped_for_pause() -> SinkError {
    SinkError::Device("the renderer was stopped for the pause; starting it again".into())
}

impl Drop for UpnpSink {
    fn drop(&mut self) {
        // Stop after the start's answers (briefly), so the renderer does not start after it
        self.cancelled.store(true, std::sync::atomic::Ordering::SeqCst);
        drop(self.control.take());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while self.pending > 0 {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if self.answers.recv_timeout(left).is_err() {
                break;
            }
            self.pending -= 1;
        }
        let av = self
            .av_transport
            .lock()
            .expect("target")
            .clone()
            .unwrap_or_else(|| self.renderer.clone());
        if let Err(e) = soap::call(&av, AV_TRANSPORT, "Stop", &[("InstanceID", "0")]) {
            tracing::debug!("renderer Stop: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::Mutex;

    /// A fake renderer: answers SOAP posts, records the actions, and on Play
    /// fetches nothing (the test takes the pull stream itself).
    fn fake_renderer() -> (String, Arc<Mutex<Vec<String>>>) {
        fake_renderer_slow(std::time::Duration::ZERO)
    }

    /// [`fake_renderer`] that takes `stop` to answer Stop.
    fn fake_renderer_slow(stop: std::time::Duration) -> (String, Arc<Mutex<Vec<String>>>) {
        fake_renderer_with(stop, Arc::default(), Arc::default(), Arc::default())
    }

    /// [`fake_renderer_slow`] that refuses the next `refuse_play` Plays (UPnP error, HTTP 500)
    /// and, with `ignores_pause`, answers Pause but plays on (as a Linn does on a live stream);
    /// with `refuses_pause`, it refuses Pause and Stop both (and plays on).
    /// It reports its transport state to GetTransportInfo, which is not logged.
    fn fake_renderer_with(
        stop: std::time::Duration,
        refuse_play: Arc<std::sync::atomic::AtomicUsize>,
        ignores_pause: Arc<std::sync::atomic::AtomicBool>,
        refuses_pause: Arc<std::sync::atomic::AtomicBool>,
    ) -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let log = Arc::new(Mutex::new(Vec::new()));
        let log2 = log.clone();
        std::thread::spawn(move || {
            let mut state = "STOPPED";
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { break };
                let mut buf = Vec::new();
                let mut tmp = [0u8; 4096];
                loop {
                    let n = s.read(&mut tmp).unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                    let text = String::from_utf8_lossy(&buf);
                    if let Some(idx) = text.find("\r\n\r\n") {
                        let len = text
                            .lines()
                            .find_map(|l| {
                                l.strip_prefix("Content-Length: ")
                                    .or_else(|| l.strip_prefix("content-length: "))
                            })
                            .and_then(|v| v.trim().parse::<usize>().ok())
                            .unwrap_or(0);
                        if buf.len() >= idx + 4 + len {
                            break;
                        }
                    }
                }
                let text = String::from_utf8_lossy(&buf).to_string();
                let action = text
                    .lines()
                    .find_map(|l| {
                        l.strip_prefix("SOAPAction: ")
                            .or_else(|| l.strip_prefix("soapaction: "))
                    })
                    .map(|a| a.trim_matches('"').rsplit('#').next().unwrap_or("").to_string())
                    .unwrap_or_default();
                let uri = soap::element_text(&text, "CurrentURI").unwrap_or_default();
                if action == "GetTransportInfo" {
                    let body = format!("<s:Envelope><s:Body><u:GetTransportInfoResponse><CurrentTransportState>{state}</CurrentTransportState></u:GetTransportInfoResponse></s:Body></s:Envelope>");
                    let _ = write!(s, "HTTP/1.1 200 OK\r\nContent-Type: text/xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body);
                    continue;
                }
                log2.lock().unwrap().push(if uri.is_empty() {
                    action.clone()
                } else {
                    format!("{action} {uri}")
                });
                if matches!(action.as_str(), "Pause" | "Stop")
                    && refuses_pause.load(std::sync::atomic::Ordering::SeqCst)
                {
                    let body = "<s:Envelope><s:Body><s:Fault><detail><UPnPError><errorCode>701</errorCode></UPnPError></detail></s:Fault></s:Body></s:Envelope>";
                    let _ = write!(s, "HTTP/1.1 500 Internal Server Error\r\nContent-Type: text/xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body);
                    continue;
                }
                if action == "Stop" {
                    std::thread::sleep(stop);
                    state = "STOPPED";
                }
                if action == "Pause" && !ignores_pause.load(std::sync::atomic::Ordering::SeqCst) {
                    state = "PAUSED_PLAYBACK";
                }
                if action == "Play" && refuse_play.load(std::sync::atomic::Ordering::SeqCst) > 0 {
                    refuse_play.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
                    let body = "<s:Envelope><s:Body><s:Fault><detail><UPnPError><errorCode>701</errorCode></UPnPError></detail></s:Fault></s:Body></s:Envelope>";
                    let _ = write!(s, "HTTP/1.1 500 Internal Server Error\r\nContent-Type: text/xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body);
                    continue;
                }
                if action == "Play" {
                    state = "PLAYING";
                }
                let body = if action == "GetVolume" {
                    "<s:Envelope><s:Body><u:GetVolumeResponse><CurrentVolume>40</CurrentVolume></u:GetVolumeResponse></s:Body></s:Envelope>"
                } else {
                    "<s:Envelope><s:Body><u:Response/></s:Body></s:Envelope>"
                };
                let _ = write!(s, "HTTP/1.1 200 OK\r\nContent-Type: text/xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body);
            }
        });
        (format!("http://{addr}"), log)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn open_points_the_renderer_at_the_stream_and_stops_on_close() {
        let (base, log) = fake_renderer();
        let renderer = Renderer {
            udn: "uuid:abc".into(),
            friendly_name: "Fake DSM".into(),
            manufacturer: None,
            model: None,
            av_transport: format!("{base}/av"),
            rendering_control: Some(format!("{base}/rc")),
            connection_manager: None,
            zone_group_topology: None,
            location: format!("{base}/desc.xml"),
        };
        let pull = Arc::new(PullOutput::new("fake"));
        let out = UpnpOutput::new(
            renderer,
            pull.clone(),
            "http://node.local:7788/v1/outputs/pull/fake/stream.wav".into(),
        );
        assert_eq!(out.id().0, "upnp:abc");
        assert!(out.caps().accepts(&AudioFormat::pcm(96_000, 24, 2)));
        assert!(!out.caps().accepts(&AudioFormat::pcm(384_000, 24, 2)));

        let fmt = AudioFormat::pcm(48_000, 16, 2);
        let mut sink = out.open(fmt).await.unwrap();
        // The renderer "fetches": take the live stream and read what the sink writes.
        let mut live = pull.take_stream(std::time::Duration::from_secs(2)).await.unwrap();
        assert_eq!(live.format, fmt);
        let block = AudioBlock::pcm_i32(fmt, vec![1, -1, 2, -2]);
        let writer = std::thread::spawn(move || {
            sink.write(&block).unwrap();
            sink.drain().unwrap();
            drop(sink); // sends Stop
        });
        // PCM only: the HTTP server puts a header before it for each listener
        let data = live.next_chunk().await.unwrap();
        assert_eq!(data, vec![1, 0, 0xff, 0xff, 2, 0, 0xfe, 0xff]);
        writer.join().unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let actions = log.lock().unwrap().clone();
        assert_eq!(
            actions,
            vec![
                "Stop".to_string(),
                format!(
                    "SetAVTransportURI http://node.local:7788/v1/outputs/pull/fake/stream.wav?g={}",
                    pull.generation()
                ),
                "Play".into(),
                "Stop".into()
            ]
        );

        out.set_volume(Volume::Percent(30)).await.unwrap();
        out.set_volume(Volume::Up).await.unwrap();
        let actions = log.lock().unwrap().clone();
        assert!(actions.iter().any(|a| a == "SetVolume"));
        assert!(actions.iter().any(|a| a == "GetVolume"));

        // A Linn gets the continuous WAV; others keep stream.wav
        let linn = Renderer {
            udn: "uuid:4c494e4e-0026".into(),
            friendly_name: "Klimax DS".into(),
            manufacturer: Some("Linn Products Ltd.".into()),
            model: None,
            av_transport: format!("{base}/av"),
            rendering_control: None,
            connection_manager: None,
            zone_group_topology: None,
            location: format!("{base}/desc.xml"),
        };
        let linn = UpnpOutput::new(
            linn,
            Arc::new(PullOutput::new("linn")),
            "http://node.local:7788/v1/outputs/pull/linn/stream.wav?k=1".into(),
        );
        assert_eq!(
            linn.url(),
            "http://node.local:7788/v1/outputs/pull/linn/live.wav?k=1"
        );
        assert_eq!(
            out.url(),
            "http://node.local:7788/v1/outputs/pull/fake/stream.wav"
        );
    }

    /// Pause sends the renderer's own Pause after its start (it stops at once and keeps its
    /// buffer, instead of playing seconds of it out), and resume its Play, in that order (NUC,
    /// 2026-10-30).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pause_and_resume_tell_the_renderer_in_order() {
        let (base, log) = fake_renderer();
        let renderer = Renderer {
            udn: "uuid:pause".into(),
            friendly_name: "Fake DSM".into(),
            manufacturer: None,
            model: None,
            av_transport: format!("{base}/av"),
            rendering_control: None,
            connection_manager: None,
            zone_group_topology: None,
            location: format!("{base}/desc.xml"),
        };
        let pull = Arc::new(PullOutput::controlled("pause"));
        let out = UpnpOutput::new(
            renderer,
            pull.clone(),
            "http://node/v1/outputs/pull/pause/stream.wav".into(),
        );
        let fmt = AudioFormat::pcm(44_100, 16, 2);
        let mut sink = out.open(fmt).await.unwrap();
        let mut live = pull.take_stream(std::time::Duration::from_secs(2)).await.unwrap();
        let block = AudioBlock::pcm_i32(fmt, vec![1, -1, 2, -2]);
        let sink = std::thread::spawn(move || {
            sink.write(&block).unwrap();
            assert!(sink.pause().unwrap(), "the renderer holds its buffer");
            sink.resume().unwrap();
            let _ = sink.pause().unwrap();
            sink
        })
        .join()
        .unwrap();
        assert!(live.next_chunk().await.is_some());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while log.lock().unwrap().len() < 6 && std::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let actions: Vec<String> = log
            .lock()
            .unwrap()
            .iter()
            .map(|a| a.split(' ').next().unwrap_or("").to_string())
            .collect();
        assert_eq!(
            actions,
            ["Stop", "SetAVTransportURI", "Play", "Pause", "Play", "Pause"]
        );
        std::thread::spawn(move || drop(sink)).join().unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert_eq!(log.lock().unwrap().last().map(String::as_str), Some("Stop"));
    }

    /// A renderer that dropped the stream during a long pause refuses Play: the sink points it
    /// at the same stream again (Stop, SetAVTransportURI, Play) instead of failing the zone.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_play_refused_after_a_pause_sets_the_stream_again() {
        let refuse = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (base, log) = fake_renderer_with(
            std::time::Duration::ZERO,
            refuse.clone(),
            Arc::default(),
            Arc::default(),
        );
        let renderer = Renderer {
            udn: "uuid:refuse".into(),
            friendly_name: "Fake DSM".into(),
            manufacturer: None,
            model: None,
            av_transport: format!("{base}/av"),
            rendering_control: None,
            connection_manager: None,
            zone_group_topology: None,
            location: format!("{base}/desc.xml"),
        };
        let pull = Arc::new(PullOutput::controlled("refuse"));
        let out = UpnpOutput::new(
            renderer,
            pull.clone(),
            "http://node/v1/outputs/pull/refuse/stream.wav".into(),
        );
        let fmt = AudioFormat::pcm(44_100, 16, 2);
        let mut sink = out.open(fmt).await.unwrap();
        let mut live = pull.take_stream(std::time::Duration::from_secs(2)).await.unwrap();
        let block = AudioBlock::pcm_i32(fmt, vec![1, -1, 2, -2]);
        let b2 = block.clone();
        let mut sink = std::thread::spawn(move || {
            sink.write(&b2).unwrap();
            sink
        })
        .join()
        .unwrap();
        assert!(live.next_chunk().await.is_some());
        // Wait for the start to finish, then the renderer refuses the next Play
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while log.lock().unwrap().len() < 3 && std::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        refuse.store(1, std::sync::atomic::Ordering::SeqCst);
        let sink = std::thread::spawn(move || {
            assert!(sink.pause().unwrap());
            sink.resume().unwrap();
            sink
        })
        .join()
        .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while log.lock().unwrap().len() < 8 && std::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let actions: Vec<String> = log
            .lock()
            .unwrap()
            .iter()
            .map(|a| a.split(' ').next().unwrap_or("").to_string())
            .collect();
        assert_eq!(
            actions,
            [
                "Stop",
                "SetAVTransportURI",
                "Play",
                "Pause",
                "Play",
                "Stop",
                "SetAVTransportURI",
                "Play"
            ]
        );
        // The second start's Play was accepted: the next write is no error
        let mut sink = sink;
        let reader = tokio::spawn(async move { while live.next_chunk().await.is_some() {} });
        std::thread::spawn(move || {
            sink.write(&block).expect("no refusal reported");
            drop(sink);
        })
        .join()
        .unwrap();
        reader.abort();
    }

    /// A renderer that answers Pause but plays on (a Linn on the node's live stream) is stopped
    /// for the pause, and its resume asks the zone to start it again; from then on a pause stops
    /// it at once (NUC, 2026-10-30).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_renderer_that_plays_on_after_pause_is_stopped_instead() {
        let ignores = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let (base, log) =
            fake_renderer_with(std::time::Duration::ZERO, Arc::default(), ignores, Arc::default());
        let renderer = Renderer {
            udn: "uuid:linnish".into(),
            friendly_name: "Fake Linn".into(),
            manufacturer: None,
            model: None,
            av_transport: format!("{base}/av"),
            rendering_control: None,
            connection_manager: None,
            zone_group_topology: None,
            location: format!("{base}/desc.xml"),
        };
        let pull = Arc::new(PullOutput::controlled("linnish"));
        let out = UpnpOutput::new(
            renderer,
            pull,
            "http://node/v1/outputs/pull/linnish/stream.wav".into(),
        );
        let fmt = AudioFormat::pcm(44_100, 16, 2);
        let actions = |skip: usize| -> Vec<String> {
            log.lock()
                .unwrap()
                .iter()
                .skip(skip)
                .map(|a| a.split(' ').next().unwrap_or("").to_string())
                .collect()
        };
        let wait_log = |log: Arc<Mutex<Vec<String>>>| {
            move |n: usize| {
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
                while log.lock().unwrap().len() < n && std::time::Instant::now() < deadline {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
            }
        };
        let sink = out.open(fmt).await.unwrap();
        let wait = wait_log(log.clone());
        let (sink, err) = tokio::task::spawn_blocking(move || {
            let mut sink = sink;
            wait(3);
            assert!(sink.pause().unwrap());
            // Resumed at once: a group asks first, and both wait for how the pause went
            let ready = sink.ready_to_resume();
            assert!(
                ready.is_err(),
                "stopped for the pause, it can't carry on: {ready:?}"
            );
            let err = sink.resume().unwrap_err();
            (sink, err)
        })
        .await
        .unwrap();
        assert_eq!(actions(0), ["Stop", "SetAVTransportURI", "Play", "Pause", "Stop"]);
        assert!(err.to_string().contains("starting it again"), "{err}");
        // Learned: the next stream's pause stops it at once, no Pause first
        tokio::task::spawn_blocking(move || drop(sink)).await.unwrap();
        let sink = out.open(fmt).await.unwrap();
        let wait = wait_log(log.clone());
        tokio::task::spawn_blocking(move || {
            let mut sink = sink;
            wait(9);
            assert!(sink.pause().unwrap());
            wait(10);
        })
        .await
        .unwrap();
        // The first sink's close (Stop), the second's start, its pause (a Stop only, no Pause),
        // and its close
        assert_eq!(
            actions(5),
            ["Stop", "Stop", "SetAVTransportURI", "Play", "Stop", "Stop"]
        );
    }

    /// A sink closed while the start is still on its first Stop sends neither the URI nor Play
    /// after it: the renderer stays stopped (nothing plays after the sink's own Stop).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_sink_closed_during_a_slow_start_never_plays() {
        let (base, log) = fake_renderer_slow(std::time::Duration::from_millis(300));
        let renderer = Renderer {
            udn: "uuid:slow".into(),
            friendly_name: "Slow TV".into(),
            manufacturer: None,
            model: None,
            av_transport: format!("{base}/av"),
            rendering_control: None,
            connection_manager: None,
            zone_group_topology: None,
            location: format!("{base}/desc.xml"),
        };
        let out = UpnpOutput::new(
            renderer,
            Arc::new(PullOutput::new("slow")),
            "http://node/v1/outputs/pull/slow/stream.wav".into(),
        );
        let sink = out.open(AudioFormat::pcm(44_100, 16, 2)).await.unwrap();
        std::thread::spawn(move || drop(sink)).join().unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        assert_eq!(*log.lock().unwrap(), vec!["Stop".to_string(), "Stop".into()]);
    }

    /// A renderer like a Linn DS: it answers Play only once it has read the start of the stream.
    /// The sink must be writable before Play's answer (it waited for it, and both waited 5 s).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_renderer_that_answers_play_after_reading_the_stream_gets_audio_at_once() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let heard = Arc::new(AtomicBool::new(false));
        let heard2 = heard.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { break };
                let heard = heard2.clone();
                std::thread::spawn(move || {
                    let mut buf = Vec::new();
                    let mut tmp = [0u8; 4096];
                    loop {
                        let n = s.read(&mut tmp).unwrap_or(0);
                        if n == 0 {
                            break;
                        }
                        buf.extend_from_slice(&tmp[..n]);
                        let text = String::from_utf8_lossy(&buf);
                        if let Some(idx) = text.find("\r\n\r\n") {
                            let len = text
                                .lines()
                                .find_map(|l| {
                                    l.to_ascii_lowercase()
                                        .strip_prefix("content-length: ")
                                        .map(str::to_string)
                                })
                                .and_then(|v| v.trim().parse::<usize>().ok())
                                .unwrap_or(0);
                            if buf.len() >= idx + 4 + len {
                                break;
                            }
                        }
                    }
                    if String::from_utf8_lossy(&buf).contains("#Play\"") {
                        let until = std::time::Instant::now() + std::time::Duration::from_secs(5);
                        while !heard.load(Ordering::SeqCst) && std::time::Instant::now() < until {
                            std::thread::sleep(std::time::Duration::from_millis(5));
                        }
                    }
                    let body = "<s:Envelope><s:Body><u:Response/></s:Body></s:Envelope>";
                    let _ = write!(s, "HTTP/1.1 200 OK\r\nContent-Type: text/xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body);
                });
            }
        });
        let renderer = Renderer {
            udn: "uuid:4c494e4e-1".into(),
            friendly_name: "Klimax DS".into(),
            manufacturer: Some("Linn".into()),
            model: None,
            av_transport: format!("{base}/av"),
            rendering_control: None,
            connection_manager: None,
            zone_group_topology: None,
            location: format!("{base}/desc.xml"),
        };
        let pull = Arc::new(PullOutput::new("linn-play"));
        let out = UpnpOutput::new(
            renderer,
            pull.clone(),
            "http://node/v1/outputs/pull/linn-play/stream.wav".into(),
        );
        let fmt = AudioFormat::pcm(44_100, 16, 2);
        let started = std::time::Instant::now();
        let mut sink = out.open(fmt).await.unwrap();
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "open waited for Play: {:?}",
            started.elapsed()
        );
        let writer = std::thread::spawn(move || {
            sink.write(&AudioBlock::pcm_i32(fmt, vec![1, -1])).unwrap();
            sink
        });
        let mut live = pull.take_stream(std::time::Duration::from_secs(2)).await.unwrap();
        assert_eq!(live.next_chunk().await.unwrap(), vec![1, 0, 0xff, 0xff]);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "audio came late: {:?}",
            started.elapsed()
        );
        heard.store(true, Ordering::SeqCst);
        drop(writer.join().unwrap());
    }

    /// A Linn DS fetches the URI during SetAVTransportURI and answers only once it has read the
    /// start of the stream (~64 KB); without bytes it gives up after 5 s and asks again. Then it
    /// closes that connection and opens a new one to play (NUC, 2026-10-30: the zone started
    /// ~10 s late, open() waited for SetAVTransportURI). Audio must flow at once, and the second
    /// fetch must get the stream from its first byte.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_linn_that_reads_the_stream_during_set_uri_starts_at_once() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::time::{Duration, Instant};
        const PROBE: usize = 64 * 1024;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let pull = Arc::new(PullOutput::new("linn-set"));
        let rt = tokio::runtime::Handle::current();
        // When the Linn had the first PROBE bytes, and what the playing fetch got first
        let probed: Arc<Mutex<Option<Instant>>> = Arc::default();
        // When the playing fetch had the first PROBE bytes, and those bytes
        type Heard = Arc<Mutex<Option<(Instant, Vec<u8>)>>>;
        let played: Heard = Arc::default();
        let first: Arc<Mutex<Vec<u8>>> = Arc::default();
        let (pull2, probed2, played2, first2) = (pull.clone(), probed.clone(), played.clone(), first.clone());
        std::thread::spawn(move || {
            let _runtime = rt.enter();
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { break };
                let mut buf = Vec::new();
                let mut tmp = [0u8; 4096];
                loop {
                    let n = s.read(&mut tmp).unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                    let text = String::from_utf8_lossy(&buf);
                    if let Some(idx) = text.find("\r\n\r\n") {
                        let len = text
                            .lines()
                            .find_map(|l| {
                                l.to_ascii_lowercase()
                                    .strip_prefix("content-length: ")
                                    .map(str::to_string)
                            })
                            .and_then(|v| v.trim().parse::<usize>().ok())
                            .unwrap_or(0);
                        if buf.len() >= idx + 4 + len {
                            break;
                        }
                    }
                }
                let text = String::from_utf8_lossy(&buf).to_string();
                if text.contains("#SetAVTransportURI\"") {
                    // Fetch and read the start; no bytes in 5 s: give up and fetch again
                    'fetch: loop {
                        let Some(mut live) = rt.block_on(pull2.take_stream(Duration::from_secs(5))) else {
                            continue;
                        };
                        let until = tokio::time::Instant::now() + Duration::from_secs(5);
                        let mut got = Vec::new();
                        while got.len() < PROBE {
                            match rt.block_on(tokio::time::timeout_at(until, live.next_chunk())) {
                                Ok(Some(c)) => got.extend(c),
                                _ => continue 'fetch,
                            }
                        }
                        *first2.lock().unwrap() = got[..PROBE].to_vec();
                        *probed2.lock().unwrap() = Some(Instant::now());
                        break; // closes this connection: the Linn opens a new one to play
                    }
                }
                if text.contains("#Play\"") {
                    let pull = pull2.clone();
                    let played = played2.clone();
                    let rt = rt.clone();
                    std::thread::spawn(move || {
                        let mut live = rt.block_on(pull.take_stream(Duration::from_secs(5))).unwrap();
                        let mut got = Vec::new();
                        while got.len() < PROBE {
                            got.extend(rt.block_on(live.next_chunk()).unwrap());
                        }
                        *played.lock().unwrap() = Some((Instant::now(), got[..PROBE].to_vec()));
                        // Keeps playing until the test ends
                        while rt.block_on(live.next_chunk()).is_some() {}
                    });
                }
                let body = "<s:Envelope><s:Body><u:Response/></s:Body></s:Envelope>";
                let _ = write!(s, "HTTP/1.1 200 OK\r\nContent-Type: text/xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body);
            }
        });
        let renderer = Renderer {
            udn: "uuid:4c494e4e-2".into(),
            friendly_name: "Klimax DS".into(),
            manufacturer: Some("Linn".into()),
            model: None,
            av_transport: format!("{base}/av"),
            rendering_control: None,
            connection_manager: None,
            zone_group_topology: None,
            location: format!("{base}/desc.xml"),
        };
        let out = UpnpOutput::new(
            renderer,
            pull.clone(),
            "http://node/v1/outputs/pull/linn-set/stream.wav".into(),
        );
        let fmt = AudioFormat::pcm(44_100, 16, 2);
        let started = Instant::now();
        let mut sink = tokio::time::timeout(Duration::from_secs(3), out.open(fmt))
            .await
            .expect("open waited for the renderer")
            .unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "open took {:?}",
            started.elapsed()
        );
        // The zone writes as it does: blocks of 4096 frames, kept to real time
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        let writer = std::thread::spawn(move || {
            let mut n = 0i32;
            while !stop2.load(Ordering::SeqCst) {
                let samples: Vec<i32> = (0..8192).map(|i| (n * 8192 + i) % 30_000).collect();
                if sink.write(&AudioBlock::pcm_i32(fmt, samples)).is_err() {
                    break;
                }
                n += 1;
            }
            sink
        });
        let deadline = Instant::now() + Duration::from_secs(8);
        while played.lock().unwrap().is_none() && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let probe_at = probed
            .lock()
            .unwrap()
            .expect("the Linn read the start during SetAVTransportURI");
        let (play_at, play_first) = played.lock().unwrap().clone().expect("the Linn played");
        assert!(
            probe_at - started < Duration::from_secs(1),
            "first audio after {:?}",
            probe_at - started
        );
        assert!(
            play_at - started < Duration::from_secs(2),
            "playing after {:?}",
            play_at - started
        );
        assert_eq!(
            play_first,
            *first.lock().unwrap(),
            "the playing fetch starts from the first byte"
        );
        stop.store(true, Ordering::SeqCst);
        drop(writer.join().unwrap());
    }

    /// A renderer that refuses Play never fetches: the zone hears it in seconds, not after the
    /// listener timeout.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_refused_play_fails_the_sink_soon_with_the_reason() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { break };
                let mut buf = Vec::new();
                let mut tmp = [0u8; 4096];
                loop {
                    let n = s.read(&mut tmp).unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                    let text = String::from_utf8_lossy(&buf);
                    if let Some(idx) = text.find("\r\n\r\n") {
                        let len = text
                            .lines()
                            .find_map(|l| {
                                l.to_ascii_lowercase()
                                    .strip_prefix("content-length: ")
                                    .map(str::to_string)
                            })
                            .and_then(|v| v.trim().parse::<usize>().ok())
                            .unwrap_or(0);
                        if buf.len() >= idx + 4 + len {
                            break;
                        }
                    }
                }
                let play = String::from_utf8_lossy(&buf).contains("#Play\"");
                let (status, body) = if play {
                    ("500 Internal Server Error", "<s:Envelope><s:Body><s:Fault><detail><UPnPError><errorCode>714</errorCode><errorDescription>Illegal MIME-type</errorDescription></UPnPError></detail></s:Fault></s:Body></s:Envelope>")
                } else {
                    (
                        "200 OK",
                        "<s:Envelope><s:Body><u:Response/></s:Body></s:Envelope>",
                    )
                };
                let _ = write!(s, "HTTP/1.1 {status}\r\nContent-Type: text/xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body);
            }
        });
        let renderer = Renderer {
            udn: "uuid:tv".into(),
            friendly_name: "TV".into(),
            manufacturer: None,
            model: None,
            av_transport: format!("{base}/av"),
            rendering_control: None,
            connection_manager: None,
            zone_group_topology: None,
            location: format!("{base}/desc.xml"),
        };
        let pull = Arc::new(PullOutput::new("tv-refuses"));
        let out = UpnpOutput::new(
            renderer,
            pull,
            "http://node/v1/outputs/pull/tv-refuses/stream.wav".into(),
        );
        let fmt = AudioFormat::pcm(44_100, 16, 2);
        let mut sink = out.open(fmt).await.unwrap();
        let started = std::time::Instant::now();
        let e = std::thread::spawn(move || loop {
            if let Err(e) = sink.write(&AudioBlock::pcm_i32(fmt, vec![0; 4096])) {
                break e;
            }
        })
        .join()
        .unwrap();
        assert!(e.to_string().contains("refused Play"), "{e}");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(6),
            "{:?}",
            started.elapsed()
        );
    }

    /// A renderer that refused both Pause and Stop played its buffer out: alone it carries on
    /// from the stream, but a group can't count it as holding its buffer (it would play out of
    /// step), so it isn't ready to resume and the group starts again in step.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_renderer_that_refused_the_pause_is_not_ready_to_resume_in_step() {
        let refuses = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (base, log) = fake_renderer_with(
            std::time::Duration::ZERO,
            Arc::default(),
            Arc::default(),
            refuses.clone(),
        );
        let renderer = Renderer {
            udn: "uuid:stubborn".into(),
            friendly_name: "Stubborn TV".into(),
            manufacturer: None,
            model: None,
            av_transport: format!("{base}/av"),
            rendering_control: None,
            connection_manager: None,
            zone_group_topology: None,
            location: format!("{base}/desc.xml"),
        };
        let out = UpnpOutput::new(
            renderer,
            Arc::new(PullOutput::controlled("stubborn")),
            "http://node/v1/outputs/pull/stubborn/stream.wav".into(),
        );
        let sink = out.open(AudioFormat::pcm(44_100, 16, 2)).await.unwrap();
        tokio::task::spawn_blocking(move || {
            let mut sink = sink;
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
            while log.lock().unwrap().len() < 3 && std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            refuses.store(true, std::sync::atomic::Ordering::SeqCst);
            assert!(sink.pause().unwrap());
            let ready = sink.ready_to_resume();
            assert!(ready.is_err(), "it played its buffer out: {ready:?}");
            refuses.store(false, std::sync::atomic::Ordering::SeqCst);
            drop(sink);
        })
        .await
        .unwrap();
    }
}
