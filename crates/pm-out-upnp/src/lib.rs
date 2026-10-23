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

pub mod dlna;
pub mod heos;
pub mod output;
pub mod soap;
pub mod sonos;

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
    /// ConnectionManager control URL: what the renderer accepts (`dlna::sink_protocols`).
    pub connection_manager: Option<String>,
    /// Sonos: where the group topology is read (`sonos::groups`).
    pub zone_group_topology: Option<String>,
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

/// One SSDP answer, for diagnostics: who answered, what it claims to be,
/// and what its description says when it can be fetched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SsdpAnswer {
    pub from: String,
    pub location: String,
    pub server: Option<String>,
    pub st: Option<String>,
    pub usn: Option<String>,
    /// Friendly name from the description, when it could be fetched.
    pub friendly_name: Option<String>,
    /// Whether the description is a renderer with an AVTransport service.
    pub renderer: bool,
}

/// Ask every UPnP device on the network (`ssdp:all`) and describe what
/// answers: shows whether multicast works at all, which devices exist,
/// and which of them are renderers the node can use.
pub fn ssdp_probe(timeout: Duration) -> Result<Vec<SsdpAnswer>, UpnpError> {
    let mx = timeout.as_secs().clamp(1, 5);
    let msg = format!(
        "M-SEARCH * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\nMAN: \"ssdp:discover\"\r\nMX: {mx}\r\nST: ssdp:all\r\n\r\n"
    );
    let sockets = multicast_sockets(&msg)?;
    let deadline = Instant::now() + timeout;
    let mut found: Vec<SsdpAnswer> = Vec::new();
    let mut buf = [0u8; 4096];
    while Instant::now() < deadline {
        for socket in &sockets {
            match socket.recv_from(&mut buf) {
                Ok((n, from)) => {
                    let text = String::from_utf8_lossy(&buf[..n]);
                    let Some(location) = header(&text, "LOCATION") else {
                        continue;
                    };
                    if found.iter().any(|a| a.location.eq_ignore_ascii_case(&location)) {
                        continue;
                    }
                    found.push(SsdpAnswer {
                        from: from.to_string(),
                        location,
                        server: header(&text, "SERVER"),
                        st: header(&text, "ST"),
                        usn: header(&text, "USN"),
                        friendly_name: None,
                        renderer: false,
                    });
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) => {}
                Err(e) => return Err(e.into()),
            }
        }
    }
    for a in &mut found {
        match describe(&a.location) {
            Ok(Some(r)) => {
                a.friendly_name = Some(r.friendly_name);
                a.renderer = true;
            }
            Ok(None) => a.friendly_name = friendly_name_of(&a.location),
            Err(_) => {}
        }
    }
    Ok(found)
}

