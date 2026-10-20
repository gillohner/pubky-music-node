//! UPnP AV / OpenHome renderers as outputs (handoff M6): the node tells the
//! renderer to fetch a pull stream (`pm_zones::outputs::PullOutput`), so the
//! renderer receives exactly the zone's post-DSP PCM as WAV and paces it by
//! fetching. Control is plain UPnP: SSDP discovery, the device description,
//! `AVTransport` (SetAVTransportURI/Play/Stop/Pause) and `RenderingControl`
//! (volume). OpenHome devices (Linn) expose these UPnP services too; their
//! richer OpenHome services (Playlist, Volume, Product) come later.
//!
//! Blocking HTTP is used on purpose: `Output::open` runs on the zone's audio
//! thread, and the few control calls per track do not justify a runtime.

pub mod output;
pub mod soap;

use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

pub use output::UpnpOutput;

pub const MEDIA_RENDERER: &str = "urn:schemas-upnp-org:device:MediaRenderer:1";
pub const AV_TRANSPORT: &str = "urn:schemas-upnp-org:service:AVTransport:1";
pub const RENDERING_CONTROL: &str = "urn:schemas-upnp-org:service:RenderingControl:1";

#[derive(Debug, thiserror::Error)]
pub enum UpnpError {
    #[error("network: {0}")]
    Io(#[from] std::io::Error),
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),
    #[error("description at {url}: {reason}")]
    Description { url: String, reason: String },
    #[error("SOAP {action} failed: {detail}")]
    Soap { action: String, detail: String },
    #[error("{0}")]
    Other(String),
}

/// A renderer as found on the network.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Renderer {
    pub udn: String,
    pub friendly_name: String,
    pub manufacturer: Option<String>,
    pub model: Option<String>,
    /// Absolute control URLs.
    pub av_transport: String,
    pub rendering_control: Option<String>,
    pub location: String,
}

/// SSDP M-SEARCH for media renderers; collects unique LOCATIONs within
/// `timeout`, then fetches and parses each description.
pub fn discover(timeout: Duration) -> Result<Vec<Renderer>, UpnpError> {
    let locations = ssdp_search(timeout)?;
    let mut out = Vec::new();
    for loc in locations {
        match describe(&loc) {
            Ok(Some(r)) => out.push(r),
            Ok(None) => {}
            Err(e) => tracing::debug!(%loc, "skipping renderer: {e}"),
        }
    }
    Ok(out)
}

/// Raw SSDP: the LOCATION headers of every MediaRenderer that answers.
pub fn ssdp_search(timeout: Duration) -> Result<Vec<String>, UpnpError> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
    socket.set_read_timeout(Some(Duration::from_millis(300)))?;
    let mx = timeout.as_secs().clamp(1, 5);
    let msg = format!(
        "M-SEARCH * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\nMAN: \"ssdp:discover\"\r\nMX: {mx}\r\nST: {MEDIA_RENDERER}\r\n\r\n"
    );
    let dest: SocketAddr = "239.255.255.250:1900".parse().expect("ssdp address");
    for _ in 0..2 {
        socket.send_to(msg.as_bytes(), dest)?;
    }
    let deadline = Instant::now() + timeout;
    let mut found: Vec<String> = Vec::new();
    let mut buf = [0u8; 4096];
    while Instant::now() < deadline {
        match socket.recv_from(&mut buf) {
            Ok((n, _)) => {
                let text = String::from_utf8_lossy(&buf[..n]);
                if let Some(loc) = header(&text, "LOCATION") {
                    if !found.iter().any(|l| l.eq_ignore_ascii_case(&loc)) {
                        found.push(loc);
                    }
                }
            }
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(found)
}

fn header(resp: &str, name: &str) -> Option<String> {
    resp.lines().find_map(|l| {
        let (k, v) = l.split_once(':')?;
        k.trim().eq_ignore_ascii_case(name).then(|| v.trim().to_string())
    })
}

/// Fetch and parse a device description; `None` when it is not a renderer
/// with an AVTransport service.
pub fn describe(location: &str) -> Result<Option<Renderer>, UpnpError> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()?;
    let xml = client.get(location).send()?.error_for_status()?.text()?;
    parse_description(location, &xml)
}

