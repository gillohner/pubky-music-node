//! Zones as UPnP/DLNA MediaRenderers, so any UPnP control point can play to them: Music
//! Assistant's DLNA player provider, Home Assistant's `dlna_dmr`, BubbleUPnP, Windows'
//! "Cast to device".
//!
//! Each zone is one MediaRenderer:1 device (standard profile) with AVTransport:1,
//! RenderingControl:1 and ConnectionManager:1, found over SSDP ([`ssdp`]) and described at
//! `/upnp/zone/<zone>/desc.xml`. Control is SOAP at `…/ctl/{avt,rc,cm}`; GENA events
//! (`LastChange`) at `…/evt/{avt,rc,cm}`. A controller sets a URI (an HTTP audio stream: FLAC,
//! WAV, LPCM, MP3, AAC, Ogg) and the zone plays it like any other queue item, through its own
//! output and DSP.
//!
//! The node implements [`Host`] over its zones. These routes take no API token: like any
//! renderer on a LAN, whoever can reach the port can play to the zones. `[renderer] enabled =
//! false` turns them off.

pub mod ssdp;
pub mod xml;

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, post};
use axum::Router;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// What a controller said (or the zone has) about the current item.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Meta {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub art: Option<String>,
    pub duration_ms: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Transport {
    Playing,
    Paused,
    #[default]
    Stopped,
    Transitioning,
    NoMedia,
}

/// A zone as a renderer reports it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct View {
    pub state: Transport,
    /// The current item's URI ("" when there is none).
    pub uri: String,
    pub meta: Meta,
    pub next_uri: String,
    pub position_ms: u64,
    /// 0-100.
    pub volume: u8,
    pub muted: bool,
}

/// What a controller asks of a zone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Act {
    /// Replace what the zone plays with this URI (stopped until `Play`).
    SetUri {
        uri: String,
        meta: Meta,
    },
    /// Play this right after the current item (gapless where the formats allow).
    SetNextUri {
        uri: String,
        meta: Meta,
    },
    Play,
    Pause,
    Stop,
    Seek(u64),
    Next,
    Previous,
    Volume(u8),
    Mute(bool),
}

/// The node's side: its zones and what they do.
#[async_trait::async_trait]
pub trait Host: Send + Sync + 'static {
    /// `(id, name)` of every zone to offer.
    fn zones(&self) -> Vec<(String, String)>;
    fn view(&self, zone: &str) -> Option<View>;
    /// The zone's output has hardware volume (otherwise no Volume or Mute is offered).
    fn has_volume(&self, _zone: &str) -> bool {
        true
    }
    async fn act(&self, zone: &str, act: Act) -> Result<(), String>;
}

/// Event subscribers of one service of one zone.
struct Sub {
    zone: String,
    service: String,
    callbacks: Vec<String>,
    expires: Instant,
    seq: u32,
    /// The last property set sent (only changes go out).
    last: String,
}

pub struct Renderer {
    host: Arc<dyn Host>,
    node_id: String,
    version: String,
    subs: Mutex<HashMap<String, Sub>>,
}

/// Subscriptions last this long unless renewed (controllers ask for 300-1800 s).
const MAX_TIMEOUT: u64 = 1800;
/// How often zone state is compared for events.
const EVENT_EVERY: Duration = Duration::from_millis(500);

impl Renderer {
    pub fn new(host: Arc<dyn Host>, node_id: &str, version: &str) -> Arc<Self> {
        Arc::new(Self {
            host,
            node_id: node_id.to_string(),
            version: version.to_string(),
            subs: Mutex::new(HashMap::new()),
        })
    }

    /// The zone's device UUID: stable across restarts (node id + zone id).
    pub fn udn(&self, zone: &str) -> String {
        let h = blake3::hash(format!("pubky-music-renderer:{}:{zone}", self.node_id).as_bytes());
        let b = h.as_bytes();
        format!(
            "{:08x}-{:04x}-4{:03x}-a{:03x}-{:012x}",
            u32::from_be_bytes([b[0], b[1], b[2], b[3]]),
            u16::from_be_bytes([b[4], b[5]]),
            u16::from_be_bytes([b[6], b[7]]) & 0x0fff,
            u16::from_be_bytes([b[8], b[9]]) & 0x0fff,
            u64::from_be_bytes([0, 0, b[10], b[11], b[12], b[13], b[14], b[15]])
        )
    }

    pub fn zones(&self) -> Vec<(String, String)> {
        self.host.zones()
    }

