//! Sonos players are UPnP renderers with house rules:
//! - **Only a group's coordinator plays.** A player that is a member of a Sonos group refuses
//!   transport commands, so the node sends them to its coordinator, and the whole Sonos group plays.
//!   That is what the Sonos app shows as one room.
//! - **Grouping is Sonos's own.** A member joins with `SetAVTransportURI x-rincon:<coordinator>` and
//!   leaves with `BecomeCoordinatorOfStandaloneGroup`, and Sonos keeps the players in tight sync. A
//!   node zone made of Sonos players only ([`SonosGroup`]) groups them that way instead of
//!   streaming to each.
//! - **PCM ceiling.** Sonos plays WAV at 16 bits up to 48 kHz; the zone converts anything above,
//!   and the signal path says so.
//!
//! The group's topology comes from `ZoneGroupTopology.GetZoneGroupState`.

use crate::output::UpnpOutput;
use crate::{soap, Renderer, UpnpError, AV_TRANSPORT};
use async_trait::async_trait;
use pm_model::{AudioFormat, OutputCaps, Volume};
use pm_zones::{AudioSink, Output, OutputId, QueueItem, SinkError};
use std::sync::Arc;

pub const ZONE_GROUP_TOPOLOGY: &str = "urn:schemas-upnp-org:service:ZoneGroupTopology:1";

pub fn is_sonos(r: &Renderer) -> bool {
    r.manufacturer.as_deref().is_some_and(|m| m.starts_with("Sonos"))
        || r.udn.trim_start_matches("uuid:").starts_with("RINCON_")
}

/// The player's id in Sonos's topology (`RINCON_…`).
pub fn rincon(r: &Renderer) -> String {
    r.udn.trim_start_matches("uuid:").to_string()
}

/// A Sonos AVTransport control URL from a player's description location (`http://ip:1400/…`).
fn av_transport_at(location: &str) -> Option<String> {
    let u = url::Url::parse(location).ok()?;
    Some(format!(
        "{}://{}:{}/MediaRenderer/AVTransport/Control",
        u.scheme(),
        u.host_str()?,
        u.port_or_known_default()?
    ))
}

/// A Sonos group as the topology reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Group {
    pub coordinator: String,
    /// `(rincon, description location)` of every member, the coordinator included.
    pub members: Vec<(String, String)>,
}

