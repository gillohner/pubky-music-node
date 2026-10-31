//! MPD (Music Player Daemon) servers as outputs, over MPD's own text protocol (the "Protocol"
//! chapter of MPD's documentation): one command per line on TCP 6600, each answered with `OK`
//! or `ACK [error@command] {name} message`.
//!
//! - **No discovery:** MPD servers are listed in the node's config (`[outputs] mpd`), as
//!   `host`, `host:port` or `password@host:port`.
//! - **Playing:** like a Cast device, an [`MpdOutput`] is a pull stream: the zone writes PCM, the
//!   node serves it as WAV, and MPD is told to play its URL (`addid`, `playid`; MPD needs its
//!   curl input, which standard builds have). When the zone closes the output the node stops
//!   MPD and removes that one entry (`deleteid`), so whatever else was in MPD's playlist stays.
//! - **Pause:** `pause 1` / `pause 0`, so MPD falls silent at once instead of playing out the
//!   seconds of stream it buffered.
//! - **Volume:** `setvol`, when MPD has a mixer.

use async_trait::async_trait;
use pm_model::{AudioBlock, AudioFormat, OutputCaps, QualityCeiling, SampleKind, Volume, VolumeKind};
use pm_zones::outputs::PullOutput;
use pm_zones::{AudioSink, BufferLevel, Output, OutputId, QueueItem, SinkError};
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::time::Duration;

/// MPD's port.
pub const PORT: u16 = 6600;
const TIMEOUT: Duration = Duration::from_secs(5);
/// How long a pause or resume waits for MPD: the zone's audio thread waits with it.
const CONTROL_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, thiserror::Error)]
pub enum MpdError {
    #[error("MPD: {0}")]
    Io(#[from] std::io::Error),
    #[error("MPD: {0}")]
    Protocol(String),
}

/// One MPD server from the config.
#[derive(Clone, PartialEq, Eq)]
pub struct Server {
    pub host: String,
    pub port: u16,
    pub password: Option<String>,
}

impl std::fmt::Debug for Server {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never the password
        write!(f, "Server({}:{})", self.host, self.port)
    }
}

impl Server {
    /// `host`, `host:port` or `password@host:port` (an IPv6 address in brackets).
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim();
        let (password, rest) = match s.rsplit_once('@') {
            Some((p, r)) => (Some(p.to_string()).filter(|p| !p.is_empty()), r),
            None => (None, s),
        };
        let (host, port) = if let Some(r) = rest.strip_prefix('[') {
            let (h, after) = r.split_once(']')?;
            let port = match after.strip_prefix(':') {
                Some(p) => p.parse().ok()?,
                None if after.is_empty() => PORT,
                None => return None,
            };
            (h.to_string(), port)
        } else {
            match rest.rsplit_once(':') {
                Some((h, p)) => (h.to_string(), p.parse().ok()?),
                None => (rest.to_string(), PORT),
            }
        };
        (!host.is_empty()).then_some(Self { host, port, password })
    }

    /// `host:port`, as the output id uses it.
    pub fn address(&self) -> String {
        if self.host.contains(':') {
            format!("[{}]:{}", self.host, self.port)
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }
}