    /// The HTTP routes (merge into the node's router; no token).
    pub fn router(self: &Arc<Self>) -> Router {
        Router::new()
            .route("/upnp/zone/{zone}/{file}", get(describe))
            .route("/upnp/zone/{zone}/ctl/{service}", post(control))
            .route("/upnp/zone/{zone}/evt/{service}", any(events))
            .with_state(self.clone())
    }

    /// Send changed state to event subscribers until the runtime stops.
    pub fn spawn_notifier(self: &Arc<Self>) {
        let me = self.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(EVENT_EVERY);
            loop {
                tick.tick().await;
                me.notify_changes().await;
            }
        });
    }

    async fn notify_changes(&self) {
        let now = Instant::now();
        let mut out: Vec<(String, Vec<String>, u32, String)> = Vec::new();
        {
            let mut subs = self.subs.lock().expect("subs");
            subs.retain(|_, s| s.expires > now);
            for (sid, s) in subs.iter_mut() {
                let Some(view) = self.host.view(&s.zone) else {
                    continue;
                };
                let body = xml::propertyset(&s.service, &view);
                if body != s.last {
                    s.last = body.clone();
                    out.push((sid.clone(), s.callbacks.clone(), s.seq, body));
                    s.seq = s.seq.wrapping_add(1).max(1);
                }
            }
        }
        for (sid, callbacks, seq, body) in out {
            tokio::spawn(async move { send_notify(&callbacks, &sid, seq, &body).await });
        }
    }

    fn zone_known(&self, zone: &str) -> Option<String> {
        self.host
            .zones()
            .into_iter()
            .find(|(id, _)| id == zone)
            .map(|(_, name)| name)
    }
}

/// `NOTIFY` to the first callback that answers (GENA). Failures are dropped: the next change
/// is sent again, and a dead subscriber expires.
async fn send_notify(callbacks: &[String], sid: &str, seq: u32, body: &str) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    for cb in callbacks {
        let Some(rest) = cb.strip_prefix("http://") else {
            continue;
        };
        let (hostport, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, "/"),
        };
        let req = format!(
            "NOTIFY {path} HTTP/1.1\r\nHOST: {hostport}\r\nCONTENT-TYPE: text/xml; charset=\"utf-8\"\r\n\
             NT: upnp:event\r\nNTS: upnp:propchange\r\nSID: {sid}\r\nSEQ: {seq}\r\nCONTENT-LENGTH: {}\r\n\
             CONNECTION: close\r\n\r\n{body}",
            body.len()
        );
        let sent = tokio::time::timeout(Duration::from_secs(3), async {
            let mut s = tokio::net::TcpStream::connect(hostport).await?;
            s.write_all(req.as_bytes()).await?;
            let mut buf = [0u8; 64];
            let _ = s.read(&mut buf).await?;
            Ok::<_, std::io::Error>(())
        })
        .await;
        if matches!(sent, Ok(Ok(()))) {
            tracing::trace!(%cb, %sid, seq, "renderer event delivered");
            return;
        }
        tracing::debug!(%cb, %sid, "renderer event not delivered");
    }
}

fn xml_response(body: String) -> Response {
    (
        StatusCode::OK,
        [("content-type", "text/xml; charset=\"utf-8\"")],
        body,
    )
        .into_response()
}

async fn describe(State(r): State<Arc<Renderer>>, Path((zone, file)): Path<(String, String)>) -> Response {
    let Some(name) = r.zone_known(&zone) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    match file.as_str() {
        "desc.xml" => xml_response(xml::device_description(
            &r.udn(&zone),
            &name,
            &format!("/upnp/zone/{}", ssdp::path_segment(&zone)),
            &r.version,
        )),
        f => match f
            .strip_suffix(".xml")
            .and_then(|s| xml::service_description(s, r.host.has_volume(&zone)))
        {
            Some(x) => xml_response(x),
            None => StatusCode::NOT_FOUND.into_response(),
        },
    }
}

