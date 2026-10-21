//! Find Pubky Music nodes on the LAN, and let a node be found (docs/CONNECTION.md).
//!
//! A node announces itself two ways, because networks differ in what they let through:
//! - **mDNS / DNS-SD** `_pubky-music._tcp` with TXT `role=node`, `id`, `name`, `version`, `api=v1`,
//!   and `auth=token|none`. Endpoints (`pm-endpoint`) use the same service type with `role=endpoint`
//!   or no `role`.
//! - **SSDP**: it answers `M-SEARCH` for [`SSDP_ST`], `upnp:rootdevice`, `ssdp:all` and its own
//!   `uuid:`, and sends `NOTIFY` alive and byebye messages. `LOCATION` points at a UPnP device
//!   description ([`DESCRIPTION_PATH`], served by the node) whose `presentationURL` is the web UI, so
//!   the node also shows up in Windows' Network folder. The `X-PM-*` headers carry the name, version
//!   and auth mode, so a browser needs no second request.
//!
//! [`browse`] runs both searches at once and merges the answers by node id.

use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// The DNS-SD service type nodes and endpoints share.
pub const SERVICE_TYPE: &str = "_pubky-music._tcp.local.";
/// The SSDP search target and UPnP device type of a node.
pub const SSDP_ST: &str = "urn:schemas-pubky-music:device:Node:1";
/// Where the node serves its UPnP device description.
pub const DESCRIPTION_PATH: &str = "/upnp/node.xml";

const SSDP_GROUP: Ipv4Addr = Ipv4Addr::new(239, 255, 255, 250);
const SSDP_PORT: u16 = 1900;
const MAX_AGE: u64 = 1800;
/// Re-announce well inside `MAX_AGE`.
const NOTIFY_EVERY: Duration = Duration::from_secs(600);

/// What a node announces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeAd {
    /// Stable id (a UUID), kept across restarts and address changes.
    pub id: String,
    pub name: String,
    /// The HTTP API port.
    pub port: u16,
    pub version: String,
    /// The API wants a token (`[api] token` is set).
    pub token_required: bool,
}

/// A node found on the LAN.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FoundNode {
    pub id: String,
    pub name: String,
    /// `http://<ip>:<port>`.
    pub url: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub token_required: bool,
    /// `mdns`, `ssdp` or both.
    pub via: Vec<String>,
}

/// Keeps a node announced while alive; says goodbye when dropped.
pub struct Advertiser {
    mdns: Option<(mdns_sd::ServiceDaemon, String)>,
    stop: Arc<AtomicBool>,
    ssdp: Option<JoinHandle<()>>,
}

impl Advertiser {
    /// Announce over mDNS and SSDP. Either may fail (no multicast, port 1900 taken); the other still
    /// runs, and failures are logged, never fatal.
    pub fn start(ad: NodeAd) -> Self {
        let mdns = match register_mdns(&ad) {
            Ok(m) => Some(m),
            Err(e) => {
                tracing::warn!("mDNS announcement failed: {e}");
                None
            }
        };
        let stop = Arc::new(AtomicBool::new(false));
        let ssdp = match ssdp_socket() {
            Ok(sock) => {
                let stop = stop.clone();
                let ad = ad.clone();
                std::thread::Builder::new()
                    .name("ssdp".into())
                    .spawn(move || ssdp_loop(sock, &ad, &stop))
                    .ok()
            }
            Err(e) => {
                tracing::warn!("SSDP announcement failed (UDP 1900): {e}");
                None
            }
        };
        tracing::info!(id = %ad.id, port = ad.port, mdns = mdns.is_some(), ssdp = ssdp.is_some(), "announcing the node on the LAN");
        Self { mdns, stop, ssdp }
    }
}

impl Drop for Advertiser {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some((daemon, fullname)) = self.mdns.take() {
            let _ = daemon.unregister(&fullname);
            let _ = daemon.shutdown();
        }
        if let Some(h) = self.ssdp.take() {
            let _ = h.join();
        }
    }
}

