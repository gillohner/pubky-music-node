//! The XML a MediaRenderer speaks: its device and service descriptions, SOAP
//! requests and answers, DIDL-Lite metadata and `LastChange` events.

use crate::{Meta, Transport, View};
use quick_xml::events::Event;
use quick_xml::Reader;

pub const DEVICE_TYPE: &str = "urn:schemas-upnp-org:device:MediaRenderer:1";
pub const AVT: &str = "urn:schemas-upnp-org:service:AVTransport:1";
pub const RC: &str = "urn:schemas-upnp-org:service:RenderingControl:1";
pub const CM: &str = "urn:schemas-upnp-org:service:ConnectionManager:1";

/// What the zones play: the decoder takes these over HTTP (Symphonia).
pub const SINK_PROTOCOLS: &[&str] = &[
    "http-get:*:audio/flac:*",
    "http-get:*:audio/x-flac:*",
    "http-get:*:audio/wav:*",
    "http-get:*:audio/x-wav:*",
    "http-get:*:audio/L16;rate=44100;channels=2:*",
    "http-get:*:audio/L16;rate=48000;channels=2:*",
    "http-get:*:audio/mpeg:*",
    "http-get:*:audio/mp4:*",
    "http-get:*:audio/x-m4a:*",
    "http-get:*:audio/aac:*",
    "http-get:*:audio/ogg:*",
    "http-get:*:audio/aiff:*",
    "http-get:*:audio/x-aiff:*",
];

pub fn esc(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            '\'' => o.push_str("&apos;"),
            c => o.push(c),
        }
    }
    o
}

/// `H:MM:SS` (UPnP time), from milliseconds.
pub fn hms(ms: u64) -> String {
    let s = ms / 1000;
    format!("{}:{:02}:{:02}", s / 3600, (s / 60) % 60, s % 60)
}

/// `H:MM:SS[.F...]` (or `MM:SS`, or seconds) to milliseconds.
pub fn parse_hms(t: &str) -> Option<u64> {
    let t = t.trim();
    let (main, frac) = match t.split_once('.') {
        Some((m, f)) => (m, f),
        None => (t, ""),
    };
    let mut secs: u64 = 0;
    for part in main.split(':') {
        secs = secs.checked_mul(60)?.checked_add(part.parse().ok()?)?;
    }
    let ms_frac = if frac.is_empty() {
        0
    } else {
        let digits: String = frac.chars().take(3).collect();
        let n: u64 = digits.parse().ok()?;
        n * 10u64.pow(3 - digits.len() as u32)
    };
    Some(secs * 1000 + ms_frac)
}

/// The device description of one zone.
pub fn device_description(udn: &str, name: &str, base: &str, version: &str) -> String {
    let svc = |ty: &str, id: &str, short: &str| {
        format!(
            "<service><serviceType>{ty}</serviceType><serviceId>urn:upnp-org:serviceId:{id}</serviceId>\
             <SCPDURL>{base}/{short}.xml</SCPDURL><controlURL>{base}/ctl/{short}</controlURL>\
             <eventSubURL>{base}/evt/{short}</eventSubURL></service>"
        )
    };
    format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\
<root xmlns=\"urn:schemas-upnp-org:device-1-0\" xmlns:dlna=\"urn:schemas-dlna-org:device-1-0\">\
<specVersion><major>1</major><minor>0</minor></specVersion>\
<device><deviceType>{DEVICE_TYPE}</deviceType><friendlyName>{name}</friendlyName>\
<manufacturer>Pubky Music</manufacturer><manufacturerURL>https://github.com/gillohner/pubky-music-node</manufacturerURL>\
<modelDescription>A zone of a Pubky Music node</modelDescription><modelName>Pubky Music zone</modelName>\
<modelNumber>{version}</modelNumber><UDN>uuid:{udn}</UDN><dlna:X_DLNADOC>DMR-1.50</dlna:X_DLNADOC>\
<serviceList>{}{}{}</serviceList></device></root>",
        svc(AVT, "AVTransport", "avt"),
        svc(RC, "RenderingControl", "rc"),
        svc(CM, "ConnectionManager", "cm"),
        name = esc(name),
    )
}

/// `(name, [(argument, direction, related state variable)])`.
type Action = (
    &'static str,
    &'static [(&'static str, &'static str, &'static str)],
);
/// `(name, data type, sends events, allowed values)`.
type Var = (&'static str, &'static str, bool, &'static [&'static str]);