async fn control(
    State(r): State<Arc<Renderer>>,
    Path((zone, service)): Path<(String, String)>,
    body: Bytes,
) -> Response {
    let soap_fault = |code: u16, msg: &str| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            [("content-type", "text/xml; charset=\"utf-8\"")],
            xml::fault(code, msg),
        )
            .into_response()
    };
    if r.zone_known(&zone).is_none() {
        return StatusCode::NOT_FOUND.into_response();
    }
    let Some(call) = xml::parse_call(&String::from_utf8_lossy(&body)) else {
        return soap_fault(401, "Invalid Action");
    };
    let urn = match service.as_str() {
        "avt" => xml::AVT,
        "rc" => xml::RC,
        "cm" => xml::CM,
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    tracing::debug!(%zone, service, action = %call.action, "renderer action");
    let view = r.host.view(&zone).unwrap_or_default();
    let act = |a: Act| {
        let r = r.clone();
        let zone = zone.clone();
        async move { r.host.act(&zone, a).await }
    };
    let ok = |out: &[(&str, String)]| xml_response(xml::response(urn, &call.action, out));
    let res: Result<Response, (u16, String)> = match (service.as_str(), call.action.as_str()) {
        ("avt", "SetAVTransportURI") => {
            let uri = call.arg("CurrentURI").trim().to_string();
            if !(uri.starts_with("http://") || uri.starts_with("https://")) {
                Err((714, "Illegal MIME-type: only http(s) URIs play here".into()))
            } else {
                let meta = xml::parse_didl(call.arg("CurrentURIMetaData"));
                act(Act::SetUri { uri, meta })
                    .await
                    .map(|_| ok(&[]))
                    .map_err(|e| (501, e))
            }
        }
        ("avt", "SetNextAVTransportURI") => {
            let uri = call.arg("NextURI").trim().to_string();
            let meta = xml::parse_didl(call.arg("NextURIMetaData"));
            act(Act::SetNextUri { uri, meta })
                .await
                .map(|_| ok(&[]))
                .map_err(|e| (501, e))
        }
        ("avt", "Play") => act(Act::Play).await.map(|_| ok(&[])).map_err(|e| (501, e)),
        ("avt", "Pause") => act(Act::Pause).await.map(|_| ok(&[])).map_err(|e| (501, e)),
        ("avt", "Stop") => act(Act::Stop).await.map(|_| ok(&[])).map_err(|e| (501, e)),
        ("avt", "Next") => act(Act::Next).await.map(|_| ok(&[])).map_err(|e| (501, e)),
        ("avt", "Previous") => act(Act::Previous).await.map(|_| ok(&[])).map_err(|e| (501, e)),
        ("avt", "Seek") => match (call.arg("Unit"), xml::parse_hms(call.arg("Target"))) {
            ("REL_TIME" | "ABS_TIME", Some(ms)) => {
                act(Act::Seek(ms)).await.map(|_| ok(&[])).map_err(|e| (710, e))
            }
            _ => Err((710, "Seek mode not supported".into())),
        },
        ("avt", "GetTransportInfo") => Ok(ok(&[
            ("CurrentTransportState", xml::transport_state(view.state).into()),
            ("CurrentTransportStatus", "OK".into()),
            ("CurrentSpeed", "1".into()),
        ])),
        ("avt", "GetPositionInfo") => {
            let has = !view.uri.is_empty();
            Ok(ok(&[
                ("Track", if has { "1" } else { "0" }.into()),
                ("TrackDuration", xml::hms(view.meta.duration_ms.unwrap_or(0))),
                ("TrackMetaData", xml::didl(&view.uri, &view.meta)),
                ("TrackURI", view.uri.clone()),
                ("RelTime", xml::hms(view.position_ms)),
                ("AbsTime", xml::hms(view.position_ms)),
                ("RelCount", "2147483647".into()),
                ("AbsCount", "2147483647".into()),
            ]))
        }
        ("avt", "GetMediaInfo") => Ok(ok(&[
            ("NrTracks", if view.uri.is_empty() { "0" } else { "1" }.into()),
            ("MediaDuration", xml::hms(view.meta.duration_ms.unwrap_or(0))),
            ("CurrentURI", view.uri.clone()),
            ("CurrentURIMetaData", xml::didl(&view.uri, &view.meta)),
            ("NextURI", view.next_uri.clone()),
            ("NextURIMetaData", String::new()),
            ("PlayMedium", "NETWORK".into()),
            ("RecordMedium", "NOT_IMPLEMENTED".into()),
            ("WriteStatus", "NOT_IMPLEMENTED".into()),
        ])),
        ("avt", "GetDeviceCapabilities") => Ok(ok(&[
            ("PlayMedia", "NETWORK".into()),
            ("RecMedia", "NOT_IMPLEMENTED".into()),
            ("RecQualityModes", "NOT_IMPLEMENTED".into()),
        ])),
        ("avt", "GetTransportSettings") => Ok(ok(&[
            ("PlayMode", "NORMAL".into()),
            ("RecQualityMode", "NOT_IMPLEMENTED".into()),
        ])),
        ("avt", "GetCurrentTransportActions") => {
            Ok(ok(&[("Actions", "Play,Pause,Stop,Seek,Next,Previous".into())]))
        }
        ("rc", "GetVolume") => Ok(ok(&[("CurrentVolume", view.volume.to_string())])),
        ("rc", "SetVolume") => match call.arg("DesiredVolume").trim().parse::<u8>() {
            Ok(v) if v <= 100 => act(Act::Volume(v)).await.map(|_| ok(&[])).map_err(|e| (501, e)),
            _ => Err((402, "Invalid Args".into())),
        },
        ("rc", "GetMute") => Ok(ok(&[("CurrentMute", u8::from(view.muted).to_string())])),
        ("rc", "SetMute") => {
            let m = matches!(call.arg("DesiredMute").trim(), "1" | "true" | "yes");
            act(Act::Mute(m)).await.map(|_| ok(&[])).map_err(|e| (501, e))
        }
        ("rc", "ListPresets") => Ok(ok(&[("CurrentPresetNameList", "FactoryDefaults".into())])),
        ("rc", "SelectPreset") => Ok(ok(&[])),
        ("cm", "GetProtocolInfo") => Ok(ok(&[
            ("Source", String::new()),
            ("Sink", xml::SINK_PROTOCOLS.join(",")),
        ])),
        ("cm", "GetCurrentConnectionIDs") => Ok(ok(&[("ConnectionIDs", "0".into())])),
        ("cm", "GetCurrentConnectionInfo") => Ok(ok(&[
            ("RcsID", "0".into()),
            ("AVTransportID", "0".into()),
            ("ProtocolInfo", String::new()),
            ("PeerConnectionManager", String::new()),
            ("PeerConnectionID", "-1".into()),
            ("Direction", "Input".into()),
            ("Status", "OK".into()),
        ])),
        _ => Err((401, "Invalid Action".into())),
    };
    match res {
        Ok(resp) => resp,
        Err((code, msg)) => {
            tracing::info!(%zone, action = %call.action, code, %msg, "renderer action failed");
            soap_fault(code, &msg)
        }
    }
}

