//! A Cast device as an `Output`: open launches the receiver app and loads
//! the pull stream with the track's metadata and art; a [`Session`] keeps the
//! link alive while it plays; the sink is the pull sink; close stops the app.
//!
//! Pause is the receiver's own media PAUSE (it plays out seconds of buffer otherwise), or a
//! media STOP when it plays on; the resume after a STOP opens the output again.
//!
//! Metadata changes at each LOAD, which is each time the zone opens the
//! output (a new format run). Inside a gapless run the stream continues and
//! the screen keeps the first track's details: reloading would break the gap.

use crate::link::{block_on, connect_tls};
use crate::session::{Media, Paused, Session, PING_EVERY};
use crate::{CastError, Device, Meta};
use async_trait::async_trait;
use pm_model::{AudioBlock, AudioFormat, OutputCaps, QualityCeiling, SampleKind, Volume, VolumeKind};
use pm_zones::outputs::PullOutput;
use pm_zones::{AudioSink, BufferLevel, Output, OutputId, QueueItem, SinkError};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::oneshot;

/// How long a resume waits for its pause's outcome (the receiver's PAUSED, or the STOP after it).
const PAUSE_OUTCOME_WAIT: Duration = Duration::from_secs(3);
/// How long a resume waits for the session to send PLAY.
const PLAY_WAIT: Duration = Duration::from_secs(2);

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

    fn model(&self) -> Option<String> {
        self.device.model.clone().filter(|m| !m.trim().is_empty())
    }

    fn sync(&self) -> pm_zones::SyncClass {
        if self.device.is_group() {
            pm_zones::SyncClass::Tight
        } else {
            pm_zones::SyncClass::Single
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
            paused: None,
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
    /// Paused on the receiver: how it went, once the session knows.
    paused: Option<oneshot::Receiver<Paused>>,
}

/// Wait for a session's answer on the Cast runtime; `None` when it doesn't come in time or the
/// session ended.
fn answer<T: Send + 'static>(rx: oneshot::Receiver<T>, within: Duration) -> Option<T> {
    block_on(async move { tokio::time::timeout(within, rx).await.ok()?.ok() })
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
    /// The receiver's own PAUSE, sent by the session without holding the zone; the stream keeps
    /// its place and pacing for the resume.
    fn pause(&mut self) -> Result<bool, SinkError> {
        let Some(rx) = self.session.lock().expect("session").as_ref().map(Session::pause) else {
            return self.inner.flush().map(|()| false);
        };
        self.paused = Some(rx);
        self.inner.pause()?;
        Ok(true)
    }
    /// PLAY after a PAUSE. A receiver stopped for the pause (or a session that ended meanwhile)
    /// is an error: the zone opens the output again from what was last heard.
    fn resume(&mut self) -> Result<(), SinkError> {
        let Some(rx) = self.paused.take() else {
            return self.inner.resume();
        };
        let reopen = |why: &str| Err(SinkError::Device(format!("Cast: {why}; starting it again")));
        match answer(rx, PAUSE_OUTCOME_WAIT) {
            Some(Paused::Stopped) => return reopen("the receiver was stopped for the pause"),
            None => return reopen("the session ended during the pause"),
            // Nothing was paused: the stream carries on
            Some(Paused::Refused) => return self.inner.resume(),
            Some(Paused::Held) => {}
        }
        let Some(play) = self.session.lock().expect("session").as_ref().map(Session::play) else {
            return reopen("the session ended during the pause");
        };
        if answer(play, PLAY_WAIT) != Some(true) {
            return reopen("the receiver has no media to play on");
        }
        self.inner.resume()
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::tests::{log, setup, until};

    async fn sink(session: Session, name: &str) -> CastSink {
        let pull = PullOutput::controlled(name);
        CastSink {
            inner: pull.open(AudioFormat::pcm(44_100, 16, 2)).await.unwrap(),
            session: Arc::new(Mutex::new(Some(session))),
            paused: None,
        }
    }

    /// The zone's pause reaches the receiver (it played out ~5 s of buffer before: the sink only
    /// stopped feeding the stream), and its resume plays on.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_zones_pause_and_resume_reach_the_receiver() {
        let (session, fake, _ctl) = setup().await;
        let mut sink = sink(session, "cast-pause").await;
        let sink = std::thread::spawn(move || {
            assert!(sink.pause().unwrap(), "the receiver holds its buffer");
            sink.resume().unwrap();
            // Not paused: nothing to tell the receiver
            sink.resume().unwrap();
            sink
        })
        .join()
        .unwrap();
        until(&fake, "1 media PLAY 9").await;
        let media: Vec<String> = log(&fake).into_iter().filter(|l| l.contains(" media ")).collect();
        assert_eq!(media, ["1 media LOAD", "1 media PAUSE 9", "1 media PLAY 9"]);
        drop(sink);
    }

    /// A receiver stopped for the pause (it played on) has nothing to play on: the resume is an
    /// error, so the zone opens the output again from what was last heard.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_receiver_stopped_for_the_pause_is_opened_again() {
        let (session, fake, _ctl) = setup().await;
        fake.ignores_pause
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let mut sink = sink(session, "cast-stop").await;
        let (paused, resumed, sink) = std::thread::spawn(move || {
            let paused = sink.pause().unwrap();
            let resumed = sink.resume();
            (paused, resumed, sink)
        })
        .join()
        .unwrap();
        assert!(paused);
        let e = resumed.unwrap_err().to_string();
        assert!(e.contains("stopped for the pause"), "{e}");
        assert!(
            !log(&fake).iter().any(|l| l.contains("media PLAY")),
            "{:?}",
            log(&fake)
        );
        drop(sink);
    }
}