const AVT_ACTIONS: &[Action] = &[
    (
        "SetAVTransportURI",
        &[
            ("InstanceID", "in", "A_ARG_TYPE_InstanceID"),
            ("CurrentURI", "in", "AVTransportURI"),
            ("CurrentURIMetaData", "in", "AVTransportURIMetaData"),
        ],
    ),
    (
        "SetNextAVTransportURI",
        &[
            ("InstanceID", "in", "A_ARG_TYPE_InstanceID"),
            ("NextURI", "in", "NextAVTransportURI"),
            ("NextURIMetaData", "in", "NextAVTransportURIMetaData"),
        ],
    ),
    (
        "GetMediaInfo",
        &[
            ("InstanceID", "in", "A_ARG_TYPE_InstanceID"),
            ("NrTracks", "out", "NumberOfTracks"),
            ("MediaDuration", "out", "CurrentMediaDuration"),
            ("CurrentURI", "out", "AVTransportURI"),
            ("CurrentURIMetaData", "out", "AVTransportURIMetaData"),
            ("NextURI", "out", "NextAVTransportURI"),
            ("NextURIMetaData", "out", "NextAVTransportURIMetaData"),
            ("PlayMedium", "out", "PlaybackStorageMedium"),
            ("RecordMedium", "out", "RecordStorageMedium"),
            ("WriteStatus", "out", "RecordMediumWriteStatus"),
        ],
    ),
    (
        "GetTransportInfo",
        &[
            ("InstanceID", "in", "A_ARG_TYPE_InstanceID"),
            ("CurrentTransportState", "out", "TransportState"),
            ("CurrentTransportStatus", "out", "TransportStatus"),
            ("CurrentSpeed", "out", "TransportPlaySpeed"),
        ],
    ),
    (
        "GetPositionInfo",
        &[
            ("InstanceID", "in", "A_ARG_TYPE_InstanceID"),
            ("Track", "out", "CurrentTrack"),
            ("TrackDuration", "out", "CurrentTrackDuration"),
            ("TrackMetaData", "out", "CurrentTrackMetaData"),
            ("TrackURI", "out", "CurrentTrackURI"),
            ("RelTime", "out", "RelativeTimePosition"),
            ("AbsTime", "out", "AbsoluteTimePosition"),
            ("RelCount", "out", "RelativeCounterPosition"),
            ("AbsCount", "out", "AbsoluteCounterPosition"),
        ],
    ),
    (
        "GetDeviceCapabilities",
        &[
            ("InstanceID", "in", "A_ARG_TYPE_InstanceID"),
            ("PlayMedia", "out", "PossiblePlaybackStorageMedia"),
            ("RecMedia", "out", "PossibleRecordStorageMedia"),
            ("RecQualityModes", "out", "PossibleRecordQualityModes"),
        ],
    ),
    (
        "GetTransportSettings",
        &[
            ("InstanceID", "in", "A_ARG_TYPE_InstanceID"),
            ("PlayMode", "out", "CurrentPlayMode"),
            ("RecQualityMode", "out", "CurrentRecordQualityMode"),
        ],
    ),
    (
        "GetCurrentTransportActions",
        &[
            ("InstanceID", "in", "A_ARG_TYPE_InstanceID"),
            ("Actions", "out", "CurrentTransportActions"),
        ],
    ),
    ("Stop", &[("InstanceID", "in", "A_ARG_TYPE_InstanceID")]),
    (
        "Play",
        &[
            ("InstanceID", "in", "A_ARG_TYPE_InstanceID"),
            ("Speed", "in", "TransportPlaySpeed"),
        ],
    ),
    ("Pause", &[("InstanceID", "in", "A_ARG_TYPE_InstanceID")]),
    (
        "Seek",
        &[
            ("InstanceID", "in", "A_ARG_TYPE_InstanceID"),
            ("Unit", "in", "A_ARG_TYPE_SeekMode"),
            ("Target", "in", "A_ARG_TYPE_SeekTarget"),
        ],
    ),
    ("Next", &[("InstanceID", "in", "A_ARG_TYPE_InstanceID")]),
    ("Previous", &[("InstanceID", "in", "A_ARG_TYPE_InstanceID")]),
];