/// Parse `ZoneGroupState` (the XML inside the SOAP answer, already unescaped).
pub fn parse_groups(xml: &str) -> Vec<Group> {
    use quick_xml::events::Event;
    use quick_xml::Reader;
    let mut reader = Reader::from_str(xml);
    let mut out: Vec<Group> = Vec::new();
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                let attr = |name: &str| {
                    e.attributes()
                        .flatten()
                        .find(|a| a.key.as_ref() == name)
                        .and_then(|a| {
                            a.normalized_value(quick_xml::XmlVersion::Implicit1_0)
                                .ok()
                                .map(|v| v.into_owned())
                        })
                };
                match e.local_name().as_ref() {
                    "ZoneGroup" => out.push(Group {
                        coordinator: attr("Coordinator").unwrap_or_default(),
                        members: Vec::new(),
                    }),
                    "ZoneGroupMember" => {
                        if let (Some(g), Some(id), Some(loc)) =
                            (out.last_mut(), attr("UUID"), attr("Location"))
                        {
                            g.members.push((id, loc));
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    out
}

fn unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/// The groups as `player` sees them (blocking).
pub fn groups(player: &Renderer) -> Result<Vec<Group>, UpnpError> {
    let Some(zgt) = &player.zone_group_topology else {
        return Ok(Vec::new());
    };
    let xml = soap::call(zgt, ZONE_GROUP_TOPOLOGY, "GetZoneGroupState", &[])?;
    Ok(parse_groups(&unescape(
        raw_element(&xml, "ZoneGroupState").unwrap_or_default(),
    )))
}

/// The raw text of element `name` (any prefix). Sonos sends the topology as one escaped XML
/// string, which XML readers hand over in pieces split at every entity; inside it the tag names
/// are escaped (`&lt;ZoneGroupState&gt;`), so `name>` only matches the real tags.
fn raw_element<'a>(xml: &'a str, name: &str) -> Option<&'a str> {
    let pat = format!("{name}>");
    let start = xml.find(&pat)? + pat.len();
    let close = start + xml[start..].find(&pat)?;
    let close_tag = xml[..close].rfind("</")?;
    (close_tag >= start).then(|| &xml[start..close_tag])
}

/// Where transport commands for `player` must go: its own AVTransport when it leads (or is alone),
/// its coordinator's when it is a member of a Sonos group.
pub fn transport_target(player: &Renderer) -> String {
    let me = rincon(player);
    match groups(player) {
        Ok(gs) => gs
            .iter()
            .find(|g| g.members.iter().any(|(id, _)| *id == me))
            .filter(|g| g.coordinator != me)
            .and_then(|g| g.members.iter().find(|(id, _)| *id == g.coordinator))
            .and_then(|(_, loc)| av_transport_at(loc))
            .unwrap_or_else(|| player.av_transport.clone()),
        Err(e) => {
            tracing::debug!(player = %player.friendly_name, "Sonos topology unavailable: {e}");
            player.av_transport.clone()
        }
    }
}

/// Join `member` to `coordinator`'s Sonos group.
pub fn join(member: &Renderer, coordinator: &Renderer) -> Result<(), UpnpError> {
    soap::call(
        &member.av_transport,
        AV_TRANSPORT,
        "SetAVTransportURI",
        &[
            ("InstanceID", "0"),
            ("CurrentURI", &format!("x-rincon:{}", rincon(coordinator))),
            ("CurrentURIMetaData", ""),
        ],
    )
    .map(|_| ())
}

/// Take `member` out of whatever group it is in.
pub fn leave(member: &Renderer) -> Result<(), UpnpError> {
    soap::call(
        &member.av_transport,
        AV_TRANSPORT,
        "BecomeCoordinatorOfStandaloneGroup",
        &[("InstanceID", "0")],
    )
    .map(|_| ())
}

/// A node zone of Sonos players only: the first leads, the others join it the Sonos way (tight sync
/// by Sonos), and they leave again when the zone stops.
pub struct SonosGroup {
    name: String,
    lead: Arc<UpnpOutput>,
    members: Vec<Arc<UpnpOutput>>,
}

impl SonosGroup {
    pub fn new(name: &str, lead: Arc<UpnpOutput>, members: Vec<Arc<UpnpOutput>>) -> Self {
        Self {
            name: name.to_string(),
            lead,
            members,
        }
    }
}

#[async_trait]
impl Output for SonosGroup {
    fn id(&self) -> OutputId {
        let mut ids = vec![self.lead.id().0];
        ids.extend(self.members.iter().map(|m| m.id().0));
        OutputId::new("sonos-group", &ids.join("+"))
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
        let joined = tokio::task::spawn_blocking(move || {
            let mut errors = Vec::new();
            for m in &members {
                if let Err(e) = join(m, &lead) {
                    errors.push(format!("{}: {e}", m.friendly_name));
                }
            }
            errors
        })
        .await
        .unwrap_or_default();
        for e in joined {
            tracing::warn!("Sonos join failed, that player stays out: {e}");
        }
        let inner = self.lead.open(fmt).await?;
        Ok(Box::new(GroupSink {
            inner: Some(inner),
            members: self.members.iter().map(|m| m.renderer().clone()).collect(),
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
    /// `Some` until dropped: the lead stops before the members leave.
    inner: Option<Box<dyn AudioSink>>,
    members: Vec<Renderer>,
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
    /// The lead pauses the group (its renderer's own Pause).
    fn pause(&mut self) -> Result<bool, SinkError> {
        self.sink_mut().pause()
    }
    fn resume(&mut self) -> Result<(), SinkError> {
        self.sink_mut().resume()
    }
    fn frames_written(&self) -> u64 {
        self.sink().frames_written()
    }
}

impl Drop for GroupSink {
    fn drop(&mut self) {
        drop(self.inner.take());
        for m in &self.members {
            if let Err(e) = leave(m) {
                tracing::debug!(player = %m.friendly_name, "Sonos leave: {e}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pm_zones::outputs::PullOutput;
    use std::io::{Read, Write};
    use std::sync::Mutex;

    /// A fake Sonos player: logs `"<name> <action> <uri>"`, answers GetZoneGroupState with `state`.
    fn fake_player(name: &'static str, state: Arc<Mutex<String>>, log: Arc<Mutex<Vec<String>>>) -> String {
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
                let action = [
                    "setavtransporturi",
                    "play",
                    "stop",
                    "getzonegroupstate",
                    "becomecoordinatorofstandalonegroup",
                ]
                .iter()
                .zip([
                    "SetAVTransportURI",
                    "Play",
                    "Stop",
                    "GetZoneGroupState",
                    "BecomeCoordinatorOfStandaloneGroup",
                ])
                .find(|(l, _)| **l == action)
                .map(|(_, a)| a.to_string())
                .unwrap_or(action);
                let uri = soap::element_text(&text, "CurrentURI").unwrap_or_default();
                if action != "GetZoneGroupState" {
                    log.lock()
                        .unwrap()
                        .push(format!("{name} {action} {uri}").trim().to_string());
                }
                let body = if action == "GetZoneGroupState" {
                    let st = state
                        .lock()
                        .unwrap()
                        .replace('&', "&amp;")
                        .replace('<', "&lt;")
                        .replace('>', "&gt;")
                        .replace('"', "&quot;");
                    format!("<s:Envelope><s:Body><u:GetZoneGroupStateResponse><ZoneGroupState>{st}</ZoneGroupState></u:GetZoneGroupStateResponse></s:Body></s:Envelope>")
                } else {
                    "<s:Envelope><s:Body><u:R/></s:Body></s:Envelope>".to_string()
                };
                let _ = write!(s, "HTTP/1.1 200 OK\r\nContent-Type: text/xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body);
            }
        });
        format!("http://{addr}")
    }

    fn player(rincon_id: &str, base: &str) -> Renderer {
        Renderer {
            udn: format!("uuid:{rincon_id}"),
            friendly_name: rincon_id.into(),
            manufacturer: Some("Sonos, Inc.".into()),
            model: Some("Sonos One".into()),
            av_transport: format!("{base}/MediaRenderer/AVTransport/Control"),
            rendering_control: None,
            connection_manager: None,
            zone_group_topology: Some(format!("{base}/ZoneGroupTopology/Control")),
            location: format!("{base}/xml/device_description.xml"),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_grouped_player_plays_through_its_coordinator_and_node_groups_are_sonos_groups() {
        let state = Arc::new(Mutex::new(String::new()));
        let log = Arc::new(Mutex::new(Vec::new()));
        let a = fake_player("A", state.clone(), log.clone());
        let b = fake_player("B", state.clone(), log.clone());
        // B is in A's Sonos group.
        *state.lock().unwrap() = format!(
            r#"<ZoneGroupState><ZoneGroups><ZoneGroup Coordinator="RINCON_A"><ZoneGroupMember UUID="RINCON_A" Location="{a}/xml/device_description.xml"/><ZoneGroupMember UUID="RINCON_B" Location="{b}/xml/device_description.xml"/></ZoneGroup></ZoneGroups></ZoneGroupState>"#
        );
        let out_b = Arc::new(UpnpOutput::new(
            player("RINCON_B", &b),
            Arc::new(PullOutput::new("b")),
            "http://node/v1/outputs/pull/b/stream.wav?k=1".into(),
        ));
        // Sonos's ceiling.
        assert!(out_b.caps().accepts(&AudioFormat::pcm(48_000, 16, 2)));
        assert!(!out_b.caps().accepts(&AudioFormat::pcm(96_000, 24, 2)));
        let sink = out_b.open(AudioFormat::pcm(44_100, 16, 2)).await.unwrap();
        // The start runs on its own thread; a sink closed before Play sends none
        for _ in 0..300 {
            if log.lock().unwrap().iter().any(|a| a.starts_with("A Play")) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        drop(sink);
        let l = log.lock().unwrap().clone();
        let g = out_b.pull().generation();
        assert_eq!(
            l,
            vec![
                "A Stop".to_string(),
                format!("A SetAVTransportURI http://node/v1/outputs/pull/b/stream.wav?k=1&g={g}"),
                "A Play".into(),
                "A Stop".into()
            ],
            "B's transport went to A"
        );

        // Now both stand alone; a node zone of A and B groups them the Sonos way.
        *state.lock().unwrap() = String::new();
        log.lock().unwrap().clear();
        let pull_a = Arc::new(PullOutput::new("a"));
        let out_a = Arc::new(UpnpOutput::new(
            player("RINCON_A", &a),
            pull_a.clone(),
            "http://node/v1/outputs/pull/a/stream.wav?k=2".into(),
        ));
        let group = SonosGroup::new("Downstairs", out_a, vec![out_b]);
        assert!(group.id().0.starts_with("sonos-group:"));
        let sink = group.open(AudioFormat::pcm(44_100, 16, 2)).await.unwrap();
        for _ in 0..300 {
            if log.lock().unwrap().iter().any(|a| a.starts_with("A Play")) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        drop(sink);
        let l = log.lock().unwrap().clone();
        assert_eq!(
            l,
            vec![
                "B SetAVTransportURI x-rincon:RINCON_A".to_string(),
                "A Stop".into(),
                format!(
                    "A SetAVTransportURI http://node/v1/outputs/pull/a/stream.wav?k=2&g={}",
                    pull_a.generation()
                ),
                "A Play".into(),
                "A Stop".into(),
                "B BecomeCoordinatorOfStandaloneGroup".into()
            ]
        );
    }

    const STATE: &str = r#"<ZoneGroupState><ZoneGroups>
<ZoneGroup Coordinator="RINCON_AAA01400" ID="RINCON_AAA01400:1">
  <ZoneGroupMember UUID="RINCON_AAA01400" Location="http://192.168.1.10:1400/xml/device_description.xml" ZoneName="Kitchen"/>
  <ZoneGroupMember UUID="RINCON_BBB01400" Location="http://192.168.1.11:1400/xml/device_description.xml" ZoneName="Dining"/>
</ZoneGroup>
<ZoneGroup Coordinator="RINCON_CCC01400" ID="RINCON_CCC01400:7">
  <ZoneGroupMember UUID="RINCON_CCC01400" Location="http://192.168.1.12:1400/xml/device_description.xml" ZoneName="Office"/>
</ZoneGroup>
</ZoneGroups></ZoneGroupState>"#;

    #[test]
    fn the_raw_topology_is_cut_out_of_the_soap_answer() {
        let soap = "<s:Body><u:R><ZoneGroupState>&lt;ZoneGroupState&gt;&lt;/ZoneGroupState&gt;</ZoneGroupState></u:R>";
        assert_eq!(
            raw_element(soap, "ZoneGroupState"),
            Some("&lt;ZoneGroupState&gt;&lt;/ZoneGroupState&gt;")
        );
        assert_eq!(
            raw_element("<a:ZoneGroupState>x</a:ZoneGroupState>", "ZoneGroupState"),
            Some("x")
        );
        assert_eq!(raw_element("<x/>", "ZoneGroupState"), None);
    }

    #[test]
    fn the_topology_reads_groups_and_coordinators() {
        let g = parse_groups(STATE);
        assert_eq!(g.len(), 2);
        assert_eq!(g[0].coordinator, "RINCON_AAA01400");
        assert_eq!(g[0].members.len(), 2);
        assert_eq!(g[0].members[1].0, "RINCON_BBB01400");
        assert_eq!(
            av_transport_at(&g[0].members[0].1).as_deref(),
            Some("http://192.168.1.10:1400/MediaRenderer/AVTransport/Control")
        );
        // Escaped inside the SOAP answer, as Sonos sends it.
        let escaped = STATE
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;");
        assert_eq!(parse_groups(&unescape(&escaped)), g);
    }
}
