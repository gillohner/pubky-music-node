//! Snapcast groups as outputs (infra brief §3). Snapcast keeps cheap players (Raspberry Pis, old
//! phones, `snapclient` anywhere) in tight sync; the owner runs `snapserver`, and the node feeds it.
//!
//! - **Control** is snapserver's JSON-RPC (TCP 1705). Every Snapcast group becomes a zone.
//! - **Opening a zone** adds a stream to snapserver (`Stream.AddStream`, `tcp://…?mode=client`), so
//!   snapserver connects to the node and reads raw PCM in the zone's exact format: rate and bits,
//!   FLAC on the wire. Then it routes the group to that stream (`Group.SetStream`). Closing puts the
//!   group back on its previous stream and removes ours.
//! - **Pacing.** snapserver reads a TCP source as it arrives, so the sink writes at the speed of the
//!   music, a quarter second ahead.
//! - **Quality.** Lossless at the stream's format (16 or 24 bits, up to 96 kHz); snapclient may
//!   resample to a sound card that cannot take it, which only the client knows.

use async_trait::async_trait;
use pm_model::{
    AudioBlock, AudioFormat, OutputCaps, QualityCeiling, SampleKind, Samples, Volume, VolumeKind,
};
use pm_zones::sink::check_block;
use pm_zones::{AudioSink, BufferLevel, Output, OutputId, SinkError};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::time::{Duration, Instant};

pub const CONTROL_PORT: u16 = 1705;
/// How far the sink may run ahead of the music.
const AHEAD: Duration = Duration::from_millis(250);

#[derive(Debug, thiserror::Error)]
pub enum SnapError {
    #[error("snapserver at {0}: {1}")]
    Io(String, std::io::Error),
    #[error("snapserver answered {method}: {message}")]
    Rpc { method: String, message: String },
    #[error("snapserver did not connect to the node's stream within 5 s (is the node reachable from it?)")]
    NoConnect,
}

/// snapserver's JSON-RPC over TCP (newline-delimited).
pub struct Control {
    addr: String,
    reader: BufReader<TcpStream>,
    writer: TcpStream,
    next_id: u64,
}

impl Control {
    pub fn connect(addr: &str) -> Result<Self, SnapError> {
        let err = |e| SnapError::Io(addr.to_string(), e);
        let sock: SocketAddr = std::net::ToSocketAddrs::to_socket_addrs(addr)
            .map_err(err)?
            .next()
            .ok_or_else(|| err(std::io::Error::other("no address")))?;
        let s = TcpStream::connect_timeout(&sock, Duration::from_secs(3)).map_err(err)?;
        s.set_read_timeout(Some(Duration::from_secs(5))).map_err(err)?;
        Ok(Self {
            addr: addr.to_string(),
            reader: BufReader::new(s.try_clone().map_err(err)?),
            writer: s,
            next_id: 1,
        })
    }

    /// Call `method`; notifications in between are skipped.
    pub fn call(&mut self, method: &str, params: Value) -> Result<Value, SnapError> {
        let id = self.next_id;
        self.next_id += 1;
        let mut req = json!({"id": id, "jsonrpc": "2.0", "method": method});
        if !params.is_null() {
            req["params"] = params;
        }
        let addr = self.addr.clone();
        let err = |e| SnapError::Io(addr.clone(), e);
        self.writer
            .write_all(format!("{req}\r\n").as_bytes())
            .map_err(err)?;
        loop {
            let mut line = String::new();
            if self.reader.read_line(&mut line).map_err(err)? == 0 {
                return Err(err(std::io::Error::other("closed")));
            }
            let Ok(v) = serde_json::from_str::<Value>(line.trim()) else {
                continue;
            };
            if v.get("id").and_then(Value::as_u64) != Some(id) {
                continue;
            }
            if let Some(e) = v.get("error") {
                return Err(SnapError::Rpc {
                    method: method.into(),
                    message: e.get("message").and_then(Value::as_str).unwrap_or("error").into(),
                });
            }
            return Ok(v.get("result").cloned().unwrap_or(Value::Null));
        }
    }
}