const AVT_VARS: &[Var] = &[
    (
        "TransportState",
        "string",
        false,
        &[
            "STOPPED",
            "PLAYING",
            "PAUSED_PLAYBACK",
            "TRANSITIONING",
            "NO_MEDIA_PRESENT",
        ],
    ),
    ("TransportStatus", "string", false, &["OK", "ERROR_OCCURRED"]),
    ("TransportPlaySpeed", "string", false, &["1"]),
    ("PlaybackStorageMedium", "string", false, &["NETWORK", "NONE"]),
    ("RecordStorageMedium", "string", false, &["NOT_IMPLEMENTED"]),
    ("PossiblePlaybackStorageMedia", "string", false, &[]),
    ("PossibleRecordStorageMedia", "string", false, &[]),
    ("PossibleRecordQualityModes", "string", false, &[]),
    ("RecordMediumWriteStatus", "string", false, &["NOT_IMPLEMENTED"]),
    ("CurrentRecordQualityMode", "string", false, &["NOT_IMPLEMENTED"]),
    ("CurrentPlayMode", "string", false, &["NORMAL"]),
    ("NumberOfTracks", "ui4", false, &[]),
    ("CurrentTrack", "ui4", false, &[]),
    ("CurrentTrackDuration", "string", false, &[]),
    ("CurrentMediaDuration", "string", false, &[]),
    ("CurrentTrackMetaData", "string", false, &[]),
    ("CurrentTrackURI", "string", false, &[]),
    ("AVTransportURI", "string", false, &[]),
    ("AVTransportURIMetaData", "string", false, &[]),
    ("NextAVTransportURI", "string", false, &[]),
    ("NextAVTransportURIMetaData", "string", false, &[]),
    ("RelativeTimePosition", "string", false, &[]),
    ("AbsoluteTimePosition", "string", false, &[]),
    ("RelativeCounterPosition", "i4", false, &[]),
    ("AbsoluteCounterPosition", "i4", false, &[]),
    ("CurrentTransportActions", "string", false, &[]),
    ("LastChange", "string", true, &[]),
    (
        "A_ARG_TYPE_SeekMode",
        "string",
        false,
        &["REL_TIME", "ABS_TIME", "TRACK_NR"],
    ),
    ("A_ARG_TYPE_SeekTarget", "string", false, &[]),
    ("A_ARG_TYPE_InstanceID", "ui4", false, &[]),
];

const RC_ACTIONS: &[Action] = &[
    (
        "ListPresets",
        &[
            ("InstanceID", "in", "A_ARG_TYPE_InstanceID"),
            ("CurrentPresetNameList", "out", "PresetNameList"),
        ],
    ),
    (
        "SelectPreset",
        &[
            ("InstanceID", "in", "A_ARG_TYPE_InstanceID"),
            ("PresetName", "in", "A_ARG_TYPE_PresetName"),
        ],
    ),
    (
        "GetVolume",
        &[
            ("InstanceID", "in", "A_ARG_TYPE_InstanceID"),
            ("Channel", "in", "A_ARG_TYPE_Channel"),
            ("CurrentVolume", "out", "Volume"),
        ],
    ),
    (
        "SetVolume",
        &[
            ("InstanceID", "in", "A_ARG_TYPE_InstanceID"),
            ("Channel", "in", "A_ARG_TYPE_Channel"),
            ("DesiredVolume", "in", "Volume"),
        ],
    ),
    (
        "GetMute",
        &[
            ("InstanceID", "in", "A_ARG_TYPE_InstanceID"),
            ("Channel", "in", "A_ARG_TYPE_Channel"),
            ("CurrentMute", "out", "Mute"),
        ],
    ),
    (
        "SetMute",
        &[
            ("InstanceID", "in", "A_ARG_TYPE_InstanceID"),
            ("Channel", "in", "A_ARG_TYPE_Channel"),
            ("DesiredMute", "in", "Mute"),
        ],
    ),
];

const RC_VARS: &[Var] = &[
    ("PresetNameList", "string", false, &[]),
    ("Volume", "ui2", false, &[]),
    ("Mute", "boolean", false, &[]),
    ("LastChange", "string", true, &[]),
    ("A_ARG_TYPE_Channel", "string", false, &["Master"]),
    ("A_ARG_TYPE_InstanceID", "ui4", false, &[]),
    ("A_ARG_TYPE_PresetName", "string", false, &["FactoryDefaults"]),
];