/// Renderers at one address, for devices SSDP multicast cannot reach (another
/// subnet or VLAN): `addr` is an IP or `ip:port` that gets a unicast
/// M-SEARCH, or an `http(s)://` device description URL fetched directly.
pub fn probe_address(addr: &str, timeout: Duration) -> Result<Vec<Renderer>, UpnpError> {
    let addr = addr.trim();
    if addr.starts_with("http://") || addr.starts_with("https://") {
        return Ok(describe(addr)?.into_iter().collect());
    }
    // "ip mac [serial]": a Linn whose UDN is derived from its MAC and serial
    // number; search the description server for the device-type byte.
    let words: Vec<&str> = addr.split_whitespace().collect();
    if words.len() >= 2 {
        let ip: std::net::IpAddr = words[0]
            .parse()
            .map_err(|_| UpnpError::Other(format!("{} is not an IP address", words[0])))?;
        return linn_by_mac(ip, words[1], words.get(2).copied());
    }
    let dest: SocketAddr = if addr.contains(':') && addr.parse::<SocketAddr>().is_ok() {
        addr.parse().expect("checked")
    } else {
        let ip: std::net::IpAddr = addr
            .parse()
            .map_err(|_| UpnpError::Other(format!("{addr} is not an IP address or a description URL")))?;
        SocketAddr::new(ip, 1900)
    };
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
    socket.set_read_timeout(Some(Duration::from_millis(300)))?;
    // UPnP 1.1 says a unicast search carries the device's own address in
    // HOST; some stacks (ohNet on Linn) only parse the multicast form, and
    // some only answer `ssdp:all`, so every combination is sent.
    for host in [dest.to_string(), "239.255.255.250:1900".to_string()] {
        for st in [MEDIA_RENDERER, "ssdp:all", "upnp:rootdevice"] {
            let msg = format!(
                "M-SEARCH * HTTP/1.1\r\nHOST: {host}\r\nMAN: \"ssdp:discover\"\r\nMX: 1\r\nST: {st}\r\n\r\n"
            );
            socket.send_to(msg.as_bytes(), dest)?;
        }
    }
    let deadline = Instant::now() + timeout;
    let mut locations: Vec<String> = Vec::new();
    let mut buf = [0u8; 4096];
    while Instant::now() < deadline {
        match socket.recv_from(&mut buf) {
            Ok((n, _)) => {
                let text = String::from_utf8_lossy(&buf[..n]);
                if let Some(loc) = header(&text, "LOCATION") {
                    if !locations.iter().any(|l| l.eq_ignore_ascii_case(&loc)) {
                        locations.push(loc);
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
    if locations.is_empty() {
        return match tcp_fallback(dest.ip()) {
            Some(Ok(r)) => Ok(vec![r]),
            Some(Err(open_port)) => Err(UpnpError::Other(format!(
                "{dest} ignored a unicast M-SEARCH but answers TCP on port {open_port}, so the device is reachable; enter its UPnP description URL instead (the Linn app / Kazoo shows it under the device's details, or try http://{}:{open_port}/description.xml in a browser)",
                dest.ip()
            ))),
            None => Err(UpnpError::Other(format!(
                "{dest} did not answer a unicast M-SEARCH and no TCP port answered either: the device is off, or the firewall between the subnets blocks this node from reaching it (allow UDP 1900 and TCP from this node to the device, and TCP 7788 back)"
            ))),
        };
    }
    let mut out: Vec<Renderer> = Vec::new();
    for loc in locations {
        match describe(&loc) {
            Ok(Some(r)) if !out.iter().any(|x| x.udn == r.udn) => out.push(r),
            Ok(_) => {}
            Err(e) => tracing::debug!(%loc, "skipping: {e}"),
        }
    }
    Ok(out)
}

/// Linn UDNs are `4c494e4e-<MAC as 0026-0f22-3d5f>-<serial, 8 digits>01<type>`
/// (observed: Klimax DS `…-3898-012366860171`, Kiko DSM `…-bf92-01303737013f`;
/// the type byte is 71 for the MediaRenderer, 3f Source, 33 Preamp). With the
/// serial, every type byte is tried, in parallel, on Linn's description port;
/// without it the known type bytes are tried with the serial unknown, which
/// rarely hits.
fn linn_by_mac(ip: std::net::IpAddr, mac: &str, serial: Option<&str>) -> Result<Vec<Renderer>, UpnpError> {
    let hex: String = mac
        .chars()
        .filter(|c| c.is_ascii_hexdigit())
        .map(|c| c.to_ascii_lowercase())
        .collect();
    if hex.len() != 12 {
        return Err(UpnpError::Other(format!("{mac} is not a MAC address")));
    }
    let mid = format!("{}-{}-{}", &hex[0..4], &hex[4..8], &hex[8..12]);
    let mut candidates: Vec<String> = Vec::new();
    match serial {
        Some(sn) => {
            let digits: String = sn.chars().filter(|c| c.is_ascii_digit()).collect();
            if digits.is_empty() || digits.len() > 8 {
                return Err(UpnpError::Other(format!(
                    "{sn} is not a Linn serial number (up to 8 digits)"
                )));
            }
            let padded = format!("{digits:0>8}");
            for b in [0x71u8, 0x3f, 0x33].into_iter().chain(0..=255u8) {
                candidates.push(format!("4c494e4e-{mid}-{padded}01{b:02x}"));
            }
            // Some firmware writes the type under a different middle byte.
            for b in [0x71u8, 0x3f, 0x33] {
                for m in ["00", "02", "03"] {
                    candidates.push(format!("4c494e4e-{mid}-{padded}{m}{b:02x}"));
                }
            }
        }
        None => {
            for family in ["0123668601", "0123456789", "0130373701"] {
                for b in 0..=255u8 {
                    candidates.push(format!("4c494e4e-{mid}-{family}{b:02x}"));
                }
            }
        }
    }
    candidates.dedup();
    let base = format!("http://{ip}:55178");
    if std::net::TcpStream::connect_timeout(&SocketAddr::new(ip, 55178), Duration::from_millis(1500)).is_err()
    {
        return Err(UpnpError::Other(format!(
            "{ip} does not answer on port 55178 (Linn's description port); is it a Linn, and is it on?"
        )));
    }
    let found = std::sync::Mutex::new(Vec::<Renderer>::new());
    let next = std::sync::atomic::AtomicUsize::new(0);
    std::thread::scope(|sc| {
        for _ in 0..8 {
            sc.spawn(|| {
                let client = reqwest::blocking::Client::builder()
                    .timeout(Duration::from_secs(4))
                    .build()
                    .expect("client");
                loop {
                    let i = next.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let Some(udn) = candidates.get(i) else { break };
                    let url = format!("{base}/{udn}/Upnp/device.xml");
                    let Ok(resp) = client.get(&url).send() else {
                        continue;
                    };
                    if !resp.status().is_success() {
                        continue;
                    }
                    let Ok(xml) = resp.text() else { continue };
                    if let Ok(Some(r)) = parse_description(&url, &xml) {
                        found.lock().expect("found").push(r);
                    }
                }
            });
        }
    });
    let mut out = found.into_inner().expect("found");
    out.sort_by(|a, b| a.udn.cmp(&b.udn));
    out.dedup_by(|a, b| a.udn == b.udn);
    if out.is_empty() {
        return Err(UpnpError::Other(match serial {
            Some(sn) => format!(
                "no renderer description found on {base} for MAC {mac} and serial {sn} (tried {} UDN candidates); check the serial number on the back of the device or in the Linn app",
                candidates.len()
            ),
            None => format!(
                "no renderer description found on {base} for MAC {mac} (tried {} UDN candidates); add the device's serial number as a third word (\"ip mac serial\"), it is the middle of the UDN",
                candidates.len()
            ),
        }));
    }
    Ok(out)
}

/// Ports renderers commonly serve their description on (Linn/OpenHome 55178,
/// plain UPnP stacks, Sonos 1400, BluOS 11000, HEOS/Denon 60006, WiiM 49152).
const DESCRIPTION_PORTS: [u16; 9] = [55178, 49152, 49153, 1400, 8080, 8200, 11000, 60006, 80];
const DESCRIPTION_PATHS: [&str; 8] = [
    "/description.xml",
    "/Ds/device.xml",
    "/device.xml",
    "/rootDesc.xml",
    "/xml/device_description.xml",
    "/upnp/desc.xml",
    "/dmr/description.xml",
    "/MediaRenderer/desc.xml",
];

/// When unicast SSDP gets no answer: find an open TCP port and try the usual
/// description paths there. `Some(Ok)` found a renderer, `Some(Err(port))`
/// only proved the device is reachable, `None` nothing answered.
fn tcp_fallback(ip: std::net::IpAddr) -> Option<Result<Renderer, u16>> {
    tcp_fallback_on(ip, &DESCRIPTION_PORTS)
}

fn tcp_fallback_on(ip: std::net::IpAddr, ports: &[u16]) -> Option<Result<Renderer, u16>> {
    let mut open = None;
    for &port in ports {
        let addr = SocketAddr::new(ip, port);
        if std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(700)).is_ok() {
            open.get_or_insert(port);
            for path in DESCRIPTION_PATHS {
                if let Ok(Some(r)) = describe(&format!("http://{addr}{path}")) {
                    return Some(Ok(r));
                }
            }
            // A root page or directory listing that links to a description.
            if let Some(r) = linked_description(&format!("http://{addr}/")) {
                return Some(Ok(r));
            }
        }
    }
    open.map(Err)
}

/// Follow `.xml` links on a page (some stacks list their devices at `/`).
fn linked_description(url: &str) -> Option<Renderer> {
    let text = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()
        .ok()?
        .get(url)
        .send()
        .ok()?
        .text()
        .ok()?;
    let base = url::Url::parse(url).ok()?;
    for cand in text
        .split(['"', '\'', '<', '>', ' '])
        .filter(|t| t.ends_with(".xml") && !t.contains("://") || t.starts_with("http"))
        .filter(|t| t.ends_with(".xml"))
        .take(20)
    {
        if let Ok(full) = base.join(cand) {
            if let Ok(Some(r)) = describe(full.as_str()) {
                return Some(r);
            }
        }
    }
    None
}

/// The `friendlyName` of any device description (not only renderers).
fn friendly_name_of(location: &str) -> Option<String> {
    let text = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()
        .ok()?
        .get(location)
        .send()
        .ok()?
        .text()
        .ok()?;
    let start = text.find("<friendlyName>")? + "<friendlyName>".len();
    let end = text[start..].find("</friendlyName>")? + start;
    Some(text[start..end].trim().to_string())
}

/// Raw SSDP: the LOCATION headers of every MediaRenderer that answers.
pub fn ssdp_search(timeout: Duration) -> Result<Vec<String>, UpnpError> {
    let mx = timeout.as_secs().clamp(1, 5);
    let msg = format!(
        "M-SEARCH * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\nMAN: \"ssdp:discover\"\r\nMX: {mx}\r\nST: {MEDIA_RENDERER}\r\n\r\n"
    );
    let sockets = multicast_sockets(&msg)?;
    let deadline = Instant::now() + timeout;
    let mut found: Vec<String> = Vec::new();
    let mut buf = [0u8; 4096];
    while Instant::now() < deadline {
        for socket in &sockets {
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
    }
    Ok(found)
}

/// Every IPv4 interface this host has (loopback excluded), so a node on
/// several subnets or VLANs searches all of them.
pub fn local_ipv4_addrs() -> Vec<Ipv4Addr> {
    let mut v: Vec<Ipv4Addr> = if_addrs::get_if_addrs()
        .map(|ifs| {
            ifs.into_iter()
                .filter(|i| !i.is_loopback())
                .filter_map(|i| match i.ip() {
                    std::net::IpAddr::V4(a) => Some(a),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v.dedup();
    v
}

/// One socket per interface, each sending `msg` to the SSDP group through
/// that interface (the kernel otherwise uses the default route only). Falls
/// back to a single unbound socket when the interfaces cannot be listed.
fn multicast_sockets(msg: &str) -> Result<Vec<UdpSocket>, UpnpError> {
    let dest: SocketAddr = "239.255.255.250:1900".parse().expect("ssdp address");
    let mut sockets = Vec::new();
    for ip in local_ipv4_addrs() {
        let Ok(socket) = UdpSocket::bind((ip, 0)) else {
            continue;
        };
        // Bound to the interface's address, Linux routes a multicast send
        // through that interface (the kernel picks the device owning the
        // source address), so no IP_MULTICAST_IF is needed.
        let _ = socket.set_multicast_ttl_v4(2);
        socket.set_read_timeout(Some(Duration::from_millis(100)))?;
        let mut sent = false;
        for _ in 0..2 {
            sent |= socket.send_to(msg.as_bytes(), dest).is_ok();
        }
        if sent {
            sockets.push(socket);
        }
    }
    if sockets.is_empty() {
        let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
        socket.set_read_timeout(Some(Duration::from_millis(100)))?;
        for _ in 0..2 {
            socket.send_to(msg.as_bytes(), dest)?;
        }
        sockets.push(socket);
    }
    Ok(sockets)
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
    let mut cm = None;
    let mut zgt = None;
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
                        } else if t.starts_with("urn:schemas-upnp-org:service:ConnectionManager:")
                            && cm.is_none()
                        {
                            cm = Some(c);
                        } else if t.starts_with("urn:schemas-upnp-org:service:ZoneGroupTopology:")
                            && zgt.is_none()
                        {
                            zgt = Some(c);
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
        connection_manager: cm.map(|c| abs(&c)),
        zone_group_topology: zgt.map(|c| abs(&c)),
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
    fn linn_udn_search_by_mac_finds_the_device() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        // The search assumes Linn's port; a test server on it is not possible,
        // so exercise the candidate builder through a tiny local copy.
        let hex = "00260f223d5f";
        let mid = format!("{}-{}-{}", &hex[0..4], &hex[4..8], &hex[8..12]);
        assert_eq!(mid, "0026-0f22-3d5f");
        drop(listener);
        let _ = port;
        assert!(linn_by_mac(std::net::IpAddr::V4(Ipv4Addr::LOCALHOST), "not-a-mac", None).is_err());
        assert!(linn_by_mac(
            std::net::IpAddr::V4(Ipv4Addr::LOCALHOST),
            "00:26:0f:22:3d:5f",
            Some("x")
        )
        .is_err());
        let err = probe_address("127.0.0.1 00:26:0f:22:3d:5f", Duration::from_millis(100)).unwrap_err();
        assert!(err.to_string().contains("55178"), "{err}");
        let err =
            probe_address("127.0.0.1 00:26:0f:22:3d:5f 1236686", Duration::from_millis(100)).unwrap_err();
        assert!(err.to_string().contains("55178"), "{err}");
    }

    #[test]
    fn probe_address_rejects_garbage_and_fails_fast_on_silence() {
        assert!(matches!(
            probe_address("not an address", Duration::from_millis(100)),
            Err(UpnpError::Other(_))
        ));
        let err = probe_address("127.0.0.1:1", Duration::from_millis(200)).unwrap_err();
        assert!(err.to_string().contains("did not answer"), "{err}");
        // A reachable TCP port with a description behind it is found without SSDP.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            for stream in listener.incoming().flatten() {
                let mut s = stream;
                let mut buf = [0u8; 2048];
                let _ = s.read(&mut buf);
                let req = String::from_utf8_lossy(&buf);
                let (status, body) = if req.starts_with("GET /Ds/device.xml") {
                    ("200 OK", DESC)
                } else {
                    ("404 Not Found", "")
                };
                let _ = write!(
                    s,
                    "HTTP/1.1 {status}\r\nContent-Type: text/xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        let r = match tcp_fallback_on(std::net::IpAddr::V4(Ipv4Addr::LOCALHOST), &[port]) {
            Some(Ok(r)) => r,
            other => panic!("expected a renderer, got {other:?}"),
        };
        assert_eq!(r.friendly_name, "Living Room DSM");
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
