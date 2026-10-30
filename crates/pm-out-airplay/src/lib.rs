//! AirPlay 1 receivers (RAOP) as outputs (infra brief §3.4): Apple TVs, AirPort Express,
//! AirPlay speakers, shairport-sync on a Pi.
//!
//! - **Discovery** is mDNS `_raop._tcp`. The instance name is `<MAC>@<name>`, and TXT says what the
//!   receiver takes: `et` (encryption types), `cn` (codecs) and `md` (metadata).
//! - **What is sent:** unencrypted (`et` must list 0), verbatim ALAC (`cn` lists 1, or says
//!   nothing), 44.1 kHz 16-bit stereo, the format AirPlay 1 receivers play. The zone converts to it
//!   and the signal path says so. Receivers that require encryption are listed as found and skipped,
//!   with the reason in the log.
//! - **AirPlay 2** (multiroom timing, HomePod) is not built. Its pairing has only reverse-engineered
//!   open implementations, which the node does not use (handoff §2). Most AirPlay 2 devices still
//!   accept AirPlay 1.

pub mod alac;
pub mod session;

use async_trait::async_trait;
use pm_model::{
    AudioBlock, AudioFormat, OutputCaps, QualityCeiling, SampleKind, Samples, Volume, VolumeKind,
};
use pm_zones::sink::check_block;
use pm_zones::{AudioSink, BufferLevel, Output, OutputId, QueueItem, SinkError, TrackMeta};
use session::{Session, Text, SAMPLE_RATE};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const SERVICE_TYPE: &str = "_raop._tcp.local.";
/// How far the sink runs ahead of the music (the receiver buffers two seconds).
const AHEAD: Duration = Duration::from_millis(500);

#[derive(Debug, thiserror::Error)]
pub enum AirPlayError {
    #[error("network: {0}")]
    Io(#[from] std::io::Error),
    #[error("protocol: {0}")]
    Protocol(String),
    #[error("the receiver refused {0} ({1})")]
    Refused(String, u16),
    /// The receiver hung up (it ends an RTSP connection it thinks is idle).
    #[error("the receiver closed the connection ({0})")]
    Closed(String),
    /// What the node cannot do for this receiver, in words for the zone's error.
    #[error("{0}")]
    Unsupported(String),
}

/// An AirPlay receiver as found on the network.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Device {
    /// The MAC part of the instance name: stable per device.
    pub id: String,
    pub name: String,
    pub model: Option<String>,
    pub addr: SocketAddr,
    /// Encryption types it accepts (`et`).
    pub encryption: Vec<String>,
    /// Codecs it accepts (`cn`); empty = not said.
    pub codecs: Vec<String>,
}

impl Device {
    /// The receiver wants `POST /auth-setup` before ANNOUNCE: AirPlay 2 speakers that list MFi
    /// authentication (`et` 4; Bluesound, many 2020s speakers) answer ANNOUNCE with 403 without
    /// it. The node sends a public key and checks nothing back (the stream stays unencrypted).
    pub fn needs_auth_setup(&self) -> bool {
        self.encryption.iter().any(|e| e == "4")
    }

    /// Why the node cannot play to it, if it cannot.
    pub fn unsupported(&self) -> Option<String> {
        if !self.encryption.is_empty() && !self.encryption.iter().any(|e| e == "0") {
            return Some("it accepts only encrypted AirPlay (et lists no 0)".into());
        }
        if !self.codecs.is_empty() && !self.codecs.iter().any(|c| c == "1") {
            return Some("it does not take Apple Lossless (cn lists no 1)".into());
        }
        None
    }
}

/// Split a `_raop` instance name `<MAC>@<name>` (the service type already removed).
pub fn split_instance(instance: &str) -> (String, String) {
    match instance.split_once('@') {
        Some((mac, name)) => (mac.to_string(), name.to_string()),
        None => (instance.to_string(), instance.to_string()),
    }
}