/// Quote an argument the way MPD reads it.
pub fn quote(arg: &str) -> String {
    let mut out = String::with_capacity(arg.len() + 2);
    out.push('"');
    for c in arg.chars() {
        if c == '"' || c == '\\' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

/// A connection to one MPD server.
pub struct Client {
    reader: BufReader<TcpStream>,
    stream: TcpStream,
    /// The server's protocol version, from its greeting.
    pub version: String,
}

impl Client {
    /// Connect, read the greeting, and send the password when there is one. Blocking.
    pub fn connect(server: &Server) -> Result<Self, MpdError> {
        Self::connect_within(server, TIMEOUT)
    }

    /// [`Self::connect`] with its own connect, read and write timeout.
    fn connect_within(server: &Server, timeout: Duration) -> Result<Self, MpdError> {
        let addr = (server.host.as_str(), server.port)
            .to_socket_addrs()?
            .next()
            .ok_or_else(|| MpdError::Protocol(format!("no address for {}", server.host)))?;
        let stream = TcpStream::connect_timeout(&addr, timeout)?;
        stream.set_read_timeout(Some(timeout))?;
        stream.set_write_timeout(Some(timeout))?;
        let mut reader = BufReader::new(stream.try_clone()?);
        let mut greeting = String::new();
        reader.read_line(&mut greeting)?;
        let version = greeting
            .trim()
            .strip_prefix("OK MPD ")
            .ok_or_else(|| MpdError::Protocol("not an MPD server".into()))?
            .to_string();
        let mut c = Self {
            reader,
            stream,
            version,
        };
        if let Some(p) = &server.password {
            c.command(&format!("password {}", quote(p)))
                .map_err(|_| MpdError::Protocol("the password was refused".into()))?;
        }
        Ok(c)
    }

    /// Send one command and read its answer: the `key: value` lines before `OK`.
    pub fn command(&mut self, cmd: &str) -> Result<Vec<(String, String)>, MpdError> {
        self.stream.write_all(format!("{cmd}\n").as_bytes())?;
        let mut pairs = Vec::new();
        loop {
            let mut line = String::new();
            if self.reader.read_line(&mut line)? == 0 {
                return Err(MpdError::Protocol("connection closed".into()));
            }
            let line = line.trim_end_matches('\n');
            if line == "OK" {
                return Ok(pairs);
            }
            if let Some(err) = line.strip_prefix("ACK ") {
                let msg = err.split_once("} ").map(|(_, m)| m).unwrap_or(err);
                let name = cmd.split_whitespace().next().unwrap_or(cmd);
                return Err(MpdError::Protocol(format!("{name}: {msg}")));
            }
            if let Some((k, v)) = line.split_once(": ") {
                pairs.push((k.to_string(), v.to_string()));
            }
        }
    }
}

/// Run blocking MPD work off the async runtime.
async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, MpdError> + Send + 'static,
) -> Result<T, MpdError> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| MpdError::Protocol(e.to_string()))?
}

fn device_err(e: MpdError) -> SinkError {
    SinkError::Device(e.to_string())
}

/// One MPD server as an output.
pub struct MpdOutput {
    server: Server,
    name: String,
    pull: Arc<PullOutput>,
    stream_url: String,
    caps: OutputCaps,
}

impl MpdOutput {
    pub fn new(server: Server, pull: Arc<PullOutput>, stream_url: String) -> Self {
        let mut caps = OutputCaps::unlimited();
        caps.pcm_rates = vec![44_100, 48_000, 88_200, 96_000, 176_400, 192_000];
        caps.max_pcm_bits = 24;
        caps.pcm_bits = vec![16, 24];
        caps.max_channels = 2;
        caps.dsd = None;
        caps.hw_volume = Some(VolumeKind::Percent);
        caps.standby = false;
        caps.gapless = true;
        caps.groupable = false;
        caps.fixed_latency_ms = Some(1000);
        caps.quality_ceiling = QualityCeiling::BitPerfect;
        let name = format!("MPD at {}", server.host);
        Self {
            server,
            name,
            pull,
            stream_url,
            caps,
        }
    }

    pub fn server(&self) -> &Server {
        &self.server
    }
}

#[async_trait]
impl Output for MpdOutput {
    fn id(&self) -> OutputId {
        OutputId::new("mpd", &self.server.address())
    }

    fn name(&self) -> String {
        self.name.clone()
    }

    fn sync(&self) -> pm_zones::SyncClass {
        pm_zones::SyncClass::Single
    }

    fn now_playing(&self, _item: Option<&QueueItem>) {}

    fn caps(&self) -> &OutputCaps {
        &self.caps
    }

    async fn open(&self, fmt: AudioFormat) -> Result<Box<dyn AudioSink>, SinkError> {
        if fmt.kind != SampleKind::Pcm || !self.caps.accepts(&fmt) {
            return Err(SinkError::UnsupportedFormat(self.id(), fmt));
        }
        let inner = self.pull.open(fmt).await?;
        let server = self.server.clone();
        let url = self.stream_url.clone();
        let id = blocking(move || {
            let mut c = Client::connect(&server)?;
            let added = c.command(&format!("addid {}", quote(&url)))?;
            let id = added
                .iter()
                .find(|(k, _)| k == "Id")
                .map(|(_, v)| v.clone())
                .ok_or_else(|| MpdError::Protocol("addid gave no id".into()))?;
            c.command(&format!("playid {id}"))?;
            Ok(id)
        })
        .await
        .map_err(device_err)?;
        Ok(Box::new(MpdSink {
            inner: Some(inner),
            stop: Some((self.server.clone(), id)),
            paused: false,
            unconfirmed: false,
        }))
    }

