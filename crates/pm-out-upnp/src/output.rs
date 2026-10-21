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
        Self {
            renderer,
            pull,
            stream_url,
            caps,
            kind: StreamKind::Wav {
                mime: "audio/wav".into(),
            },
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

    /// The URL for this renderer's stream kind (`…/stream.wav` → `…/stream.l16`).
    fn url(&self) -> String {
        self.stream_url
            .replacen("/stream.wav", &format!("/{}", self.kind.file()), 1)
    }

    pub fn renderer(&self) -> &Renderer {
        &self.renderer
    }

    pub fn pull(&self) -> &Arc<PullOutput> {
        &self.pull
    }
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
        let url = self.url();
        let protocol_info = self.kind.protocol_info(fmt.rate_hz, fmt.channels);
        let meta = didl(
            &self.meta.lock().expect("meta").clone().unwrap_or_default(),
            &url,
            &protocol_info,
        );
        let av = self.renderer.av_transport.clone();
        offload(move || -> Result<(), UpnpError> {
            // Some TVs refuse a new URI while they hold one; a renderer with nothing loaded may
            // answer Stop with an error, which is fine.
            let _ = soap::call(&av, AV_TRANSPORT, "Stop", &[("InstanceID", "0")]);
            soap::call(
                &av,
                AV_TRANSPORT,
                "SetAVTransportURI",
                &[
                    ("InstanceID", "0"),
                    ("CurrentURI", &url),
                    ("CurrentURIMetaData", &meta),
                ],
            )?;
            // TVs answer 701 while they still open the URI: try again for a moment.
            let mut tries = 0;
            loop {
                match soap::call(&av, AV_TRANSPORT, "Play", &[("InstanceID", "0"), ("Speed", "1")]) {
                    Ok(_) => return Ok(()),
                    Err(e) if tries < 4 => {
                        tries += 1;
                        tracing::debug!(tries, "renderer Play refused, retrying: {e}");
                        std::thread::sleep(std::time::Duration::from_millis(400));
                    }
                    Err(e) => return Err(e),
                }
            }
        })
        .await
        .map_err(soap_err)?;
        Ok(Box::new(UpnpSink {
            inner,
            av_transport: self.renderer.av_transport.clone(),
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
    av_transport: String,
}

impl AudioSink for UpnpSink {
    fn format(&self) -> AudioFormat {
        self.inner.format()
    }
    fn write(&mut self, block: &AudioBlock) -> Result<(), SinkError> {
        self.inner.write(block)
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
        if let Err(e) = soap::call(&self.av_transport, AV_TRANSPORT, "Stop", &[("InstanceID", "0")]) {
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
        let header = live.rx.recv().await.unwrap();
        assert_eq!(&header[0..4], b"RIFF");
        let data = live.rx.recv().await.unwrap();
        assert_eq!(data, vec![1, 0, 0xff, 0xff, 2, 0, 0xfe, 0xff]);
        writer.join().unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let actions = log.lock().unwrap().clone();
        assert_eq!(
            actions,
            vec![
                "Stop".to_string(),
                "SetAVTransportURI http://node.local:7788/v1/outputs/pull/fake/stream.wav".into(),
                "Play".into(),
                "Stop".into()
            ]
        );

        out.set_volume(Volume::Percent(30)).await.unwrap();
        out.set_volume(Volume::Up).await.unwrap();
        let actions = log.lock().unwrap().clone();
        assert!(actions.iter().any(|a| a == "SetVolume"));
        assert!(actions.iter().any(|a| a == "GetVolume"));
    }
}
