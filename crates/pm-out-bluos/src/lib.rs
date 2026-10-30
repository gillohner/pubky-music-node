//! BluOS players (Bluesound, NAD and other BluOS devices) as outputs, through the **BluOS
//! Custom Integration API**: Lenbrook's published HTTP API, plain `GET`s on port 11000 with XML
//! answers, no account and no cloud.
//! - **Finding them:** mDNS `_musc._tcp`, then `/SyncStatus` for the player's name and MAC (a
//!   stable id when its address changes). A device with several players (NAD CI580) answers on
//!   11000, 11010, …; each is its own output.
//! - **Playing:** like a Cast device, a [`BluosOutput`] is a pull stream: the zone writes PCM, the
//!   node serves it as WAV, and `/Play?url=` tells the player to fetch it. `/Stop` when the zone
//!   closes the output; `/Volume?level=` for volume.
//! - **Grouping is BluOS's own** ([`BluosGroup`]): `/AddSlave?slaves=…&ports=…` on the leader
//!   makes the others play its stream in sync; `/RemoveSlave` ends it when the zone stops.
//!
//! Follows Music Assistant's `bluesound` provider (Apache-2.0; see NOTICE) and its `pyblu`
//! client (MIT), which use the same calls.

use async_trait::async_trait;
use pm_model::{AudioBlock, AudioFormat, OutputCaps, QualityCeiling, SampleKind, Volume, VolumeKind};
use pm_zones::outputs::PullOutput;
use pm_zones::{AudioSink, BufferLevel, Output, OutputId, QueueItem, SinkError};
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The API's port on a single-player device.
pub const PORT: u16 = 11000;
/// The mDNS service BluOS players announce.
pub const SERVICE_TYPE: &str = "_musc._tcp.local.";
const TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, thiserror::Error)]
pub enum BluosError {
    #[error("BluOS: {0}")]
    Io(#[from] std::io::Error),
    #[error("BluOS {0}")]
    Http(String),
}

/// A BluOS player on the network.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Player {
    /// Its MAC address (lowercase hex, no separators), or `host:port` when it reports none.
    pub id: String,
    pub name: String,
    pub host: String,
    pub port: u16,
    pub model: Option<String>,
}

/// Percent-encoding for a query value.
pub fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// One API call (`/Status`, `/Play?url=…`), blocking. Returns the XML body of a `200`.
pub fn get(host: &str, port: u16, path: &str) -> Result<String, BluosError> {
    let name = path.split('?').next().unwrap_or(path);
    let addr = (host, port)
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| BluosError::Http(format!("{name}: no address for {host}")))?;
    let mut s = TcpStream::connect_timeout(&addr, TIMEOUT)?;
    s.set_read_timeout(Some(TIMEOUT))?;
    s.set_write_timeout(Some(TIMEOUT))?;
    let host_header = if host.contains(':') {
        format!("[{host}]")
    } else {
        host.to_string()
    };
    s.write_all(
        format!("GET {path} HTTP/1.0\r\nHost: {host_header}:{port}\r\nConnection: close\r\n\r\n").as_bytes(),
    )?;
    let mut buf = Vec::new();
    s.take(1 << 20).read_to_end(&mut buf)?;
    let text = String::from_utf8_lossy(&buf);
    let (head, body) = text
        .split_once("\r\n\r\n")
        .ok_or_else(|| BluosError::Http(format!("{name}: no HTTP answer")))?;
    let code = head.split_whitespace().nth(1).unwrap_or("");
    if code != "200" {
        return Err(BluosError::Http(format!("{name}: HTTP {code}")));
    }
    Ok(body.to_string())
}

/// The value of `attr` on the first `<tag …>` element.
fn attr(xml: &str, tag: &str, attr: &str) -> Option<String> {
    let start = xml.find(&format!("<{tag}"))?;
    let rest = &xml[start..];
    let el = &rest[..rest.find('>')?];
    let key = format!(" {attr}=\"");
    let at = el.find(&key)? + key.len();
    let v = &el[at..];
    Some(unescape(&v[..v.find('"')?]))
}