/// A Snapcast group as the server reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Group {
    pub id: String,
    pub name: String,
    pub stream_id: String,
    /// `(client id, display name)`.
    pub clients: Vec<(String, String)>,
}

impl Group {
    /// The group's name, or its clients' names.
    pub fn label(&self) -> String {
        if !self.name.is_empty() {
            return self.name.clone();
        }
        let names: Vec<&str> = self.clients.iter().map(|(_, n)| n.as_str()).collect();
        if names.is_empty() {
            "Snapcast group".into()
        } else {
            names.join(" + ")
        }
    }
}

pub fn parse_groups(status: &Value) -> Vec<Group> {
    let groups = status
        .pointer("/server/groups")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    groups
        .iter()
        .map(|g| Group {
            id: g.get("id").and_then(Value::as_str).unwrap_or_default().into(),
            name: g.get("name").and_then(Value::as_str).unwrap_or_default().into(),
            stream_id: g
                .get("stream_id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .into(),
            clients: g
                .get("clients")
                .and_then(Value::as_array)
                .map(|cs| {
                    cs.iter()
                        .map(|c| {
                            let id = c
                                .get("id")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string();
                            let name = c
                                .pointer("/config/name")
                                .and_then(Value::as_str)
                                .filter(|n| !n.is_empty())
                                .or_else(|| c.pointer("/host/name").and_then(Value::as_str))
                                .unwrap_or(&id)
                                .to_string();
                            (id, name)
                        })
                        .collect()
                })
                .unwrap_or_default(),
        })
        .collect()
}

/// The groups on the snapserver at `addr` (`host` or `host:port`). Groups whose clients share a
/// host name (several snapclients on one machine) get their client ids added to tell them apart.
pub fn discover(addr: &str) -> Result<Vec<Group>, SnapError> {
    let addr = with_port(addr);
    let mut c = Control::connect(&addr)?;
    Ok(distinct(parse_groups(&c.call("Server.GetStatus", Value::Null)?)))
}

fn distinct(mut groups: Vec<Group>) -> Vec<Group> {
    let labels: Vec<String> = groups.iter().map(Group::label).collect();
    for (i, g) in groups.iter_mut().enumerate() {
        if g.name.is_empty() && labels.iter().filter(|l| **l == labels[i]).count() > 1 {
            for (id, name) in g.clients.iter_mut() {
                *name = format!("{name} · {id}");
            }
        }
    }
    groups
}

fn with_port(addr: &str) -> String {
    if addr
        .rsplit_once(':')
        .is_some_and(|(_, p)| p.parse::<u16>().is_ok())
    {
        addr.to_string()
    } else {
        format!("{addr}:{CONTROL_PORT}")
    }
}

/// The local address this machine uses to reach `peer`.
fn local_ip_towards(peer: IpAddr) -> Option<IpAddr> {
    let s = UdpSocket::bind(("0.0.0.0", 0)).ok()?;
    s.connect((peer, 9)).ok()?;
    s.local_addr().ok().map(|a| a.ip())
}

/// One Snapcast group as an output.
pub struct SnapcastOutput {
    server: String,
    group: Group,
    caps: OutputCaps,
}

impl SnapcastOutput {
    pub fn new(server: &str, group: Group) -> Self {
        let mut caps = OutputCaps::unlimited();
        caps.pcm_rates = vec![44_100, 48_000, 88_200, 96_000];
        caps.pcm_bits = vec![16, 24];
        caps.max_pcm_bits = 24;
        caps.max_channels = 2;
        caps.dsd = None;
        caps.hw_volume = Some(VolumeKind::Percent);
        caps.standby = false;
        caps.gapless = true;
        caps.groupable = false;
        // snapserver's default buffer.
        caps.fixed_latency_ms = Some(1000);
        caps.quality_ceiling = QualityCeiling::LosslessUpTo {
            rate_hz: 96_000,
            bits: 24,
        };
        Self {
            server: with_port(server),
            group,
            caps,
        }
    }

    pub fn group(&self) -> &Group {
        &self.group
    }

    fn stream_id(&self) -> String {
        format!("pubky-{}", self.group.id.chars().take(8).collect::<String>())
    }
}

fn snap_err(e: SnapError) -> SinkError {
    SinkError::Device(e.to_string())
}

#[async_trait]
impl Output for SnapcastOutput {
    fn id(&self) -> OutputId {
        OutputId::new("snapcast", &self.group.id)
    }

    fn name(&self) -> String {
        format!("{} (Snapcast)", self.group.label())
    }

    fn caps(&self) -> &OutputCaps {
        &self.caps
    }

    fn sync(&self) -> pm_zones::SyncClass {
        if self.group.clients.len() > 1 {
            pm_zones::SyncClass::Tight
        } else {
            pm_zones::SyncClass::Single
        }
    }

    async fn open(&self, fmt: AudioFormat) -> Result<Box<dyn AudioSink>, SinkError> {
        if fmt.kind != SampleKind::Pcm || !self.caps.accepts(&fmt) {
            return Err(SinkError::UnsupportedFormat(self.id(), fmt));
        }
        let server = self.server.clone();
        let group = self.group.id.clone();
        let stream = self.stream_id();
        let opened = std::thread::spawn(move || open_stream(&server, &group, &stream, fmt))
            .join()
            .map_err(|_| SinkError::Device("snapcast thread panicked".into()))?
            .map_err(snap_err)?;
        Ok(Box::new(opened))
    }

    async fn set_volume(&self, v: Volume) -> Result<(), SinkError> {
        let (percent, muted) = match v {
            Volume::Percent(p) => (Some(p.min(100)), None),
            Volume::Db(db) => (
                Some(((db.clamp(-60.0, 0.0) + 60.0) / 60.0 * 100.0).round() as u8),
                None,
            ),
            Volume::Mute(m) => (None, Some(m)),
            Volume::Up | Volume::Down => {
                return Err(SinkError::Device("snapcast volume needs a level".into()))
            }
        };
        let server = self.server.clone();
        let clients: Vec<String> = self.group.clients.iter().map(|(id, _)| id.clone()).collect();
        std::thread::spawn(move || -> Result<(), SnapError> {
            let mut c = Control::connect(&server)?;
            for id in clients {
                let mut vol = serde_json::Map::new();
                if let Some(p) = percent {
                    vol.insert("percent".into(), json!(p));
                    vol.insert("muted".into(), json!(false));
                }
                if let Some(m) = muted {
                    vol.insert("muted".into(), json!(m));
                }
                c.call(
                    "Client.SetVolume",
                    json!({"id": id, "volume": Value::Object(vol)}),
                )?;
            }
            Ok(())
        })
        .join()
        .map_err(|_| SinkError::Device("snapcast thread panicked".into()))?
        .map_err(snap_err)
    }
}

/// Add our stream, wait for snapserver to connect, route the group to it.
fn open_stream(server: &str, group: &str, stream: &str, fmt: AudioFormat) -> Result<SnapSink, SnapError> {
    let mut c = Control::connect(server)?;
    let server_ip = std::net::ToSocketAddrs::to_socket_addrs(server)
        .ok()
        .and_then(|mut a| a.next())
        .map(|a| a.ip())
        .unwrap_or(IpAddr::from([127, 0, 0, 1]));
    let my_ip = if server_ip.is_loopback() {
        server_ip
    } else {
        local_ip_towards(server_ip).unwrap_or(server_ip)
    };
    let listener = TcpListener::bind((my_ip, 0)).map_err(|e| SnapError::Io(server.into(), e))?;
    let port = listener
        .local_addr()
        .map_err(|e| SnapError::Io(server.into(), e))?
        .port();
    // A stream left over from a crash would keep the name.
    let _ = c.call("Stream.RemoveStream", json!({"id": stream}));
    let uri = format!(
        "tcp://{my_ip}:{port}?name={stream}&mode=client&sampleformat={}:{}:{}&codec=flac",
        fmt.rate_hz, fmt.bits, fmt.channels
    );
    c.call("Stream.AddStream", json!({"streamUri": uri}))?;
    listener
        .set_nonblocking(true)
        .map_err(|e| SnapError::Io(server.into(), e))?;
    let deadline = Instant::now() + Duration::from_secs(5);
    let conn = loop {
        match listener.accept() {
            Ok((s, _)) => break s,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock && Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(_) => {
                let _ = c.call("Stream.RemoveStream", json!({"id": stream}));
                return Err(SnapError::NoConnect);
            }
        }
    };
    let _ = conn.set_nonblocking(false);
    let _ = conn.set_nodelay(true);
    // Remember what the group played, to give it back.
    let previous = c
        .call("Server.GetStatus", Value::Null)
        .ok()
        .and_then(|st| parse_groups(&st).into_iter().find(|g| g.id == group))
        .map(|g| g.stream_id)
        .filter(|s| s != stream);
    c.call("Group.SetStream", json!({"id": group, "stream_id": stream}))?;
    Ok(SnapSink {
        fmt,
        conn,
        written: 0,
        clock: (Instant::now(), 0),
        server: server.to_string(),
        group: group.to_string(),
        stream: stream.to_string(),
        previous,
    })
}

struct SnapSink {
    fmt: AudioFormat,
    conn: TcpStream,
    written: u64,
    clock: (Instant, u64),
    server: String,
    group: String,
    stream: String,
    previous: Option<String>,
}

/// Snapcast's sample layout: 16-bit in 2 bytes, 24-bit in the low bytes of 4 (little-endian).
fn pack(block: &AudioBlock) -> Vec<u8> {
    let Samples::PcmI32(s) = &block.samples else {
        return Vec::new();
    };
    let width = if block.format.bits == 16 { 2 } else { 4 };
    let mut v = Vec::with_capacity(s.len() * width);
    for x in s {
        v.extend_from_slice(&x.to_le_bytes()[..width]);
    }
    v
}

impl SnapSink {
    fn played(&self) -> Duration {
        Duration::from_secs_f64((self.written - self.clock.1) as f64 / f64::from(self.fmt.rate_hz))
    }
}

impl AudioSink for SnapSink {
    fn format(&self) -> AudioFormat {
        self.fmt
    }

    fn write(&mut self, block: &AudioBlock) -> Result<(), SinkError> {
        check_block(self.fmt, block)?;
        self.conn
            .write_all(&pack(block))
            .map_err(|e| SinkError::Device(format!("snapserver stream: {e}")))?;
        self.written += block.frames() as u64;
        let (start, _) = self.clock;
        let elapsed = start.elapsed();
        let played = self.played();
        if elapsed > played + AHEAD {
            // Resumed after a pause: measure from now.
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
        self.clock = (Instant::now(), self.written);
        Ok(())
    }

    fn drain(&mut self) -> Result<(), SinkError> {
        std::thread::sleep(self.played().saturating_sub(self.clock.0.elapsed()));
        Ok(())
    }

    fn frames_written(&self) -> u64 {
        self.written
    }
}

impl Drop for SnapSink {
    fn drop(&mut self) {
        let _ = self.conn.shutdown(std::net::Shutdown::Both);
        match Control::connect(&self.server) {
            Ok(mut c) => {
                if let Some(prev) = &self.previous {
                    let _ = c.call("Group.SetStream", json!({"id": self.group, "stream_id": prev}));
                }
                let _ = c.call("Stream.RemoveStream", json!({"id": self.stream}));
            }
            Err(e) => tracing::debug!("snapserver cleanup: {e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn groups_are_read_with_client_names() {
        let st = json!({"server": {"groups": [
            {"id": "g1", "name": "", "stream_id": "default", "clients": [
                {"id": "kitchen", "host": {"name": "pi-kitchen"}, "config": {"name": "Kitchen"}},
                {"id": "bath", "host": {"name": "pi-bath"}, "config": {"name": ""}}
            ]},
            {"id": "g2", "name": "Garden", "stream_id": "default", "clients": []}
        ]}});
        let g = parse_groups(&st);
        assert_eq!(g[0].label(), "Kitchen + pi-bath");
        assert_eq!(g[1].label(), "Garden");
        assert_eq!(with_port("10.0.0.2"), "10.0.0.2:1705");
        // Two snapclients on one machine.
        let st = json!({"server": {"groups": [
            {"id": "a", "clients": [{"id": "kitchen", "host": {"name": "pi"}}]},
            {"id": "b", "clients": [{"id": "bath", "host": {"name": "pi"}}]}
        ]}});
        let g = distinct(parse_groups(&st));
        assert_eq!(g[0].label(), "pi · kitchen");
        assert_eq!(g[1].label(), "pi · bath");
        assert_eq!(with_port("10.0.0.2:1800"), "10.0.0.2:1800");
    }

    #[test]
    fn samples_are_packed_the_snapcast_way() {
        let b16 = AudioBlock::pcm_i32(AudioFormat::pcm(48_000, 16, 2), vec![1, -1]);
        assert_eq!(pack(&b16), vec![1, 0, 0xff, 0xff]);
        let b24 = AudioBlock::pcm_i32(AudioFormat::pcm(48_000, 24, 2), vec![1, -1]);
        assert_eq!(pack(&b24), vec![1, 0, 0, 0, 0xff, 0xff, 0xff, 0xff]);
    }

    /// Bit-exact through a real snapserver and snapclient: a pattern written into the zone's stream
    /// comes out of the client unchanged (FLAC on the wire). Needs `SNAPSERVER` and a snapclient
    /// with `--hostID $SNAP_CLIENT --player file:filename=$SNAP_CLIENT_FILE` (docs/CONNECTION.md
    /// has the setup).
    #[test]
    #[ignore = "needs a running snapserver and snapclient"]
    fn a_real_snapclient_plays_the_exact_samples() {
        let addr = std::env::var("SNAPSERVER").unwrap_or_else(|_| "127.0.0.1".into());
        let client = std::env::var("SNAP_CLIENT").unwrap_or_else(|_| "kitchen".into());
        let file = std::env::var("SNAP_CLIENT_FILE").expect("SNAP_CLIENT_FILE");
        let groups = discover(&addr).unwrap();
        let g = groups
            .into_iter()
            .find(|g| g.clients.iter().any(|(id, _)| *id == client))
            .expect("the client's group");
        let out = SnapcastOutput::new(&addr, g);
        for bits in [16u8, 24] {
            let before = std::fs::metadata(&file).map(|m| m.len()).unwrap_or(0);
            let fmt = AudioFormat::pcm(48_000, bits, 2);
            let mut sink = block_on(out.open(fmt)).unwrap();
            // A ramp no codec could guess: 2 s of it.
            let frames = 96_000usize;
            let max: i64 = if bits == 16 { 30_000 } else { 8_000_000 };
            let samples: Vec<i32> = (0..frames * 2)
                .map(|i| ((i as i64 * 7919) % (2 * max) - max) as i32)
                .collect();
            for chunk in samples.chunks(4800) {
                sink.write(&AudioBlock::pcm_i32(fmt, chunk.to_vec())).unwrap();
            }
            sink.drain().unwrap();
            std::thread::sleep(Duration::from_millis(1500));
            drop(sink);
            let raw = std::fs::read(&file).unwrap();
            let fresh = &raw[(before as usize).min(raw.len())..];
            let want = pack(&AudioBlock::pcm_i32(fmt, samples[2000..2400].to_vec()));
            assert!(
                fresh.windows(want.len()).any(|w| w == want.as_slice()),
                "{bits}-bit pattern not found in the client's output ({} new bytes)",
                fresh.len()
            );
        }
    }

    /// The output's `open` is async but awaits nothing that needs a runtime.
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

    /// Against a real snapserver (`SNAPSERVER=127.0.0.1 cargo test -p pm-out-snapcast -- --ignored`).
    #[test]
    #[ignore = "needs a running snapserver"]
    fn a_real_snapserver_lists_groups() {
        let addr = std::env::var("SNAPSERVER").unwrap_or_else(|_| "127.0.0.1".into());
        let groups = discover(&addr).unwrap();
        assert!(!groups.is_empty());
    }
}
