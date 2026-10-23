//! HEOS players (Denon, Marantz) are UPnP renderers, and also answer the **HEOS CLI**: Denon's
//! published control protocol (the "HEOS CLI Protocol Specification"), JSON answers to
//! `heos://group/command?attr=value` lines on TCP port 1255.
//! - **Grouping is HEOS's own.** `group/set_group?pid=<leader>,<member>,…` makes a HEOS group, and
//!   HEOS plays the leader's source on every member in sync; `set_group?pid=<leader>` ends it. A
//!   node zone of HEOS players only ([`HeosGroup`]) groups them that way, plays to the leader over
//!   UPnP like any renderer, and ends the group when the zone stops.
//! - **Player ids.** The CLI names players by `pid`; `player/get_players` lists each with its IP
//!   address and name, which is how a renderer is matched to its pid.
//!
//! Follows Music Assistant's `heos` provider (Apache-2.0; see NOTICE), which groups the same way.

use crate::output::UpnpOutput;
use crate::{Renderer, UpnpError};
use async_trait::async_trait;
use pm_model::{AudioFormat, OutputCaps, Volume};
use pm_zones::{AudioSink, Output, OutputId, QueueItem, SinkError};
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::time::Duration;

/// The HEOS CLI port.
pub const CLI_PORT: u16 = 1255;
const TIMEOUT: Duration = Duration::from_secs(5);

/// A Denon or Marantz renderer, or one that names HEOS in its model: worth asking the CLI.
pub fn is_heos(r: &Renderer) -> bool {
    let m = r.manufacturer.as_deref().unwrap_or("").to_ascii_lowercase();
    let model = r.model.as_deref().unwrap_or("").to_ascii_lowercase();
    m.starts_with("denon") || m.starts_with("marantz") || model.contains("heos")
}

/// The renderer's host, from its description address.
pub fn host(r: &Renderer) -> Option<String> {
    url::Url::parse(&r.location).ok()?.host_str().map(str::to_string)
}

/// A player as `player/get_players` lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Player {
    pub pid: i64,
    pub name: String,
    pub ip: String,
}

/// One CLI command (`player/get_players`, `group/set_group?pid=1,2`) to `host:port`, blocking.
/// Skips the interim "command under process" answers and the change events a connection may
/// carry, and fails unless the answer says `success`.
pub fn command(host: &str, port: u16, cmd: &str) -> Result<serde_json::Value, UpnpError> {
    let addr = (host, port)
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| UpnpError::Other(format!("HEOS: no address for {host}")))?;
    let mut stream = TcpStream::connect_timeout(&addr, TIMEOUT)?;
    stream.set_read_timeout(Some(TIMEOUT))?;
    stream.set_write_timeout(Some(TIMEOUT))?;
    stream.write_all(format!("heos://{cmd}\r\n").as_bytes())?;
    let name = cmd.split('?').next().unwrap_or(cmd);
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            return Err(UpnpError::Other(format!("HEOS {name}: connection closed")));
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
            continue;
        };
        let heos = &v["heos"];
        if heos["command"].as_str() != Some(name) {
            continue;
        }
        let message = heos["message"].as_str().unwrap_or("");
        if message.contains("command under process") {
            continue;
        }
        return match heos["result"].as_str() {
            Some("success") => Ok(v),
            _ => Err(UpnpError::Other(format!("HEOS {name} failed: {message}"))),
        };
    }
}

/// The players a HEOS device knows (every player on its network).
pub fn players(host: &str, port: u16) -> Result<Vec<Player>, UpnpError> {
    let v = command(host, port, "player/get_players")?;
    Ok(parse_players(&v))
}