// ---- mDNS ----

fn register_mdns(ad: &NodeAd) -> Result<(mdns_sd::ServiceDaemon, String), mdns_sd::Error> {
    let daemon = mdns_sd::ServiceDaemon::new()?;
    let host = format!("{}.local.", hostname_label());
    let instance = ad.name.replace(['.', '\n'], " ");
    let props = [
        ("role", "node".to_string()),
        ("id", ad.id.clone()),
        ("name", ad.name.clone()),
        ("version", ad.version.clone()),
        ("api", "v1".to_string()),
        (
            "auth",
            if ad.token_required { "token" } else { "none" }.to_string(),
        ),
    ];
    let info = mdns_sd::ServiceInfo::new(SERVICE_TYPE, &instance, &host, "", ad.port, &props[..])?
        .enable_addr_auto();
    let fullname = info.get_fullname().to_string();
    daemon.register(info)?;
    Ok((daemon, fullname))
}

/// This machine's name as a DNS label.
pub fn hostname_label() -> String {
    let raw = std::env::var("HOSTNAME")
        .ok()
        .or_else(|| std::fs::read_to_string("/etc/hostname").ok())
        .or_else(|| std::env::var("COMPUTERNAME").ok())
        .unwrap_or_else(|| "pubky-music".into());
    let label: String = raw
        .trim()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .to_lowercase();
    if label.is_empty() {
        "pubky-music".into()
    } else {
        label
    }
}

fn browse_mdns(timeout: Duration) -> Vec<FoundNode> {
    let Ok(daemon) = mdns_sd::ServiceDaemon::new() else {
        return Vec::new();
    };
    let Ok(rx) = daemon.browse(SERVICE_TYPE) else {
        let _ = daemon.shutdown();
        return Vec::new();
    };
    let deadline = Instant::now() + timeout;
    let mut found = Vec::new();
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        match rx.recv_timeout(left) {
            Ok(mdns_sd::ServiceEvent::ServiceResolved(info)) => {
                if info.get_property_val_str("role") != Some("node") {
                    continue;
                }
                let addrs: Vec<IpAddr> = info.get_addresses().iter().map(|a| a.to_ip_addr()).collect();
                let Some(ip) = best_ip(&addrs) else { continue };
                let prop = |k: &str| info.get_property_val_str(k).unwrap_or_default().to_string();
                let url = http_url(ip, info.get_port());
                let id = Some(prop("id"))
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| url.clone());
                found.push(FoundNode {
                    id,
                    name: Some(prop("name"))
                        .filter(|s| !s.is_empty())
                        .unwrap_or_else(|| info.get_fullname().to_string()),
                    url,
                    version: prop("version"),
                    token_required: prop("auth") != "none",
                    via: vec!["mdns".into()],
                });
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    let _ = daemon.stop_browse(SERVICE_TYPE);
    let _ = daemon.shutdown();
    found
}

/// Prefer routable IPv4, then other IPv4, then IPv6 that is not link-local (which needs a scope id
/// a URL cannot carry).
fn best_ip(addrs: &[IpAddr]) -> Option<IpAddr> {
    let rank = |ip: &IpAddr| match ip {
        IpAddr::V4(v4) if v4.is_loopback() => 3,
        IpAddr::V4(v4) if v4.is_link_local() => 2,
        IpAddr::V4(_) => 0,
        IpAddr::V6(v6) if (v6.segments()[0] & 0xffc0) == 0xfe80 => 9,
        IpAddr::V6(_) => 1,
    };
    addrs
        .iter()
        .filter(|ip| rank(ip) < 9)
        .min_by_key(|ip| rank(ip))
        .copied()
}

fn http_url(ip: IpAddr, port: u16) -> String {
    match ip {
        IpAddr::V4(v4) => format!("http://{v4}:{port}"),
        IpAddr::V6(v6) => format!("http://[{v6}]:{port}"),
    }
}

// ---- SSDP ----

/// IPv4 addresses of the interfaces multicast can go out on.
fn lan_ipv4() -> Vec<Ipv4Addr> {
    let mut ips: Vec<Ipv4Addr> = if_addrs::get_if_addrs()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|i| match i.ip() {
            IpAddr::V4(v4) if !v4.is_loopback() => Some(v4),
            _ => None,
        })
        .collect();
    ips.sort();
    ips.dedup();
    ips
}