const CM_ACTIONS: &[Action] = &[
    (
        "GetProtocolInfo",
        &[
            ("Source", "out", "SourceProtocolInfo"),
            ("Sink", "out", "SinkProtocolInfo"),
        ],
    ),
    (
        "GetCurrentConnectionIDs",
        &[("ConnectionIDs", "out", "CurrentConnectionIDs")],
    ),
    (
        "GetCurrentConnectionInfo",
        &[
            ("ConnectionID", "in", "A_ARG_TYPE_ConnectionID"),
            ("RcsID", "out", "A_ARG_TYPE_RcsID"),
            ("AVTransportID", "out", "A_ARG_TYPE_AVTransportID"),
            ("ProtocolInfo", "out", "A_ARG_TYPE_ProtocolInfo"),
            ("PeerConnectionManager", "out", "A_ARG_TYPE_ConnectionManager"),
            ("PeerConnectionID", "out", "A_ARG_TYPE_ConnectionID"),
            ("Direction", "out", "A_ARG_TYPE_Direction"),
            ("Status", "out", "A_ARG_TYPE_ConnectionStatus"),
        ],
    ),
];

const CM_VARS: &[Var] = &[
    ("SourceProtocolInfo", "string", true, &[]),
    ("SinkProtocolInfo", "string", true, &[]),
    ("CurrentConnectionIDs", "string", true, &[]),
    (
        "A_ARG_TYPE_ConnectionStatus",
        "string",
        false,
        &[
            "OK",
            "ContentFormatMismatch",
            "InsufficientBandwidth",
            "UnreliableChannel",
            "Unknown",
        ],
    ),
    ("A_ARG_TYPE_ConnectionManager", "string", false, &[]),
    ("A_ARG_TYPE_Direction", "string", false, &["Input", "Output"]),
    ("A_ARG_TYPE_ProtocolInfo", "string", false, &[]),
    ("A_ARG_TYPE_ConnectionID", "i4", false, &[]),
    ("A_ARG_TYPE_AVTransportID", "i4", false, &[]),
    ("A_ARG_TYPE_RcsID", "i4", false, &[]),
];

fn scpd(actions: &[Action], vars: &[Var], keep: impl Fn(&str) -> bool) -> String {
    let mut o = String::from(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?><scpd xmlns=\"urn:schemas-upnp-org:service-1-0\">\
         <specVersion><major>1</major><minor>0</minor></specVersion><actionList>",
    );
    for (name, args) in actions.iter().filter(|(n, _)| keep(n)) {
        o.push_str(&format!("<action><name>{name}</name><argumentList>"));
        for (arg, dir, var) in *args {
            o.push_str(&format!(
                "<argument><name>{arg}</name><direction>{dir}</direction><relatedStateVariable>{var}</relatedStateVariable></argument>"
            ));
        }
        o.push_str("</argumentList></action>");
    }
    o.push_str("</actionList><serviceStateTable>");
    for (name, ty, events, allowed) in vars.iter().filter(|(n, ..)| keep(n)) {
        o.push_str(&format!(
            "<stateVariable sendEvents=\"{}\"><name>{name}</name><dataType>{ty}</dataType>",
            if *events { "yes" } else { "no" }
        ));
        if *name == "Volume" {
            o.push_str("<allowedValueRange><minimum>0</minimum><maximum>100</maximum><step>1</step></allowedValueRange>");
        }
        if !allowed.is_empty() {
            o.push_str("<allowedValueList>");
            for v in *allowed {
                o.push_str(&format!("<allowedValue>{v}</allowedValue>"));
            }
            o.push_str("</allowedValueList>");
        }
        o.push_str("</stateVariable>");
    }
    o.push_str("</serviceStateTable></scpd>");
    o
}

/// The service description (`avt`, `rc`, `cm`). Without `volume` (an output with no hardware
/// volume), RenderingControl has no Volume or Mute, so controllers offer no slider that fails.
pub fn service_description(short: &str, volume: bool) -> Option<String> {
    let all = |_: &str| true;
    match short {
        "avt" => Some(scpd(AVT_ACTIONS, AVT_VARS, all)),
        "rc" => Some(scpd(RC_ACTIONS, RC_VARS, |n| {
            volume || !(n.contains("Volume") || n.contains("Mute"))
        })),
        "cm" => Some(scpd(CM_ACTIONS, CM_VARS, all)),
        _ => None,
    }
}

