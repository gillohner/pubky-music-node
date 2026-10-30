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
        let target = Arc::new(Mutex::new(None::<String>));
        let target_set = target.clone();
        let (pull, generation) = (self.pull.clone(), self.pull.generation());
        let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop_retrying = cancelled.clone();
        std::thread::Builder::new()
            .name("upnp-play".into())
            .spawn(move || {
                let result = start(&renderer, &url, &meta, &target_set, &stop_retrying);
                if let Err(e) = &result {
                    // Nobody will fetch the stream: the zone hears why now, not after 15 s
                    pull.abort(generation, e.clone());
                }
                let _ = played_tx.send(result);
            })
            .map_err(|e| SinkError::Device(format!("cannot start the Play call: {e}")))?;
        Ok(Box::new(UpnpSink {
            inner,
            renderer: self.renderer.av_transport.clone(),
            av_transport: target,
            played: Some(played),
            cancelled,
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
    /// The answer to SetAVTransportURI and Play, until it came.
    played: Option<std::sync::mpsc::Receiver<Result<(), String>>>,
    /// Set on close: the Play thread stops retrying.
    cancelled: Arc<std::sync::atomic::AtomicBool>,
}

impl UpnpSink {
    /// Play's answer if it came: a refusal is this sink's error from now on.
    fn play_refused(&mut self) -> Option<SinkError> {
        let rx = self.played.as_ref()?;
        match rx.try_recv() {
            Ok(Ok(())) | Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.played = None;
                None
            }
            Ok(Err(e)) => {
                self.played = None;
                Some(SinkError::Device(e))
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => None,
        }
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
    fn frames_written(&self) -> u64 {
        self.inner.frames_written()
    }
}

impl Drop for UpnpSink {
    fn drop(&mut self) {
        // Stop after the start's answers (briefly), so the renderer does not start after it
        self.cancelled.store(true, std::sync::atomic::Ordering::SeqCst);
        if let Some(rx) = self.played.take() {
            let _ = rx.recv_timeout(std::time::Duration::from_secs(2));
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
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let log = Arc::new(Mutex::new(Vec::new()));
        let log2 = log.clone();
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
                log2.lock().unwrap().push(if uri.is_empty() {
                    action.clone()
                } else {
                    format!("{action} {uri}")
                });
                if action == "Stop" {
                    std::thread::sleep(stop);
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
                "SetAVTransportURI http://node.local:7788/v1/outputs/pull/fake/stream.wav?g=1".into(),
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
}