/// Parse a device description XML (any device nesting) into a [`Renderer`].
pub fn parse_description(location: &str, xml: &str) -> Result<Option<Renderer>, UpnpError> {
    use quick_xml::events::Event;
    use quick_xml::Reader;
    let base = url::Url::parse(location).map_err(|e| UpnpError::Description {
        url: location.into(),
        reason: e.to_string(),
    })?;
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut path: Vec<String> = Vec::new();
    let mut udn = None;
    let mut friendly = None;
    let mut manufacturer = None;
    let mut model = None;
    let mut device_type = None;
    let mut in_service: Option<(Option<String>, Option<String>)> = None; // (serviceType, controlURL)
    let mut av = None;
    let mut rc = None;
    let mut url_base: Option<String> = None;
    let mut text_target: Option<String> = None;
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                let name = e.local_name().as_ref().to_string();
                path.push(name.clone());
                if name == "service" {
                    in_service = Some((None, None));
                }
                text_target = Some(name);
            }
            Ok(Event::Text(t)) => {
                let Some(tag) = text_target.as_deref() else {
                    continue;
                };
                let v = t.xml_content(quick_xml::XmlVersion::Implicit1_0).into_owned();
                let v = v.trim().to_string();
                if v.is_empty() {
                    continue;
                }
                // Only the first (root) device's identity matters; nested devices keep their own.
                let depth_device = path.iter().filter(|p| *p == "device").count();
                match tag {
                    "URLBase" => url_base = Some(v),
                    "UDN" if depth_device == 1 && udn.is_none() => udn = Some(v),
                    "friendlyName" if depth_device == 1 && friendly.is_none() => friendly = Some(v),
                    "manufacturer" if depth_device == 1 && manufacturer.is_none() => manufacturer = Some(v),
                    "modelName" if depth_device == 1 && model.is_none() => model = Some(v),
                    "deviceType" if depth_device == 1 && device_type.is_none() => device_type = Some(v),
                    "serviceType" => {
                        if let Some(s) = in_service.as_mut() {
                            s.0 = Some(v);
                        }
                    }
                    "controlURL" => {
                        if let Some(s) = in_service.as_mut() {
                            s.1 = Some(v);
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::End(e)) => {
                let name = e.local_name().as_ref().to_string();
                if name == "service" {
                    if let Some((Some(t), Some(c))) = in_service.take() {
                        if t.starts_with("urn:schemas-upnp-org:service:AVTransport:") && av.is_none() {
                            av = Some(c);
                        } else if t.starts_with("urn:schemas-upnp-org:service:RenderingControl:")
                            && rc.is_none()
                        {
                            rc = Some(c);
                        }
                    }
                }
                path.pop();
                text_target = None;
            }
            Ok(Event::Eof) => break,
            Err(e) => {
                return Err(UpnpError::Description {
                    url: location.into(),
                    reason: e.to_string(),
                })
            }
            _ => {}
        }
    }
    let Some(av) = av else {
        return Ok(None);
    };
    let base = match url_base.as_deref().and_then(|b| url::Url::parse(b).ok()) {
        Some(b) => b,
        None => base,
    };
    let abs = |u: &str| {
        base.join(u)
            .map(|x| x.to_string())
            .unwrap_or_else(|_| u.to_string())
    };
    Ok(Some(Renderer {
        udn: udn.unwrap_or_else(|| location.to_string()),
        friendly_name: friendly.unwrap_or_else(|| "UPnP renderer".into()),
        manufacturer,
        model,
        av_transport: abs(&av),
        rendering_control: rc.map(|c| abs(&c)),
        location: location.to_string(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    const DESC: &str = r#"<?xml version="1.0"?>
<root xmlns="urn:schemas-upnp-org:device-1-0">
  <specVersion><major>1</major><minor>0</minor></specVersion>
  <device>
    <deviceType>urn:schemas-upnp-org:device:MediaRenderer:1</deviceType>
    <friendlyName>Living Room DSM</friendlyName>
    <manufacturer>Linn Products Ltd.</manufacturer>
    <modelName>Klimax DSM</modelName>
    <UDN>uuid:4c494e4e-0026-0f21-cc3f-01234567890a</UDN>
    <serviceList>
      <service>
        <serviceType>urn:schemas-upnp-org:service:RenderingControl:1</serviceType>
        <serviceId>urn:upnp-org:serviceId:RenderingControl</serviceId>
        <controlURL>/RenderingControl/control</controlURL>
      </service>
      <service>
        <serviceType>urn:schemas-upnp-org:service:AVTransport:1</serviceType>
        <serviceId>urn:upnp-org:serviceId:AVTransport</serviceId>
        <controlURL>AVTransport/control</controlURL>
      </service>
    </serviceList>
    <deviceList>
      <device>
        <deviceType>urn:schemas-upnp-org:device:Other:1</deviceType>
        <friendlyName>ignored sub device</friendlyName>
        <UDN>uuid:sub</UDN>
      </device>
    </deviceList>
  </device>
</root>"#;

    #[test]
    fn parses_a_linn_style_description() {
        let r = parse_description("http://192.168.1.20:55178/Ds/device.xml", DESC)
            .unwrap()
            .unwrap();
        assert_eq!(r.friendly_name, "Living Room DSM");
        assert_eq!(r.udn, "uuid:4c494e4e-0026-0f21-cc3f-01234567890a");
        assert_eq!(r.manufacturer.as_deref(), Some("Linn Products Ltd."));
        assert_eq!(r.av_transport, "http://192.168.1.20:55178/Ds/AVTransport/control");
        assert_eq!(
            r.rendering_control.as_deref(),
            Some("http://192.168.1.20:55178/RenderingControl/control")
        );
    }

    #[test]
    fn non_renderers_and_bad_xml() {
        let no_av = DESC.replace(
            "urn:schemas-upnp-org:service:AVTransport:1",
            "urn:schemas-upnp-org:service:Nothing:1",
        );
        assert!(parse_description("http://h/d.xml", &no_av).unwrap().is_none());
        assert!(
            parse_description("http://h/d.xml", "<root><device>").is_err()
                || parse_description("http://h/d.xml", "<root><device>")
                    .unwrap()
                    .is_none()
        );
        assert_eq!(
            header(
                "HTTP/1.1 200 OK\r\nLocation: http://x/d.xml\r\nST: a\r\n",
                "LOCATION"
            )
            .as_deref(),
            Some("http://x/d.xml")
        );
    }
}