    async fn set_volume(&self, v: Volume) -> Result<(), SinkError> {
        let cmd = match v {
            Volume::Percent(p) => format!("setvol {}", p.min(100)),
            Volume::Db(db) => format!(
                "setvol {}",
                (10f64.powf(f64::from(db.clamp(-80.0, 0.0)) / 20.0) * 100.0).round() as u32
            ),
            Volume::Mute(_) => {
                return Err(SinkError::Device(
                    "MPD has no mute; set the volume to 0 instead".into(),
                ));
            }
            Volume::Up | Volume::Down => {
                return Err(SinkError::Device("MPD volume needs a level".into()));
            }
        };
        let server = self.server.clone();
        blocking(move || Client::connect(&server)?.command(&cmd).map(drop))
            .await
            .map_err(device_err)
    }
}

struct MpdSink {
    /// `Some` until dropped: the stream ends before MPD is told to stop.
    inner: Option<Box<dyn AudioSink>>,
    /// The server and the playlist entry the node added.
    stop: Option<(Server, String)>,
    /// MPD was told to pause.
    paused: bool,
    /// ...but didn't confirm it (it may have played its buffer out).
    unconfirmed: bool,
}

impl MpdSink {
    /// After a pause: MPD still holds the node's entry, paused (or playing it, if someone pressed
    /// play there), and confirmed the pause. Returns the connection and whether it is paused.
    fn can_carry_on(&self) -> Result<(Client, bool), SinkError> {
        let Some((server, id)) = &self.stop else {
            return Err(SinkError::Device("MPD: the sink is closed".into()));
        };
        if self.unconfirmed {
            return Err(SinkError::Device(
                "MPD didn't confirm the pause (it may have played on)".into(),
            ));
        }
        let mut c = Client::connect_within(server, CONTROL_TIMEOUT).map_err(device_err)?;
        let status = c.command("status").map_err(device_err)?;
        let get = |k: &str| status.iter().find(|(key, _)| key == k).map(|(_, v)| v.as_str());
        let state = get("state");
        if !matches!(state, Some("pause" | "play")) || get("songid") != Some(id.as_str()) {
            return Err(SinkError::Device(format!(
                "MPD is not holding the node's stream after the pause (state {})",
                state.unwrap_or("unknown")
            )));
        }
        Ok((c, state == Some("pause")))
    }

    fn sink(&self) -> &dyn AudioSink {
        self.inner.as_deref().expect("sink open")
    }
    fn sink_mut(&mut self) -> &mut Box<dyn AudioSink> {
        self.inner.as_mut().expect("sink open")
    }
}

impl AudioSink for MpdSink {
    fn format(&self) -> AudioFormat {
        self.sink().format()
    }
    fn write(&mut self, block: &AudioBlock) -> Result<(), SinkError> {
        self.sink_mut().write(block)
    }
    fn buffer_level(&self) -> BufferLevel {
        self.sink().buffer_level()
    }
    fn flush(&mut self) -> Result<(), SinkError> {
        self.sink_mut().flush()
    }
    fn drain(&mut self) -> Result<(), SinkError> {
        self.sink_mut().drain()
    }
    /// MPD's own `pause 1`: it stops at once and keeps what it buffered (it plays seconds of it
    /// out otherwise).
    fn pause(&mut self) -> Result<bool, SinkError> {
        let Some((server, _)) = self.stop.clone() else {
            return self.sink_mut().flush().map(|()| false);
        };
        self.paused = true;
        match Client::connect_within(&server, CONTROL_TIMEOUT).and_then(|mut c| c.command("pause 1")) {
            Ok(_) => {
                self.unconfirmed = false;
                self.sink_mut().pause().map(|_| true)
            }
            // It may have played its buffer out: the resume starts it again from what was heard
            Err(e) => {
                tracing::warn!(server = ?server, "MPD pause not confirmed, it may play out its buffer: {e}");
                self.unconfirmed = true;
                self.sink_mut().flush().map(|()| true)
            }
        }
    }
    /// Whether MPD can carry on from the pause (see [`Self::resume`]), without resuming yet.
    fn ready_to_resume(&mut self) -> Result<(), SinkError> {
        if self.paused {
            self.can_carry_on()?;
        }
        Ok(())
    }
    /// `pause 0`, only while MPD still holds the node's entry: one that doesn't (the stream
    /// dropped during a long pause, or someone picked another song meanwhile, which `pause 0`
    /// would start) or didn't confirm the pause is an error, and the zone opens the output again
    /// from what was last heard.
    fn resume(&mut self) -> Result<(), SinkError> {
        if std::mem::take(&mut self.paused) {
            let carry_on = self.can_carry_on();
            self.unconfirmed = false;
            let (mut c, paused) = carry_on?;
            if paused {
                c.command("pause 0").map_err(device_err)?;
            }
        }
        self.sink_mut().resume()
    }
    fn frames_written(&self) -> u64 {
        self.sink().frames_written()
    }
}