/// UDP 1900, shared with other SSDP services on this machine, in the SSDP group on every interface.
fn ssdp_socket() -> std::io::Result<UdpSocket> {
    use socket2::{Domain, Protocol, Socket, Type};
    let s = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    s.set_reuse_address(true)?;
    #[cfg(all(unix, not(any(target_os = "solaris", target_os = "illumos"))))]
    s.set_reuse_port(true)?;
    s.bind(&SocketAddr::from((Ipv4Addr::UNSPECIFIED, SSDP_PORT)).into())?;
    let mut joined = 0;
    for ip in lan_ipv4() {
        if s.join_multicast_v4(&SSDP_GROUP, &ip).is_ok() {
            joined += 1;
        }
    }
    if joined == 0 {
        s.join_multicast_v4(&SSDP_GROUP, &Ipv4Addr::UNSPECIFIED)?;
    }
    s.set_multicast_loop_v4(true)?;
    s.set_read_timeout(Some(Duration::from_millis(500)))?;
    Ok(s.into())
}

/// The local address this machine uses to reach `peer` (no packet is sent).
fn local_ip_towards(peer: IpAddr) -> Option<IpAddr> {
    let s = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    s.connect((peer, 9)).ok()?;
    s.local_addr().ok().map(|a| a.ip())
}

fn location(ip: IpAddr, ad: &NodeAd) -> String {
    format!("{}{DESCRIPTION_PATH}", http_url(ip, ad.port))
}

/// Header values are ASCII: percent-encode the name.
fn pct(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b" -_.~()'!".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn unpct(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 3 <= b.len() {
            if let Some(v) = std::str::from_utf8(&b[i + 1..i + 3])
                .ok()
                .and_then(|h| u8::from_str_radix(h, 16).ok())
            {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn server_header(ad: &NodeAd) -> String {
    format!("{}/1 UPnP/1.1 pubky-music/{}", std::env::consts::OS, ad.version)
}

fn pm_headers(ad: &NodeAd) -> String {
    format!(
        "X-PM-ID: {}\r\nX-PM-NAME: {}\r\nX-PM-VERSION: {}\r\nX-PM-AUTH: {}\r\n",
        ad.id,
        pct(&ad.name),
        ad.version,
        if ad.token_required { "token" } else { "none" }
    )
}

/// The three (NT, USN) pairs a root device announces.
fn targets(ad: &NodeAd) -> [(String, String); 3] {
    let uuid = format!("uuid:{}", ad.id);
    [
        ("upnp:rootdevice".into(), format!("{uuid}::upnp:rootdevice")),
        (uuid.clone(), uuid.clone()),
        (SSDP_ST.into(), format!("{uuid}::{SSDP_ST}")),
    ]
}

fn notify(ad: &NodeAd, nts: &str) {
    use socket2::{Domain, Protocol, Socket, Type};
    for ip in lan_ipv4() {
        let Ok(s) = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP)) else {
            continue;
        };
        if s.set_multicast_if_v4(&ip).is_err() || s.set_multicast_ttl_v4(2).is_err() {
            continue;
        }
        let dest: SocketAddr = SocketAddrV4::new(SSDP_GROUP, SSDP_PORT).into();
        for (nt, usn) in targets(ad) {
            let msg = format!(
                "NOTIFY * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\nCACHE-CONTROL: max-age={MAX_AGE}\r\nLOCATION: {}\r\nNT: {nt}\r\nNTS: {nts}\r\nSERVER: {}\r\nUSN: {usn}\r\n{}\r\n",
                location(IpAddr::V4(ip), ad),
                server_header(ad),
                pm_headers(ad),
            );
            let _ = s.send_to(msg.as_bytes(), &dest.into());
        }
    }
}

/// Parse an SSDP message: the start line and the headers (names upper-cased).
fn parse(msg: &str) -> (String, BTreeMap<String, String>) {
    let mut lines = msg.split("\r\n");
    let start = lines.next().unwrap_or_default().trim().to_string();
    let headers = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_uppercase(), v.trim().to_string()))
        .collect();
    (start, headers)
}

