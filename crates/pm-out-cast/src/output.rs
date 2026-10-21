//! A Cast device as an `Output`: open launches the receiver app and loads
//! the pull stream with the track's metadata and art; a [`Session`] keeps the
//! link alive while it plays; the sink is the pull sink; close stops the app.
//!
//! Metadata changes at each LOAD, which is each time the zone opens the
//! output (a new format run). Inside a gapless run the stream continues and
//! the screen keeps the first track's details: reloading would break the gap.

use crate::link::{block_on, connect_tls};
use crate::session::{Media, Session, PING_EVERY};
use crate::{CastError, Device, Meta};
use async_trait::async_trait;
use pm_model::{AudioBlock, AudioFormat, OutputCaps, QualityCeiling, SampleKind, Volume, VolumeKind};
use pm_zones::outputs::PullOutput;
use pm_zones::{AudioSink, BufferLevel, Output, OutputId, QueueItem, SinkError};
use std::sync::{Arc, Mutex};

pub use pm_zones::Describe;

pub struct CastOutput {
    device: Device,
    pull: Arc<PullOutput>,
    stream_url: String,
    caps: OutputCaps,
    /// The live session while a sink is open.
    session: Arc<Mutex<Option<Session>>>,
    describe: Option<Describe>,
    /// The track about to play or playing (`Output::now_playing`).
    meta: Mutex<Option<Meta>>,
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
            session: Arc::new(Mutex::new(None)),
            describe: None,
            meta: Mutex::new(None),
        }
    }

    /// Show artist, album and art from the node's library, not only the queue item's fields.
    pub fn with_describe(mut self, describe: Describe) -> Self {
        self.describe = Some(describe);
        self
    }

    /// What the current session has seen (resumes, takeover), if one is open.
    pub fn session_status(&self) -> Option<crate::session::SessionStatus> {
        self.session
            .lock()
            .expect("session")
            .as_ref()
            .map(Session::status)
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
        if self.device.is_group() {
            return format!("{} (Cast group)", self.device.friendly_name);
        }
        match &self.device.model {
            Some(m) => format!("{} ({m})", self.device.friendly_name),
            None => self.device.friendly_name.clone(),
        }
    }

    fn now_playing(&self, item: Option<&QueueItem>) {
        let meta = item.map(|it| match &self.describe {
            Some(d) => d(it),
            None => Meta::from_item(it),
        });
        *self.meta.lock().expect("meta") = meta;
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
        let media = Media {
            url: self.stream_url.clone(),
            content_type: "audio/wav".into(),
            meta: self.meta.lock().expect("meta").clone().unwrap_or_default(),
        };
        // A sink left open (a format change) hands over to the new one.
        let old = self.session.lock().expect("session").take();
        let session = block_on(async move {
            if let Some(old) = old {
                old.stop().await;
            }
            let mut link = connect_tls(addr).await?;
            link.launch().await?;
            link.load(&media.url, &media.content_type, &media.meta).await?;
            Ok::<_, CastError>(Session::spawn(link, media, PING_EVERY, move || connect_tls(addr)))
        })
        .map_err(cast_err)?;
        *self.session.lock().expect("session") = Some(session);
        Ok(Box::new(CastSink {
            inner,
            session: self.session.clone(),
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
        if let Some(s) = self.session.lock().expect("session").as_ref() {
            s.set_volume(level, muted);
            return Ok(());
        }
        let addr = self.device.addr;
        block_on(async move {
            // Not playing: volume is a receiver-level command, a short connection suffices.
            let mut link = connect_tls(addr).await?;
            link.set_volume(level, muted).await
        })
        .map_err(cast_err)
    }
}

struct CastSink {
    inner: Box<dyn AudioSink>,
    session: Arc<Mutex<Option<Session>>>,
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
        if let Some(session) = self.session.lock().expect("session").take() {
            block_on(session.stop());
        }
    }
}