impl Drop for MpdSink {
    fn drop(&mut self) {
        drop(self.inner.take());
        if let Some((server, id)) = self.stop.take() {
            let r = Client::connect(&server).and_then(|mut c| {
                c.command("stop")?;
                c.command(&format!("deleteid {id}"))
            });
            if let Err(e) = r {
                tracing::debug!(server = ?server, "MPD stop: {e}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// A fake MPD: greets, logs each command, answers `addid` with an id, refuses a wrong
    /// password, and `OK` to the rest.
    fn fake_mpd(log: Arc<Mutex<Vec<String>>>) -> u16 {
        fake_mpd_with(
            log,
            Arc::new(Mutex::new("state: pause\nsongid: 42\n".into())),
            &[],
        )
    }

    /// [`fake_mpd`] answering `status` with `status`, and refusing the commands in `refuse`.
    fn fake_mpd_with(
        log: Arc<Mutex<Vec<String>>>,
        status: Arc<Mutex<String>>,
        refuse: &'static [&'static str],
    ) -> u16 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                let Ok(mut conn) = conn else { continue };
                let log = log.clone();
                let status = status.clone();
                std::thread::spawn(move || {
                    conn.write_all(b"OK MPD 0.24.0\n").unwrap();
                    let mut r = BufReader::new(conn.try_clone().unwrap());
                    loop {
                        let mut line = String::new();
                        if r.read_line(&mut line).unwrap_or(0) == 0 {
                            return;
                        }
                        let cmd = line.trim_end().to_string();
                        log.lock().unwrap().push(cmd.clone());
                        let answer = if refuse.contains(&cmd.as_str()) {
                            format!("ACK [5@0] {{{cmd}}} refused\n")
                        } else if cmd.starts_with("addid ") {
                            "Id: 42\nOK\n".to_string()
                        } else if cmd == "status" {
                            format!("{}OK\n", status.lock().unwrap())
                        } else if cmd == "password \"wrong\"" {
                            "ACK [3@0] {password} incorrect password\n".to_string()
                        } else {
                            "OK\n".to_string()
                        };
                        let _ = conn.write_all(answer.as_bytes());
                    }
                });
            }
        });
        port
    }

    #[test]
    fn parses_servers_and_quotes() {
        let s = Server::parse("secret@10.0.0.2:6601").unwrap();
        assert_eq!(
            (s.host.as_str(), s.port, s.password.as_deref()),
            ("10.0.0.2", 6601, Some("secret"))
        );
        assert_eq!(Server::parse("music.local").unwrap().port, PORT);
        let v6 = Server::parse("[fe80::1]:6602").unwrap();
        assert_eq!(
            (v6.host.as_str(), v6.address()),
            ("fe80::1", "[fe80::1]:6602".to_string())
        );
        assert!(Server::parse("").is_none());
        assert!(!format!("{s:?}").contains("secret"), "no password in logs");
        assert_eq!(quote(r#"a "b" \c"#), r#""a \"b\" \\c""#);
    }

    #[test]
    fn a_wrong_password_is_refused() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let port = fake_mpd(log);
        let s = Server::parse(&format!("wrong@127.0.0.1:{port}")).unwrap();
        let e = Client::connect(&s).err().unwrap();
        assert!(e.to_string().contains("password was refused"), "{e}");
        let ok = Client::connect(&Server::parse(&format!("127.0.0.1:{port}")).unwrap()).unwrap();
        assert_eq!(ok.version, "0.24.0");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn plays_the_pull_stream_and_leaves_the_rest_of_the_playlist() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let port = fake_mpd(log.clone());
        let server = Server::parse(&format!("pw@127.0.0.1:{port}")).unwrap();
        let out = MpdOutput::new(
            server,
            Arc::new(PullOutput::new("mpd-1")),
            "http://node:7788/v1/outputs/pull/mpd-1/stream.wav?k=key".into(),
        );
        assert_eq!(out.id().0, format!("mpd:127.0.0.1:{port}"));
        out.set_volume(Volume::Percent(40)).await.unwrap();
        let sink = out.open(AudioFormat::pcm(44_100, 16, 2)).await.unwrap();
        drop(sink);
        let log = log.lock().unwrap().clone();
        assert_eq!(
            log,
            [
                "password \"pw\"",
                "setvol 40",
                "password \"pw\"",
                "addid \"http://node:7788/v1/outputs/pull/mpd-1/stream.wav?k=key\"",
                "playid 42",
                "password \"pw\"",
                "stop",
                "deleteid 42",
            ]
        );
    }

    /// Pause is MPD's own `pause 1` (it played out the stream it buffered before: the sink only
    /// stopped feeding it), and the resume `pause 0`.
    #[tokio::test(flavor = "multi_thread")]
    async fn pause_and_resume_are_mpds_own_commands() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let port = fake_mpd(log.clone());
        let out = MpdOutput::new(
            Server::parse(&format!("127.0.0.1:{port}")).unwrap(),
            Arc::new(PullOutput::controlled("mpd-pause")),
            "http://node/s".into(),
        );
        let mut sink = out.open(AudioFormat::pcm(44_100, 16, 2)).await.unwrap();
        std::thread::spawn(move || {
            assert!(sink.pause().unwrap(), "MPD holds its buffer");
            sink.resume().unwrap();
            // Not paused: nothing to tell MPD
            sink.resume().unwrap();
            drop(sink);
        })
        .join()
        .unwrap();
        let log = log.lock().unwrap().clone();
        assert_eq!(
            log,
            [
                "addid \"http://node/s\"",
                "playid 42",
                "pause 1",
                "status",
                "pause 0",
                "stop",
                "deleteid 42",
            ]
        );
    }

    /// An MPD that no longer plays the node's entry after the pause (the stream dropped, or
    /// someone played something else) can't carry on: the zone opens the output again.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_resume_mpd_cannot_carry_on_is_an_error() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let status = Arc::new(Mutex::new(String::new()));
        let port = fake_mpd_with(log.clone(), status.clone(), &[]);
        let out = MpdOutput::new(
            Server::parse(&format!("127.0.0.1:{port}")).unwrap(),
            Arc::new(PullOutput::controlled("mpd-gone")),
            "http://node/s".into(),
        );
        // Another client picked a song and paused it: `pause 0` would start that song
        for other in ["state: stop\n", "state: pause\nsongid: 7\n"] {
            *status.lock().unwrap() = other.into();
            let mut sink = out.open(AudioFormat::pcm(44_100, 16, 2)).await.unwrap();
            let (ready, r) = std::thread::spawn(move || {
                assert!(sink.pause().unwrap());
                (sink.ready_to_resume(), sink.resume())
            })
            .join()
            .unwrap();
            assert!(ready.is_err(), "{other:?}");
            let e = r.unwrap_err().to_string();
            assert!(e.contains("not holding the node's stream"), "{other:?}: {e}");
        }
        assert!(
            !log.lock().unwrap().iter().any(|c| c == "pause 0"),
            "{:?}",
            log.lock().unwrap()
        );
    }

    /// A pause MPD didn't confirm may have played its buffer out: the resume doesn't carry on as
    /// if it held it (in a group it would play out of step), and the zone starts it again.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_unconfirmed_pause_is_started_again() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let status = Arc::new(Mutex::new("state: play\nsongid: 42\n".to_string()));
        let port = fake_mpd_with(log.clone(), status, &["pause 1"]);
        let out = MpdOutput::new(
            Server::parse(&format!("127.0.0.1:{port}")).unwrap(),
            Arc::new(PullOutput::controlled("mpd-unconfirmed")),
            "http://node/s".into(),
        );
        let mut sink = out.open(AudioFormat::pcm(44_100, 16, 2)).await.unwrap();
        let (ready, r) = std::thread::spawn(move || {
            assert!(sink.pause().unwrap());
            (sink.ready_to_resume(), sink.resume())
        })
        .join()
        .unwrap();
        assert!(ready.is_err());
        let e = r.unwrap_err().to_string();
        assert!(e.contains("didn't confirm the pause"), "{e}");
    }
}