/// The search targets an `M-SEARCH` for `st` should be answered with.
fn answers_for(st: &str, ad: &NodeAd) -> Vec<(String, String)> {
    let [root, uuid, dev] = targets(ad);
    match st {
        "ssdp:all" => vec![root, uuid, dev],
        "upnp:rootdevice" => vec![root],
        s if s == uuid.0 => vec![uuid],
        s if s.eq_ignore_ascii_case(SSDP_ST) => vec![dev],
        _ => vec![],
    }
}

fn ssdp_loop(sock: UdpSocket, ad: &NodeAd, stop: &AtomicBool) {
    notify(ad, "ssdp:alive");
    let mut last_notify = Instant::now();
    let mut buf = [0u8; 2048];
    while !stop.load(Ordering::Relaxed) {
        if last_notify.elapsed() >= NOTIFY_EVERY {
            notify(ad, "ssdp:alive");
            last_notify = Instant::now();
        }
        let Ok((n, from)) = sock.recv_from(&mut buf) else {
            continue;
        };
        let Ok(msg) = std::str::from_utf8(&buf[..n]) else {
            continue;
        };
        let (start, h) = parse(msg);
        if !start.starts_with("M-SEARCH") || !h.get("MAN").is_some_and(|m| m.contains("ssdp:discover")) {
            continue;
        }
        let Some(st) = h.get("ST") else { continue };
        let answers = answers_for(st, ad);
        if answers.is_empty() {
            continue;
        }
        let Some(ip) = local_ip_towards(from.ip()) else {
            continue;
        };
        for (st, usn) in answers {
            let msg = format!(
                "HTTP/1.1 200 OK\r\nCACHE-CONTROL: max-age={MAX_AGE}\r\nEXT:\r\nLOCATION: {}\r\nSERVER: {}\r\nST: {st}\r\nUSN: {usn}\r\n{}\r\n",
                location(ip, ad),
                server_header(ad),
                pm_headers(ad),
            );
            let _ = sock.send_to(msg.as_bytes(), from);
        }
    }
    notify(ad, "ssdp:byebye");
}

fn browse_ssdp(timeout: Duration) -> Vec<FoundNode> {
    let Ok(sock) = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)) else {
        return Vec::new();
    };
    let search = format!(
        "M-SEARCH * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\nMAN: \"ssdp:discover\"\r\nMX: 1\r\nST: {SSDP_ST}\r\n\r\n"
    );
    let dest = SocketAddrV4::new(SSDP_GROUP, SSDP_PORT);
    // Out of every interface, twice (UDP gets lost).
    let send_all = || {
        let ifs = lan_ipv4();
        if ifs.is_empty() {
            let _ = sock.send_to(search.as_bytes(), dest);
        }
        for ip in ifs {
            let s2 = socket2::SockRef::from(&sock);
            if s2.set_multicast_if_v4(&ip).is_ok() {
                let _ = sock.send_to(search.as_bytes(), dest);
            }
        }
    };
    send_all();
    let deadline = Instant::now() + timeout;
    let mut resent = false;
    let mut found: Vec<FoundNode> = Vec::new();
    let mut buf = [0u8; 2048];
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        if !resent && left < timeout / 2 {
            send_all();
            resent = true;
        }
        let _ = sock.set_read_timeout(Some(
            left.min(Duration::from_millis(200)).max(Duration::from_millis(1)),
        ));
        let Ok((n, _)) = sock.recv_from(&mut buf) else {
            continue;
        };
        let Ok(msg) = std::str::from_utf8(&buf[..n]) else {
            continue;
        };
        if let Some(node) = parse_search_answer(msg) {
            if !found.iter().any(|f| f.id == node.id) {
                found.push(node);
            }
        }
    }
    found
}