/// GENA `SUBSCRIBE` (new or renewal) and `UNSUBSCRIBE`.
async fn events(
    State(r): State<Arc<Renderer>>,
    Path((zone, service)): Path<(String, String)>,
    method: Method,
    headers: HeaderMap,
) -> Response {
    if r.zone_known(&zone).is_none() || !matches!(service.as_str(), "avt" | "rc" | "cm") {
        return StatusCode::NOT_FOUND.into_response();
    }
    let h = |k: &str| {
        headers
            .get(k)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string()
    };
    let timeout = h("timeout")
        .strip_prefix("Second-")
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(MAX_TIMEOUT)
        .clamp(60, MAX_TIMEOUT);
    match method.as_str() {
        "SUBSCRIBE" => {
            let sid = h("sid");
            let mut subs = r.subs.lock().expect("subs");
            if !sid.is_empty() {
                // Renewal.
                return match subs.get_mut(&sid) {
                    Some(s) => {
                        s.expires = Instant::now() + Duration::from_secs(timeout);
                        gena_ok(&sid, timeout)
                    }
                    None => StatusCode::PRECONDITION_FAILED.into_response(),
                };
            }
            let callbacks: Vec<String> = h("callback")
                .split('>')
                .filter_map(|c| c.trim().strip_prefix('<'))
                .map(str::to_string)
                .filter(|c| c.starts_with("http://"))
                .collect();
            if callbacks.is_empty() || h("nt") != "upnp:event" {
                return StatusCode::PRECONDITION_FAILED.into_response();
            }
            let sid = format!(
                "uuid:{}",
                r.udn(&format!("{zone}/{service}/{:?}", Instant::now()))
            );
            subs.insert(
                sid.clone(),
                Sub {
                    zone,
                    service,
                    callbacks,
                    expires: Instant::now() + Duration::from_secs(timeout),
                    // The first event (SEQ 0) carries the full state; it goes out on the next tick.
                    seq: 0,
                    last: String::new(),
                },
            );
            tracing::debug!(%sid, timeout, "renderer: new event subscriber");
            gena_ok(&sid, timeout)
        }
        "UNSUBSCRIBE" => {
            let removed = r.subs.lock().expect("subs").remove(&h("sid")).is_some();
            if removed {
                StatusCode::OK.into_response()
            } else {
                StatusCode::PRECONDITION_FAILED.into_response()
            }
        }
        _ => StatusCode::METHOD_NOT_ALLOWED.into_response(),
    }
}

fn gena_ok(sid: &str, timeout: u64) -> Response {
    (
        StatusCode::OK,
        [
            ("SID", sid.to_string()),
            ("TIMEOUT", format!("Second-{timeout}")),
            ("Content-Length", "0".to_string()),
        ],
    )
        .into_response()
}

#[cfg(test)]
mod tests;