/// A SOAP action call: its name and the arguments by name (text, unescaped).
#[derive(Debug, Default, PartialEq)]
pub struct Call {
    pub action: String,
    pub args: Vec<(String, String)>,
}

impl Call {
    pub fn arg(&self, name: &str) -> &str {
        self.args
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
            .unwrap_or("")
    }
}

/// Parse a SOAP request body: the element under `Body` is the action, its children the
/// arguments.
pub fn parse_call(body: &str) -> Option<Call> {
    let mut r = Reader::from_str(body);
    let mut depth_body: Option<usize> = None;
    let mut depth = 0usize;
    let mut call = Call::default();
    let mut current: Option<String> = None;
    let mut text = String::new();
    loop {
        match r.read_event().ok()? {
            Event::Start(e) => {
                depth += 1;
                let local = e.local_name().as_ref().to_string();
                match depth_body {
                    None if local == "Body" => depth_body = Some(depth),
                    Some(b) if depth == b + 1 => call.action = local,
                    Some(b) if depth == b + 2 => {
                        current = Some(local);
                        text.clear();
                    }
                    _ => {}
                }
            }
            Event::Empty(e) => {
                if let Some(b) = depth_body {
                    let local = e.local_name().as_ref().to_string();
                    if depth == b {
                        call.action = local;
                    } else if depth == b + 1 {
                        call.args.push((local, String::new()));
                    }
                }
            }
            Event::Text(t) => {
                if current.is_some() {
                    text.push_str(&t.xml_content(quick_xml::XmlVersion::Implicit1_0));
                }
            }
            Event::GeneralRef(g) => {
                if current.is_some() {
                    let name = g.as_ref().to_string();
                    text.push_str(&unref(&name));
                }
            }
            Event::CData(c) => {
                if current.is_some() {
                    text.push_str(c.as_ref());
                }
            }
            Event::End(_) => {
                if let (Some(b), Some(name)) = (depth_body, current.as_ref()) {
                    if depth == b + 2 {
                        call.args.push((name.clone(), std::mem::take(&mut text)));
                        current = None;
                    }
                }
                depth = depth.saturating_sub(1);
            }
            Event::Eof => break,
            _ => {}
        }
    }
    (!call.action.is_empty()).then_some(call)
}

/// `&amp;` and friends, and numeric references, as text.
fn unref(name: &str) -> String {
    match name {
        "amp" => "&".into(),
        "lt" => "<".into(),
        "gt" => ">".into(),
        "quot" => "\"".into(),
        "apos" => "'".into(),
        n if n.starts_with("#x") => u32::from_str_radix(&n[2..], 16)
            .ok()
            .and_then(char::from_u32)
            .map(String::from)
            .unwrap_or_default(),
        n if n.starts_with('#') => n[1..]
            .parse::<u32>()
            .ok()
            .and_then(char::from_u32)
            .map(String::from)
            .unwrap_or_default(),
        _ => String::new(),
    }
}

/// A SOAP answer for `action` of `service` with `out` arguments (values are escaped here).
pub fn response(service: &str, action: &str, out: &[(&str, String)]) -> String {
    let mut args = String::new();
    for (k, v) in out {
        args.push_str(&format!("<{k}>{}</{k}>", esc(v)));
    }
    format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?><s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" \
         s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\"><s:Body>\
         <u:{action}Response xmlns:u=\"{service}\">{args}</u:{action}Response></s:Body></s:Envelope>"
    )
}

/// A UPnP error (SOAP fault).
pub fn fault(code: u16, description: &str) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?><s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" \
         s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\"><s:Body><s:Fault>\
         <faultcode>s:Client</faultcode><faultstring>UPnPError</faultstring><detail>\
         <UPnPError xmlns=\"urn:schemas-upnp-org:control-1-0\"><errorCode>{code}</errorCode>\
         <errorDescription>{}</errorDescription></UPnPError></detail></s:Fault></s:Body></s:Envelope>",
        esc(description)
    )
}