fn parse_search_answer(msg: &str) -> Option<FoundNode> {
    let (start, h) = parse(msg);
    if !start.starts_with("HTTP/1.1 200") || !h.get("ST")?.eq_ignore_ascii_case(SSDP_ST) {
        return None;
    }
    let loc = h.get("LOCATION")?;
    let rest = loc.strip_prefix("http://")?;
    let hostport = rest.split('/').next()?;
    let url = format!("http://{hostport}");
    let id = h
        .get("X-PM-ID")
        .cloned()
        .or_else(|| {
            h.get("USN")
                .and_then(|u| u.strip_prefix("uuid:"))
                .map(|u| u.split("::").next().unwrap_or(u).to_string())
        })
        .unwrap_or_else(|| url.clone());
    Some(FoundNode {
        id,
        name: h
            .get("X-PM-NAME")
            .map(|n| unpct(n))
            .unwrap_or_else(|| hostport.to_string()),
        url,
        version: h.get("X-PM-VERSION").cloned().unwrap_or_default(),
        token_required: h.get("X-PM-AUTH").map(String::as_str) != Some("none"),
        via: vec!["ssdp".into()],
    })
}

// ---- both ----

/// Look for nodes over mDNS and SSDP at once for `timeout`; one entry per node.
pub fn browse(timeout: Duration) -> Vec<FoundNode> {
    let mdns = std::thread::spawn(move || browse_mdns(timeout));
    let ssdp = browse_ssdp(timeout);
    let mdns = mdns.join().unwrap_or_default();
    merge(mdns, ssdp)
}

/// One entry per id; mDNS first (it carries the most), `via` lists both.
fn merge(mdns: Vec<FoundNode>, ssdp: Vec<FoundNode>) -> Vec<FoundNode> {
    let mut out: Vec<FoundNode> = Vec::new();
    for n in mdns.into_iter().chain(ssdp) {
        match out.iter_mut().find(|o| o.id == n.id) {
            Some(o) => {
                for v in n.via {
                    if !o.via.contains(&v) {
                        o.via.push(v);
                    }
                }
            }
            None => out.push(n),
        }
    }
    out.sort_by(|a, b| {
        a.name
            .to_lowercase()
            .cmp(&b.name.to_lowercase())
            .then(a.url.cmp(&b.url))
    });
    out
}

