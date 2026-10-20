//! A Cast device as an `Output`: open launches the receiver app and loads
//! the pull stream; the sink is the pull sink; close stops the app.

use crate::link::{block_on, connect_tls, CastLink};
use crate::{CastError, Device};
use async_trait::async_trait;
use pm_model::{AudioBlock, AudioFormat, OutputCaps, QualityCeiling, SampleKind, Volume, VolumeKind};
use pm_zones::outputs::PullOutput;
use pm_zones::{AudioSink, BufferLevel, Output, OutputId, SinkError};
use std::sync::{Arc, Mutex};

type TlsLink = CastLink<tokio_rustls::client::TlsStream<tokio::net::TcpStream>>;

pub struct CastOutput {
    device: Device,
    pull: Arc<PullOutput>,
    stream_url: String,
    caps: OutputCaps,
    /// The live session while a sink is open.
    link: Arc<Mutex<Option<TlsLink>>>,
}

impl CastOutput {
    pub fn new(device: Device, pull: Arc<PullOutput>, stream_url: String) -> Self {
        let mut caps = OutputCaps::unlimited();
        caps.pcm_rates = vec![44_100, 48_000];
        caps.max_pcm_bits = 24;
        caps.pcm_bits = vec![16, 24];
        caps.max_channels = 2;
        caps.dsd = None;
        caps.hw_volume = Some(VolumeKind::Percent);
        caps.standby = false;
        caps.gapless = true;
        caps.groupable = false;
        caps.fixed_latency_ms = Some(1500);
        caps.quality_ceiling = QualityCeiling::LosslessUpTo {
            rate_hz: 48_000,
            bits: 24,
        };
        Self {
            device,
            pull,
            stream_url,
            caps,
            link: Arc::new(Mutex::new(None)),
        }
    }

    pub fn device(&self) -> &Device {
        &self.device
    }

    pub fn pull(&self) -> &Arc<PullOutput> {
        &self.pull
    }
}

fn cast_err(e: CastError) -> SinkError {
    SinkError::Device(e.to_string())
}

#[async_trait]
impl Output for CastOutput {
    fn id(&self) -> OutputId {
        OutputId::new("cast", &self.device.id)
    }

    fn name(&self) -> String {
        match &self.device.model {
            Some(m) => format!("{} ({m})", self.device.friendly_name),
            None => self.device.friendly_name.clone(),
        }
    }

    fn caps(&self) -> &OutputCaps {
        &self.caps
    }

    async fn open(&self, fmt: AudioFormat) -> Result<Box<dyn AudioSink>, SinkError> {
        if fmt.kind != SampleKind::Pcm || !self.caps.accepts(&fmt) {
            return Err(SinkError::UnsupportedFormat(self.id(), fmt));
        }
        let inner = self.pull.open(fmt).await?;
        let addr = self.device.addr;
        let url = self.stream_url.clone();
        let name = self.device.friendly_name.clone();
        let link = block_on(async move {
            let mut link = connect_tls(addr).await?;
            link.launch().await?;
            link.load(&url, "audio/wav", &format!("Pubky Music → {name}"))
                .await?;
            Ok::<_, CastError>(link)
        })
        .map_err(cast_err)?;
        *self.link.lock().expect("link") = Some(link);
        Ok(Box::new(CastSink {
            inner,
            link: self.link.clone(),
        }))
    }

    async fn set_volume(&self, v: Volume) -> Result<(), SinkError> {
        let (level, muted) = match v {
            Volume::Percent(p) => (Some(p.min(100) as f64 / 100.0), None),
            Volume::Db(db) => (Some(10f64.powf(db.clamp(-60.0, 0.0) as f64 / 20.0)), None),
            Volume::Mute(m) => (None, Some(m)),
            Volume::Up | Volume::Down => {
                return Err(SinkError::Device("cast volume needs an absolute level".into()));
            }
        };
        let addr = self.device.addr;
        block_on(async move {
            // Volume is a receiver-level command: a short connection suffices.
            let mut link = connect_tls(addr).await?;
            link.set_volume(level, muted).await
        })
        .map_err(cast_err)
    }
}

struct CastSink {
    inner: Box<dyn AudioSink>,
    link: Arc<Mutex<Option<TlsLink>>>,
}

impl AudioSink for CastSink {
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

impl Drop for CastSink {
    fn drop(&mut self) {
        if let Some(mut link) = self.link.lock().expect("link").take() {
            block_on(async move {
                if let Err(e) = link.stop().await {
                    tracing::debug!("cast stop: {e}");
                }
            });
        }
    }
}