/// mDNS browse for AirPlay receivers within `timeout`.
pub fn discover(timeout: Duration) -> Vec<Device> {
    let Ok(daemon) = mdns_sd::ServiceDaemon::new() else {
        return Vec::new();
    };
    let Ok(rx) = daemon.browse(SERVICE_TYPE) else {
        let _ = daemon.shutdown();
        return Vec::new();
    };
    let deadline = Instant::now() + timeout;
    let mut out: Vec<Device> = Vec::new();
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        match rx.recv_timeout(left) {
            Ok(mdns_sd::ServiceEvent::ServiceResolved(info)) => {
                let addrs: Vec<std::net::IpAddr> =
                    info.get_addresses().iter().map(|a| a.to_ip_addr()).collect();
                let Some(ip) = addrs
                    .iter()
                    .find(|a| a.is_ipv4())
                    .or_else(|| addrs.first())
                    .copied()
                else {
                    continue;
                };
                let instance = info
                    .get_fullname()
                    .trim_end_matches(SERVICE_TYPE)
                    .trim_end_matches('.');
                let (id, name) = split_instance(instance);
                if out.iter().any(|d| d.id == id) {
                    continue;
                }
                let list = |k: &str| {
                    info.get_property_val_str(k)
                        .map(|v| {
                            v.split(',')
                                .map(|x| x.trim().to_string())
                                .filter(|x| !x.is_empty())
                                .collect()
                        })
                        .unwrap_or_default()
                };
                out.push(Device {
                    id,
                    name,
                    model: info.get_property_val_str("am").map(str::to_string),
                    addr: SocketAddr::new(ip, info.get_port()),
                    encryption: list("et"),
                    codecs: list("cn"),
                });
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    let _ = daemon.stop_browse(SERVICE_TYPE);
    let _ = daemon.shutdown();
    out
}

pub struct AirPlayOutput {
    device: Device,
    caps: OutputCaps,
    describe: Option<pm_zones::Describe>,
    meta: Mutex<Option<TrackMeta>>,
    /// The session while a sink is open, for volume.
    session: Arc<Mutex<Option<Session>>>,
}

impl AirPlayOutput {
    pub fn new(device: Device) -> Self {
        let mut caps = OutputCaps::unlimited();
        caps.pcm_rates = vec![SAMPLE_RATE];
        caps.pcm_bits = vec![16];
        caps.max_pcm_bits = 16;
        caps.max_channels = 2;
        caps.dsd = None;
        caps.hw_volume = Some(VolumeKind::Percent);
        caps.standby = false;
        caps.gapless = true;
        caps.groupable = false;
        caps.fixed_latency_ms = Some(2000);
        caps.quality_ceiling = QualityCeiling::LosslessUpTo {
            rate_hz: SAMPLE_RATE,
            bits: 16,
        };
        Self {
            device,
            caps,
            describe: None,
            meta: Mutex::new(None),
            session: Arc::new(Mutex::new(None)),
        }
    }

    pub fn with_describe(mut self, d: pm_zones::Describe) -> Self {
        self.describe = Some(d);
        self
    }

    pub fn device(&self) -> &Device {
        &self.device
    }
}

fn err(e: AirPlayError) -> SinkError {
    SinkError::Device(e.to_string())
}

/// Open a session and show the track's text.
fn start_session(addr: SocketAddr, auth: bool, text: &Text) -> Result<Session, AirPlayError> {
    let mut s = Session::start(addr, auth)?;
    let _ = s.set_text(text);
    Ok(s)
}

/// AirPlay volume: -30 dB (quietest) to 0 dB; -144 is mute.
fn percent_to_db(p: u8) -> f32 {
    if p == 0 {
        -144.0
    } else {
        -30.0 + 30.0 * f32::from(p.min(100)) / 100.0
    }
}

#[async_trait]
impl Output for AirPlayOutput {
    fn id(&self) -> OutputId {
        OutputId::new("airplay", &self.device.id)
    }

    fn name(&self) -> String {
        format!("{} (AirPlay)", self.device.name)
    }

    fn model(&self) -> Option<String> {
        self.device.model.clone().filter(|m| !m.trim().is_empty())
    }

    fn caps(&self) -> &OutputCaps {
        &self.caps
    }

    fn now_playing(&self, item: Option<&QueueItem>) {
        let meta = item.map(|it| match &self.describe {
            Some(d) => d(it),
            None => TrackMeta::from_item(it),
        });
        if let (Some(m), Some(s)) = (&meta, self.session.lock().expect("session").as_mut()) {
            // Mid-run (gapless): update the text on the receiver right away.
            let _ = s.set_text(&Text {
                title: m.title.clone(),
                artist: m.artist.clone(),
                album: m.album.clone(),
            });
        }
        *self.meta.lock().expect("meta") = meta;
    }

    async fn open(&self, fmt: AudioFormat) -> Result<Box<dyn AudioSink>, SinkError> {
        if fmt.kind != SampleKind::Pcm || !self.caps.accepts(&fmt) {
            return Err(SinkError::UnsupportedFormat(self.id(), fmt));
        }
        if let Some(old) = self.session.lock().expect("session").take() {
            old.teardown();
        }
        let (addr, auth) = (self.device.addr, self.device.needs_auth_setup());
        let meta = self.meta.lock().expect("meta").clone().unwrap_or_default();
        let text = Text {
            title: meta.title,
            artist: meta.artist,
            album: meta.album,
        };
        let first = text.clone();
        let session = std::thread::spawn(move || start_session(addr, auth, &first))
            .join()
            .map_err(|_| SinkError::Device("airplay thread panicked".into()))?
            .map_err(err)?;
        *self.session.lock().expect("session") = Some(session);
        Ok(Box::new(AirPlaySink {
            fmt,
            session: self.session.clone(),
            written: 0,
            clock: (Instant::now(), 0),
            addr,
            auth,
            text,
        }))
    }

    async fn set_volume(&self, v: Volume) -> Result<(), SinkError> {
        let db = match v {
            Volume::Percent(p) => percent_to_db(p),
            Volume::Db(db) => db.clamp(-30.0, 0.0),
            Volume::Mute(true) => -144.0,
            Volume::Mute(false) => 0.0,
            Volume::Up | Volume::Down => {
                return Err(SinkError::Device("AirPlay volume needs a level".into()))
            }
        };
        match self.session.lock().expect("session").as_mut() {
            Some(s) => s.set_volume_db(db).map_err(err),
            // AirPlay 1 volume lives in a session; it applies when one is open.
            None => Err(SinkError::Device("AirPlay sets volume while playing".into())),
        }
    }
}

struct AirPlaySink {
    fmt: AudioFormat,
    session: Arc<Mutex<Option<Session>>>,
    written: u64,
    clock: (Instant, u64),
    /// To open the session again when the receiver closed it (a Linn does after a pause).
    addr: SocketAddr,
    auth: bool,
    text: Text,
}

impl AirPlaySink {
    fn played(&self) -> Duration {
        Duration::from_secs_f64((self.written - self.clock.1) as f64 / f64::from(SAMPLE_RATE))
    }
}

impl AudioSink for AirPlaySink {
    fn format(&self) -> AudioFormat {
        self.fmt
    }

    fn write(&mut self, block: &AudioBlock) -> Result<(), SinkError> {
        check_block(self.fmt, block)?;
        let Samples::PcmI32(s) = &block.samples else {
            return Err(SinkError::FloatNotAccepted);
        };
        // Mono doubles to stereo; 16-bit values as they are.
        let stereo: Vec<i16> = if self.fmt.channels == 1 {
            s.iter().flat_map(|x| [*x as i16, *x as i16]).collect()
        } else {
            s.iter().map(|x| *x as i16).collect()
        };
        {
            let mut guard = self.session.lock().expect("session");
            // The receiver closed the last session: open a new one and go on
            if guard.is_none() {
                tracing::info!(addr = %self.addr, "AirPlay: opening the session again");
                *guard = Some(start_session(self.addr, self.auth, &self.text).map_err(err)?);
                self.clock = (Instant::now(), self.written);
            }
            let session = guard.as_mut().expect("session just opened");
            match session.push(&stereo) {
                Ok(_) => {}
                Err(AirPlayError::Closed(why)) => {
                    tracing::info!(addr = %self.addr, "AirPlay receiver closed the session ({why}); opening it again");
                    if let Some(old) = guard.take() {
                        old.teardown();
                    }
                    let mut fresh = start_session(self.addr, self.auth, &self.text).map_err(err)?;
                    fresh.push(&stereo).map_err(err)?;
                    *guard = Some(fresh);
                    self.clock = (Instant::now(), self.written);
                }
                Err(e) => return Err(err(e)),
            }
        }
        self.written += block.frames() as u64;
        let elapsed = self.clock.0.elapsed();
        let played = self.played();
        if elapsed > played + AHEAD {
            self.clock = (Instant::now(), self.written);
        } else if played > elapsed + AHEAD {
            std::thread::sleep(played - elapsed - AHEAD);
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
        let mut guard = self.session.lock().expect("session");
        if let Some(s) = guard.as_mut() {
            match s.flush() {
                Ok(()) => {}
                // Nothing left to flush on a receiver that hung up; the next write opens a new session
                Err(AirPlayError::Closed(why)) => {
                    tracing::info!(addr = %self.addr, "AirPlay receiver closed the session ({why})");
                    if let Some(old) = guard.take() {
                        old.teardown();
                    }
                }
                Err(e) => return Err(err(e)),
            }
        }
        drop(guard);
        self.clock = (Instant::now(), self.written);
        Ok(())
    }

    fn drain(&mut self) -> Result<(), SinkError> {
        if let Some(s) = self.session.lock().expect("session").as_mut() {
            s.flush_pending().map_err(err)?;
        }
        std::thread::sleep(self.played().saturating_sub(self.clock.0.elapsed()));
        Ok(())
    }

    fn frames_written(&self) -> u64 {
        self.written
    }
}

impl Drop for AirPlaySink {
    fn drop(&mut self) {
        if let Some(s) = self.session.lock().expect("session").take() {
            s.teardown();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instance_names_and_support() {
        assert_eq!(
            split_instance("AABBCCDDEEFF@Living Room"),
            ("AABBCCDDEEFF".into(), "Living Room".into())
        );
        let mut d = Device {
            id: "x".into(),
            name: "x".into(),
            model: None,
            addr: "127.0.0.1:5000".parse().unwrap(),
            encryption: vec!["0".into(), "1".into()],
            codecs: vec!["0".into(), "1".into(), "2".into()],
        };
        assert_eq!(d.unsupported(), None);
        d.encryption = vec!["1".into()];
        assert!(d.unsupported().unwrap().contains("encrypted"));
        d.encryption = vec!["0".into()];
        d.codecs = vec!["2".into()];
        assert!(d.unsupported().unwrap().contains("Apple Lossless"));
        assert_eq!(percent_to_db(0), -144.0);
        assert_eq!(percent_to_db(100), 0.0);
        assert_eq!(percent_to_db(50), -15.0);
    }
}

/// Against a real receiver: `AIRPLAY=127.0.0.1:5000 AIRPLAY_OUT=<shairport-sync stdout file>
/// cargo test -p pm-out-airplay -- --ignored` (shairport-sync with `-o stdout`; docs/CONNECTION.md).
#[cfg(test)]
mod receiver_tests {
    use super::*;

    fn block_on<F: std::future::Future>(f: F) -> F::Output {
        let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
        let mut f = std::pin::pin!(f);
        loop {
            if let std::task::Poll::Ready(v) = f.as_mut().poll(&mut cx) {
                return v;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    #[ignore = "needs an AirPlay receiver (shairport-sync -o stdout)"]
    fn a_real_receiver_plays_the_exact_samples() {
        let addr: SocketAddr = std::env::var("AIRPLAY")
            .unwrap_or_else(|_| "127.0.0.1:5000".into())
            .parse()
            .unwrap();
        let file = std::env::var("AIRPLAY_OUT").expect("AIRPLAY_OUT");
        let before = std::fs::metadata(&file).map(|m| m.len()).unwrap_or(0) as usize;
        let out = AirPlayOutput::new(Device {
            id: "test".into(),
            name: "test".into(),
            model: None,
            addr,
            encryption: vec!["0".into()],
            codecs: vec!["1".into()],
        });
        let fmt = AudioFormat::pcm(44_100, 16, 2);
        let mut sink = block_on(out.open(fmt)).unwrap();
        block_on(out.set_volume(Volume::Percent(100))).unwrap();
        let frames = 44_100 * 3;
        let samples: Vec<i32> = (0..frames * 2)
            .map(|i| ((i as i64 * 7919) % 60_000 - 30_000) as i32)
            .collect();
        for chunk in samples.chunks(4410) {
            sink.write(&AudioBlock::pcm_i32(fmt, chunk.to_vec())).unwrap();
        }
        sink.drain().unwrap();
        // The receiver plays two seconds behind.
        std::thread::sleep(Duration::from_millis(3000));
        drop(sink);
        let raw = std::fs::read(&file).unwrap();
        let fresh = &raw[before.min(raw.len())..];
        let window: Vec<u8> = samples[44_100..44_100 + 400]
            .iter()
            .flat_map(|x| (*x as i16).to_le_bytes())
            .collect();
        assert!(
            fresh.windows(window.len()).any(|w| w == window.as_slice()),
            "pattern not found in {} new bytes of the receiver's output",
            fresh.len()
        );
    }

    #[test]
    #[ignore = "needs an AirPlay receiver advertised over mDNS"]
    fn receivers_are_discovered() {
        let found = discover(Duration::from_secs(3));
        eprintln!("{found:#?}");
        assert!(!found.is_empty());
    }
}