/// Title, artist, album, art and duration from DIDL-Lite (what controllers send with a URI).
pub fn parse_didl(didl: &str) -> Meta {
    let mut m = Meta::default();
    if didl.trim().is_empty() {
        return m;
    }
    let mut r = Reader::from_str(didl);
    let mut current: Option<String> = None;
    let mut text = String::new();
    while let Ok(ev) = r.read_event() {
        match ev {
            Event::Start(e) => {
                let local = e.local_name().as_ref().to_string();
                if local == "res" && m.duration_ms.is_none() {
                    for a in e.attributes().flatten() {
                        if a.key.local_name().as_ref() == "duration" {
                            m.duration_ms = a
                                .normalized_value(quick_xml::XmlVersion::Implicit1_0)
                                .ok()
                                .and_then(|v| parse_hms(&v));
                        }
                    }
                }
                current = Some(local);
                text.clear();
            }
            Event::Text(t) => {
                text.push_str(&t.xml_content(quick_xml::XmlVersion::Implicit1_0));
            }
            Event::GeneralRef(g) => text.push_str(&unref(g.as_ref())),
            Event::End(_) => {
                let v = std::mem::take(&mut text).trim().to_string();
                match current.take().as_deref() {
                    Some("title") if m.title.is_none() && !v.is_empty() => m.title = Some(v),
                    Some("creator" | "artist") if m.artist.is_none() && !v.is_empty() => m.artist = Some(v),
                    Some("album") if m.album.is_none() && !v.is_empty() => m.album = Some(v),
                    Some("albumArtURI") if m.art.is_none() && !v.is_empty() => m.art = Some(v),
                    _ => {}
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    m
}

/// DIDL-Lite for what a zone plays (controllers show it).
pub fn didl(uri: &str, m: &Meta) -> String {
    if uri.is_empty() && m.title.is_none() {
        return String::new();
    }
    let opt = |tag: &str, v: &Option<String>| {
        v.as_ref()
            .map(|v| format!("<{tag}>{}</{tag}>", esc(v)))
            .unwrap_or_default()
    };
    let duration = m
        .duration_ms
        .map(|d| format!(" duration=\"{}.000\"", hms(d)))
        .unwrap_or_default();
    format!(
        "<DIDL-Lite xmlns=\"urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/\" xmlns:dc=\"http://purl.org/dc/elements/1.1/\" \
         xmlns:upnp=\"urn:schemas-upnp-org:metadata-1-0/upnp/\"><item id=\"0\" parentID=\"-1\" restricted=\"1\">\
         <dc:title>{}</dc:title>{}{}{}<upnp:class>object.item.audioItem.musicTrack</upnp:class>\
         <res protocolInfo=\"http-get:*:audio/*:*\"{duration}>{}</res></item></DIDL-Lite>",
        esc(m.title.as_deref().unwrap_or("")),
        opt("dc:creator", &m.artist),
        opt("upnp:album", &m.album),
        opt("upnp:albumArtURI", &m.art),
        esc(uri),
    )
}

pub fn transport_state(t: Transport) -> &'static str {
    match t {
        Transport::Playing => "PLAYING",
        Transport::Paused => "PAUSED_PLAYBACK",
        Transport::Stopped => "STOPPED",
        Transport::Transitioning => "TRANSITIONING",
        Transport::NoMedia => "NO_MEDIA_PRESENT",
    }
}

/// The `LastChange` event of a service for a zone's state (`avt` or `rc`).
pub fn last_change(service: &str, v: &View) -> String {
    let mut inner = String::new();
    let mut var = |name: &str, val: &str| inner.push_str(&format!("<{name} val=\"{}\"/>", esc(val)));
    let ns = match service {
        "avt" => {
            var("TransportState", transport_state(v.state));
            var("TransportStatus", "OK");
            var("TransportPlaySpeed", "1");
            var("CurrentPlayMode", "NORMAL");
            var("NumberOfTracks", if v.uri.is_empty() { "0" } else { "1" });
            var("CurrentTrack", if v.uri.is_empty() { "0" } else { "1" });
            var("AVTransportURI", &v.uri);
            var("AVTransportURIMetaData", &didl(&v.uri, &v.meta));
            var("CurrentTrackURI", &v.uri);
            var("CurrentTrackMetaData", &didl(&v.uri, &v.meta));
            var("CurrentTrackDuration", &hms(v.meta.duration_ms.unwrap_or(0)));
            var("CurrentMediaDuration", &hms(v.meta.duration_ms.unwrap_or(0)));
            var("NextAVTransportURI", &v.next_uri);
            var("CurrentTransportActions", "Play,Pause,Stop,Seek,Next,Previous");
            "urn:schemas-upnp-org:metadata-1-0/AVT/"
        }
        _ => {
            inner.push_str(&format!("<Volume channel=\"Master\" val=\"{}\"/>", v.volume));
            inner.push_str(&format!(
                "<Mute channel=\"Master\" val=\"{}\"/>",
                u8::from(v.muted)
            ));
            "urn:schemas-upnp-org:metadata-1-0/RCS/"
        }
    };
    format!("<Event xmlns=\"{ns}\"><InstanceID val=\"0\">{inner}</InstanceID></Event>")
}

/// A GENA property set: `LastChange` for AVTransport and RenderingControl, the protocol
/// lists for ConnectionManager.
pub fn propertyset(service: &str, v: &View) -> String {
    let props = match service {
        "cm" => format!(
            "<e:property><SourceProtocolInfo></SourceProtocolInfo></e:property>\
             <e:property><SinkProtocolInfo>{}</SinkProtocolInfo></e:property>\
             <e:property><CurrentConnectionIDs>0</CurrentConnectionIDs></e:property>",
            esc(&SINK_PROTOCOLS.join(","))
        ),
        s => format!(
            "<e:property><LastChange>{}</LastChange></e:property>",
            esc(&last_change(s, v))
        ),
    };
    format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?><e:propertyset xmlns:e=\"urn:schemas-upnp-org:event-1-0\">{props}</e:propertyset>"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times() {
        assert_eq!(hms(3_725_000), "1:02:05");
        assert_eq!(parse_hms("1:02:05"), Some(3_725_000));
        assert_eq!(parse_hms("0:00:07.5"), Some(7_500));
        assert_eq!(parse_hms("02:05"), Some(125_000));
        assert_eq!(parse_hms("x"), None);
    }

    #[test]
    fn soap_calls_with_escaped_metadata() {
        let body = r#"<?xml version="1.0"?><s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body>
<u:SetAVTransportURI xmlns:u="urn:schemas-upnp-org:service:AVTransport:1"><InstanceID>0</InstanceID>
<CurrentURI>http://ma:8097/flow/abc.flac?a=1&amp;b=2</CurrentURI>
<CurrentURIMetaData>&lt;DIDL-Lite xmlns:dc="http://purl.org/dc/elements/1.1/"&gt;&lt;item&gt;&lt;dc:title&gt;Get Lucky&lt;/dc:title&gt;&lt;res duration="0:04:08"&gt;x&lt;/res&gt;&lt;/item&gt;&lt;/DIDL-Lite&gt;</CurrentURIMetaData>
</u:SetAVTransportURI></s:Body></s:Envelope>"#;
        let c = parse_call(body).unwrap();
        assert_eq!(c.action, "SetAVTransportURI");
        assert_eq!(c.arg("CurrentURI"), "http://ma:8097/flow/abc.flac?a=1&b=2");
        let m = parse_didl(c.arg("CurrentURIMetaData"));
        assert_eq!(m.title.as_deref(), Some("Get Lucky"));
        assert_eq!(m.duration_ms, Some(248_000));
        let empty = r#"<s:Envelope xmlns:s="x"><s:Body><u:Play xmlns:u="y"><InstanceID>0</InstanceID><Speed>1</Speed></u:Play></s:Body></s:Envelope>"#;
        let c = parse_call(empty).unwrap();
        assert_eq!((c.action.as_str(), c.arg("Speed")), ("Play", "1"));
    }

    #[test]
    fn didl_round_trip() {
        let m = Meta {
            title: Some("A & B".into()),
            artist: Some("C".into()),
            album: None,
            art: Some("http://x/a.jpg".into()),
            duration_ms: Some(61_000),
        };
        let back = parse_didl(&didl("http://x/s.flac", &m));
        assert_eq!(back, m);
    }

    #[test]
    fn descriptions_are_wellformed() {
        assert!(service_description("rc", true)
            .unwrap()
            .contains("<name>SetVolume</name>"));
        let quiet = service_description("rc", false).unwrap();
        assert!(!quiet.contains("Volume") && !quiet.contains("Mute"), "{quiet}");
        for s in ["avt", "rc", "cm"] {
            let x = service_description(s, true).unwrap();
            let mut r = Reader::from_str(&x);
            while !matches!(r.read_event().unwrap(), Event::Eof) {}
        }
        let d = device_description("abc", "Kitchen & Co", "/upnp/zone/k", "0.1.0");
        assert!(d.contains("<friendlyName>Kitchen &amp; Co</friendlyName>"));
        assert!(d.contains("<controlURL>/upnp/zone/k/ctl/avt</controlURL>"));
    }
}