/// The UPnP device description the node serves at [`DESCRIPTION_PATH`].
pub fn description_xml(ad: &NodeAd) -> String {
    let esc = |s: &str| {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
    };
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<root xmlns="urn:schemas-upnp-org:device-1-0">
  <specVersion><major>1</major><minor>1</minor></specVersion>
  <device>
    <deviceType>{SSDP_ST}</deviceType>
    <friendlyName>{}</friendlyName>
    <manufacturer>Pubky Music</manufacturer>
    <manufacturerURL>https://github.com/gillohner/pubky-music-node</manufacturerURL>
    <modelName>Pubky Music Node</modelName>
    <modelNumber>{}</modelNumber>
    <UDN>uuid:{}</UDN>
    <presentationURL>/</presentationURL>
  </device>
</root>
"#,
        esc(&ad.name),
        esc(&ad.version),
        esc(&ad.id)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ad() -> NodeAd {
        NodeAd {
            id: "0f5e2c1a-1111-4222-8333-444455556666".into(),
            name: "Living room · Café".into(),
            port: 7788,
            version: "0.1.0".into(),
            token_required: true,
        }
    }

    #[test]
    fn names_survive_the_header_encoding() {
        assert_eq!(unpct(&pct("Living room · Café & co")), "Living room · Café & co");
        assert_eq!(unpct("100%"), "100%");
    }

    #[test]
    fn searches_are_answered_for_our_targets_only() {
        let ad = ad();
        assert_eq!(answers_for("ssdp:all", &ad).len(), 3);
        assert_eq!(answers_for("upnp:rootdevice", &ad).len(), 1);
        assert_eq!(answers_for(SSDP_ST, &ad)[0].0, SSDP_ST);
        assert_eq!(answers_for(&format!("uuid:{}", ad.id), &ad).len(), 1);
        assert!(answers_for("urn:schemas-upnp-org:device:MediaRenderer:1", &ad).is_empty());
    }

    #[test]
    fn a_search_answer_reads_back_as_the_node() {
        let ad = ad();
        let msg = format!(
            "HTTP/1.1 200 OK\r\nCACHE-CONTROL: max-age=1800\r\nEXT:\r\nLOCATION: http://192.168.1.20:7788{DESCRIPTION_PATH}\r\nST: {SSDP_ST}\r\nUSN: uuid:{}::{SSDP_ST}\r\n{}\r\n",
            ad.id,
            pm_headers(&ad)
        );
        let n = parse_search_answer(&msg).unwrap();
        assert_eq!(n.id, ad.id);
        assert_eq!(n.name, ad.name);
        assert_eq!(n.url, "http://192.168.1.20:7788");
        assert!(n.token_required);
        // Another device's answer is not a node.
        assert!(
            parse_search_answer("HTTP/1.1 200 OK\r\nST: upnp:rootdevice\r\nLOCATION: http://x/\r\n\r\n")
                .is_none()
        );
    }

    #[test]
    fn mdns_and_ssdp_answers_merge_by_id() {
        let n = |via: &str, url: &str| FoundNode {
            id: "a".into(),
            name: "A".into(),
            url: url.into(),
            version: String::new(),
            token_required: true,
            via: vec![via.into()],
        };
        let m = merge(
            vec![n("mdns", "http://10.0.0.2:7788")],
            vec![n("ssdp", "http://10.0.0.2:7788")],
        );
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].via, ["mdns", "ssdp"]);
    }

    #[test]
    fn routable_ipv4_wins() {
        let ips: Vec<IpAddr> = ["fe80::1", "169.254.3.4", "192.168.1.5", "2001:db8::5"]
            .iter()
            .map(|s| s.parse().unwrap())
            .collect();
        assert_eq!(best_ip(&ips), Some("192.168.1.5".parse().unwrap()));
        assert_eq!(best_ip(&ips[..1]), None);
    }

    #[test]
    fn the_description_escapes_the_name() {
        let x = description_xml(&NodeAd {
            name: "A & <B>".into(),
            ..ad()
        });
        assert!(x.contains("<friendlyName>A &amp; &lt;B&gt;</friendlyName>"));
        assert!(x.contains(&format!("<UDN>uuid:{}</UDN>", ad().id)));
    }

    /// A node and a browser on this machine find each other (needs multicast on loopback or a LAN
    /// interface; skipped where the sandbox has none).
    #[test]
    #[ignore = "needs multicast; run with --ignored on a machine with a LAN interface"]
    fn a_browser_finds_an_advertised_node() {
        let ad = NodeAd { port: 7123, ..ad() };
        let _adv = Advertiser::start(ad.clone());
        std::thread::sleep(Duration::from_millis(300));
        let found = browse(Duration::from_secs(3));
        let me = found.iter().find(|f| f.id == ad.id).expect("found");
        eprintln!("found via {:?} at {}", me.via, me.url);
        assert!(me.url.ends_with(":7123"));
        assert_eq!(me.name, ad.name);
    }
}