pub fn parse_players(v: &serde_json::Value) -> Vec<Player> {
    v["payload"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|p| {
                    Some(Player {
                        pid: p["pid"].as_i64()?,
                        name: p["name"].as_str().unwrap_or("").to_string(),
                        ip: p["ip"].as_str().unwrap_or("").to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The pid of renderer `r`: the player at its address, or, when several share one (a test, or
/// a device with zones), the one with its name.
pub fn pid_of(players: &[Player], r: &Renderer) -> Option<i64> {
    let host = host(r)?;
    let at: Vec<&Player> = players.iter().filter(|p| p.ip == host).collect();
    if let [one] = at.as_slice() {
        return Some(one.pid);
    }
    let named = |p: &&Player| p.name.eq_ignore_ascii_case(r.friendly_name.trim());
    if at.is_empty() {
        players.iter().find(named).map(|p| p.pid)
    } else {
        at.into_iter().find(named).map(|p| p.pid)
    }
}

/// Whether every renderer is a player the first one's HEOS CLI knows (blocking): only then is a
/// zone of them a [`HeosGroup`]. A Denon or Marantz device without HEOS answers nothing.
pub fn can_group(renderers: &[Renderer], port: u16) -> bool {
    let Some(host) = renderers.first().and_then(host) else {
        return false;
    };
    match players(&host, port) {
        Ok(all) => renderers.iter().all(|r| pid_of(&all, r).is_some()),
        Err(e) => {
            tracing::debug!("HEOS CLI at {host}: {e}");
            false
        }
    }
}

/// A node zone of HEOS players only: the first leads, the others join its HEOS group (tight sync
/// by HEOS), and the group ends when the zone stops.
pub struct HeosGroup {
    name: String,
    lead: Arc<UpnpOutput>,
    members: Vec<Arc<UpnpOutput>>,
    port: u16,
}

impl HeosGroup {
    pub fn new(name: &str, lead: Arc<UpnpOutput>, members: Vec<Arc<UpnpOutput>>) -> Self {
        Self {
            name: name.to_string(),
            lead,
            members,
            port: CLI_PORT,
        }
    }

    /// Talk to the CLI on another port (tests).
    pub fn with_cli_port(mut self, port: u16) -> Self {
        self.port = port;
        self
    }
}

/// Make the HEOS group: `lead` first. Returns the leader's pid, to end the group with.
fn make_group(lead: &Renderer, members: &[Renderer], port: u16) -> Result<i64, UpnpError> {
    let host = host(lead).ok_or_else(|| UpnpError::Other("HEOS: the leader has no address".into()))?;
    let all = players(&host, port)?;
    let lead_pid = pid_of(&all, lead).ok_or_else(|| {
        UpnpError::Other(format!(
            "HEOS: {} is not among the HEOS players",
            lead.friendly_name
        ))
    })?;
    let mut pids = vec![lead_pid.to_string()];
    for m in members {
        match pid_of(&all, m) {
            Some(pid) if pid != lead_pid => pids.push(pid.to_string()),
            _ => tracing::warn!(player = %m.friendly_name, "not a HEOS player here; it stays out"),
        }
    }
    command(&host, port, &format!("group/set_group?pid={}", pids.join(",")))?;
    Ok(lead_pid)
}

#[async_trait]
impl Output for HeosGroup {
    fn id(&self) -> OutputId {
        let mut ids = vec![self.lead.id().0];
        ids.extend(self.members.iter().map(|m| m.id().0));
        OutputId::new("heos-group", &ids.join("+"))
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
        let lead = self.lead.renderer().clone();
        let members: Vec<Renderer> = self.members.iter().map(|m| m.renderer().clone()).collect();
        let port = self.port;
        let grouped = tokio::task::spawn_blocking(move || make_group(&lead, &members, port))
            .await
            .map_err(|e| SinkError::Unavailable(self.id(), format!("HEOS: {e}")))?;
        let lead_pid = match grouped {
            Ok(pid) => Some(pid),
            Err(e) => {
                tracing::warn!("HEOS grouping failed, the leader plays alone: {e}");
                None
            }
        };
        let inner = self.lead.open(fmt).await?;
        Ok(Box::new(GroupSink {
            inner: Some(inner),
            ungroup: lead_pid.and_then(|pid| Some((host(self.lead.renderer())?, port, pid))),
        }))
    }

    async fn set_volume(&self, v: Volume) -> Result<(), SinkError> {
        self.lead.set_volume(v).await?;
        for m in &self.members {
            m.set_volume(v).await?;
        }
        Ok(())
    }
}

struct GroupSink {
    /// `Some` until dropped: the leader stops before the group ends.
    inner: Option<Box<dyn AudioSink>>,
    /// The leader's host, CLI port and pid, when the group was made.
    ungroup: Option<(String, u16, i64)>,
}

impl GroupSink {
    fn sink(&self) -> &dyn AudioSink {
        self.inner.as_deref().expect("sink open")
    }
    fn sink_mut(&mut self) -> &mut Box<dyn AudioSink> {
        self.inner.as_mut().expect("sink open")
    }
}

impl AudioSink for GroupSink {
    fn format(&self) -> AudioFormat {
        self.sink().format()
    }
    fn write(&mut self, block: &pm_model::AudioBlock) -> Result<(), SinkError> {
        self.sink_mut().write(block)
    }
    fn buffer_level(&self) -> pm_zones::BufferLevel {
        self.sink().buffer_level()
    }
    fn flush(&mut self) -> Result<(), SinkError> {
        self.sink_mut().flush()
    }
    fn drain(&mut self) -> Result<(), SinkError> {
        self.sink_mut().drain()
    }
    fn frames_written(&self) -> u64 {
        self.sink().frames_written()
    }
}

impl Drop for GroupSink {
    fn drop(&mut self) {
        drop(self.inner.take());
        if let Some((host, port, pid)) = self.ungroup.take() {
            if let Err(e) = command(&host, port, &format!("group/set_group?pid={pid}")) {
                tracing::debug!("HEOS ungroup: {e}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pm_zones::outputs::PullOutput;
    use std::io::Read;
    use std::sync::Mutex;

    /// A fake HEOS CLI: logs each command, answers `get_players` after an interim "command under
    /// process" and a change event, and `set_group` with success.
    fn fake_cli(log: Arc<Mutex<Vec<String>>>) -> u16 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { break };
                let mut line = String::new();
                BufReader::new(s.try_clone().unwrap())
                    .read_line(&mut line)
                    .unwrap();
                let cmd = line.trim().trim_start_matches("heos://").to_string();
                log.lock().unwrap().push(cmd.clone());
                let name = cmd.split('?').next().unwrap().to_string();
                let mut out = String::new();
                if name == "player/get_players" {
                    out.push_str(r#"{"heos": {"command": "player/get_players", "result": "success", "message": "command under process"}}"#);
                    out.push_str("\r\n");
                    out.push_str(r#"{"heos": {"command": "event/players_changed", "message": ""}}"#);
                    out.push_str("\r\n");
                    out.push_str(r#"{"heos": {"command": "player/get_players", "result": "success", "message": ""}, "payload": [
                        {"name": "Living Room", "pid": 101, "model": "HEOS 7", "ip": "127.0.0.1"},
                        {"name": "Kitchen", "pid": 202, "model": "HEOS 1", "ip": "127.0.0.1"},
                        {"name": "Elsewhere", "pid": 303, "model": "HEOS 3", "ip": "10.0.0.9"}]}"#.replace('\n', " ").as_str());
                } else {
                    out.push_str(&format!(
                        r#"{{"heos": {{"command": "{name}", "result": "success", "message": "pid={}"}}}}"#,
                        cmd.split("pid=").nth(1).unwrap_or("")
                    ));
                }
                out.push_str("\r\n");
                let _ = s.write_all(out.as_bytes());
            }
        });
        port
    }

    /// A fake UPnP renderer: logs `"<name> <action>"` (lower case) and answers every SOAP call.
    fn fake_renderer(name: &'static str, log: Arc<Mutex<Vec<String>>>) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { break };
                let mut buf = Vec::new();
                let mut tmp = [0u8; 8192];
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
                let action = text
                    .lines()
                    .find_map(|l| {
                        l.to_ascii_lowercase()
                            .strip_prefix("soapaction: ")
                            .map(str::to_string)
                    })
                    .map(|a| a.trim_matches('"').rsplit('#').next().unwrap_or("").to_string())
                    .unwrap_or_default();
                log.lock().unwrap().push(format!("{name} {action}"));
                let body = "<s:Envelope><s:Body><u:R/></s:Body></s:Envelope>";
                let _ = write!(s, "HTTP/1.1 200 OK\r\nContent-Type: text/xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body);
            }
        });
        format!("http://{addr}")
    }

    fn renderer(name: &str, base: &str) -> Renderer {
        Renderer {
            udn: format!("uuid:{name}"),
            friendly_name: name.into(),
            manufacturer: Some("Denon".into()),
            model: Some("HEOS 7".into()),
            av_transport: format!("{base}/upnp/control/AVTransport"),
            rendering_control: None,
            connection_manager: None,
            zone_group_topology: None,
            location: format!("{base}/upnp/desc.xml"),
        }
    }

    #[test]
    fn players_and_pids() {
        let v: serde_json::Value = serde_json::from_str(
            r#"{"heos": {"command": "player/get_players", "result": "success"}, "payload": [
                {"name": "Living Room", "pid": 101, "ip": "192.168.1.20"},
                {"name": "Zone 2", "pid": 102, "ip": "192.168.1.21"},
                {"name": "Kitchen", "pid": 202, "ip": "192.168.1.21"}]}"#,
        )
        .unwrap();
        let p = parse_players(&v);
        assert_eq!(p.len(), 3);
        let mut r = renderer("Living Room", "http://192.168.1.20:60006");
        assert_eq!(pid_of(&p, &r), Some(101), "the only player at that address");
        r = renderer("kitchen", "http://192.168.1.21:60006");
        assert_eq!(pid_of(&p, &r), Some(202), "two at that address: by name");
        r = renderer("Kitchen", "http://192.168.1.99:60006");
        assert_eq!(pid_of(&p, &r), Some(202), "none at that address: by name");
        r = renderer("Attic", "http://192.168.1.99:60006");
        assert_eq!(pid_of(&p, &r), None);
        assert!(is_heos(&r));
        r.manufacturer = Some("Sonos, Inc.".into());
        r.model = Some("One".into());
        assert!(!is_heos(&r));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_node_zone_of_heos_players_is_a_heos_group() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let port = fake_cli(log.clone());
        let a = fake_renderer("A", log.clone());
        let b = fake_renderer("B", log.clone());
        let out = |name: &str, base: &str, id: &str| {
            Arc::new(UpnpOutput::new(
                renderer(name, base),
                Arc::new(PullOutput::new(id)),
                format!("http://node/v1/outputs/pull/{id}/stream.wav?k=1"),
            ))
        };
        let group = HeosGroup::new(
            "Downstairs",
            out("Living Room", &a, "a"),
            vec![out("Kitchen", &b, "b")],
        )
        .with_cli_port(port);
        assert!(group.id().0.starts_with("heos-group:"));
        assert_eq!(group.sync(), pm_zones::SyncClass::Tight);
        let sink = group.open(AudioFormat::pcm(44_100, 16, 2)).await.unwrap();
        drop(sink);
        let l = log.lock().unwrap().clone();
        assert_eq!(
            l,
            vec![
                "player/get_players",
                "group/set_group?pid=101,202",
                "A stop",
                "A setavtransporturi",
                "A play",
                "A stop",
                "group/set_group?pid=101",
            ],
            "grouped by HEOS, played to the leader over UPnP, ungrouped at the end"
        );
    }
}