fn unescape(s: &str) -> String {
    s.replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

/// The player at `host:port`, from its `/SyncStatus`.
pub fn sync_status(host: &str, port: u16) -> Result<Player, BluosError> {
    let xml = get(host, port, "/SyncStatus")?;
    if !xml.contains("<SyncStatus") {
        return Err(BluosError::Http("/SyncStatus: not a BluOS answer".into()));
    }
    let mac: String = attr(&xml, "SyncStatus", "mac")
        .unwrap_or_default()
        .chars()
        .filter(char::is_ascii_hexdigit)
        .collect::<String>()
        .to_ascii_lowercase();
    Ok(Player {
        id: if mac.is_empty() {
            format!("{host}:{port}")
        } else {
            mac
        },
        name: attr(&xml, "SyncStatus", "name").unwrap_or_else(|| format!("BluOS {host}")),
        host: host.to_string(),
        port,
        model: attr(&xml, "SyncStatus", "modelName").or_else(|| attr(&xml, "SyncStatus", "model")),
    })
}

/// BluOS players found over mDNS within `timeout`, each confirmed with `/SyncStatus`. Blocking.
pub fn discover(timeout: Duration) -> Vec<Player> {
    let Ok(daemon) = mdns_sd::ServiceDaemon::new() else {
        return Vec::new();
    };
    let Ok(rx) = daemon.browse(SERVICE_TYPE) else {
        let _ = daemon.shutdown();
        return Vec::new();
    };
    let deadline = Instant::now() + timeout;
    let mut found: Vec<(String, u16)> = Vec::new();
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        match rx.recv_timeout(left) {
            Ok(mdns_sd::ServiceEvent::ServiceResolved(info)) => {
                let addrs: Vec<std::net::IpAddr> =
                    info.get_addresses().iter().map(|a| a.to_ip_addr()).collect();
                let Some(ip) = addrs.iter().find(|a| a.is_ipv4()).or_else(|| addrs.first()) else {
                    continue;
                };
                let at = (ip.to_string(), info.get_port());
                if !found.contains(&at) {
                    found.push(at);
                }
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    let _ = daemon.shutdown();
    let mut players: Vec<Player> = Vec::new();
    for (host, port) in found {
        match sync_status(&host, port) {
            Ok(p) if !players.iter().any(|q| q.id == p.id) => players.push(p),
            Ok(_) => {}
            Err(e) => tracing::debug!(%host, port, "BluOS player didn't answer: {e}"),
        }
    }
    players
}

fn device_err(e: BluosError) -> SinkError {
    SinkError::Device(e.to_string())
}

/// Run a blocking API call off the async runtime when there is one (API handlers), directly
/// otherwise (a zone's audio thread opening its sink): `spawn_blocking` panics outside a runtime.
async fn call(host: String, port: u16, path: String) -> Result<String, BluosError> {
    if tokio::runtime::Handle::try_current().is_err() {
        return get(&host, port, &path);
    }
    tokio::task::spawn_blocking(move || get(&host, port, &path))
        .await
        .map_err(|e| BluosError::Http(e.to_string()))?
}

/// One BluOS player as an output.
pub struct BluosOutput {
    player: Player,
    pull: Arc<PullOutput>,
    stream_url: String,
    caps: OutputCaps,
}

impl BluosOutput {
    pub fn new(player: Player, pull: Arc<PullOutput>, stream_url: String) -> Self {
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
        Self {
            player,
            pull,
            stream_url,
            caps,
        }
    }

    pub fn player(&self) -> &Player {
        &self.player
    }

    /// Open the pull stream and start the player on it; `ungroup` ends a group when it closes.
    async fn open_sink(
        &self,
        fmt: AudioFormat,
        ungroup: Option<(String, u16, String)>,
    ) -> Result<BluosSink, SinkError> {
        if fmt.kind != SampleKind::Pcm || !self.caps.accepts(&fmt) {
            return Err(SinkError::UnsupportedFormat(self.id(), fmt));
        }
        let inner = self.pull.open(fmt).await?;
        let path = format!("/Play?url={}", encode(&self.stream_url));
        call(self.player.host.clone(), self.player.port, path)
            .await
            .map_err(device_err)?;
        Ok(BluosSink {
            inner: Some(inner),
            stop: Some((self.player.host.clone(), self.player.port)),
            ungroup,
            paused: false,
        })
    }

    fn volume_path(v: Volume) -> Result<String, SinkError> {
        Ok(match v {
            Volume::Percent(p) => format!("/Volume?level={}", p.min(100)),
            Volume::Db(db) => format!(
                "/Volume?level={}",
                (10f64.powf(f64::from(db.clamp(-80.0, 0.0)) / 20.0) * 100.0).round() as u32
            ),
            Volume::Mute(m) => format!("/Volume?mute={}", u8::from(m)),
            Volume::Up | Volume::Down => {
                return Err(SinkError::Device("BluOS volume needs a level".into()));
            }
        })
    }
}

#[async_trait]
impl Output for BluosOutput {
    fn id(&self) -> OutputId {
        OutputId::new("bluos", &self.player.id)
    }

    fn name(&self) -> String {
        match &self.player.model {
            Some(m) if !m.is_empty() => format!("{} ({m})", self.player.name),
            _ => self.player.name.clone(),
        }
    }

    fn model(&self) -> Option<String> {
        self.player.model.clone().filter(|m| !m.trim().is_empty())
    }

    fn sync(&self) -> pm_zones::SyncClass {
        pm_zones::SyncClass::Single
    }

    fn now_playing(&self, _item: Option<&QueueItem>) {}

    fn caps(&self) -> &OutputCaps {
        &self.caps
    }

    async fn open(&self, fmt: AudioFormat) -> Result<Box<dyn AudioSink>, SinkError> {
        Ok(Box::new(self.open_sink(fmt, None).await?))
    }

    async fn set_volume(&self, v: Volume) -> Result<(), SinkError> {
        let path = Self::volume_path(v)?;
        call(self.player.host.clone(), self.player.port, path)
            .await
            .map(drop)
            .map_err(device_err)
    }
}

/// A node zone of BluOS players only: the first leads, the others follow it in a BluOS group
/// (tight sync by BluOS), and the group ends when the zone stops.
pub struct BluosGroup {
    name: String,
    lead: Arc<BluosOutput>,
    members: Vec<Arc<BluosOutput>>,
}

impl BluosGroup {
    pub fn new(name: &str, lead: Arc<BluosOutput>, members: Vec<Arc<BluosOutput>>) -> Self {
        Self {
            name: name.to_string(),
            lead,
            members,
        }
    }

    /// `?slaves=a,b&ports=11000,11000` for the followers.
    fn followers_query(&self) -> String {
        let hosts: Vec<&str> = self.members.iter().map(|m| m.player.host.as_str()).collect();
        let ports: Vec<String> = self.members.iter().map(|m| m.player.port.to_string()).collect();
        format!(
            "slaves={}&ports={}",
            encode(&hosts.join(",")),
            encode(&ports.join(","))
        )
    }
}

#[async_trait]
impl Output for BluosGroup {
    fn id(&self) -> OutputId {
        let mut ids = vec![self.lead.id().0];
        ids.extend(self.members.iter().map(|m| m.id().0));
        OutputId::new("bluos-group", &ids.join("+"))
    }

    fn name(&self) -> String {
        self.name.clone()
    }

    fn caps(&self) -> &OutputCaps {
        self.lead.caps()
    }

    fn sync(&self) -> pm_zones::SyncClass {
        pm_zones::SyncClass::Tight
    }

    fn now_playing(&self, item: Option<&QueueItem>) {
        self.lead.now_playing(item);
    }

    async fn open(&self, fmt: AudioFormat) -> Result<Box<dyn AudioSink>, SinkError> {
        let (host, port) = (self.lead.player.host.clone(), self.lead.player.port);
        let q = self.followers_query();
        let grouped = match call(host.clone(), port, format!("/AddSlave?{q}")).await {
            Ok(_) => true,
            Err(e) => {
                tracing::warn!("BluOS grouping failed, the leader plays alone: {e}");
                false
            }
        };
        let ungroup = grouped.then_some((host, port, q));
        Ok(Box::new(self.lead.open_sink(fmt, ungroup).await?))
    }

    async fn set_volume(&self, v: Volume) -> Result<(), SinkError> {
        self.lead.set_volume(v).await?;
        for m in &self.members {
            m.set_volume(v).await?;
        }
        Ok(())
    }
}

struct BluosSink {
    /// `Some` until dropped: the stream ends before the player is told to stop.
    inner: Option<Box<dyn AudioSink>>,
    /// The player to stop.
    stop: Option<(String, u16)>,
    /// The leader and its followers' query, for a group to end.
    ungroup: Option<(String, u16, String)>,
    /// The player was told to pause.
    paused: bool,
}

impl BluosSink {
    fn sink(&self) -> &dyn AudioSink {
        self.inner.as_deref().expect("sink open")
    }
    fn sink_mut(&mut self) -> &mut Box<dyn AudioSink> {
        self.inner.as_mut().expect("sink open")
    }
}

impl AudioSink for BluosSink {
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
    /// The player's own `/Pause`: it stops at once and keeps its buffer (it plays seconds of
    /// it out otherwise). A group's leader pauses its followers with it.
    fn pause(&mut self) -> Result<(), SinkError> {
        let Some((host, port)) = self.stop.clone() else {
            return self.sink_mut().flush();
        };
        match get(&host, port, "/Pause") {
            Ok(_) => {
                self.paused = true;
                self.sink_mut().pause()
            }
            Err(e) => {
                tracing::warn!("BluOS pause refused, the player plays out its buffer: {e}");
                self.sink_mut().flush()
            }
        }
    }
    /// `/Play` carries on from the pause (the player fetches the stream again if it hung up).
    fn resume(&mut self) -> Result<(), SinkError> {
        if std::mem::take(&mut self.paused) {
            if let Some((host, port)) = self.stop.clone() {
                get(&host, port, "/Play").map_err(device_err)?;
            }
        }
        self.sink_mut().resume()
    }
    fn frames_written(&self) -> u64 {
        self.sink().frames_written()
    }
}

impl Drop for BluosSink {
    fn drop(&mut self) {
        drop(self.inner.take());
        if let Some((host, port)) = self.stop.take() {
            if let Err(e) = get(&host, port, "/Stop") {
                tracing::debug!("BluOS stop: {e}");
            }
        }
        if let Some((host, port, q)) = self.ungroup.take() {
            if let Err(e) = get(&host, port, &format!("/RemoveSlave?{q}")) {
                tracing::debug!("BluOS ungroup: {e}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::sync::Mutex;

    /// A fake BluOS player: logs each request's path and answers like the API.
    fn fake_player(name: &'static str, mac: &'static str, log: Arc<Mutex<Vec<String>>>) -> u16 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                let Ok(mut conn) = conn else { continue };
                let mut first = String::new();
                let mut r = BufReader::new(conn.try_clone().unwrap());
                r.read_line(&mut first).unwrap();
                loop {
                    let mut l = String::new();
                    if r.read_line(&mut l).unwrap() == 0 || l == "\r\n" {
                        break;
                    }
                }
                let path = first.split_whitespace().nth(1).unwrap_or("").to_string();
                log.lock().unwrap().push(path.clone());
                let body = if path == "/SyncStatus" {
                    format!(
                        r#"<?xml version="1.0" encoding="UTF-8"?><SyncStatus etag="7" id="127.0.0.1:{port}" mac="{mac}" name="{name}" modelName="NODE" brand="Bluesound" initialized="true"></SyncStatus>"#
                    )
                } else if path.starts_with("/Nope") {
                    let _ = conn.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n");
                    continue;
                } else {
                    "<state>stream</state>".to_string()
                };
                let _ = conn.write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/xml\r\nContent-Length: {}\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                );
            }
        });
        port
    }

    fn fmt() -> AudioFormat {
        AudioFormat::pcm(44_100, 16, 2)
    }

    #[test]
    fn encodes_query_values_and_reads_sync_status() {
        assert_eq!(
            encode("http://n:7788/v1/outputs/pull/b/stream.wav?k=a b"),
            "http%3A%2F%2Fn%3A7788%2Fv1%2Foutputs%2Fpull%2Fb%2Fstream.wav%3Fk%3Da%20b"
        );
        let log = Arc::new(Mutex::new(Vec::new()));
        let port = fake_player("Kitchen & Bar", "90:56:82:AA:BB:CC", log);
        let p = sync_status("127.0.0.1", port).unwrap();
        assert_eq!(p.id, "905682aabbcc");
        assert_eq!(p.model.as_deref(), Some("NODE"));
        assert_eq!(p.name, "Kitchen & Bar");
        assert!(get("127.0.0.1", port, "/Nope")
            .unwrap_err()
            .to_string()
            .contains("404"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn plays_the_pull_stream_and_stops() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let port = fake_player("Den", "00:11:22:33:44:55", log.clone());
        let player = sync_status("127.0.0.1", port).unwrap();
        let out = BluosOutput::new(
            player,
            Arc::new(PullOutput::new("bluos-den")),
            "http://node:7788/v1/outputs/pull/bluos-den/stream.wav?k=key".into(),
        );
        assert_eq!(out.id().0, "bluos:001122334455");
        assert_eq!(out.name(), "Den (NODE)");
        out.set_volume(Volume::Percent(30)).await.unwrap();
        let sink = out.open(fmt()).await.unwrap();
        drop(sink);
        let log = log.lock().unwrap().clone();
        assert_eq!(
            log[1..],
            [
                "/Volume?level=30".to_string(),
                "/Play?url=http%3A%2F%2Fnode%3A7788%2Fv1%2Foutputs%2Fpull%2Fbluos-den%2Fstream.wav%3Fk%3Dkey"
                    .into(),
                "/Stop".into(),
            ]
        );
    }

    /// Pause is the player's own `/Pause` (it stopped only after ~5 s of buffer before, NUC
    /// 2026-10-30) and resume its `/Play`.
    #[tokio::test(flavor = "multi_thread")]
    async fn pause_and_resume_use_the_players_own_calls() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let port = fake_player("Den", "00:11:22:33:44:77", log.clone());
        let out = BluosOutput::new(
            sync_status("127.0.0.1", port).unwrap(),
            Arc::new(PullOutput::controlled("bluos-pause")),
            "http://node/s".into(),
        );
        let mut sink = out.open(fmt()).await.unwrap();
        std::thread::spawn(move || {
            sink.pause().unwrap();
            sink.resume().unwrap();
            // Not paused: nothing to tell the player
            sink.resume().unwrap();
            drop(sink);
        })
        .join()
        .unwrap();
        let log = log.lock().unwrap().clone();
        assert_eq!(
            log[1..],
            ["/Play?url=http%3A%2F%2Fnode%2Fs", "/Pause", "/Play", "/Stop"]
        );
    }

    /// A zone opens its sink on its own thread, outside any runtime: this panicked
    /// (`spawn_blocking` without a runtime) and left the zone dead.
    #[test]
    fn opens_from_a_thread_without_a_runtime() {
        fn block_on<F: std::future::Future>(fut: F) -> F::Output {
            use std::task::{Context, Poll, Wake, Waker};
            struct W(std::thread::Thread);
            impl Wake for W {
                fn wake(self: Arc<Self>) {
                    self.0.unpark();
                }
            }
            let waker = Waker::from(Arc::new(W(std::thread::current())));
            let mut cx = Context::from_waker(&waker);
            let mut fut = std::pin::pin!(fut);
            loop {
                match fut.as_mut().poll(&mut cx) {
                    Poll::Ready(v) => return v,
                    Poll::Pending => std::thread::park(),
                }
            }
        }
        let log = Arc::new(Mutex::new(Vec::new()));
        let port = fake_player("Den", "00:11:22:33:44:66", log.clone());
        let out = BluosOutput::new(
            sync_status("127.0.0.1", port).unwrap(),
            Arc::new(PullOutput::new("bluos-den2")),
            "http://node/s".into(),
        );
        let opened = std::thread::spawn(move || block_on(out.open(fmt())).map(drop))
            .join()
            .expect("open must not panic outside a runtime");
        assert!(opened.is_ok(), "{opened:?}");
        assert!(log.lock().unwrap().iter().any(|p| p.starts_with("/Play?url=")));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_group_follows_the_leader_and_ends_with_the_zone() {
        let lead_log = Arc::new(Mutex::new(Vec::new()));
        let member_log = Arc::new(Mutex::new(Vec::new()));
        let lead_port = fake_player("Living", "aa:aa:aa:aa:aa:01", lead_log.clone());
        let member_port = fake_player("Kitchen", "aa:aa:aa:aa:aa:02", member_log.clone());
        let lead = Arc::new(BluosOutput::new(
            sync_status("127.0.0.1", lead_port).unwrap(),
            Arc::new(PullOutput::new("bluos-1")),
            "http://node/s".into(),
        ));
        let member = Arc::new(BluosOutput::new(
            sync_status("127.0.0.1", member_port).unwrap(),
            Arc::new(PullOutput::new("bluos-2")),
            "http://node/t".into(),
        ));
        let g = BluosGroup::new("Downstairs", lead, vec![member]);
        assert_eq!(g.id().0, "bluos-group:bluos:aaaaaaaaaa01+bluos:aaaaaaaaaa02");
        assert_eq!(g.sync(), pm_zones::SyncClass::Tight);
        let sink = g.open(fmt()).await.unwrap();
        drop(sink);
        let lead_log = lead_log.lock().unwrap().clone();
        assert_eq!(
            lead_log[1..],
            [
                format!("/AddSlave?slaves=127.0.0.1&ports={member_port}"),
                "/Play?url=http%3A%2F%2Fnode%2Fs".into(),
                "/Stop".into(),
                format!("/RemoveSlave?slaves=127.0.0.1&ports={member_port}"),
            ]
        );
        // The follower is told nothing directly: BluOS plays the leader's stream on it
        assert_eq!(member_log.lock().unwrap().len(), 1, "only its /SyncStatus");
    }
}
