//! SSDP for the zone renderers: answers `M-SEARCH` (for `ssdp:all`, `upnp:rootdevice`, the
//! MediaRenderer device type, its three service types and each zone's `uuid:`) and sends
//! `NOTIFY` alive for every zone (again for new zones, and every ten minutes) and byebye for
//! zones that went away and on shutdown.

use crate::xml::{AVT, CM, DEVICE_TYPE, RC};
use crate::Renderer;
use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const GROUP: Ipv4Addr = Ipv4Addr::new(239, 255, 255, 250);
const PORT: u16 = 1900;
const MAX_AGE: u64 = 1800;
const NOTIFY_EVERY: Duration = Duration::from_secs(600);
/// New or removed zones are announced within this.
const ZONES_EVERY: Duration = Duration::from_secs(5);

/// The zone id as one URL path segment.
pub fn path_segment(zone: &str) -> String {
    let mut o = String::new();
    for b in zone.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
            o.push(b as char);
        } else {
            o.push_str(&format!("%{b:02X}"));
        }
    }
    o
}

/// The (NT, USN) pairs one zone device announces.
fn targets(udn: &str) -> Vec<(String, String)> {
    let mut v = vec![
        (
            "upnp:rootdevice".to_string(),
            format!("uuid:{udn}::upnp:rootdevice"),
        ),
        (format!("uuid:{udn}"), format!("uuid:{udn}")),
        (DEVICE_TYPE.to_string(), format!("uuid:{udn}::{DEVICE_TYPE}")),
    ];
    for s in [AVT, RC, CM] {
        v.push((s.to_string(), format!("uuid:{udn}::{s}")));
    }
    v
}

/// Keeps the zones announced while alive; byebye when dropped.
pub struct Advertiser {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for Advertiser {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Announce `renderer`'s zones, described on HTTP `port`. Failure (UDP 1900 taken, no
/// multicast) is logged; the renderer still works for controllers that know the address.
pub fn start(renderer: Arc<Renderer>, port: u16) -> Option<Advertiser> {
    let sock = match socket() {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("renderer SSDP unavailable (UDP 1900): {e}");
            return None;
        }
    };
    let stop = Arc::new(AtomicBool::new(false));
    let s2 = stop.clone();
    let thread = std::thread::Builder::new()
        .name("renderer-ssdp".into())
        .spawn(move || run(sock, &renderer, port, &s2))
        .ok()?;
    Some(Advertiser {
        stop,
        thread: Some(thread),
    })
}

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

fn socket() -> std::io::Result<UdpSocket> {
    use socket2::{Domain, Protocol, Socket, Type};
    let s = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    s.set_reuse_address(true)?;
    #[cfg(all(unix, not(any(target_os = "solaris", target_os = "illumos"))))]
    s.set_reuse_port(true)?;
    s.bind(&SocketAddr::from((Ipv4Addr::UNSPECIFIED, PORT)).into())?;
    let mut joined = 0;
    for ip in lan_ipv4() {
        if s.join_multicast_v4(&GROUP, &ip).is_ok() {
            joined += 1;
        }
    }
    if joined == 0 {
        s.join_multicast_v4(&GROUP, &Ipv4Addr::UNSPECIFIED)?;
    }
    s.set_multicast_loop_v4(true)?;
    s.set_read_timeout(Some(Duration::from_millis(500)))?;
    Ok(s.into())
}

fn local_ip_towards(peer: IpAddr) -> Option<IpAddr> {
    let s = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    s.connect((peer, 9)).ok()?;
    s.local_addr().ok().map(|a| a.ip())
}

fn location(ip: IpAddr, port: u16, zone: &str) -> String {
    format!("http://{ip}:{port}/upnp/zone/{}/desc.xml", path_segment(zone))
}

fn server(r: &Renderer) -> String {
    format!("{}/1 UPnP/1.0 pubky-music/{}", std::env::consts::OS, r.version)
}

fn notify(sock: &UdpSocket, r: &Renderer, port: u16, zone: &str, alive: bool) {
    let udn = r.udn(zone);
    for ip in lan_ipv4() {
        let loc = location(IpAddr::V4(ip), port, zone);
        for (nt, usn) in targets(&udn) {
            let msg = if alive {
                format!(
                    "NOTIFY * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\nCACHE-CONTROL: max-age={MAX_AGE}\r\n\
                     LOCATION: {loc}\r\nNT: {nt}\r\nNTS: ssdp:alive\r\nSERVER: {}\r\nUSN: {usn}\r\n\r\n",
                    server(r)
                )
            } else {
                format!(
                    "NOTIFY * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\nNT: {nt}\r\nNTS: ssdp:byebye\r\nUSN: {usn}\r\n\r\n"
                )
            };
            let _ = sock.send_to(msg.as_bytes(), (GROUP, PORT));
        }
    }
}

/// The `ST` of an `M-SEARCH`, if it is one.
fn search_target(msg: &str) -> Option<String> {
    let mut lines = msg.lines();
    if !lines.next()?.starts_with("M-SEARCH") {
        return None;
    }
    lines
        .filter_map(|l| l.split_once(':'))
        .find(|(k, _)| k.trim().eq_ignore_ascii_case("ST"))
        .map(|(_, v)| v.trim().to_string())
}

/// The zones' `(ST, USN)` answers to a search target.
fn answers(r: &Renderer, st: &str, zone: &str) -> Vec<(String, String)> {
    let udn = r.udn(zone);
    targets(&udn)
        .into_iter()
        .filter(|(nt, _)| st == "ssdp:all" || st == nt)
        .collect()
}

fn run(sock: UdpSocket, r: &Renderer, port: u16, stop: &AtomicBool) {
    let mut known: HashMap<String, Instant> = HashMap::new();
    let mut last_zones = Instant::now() - ZONES_EVERY;
    let mut buf = [0u8; 2048];
    while !stop.load(Ordering::Relaxed) {
        if last_zones.elapsed() >= ZONES_EVERY {
            last_zones = Instant::now();
            let zones: Vec<String> = r.zones().into_iter().map(|(id, _)| id).collect();
            for z in &zones {
                let due = known.get(z).is_none_or(|t| t.elapsed() >= NOTIFY_EVERY);
                if due {
                    notify(&sock, r, port, z, true);
                    known.insert(z.clone(), Instant::now());
                }
            }
            let gone: Vec<String> = known.keys().filter(|k| !zones.contains(k)).cloned().collect();
            for z in gone {
                notify(&sock, r, port, &z, false);
                known.remove(&z);
            }
        }
        let Ok((n, from)) = sock.recv_from(&mut buf) else {
            continue;
        };
        let Some(st) = search_target(&String::from_utf8_lossy(&buf[..n])) else {
            continue;
        };
        let Some(ip) = local_ip_towards(from.ip()) else {
            continue;
        };
        for (zone, _) in r.zones() {
            for (st, usn) in answers(r, &st, &zone) {
                let msg = format!(
                    "HTTP/1.1 200 OK\r\nCACHE-CONTROL: max-age={MAX_AGE}\r\nEXT:\r\nLOCATION: {}\r\nSERVER: {}\r\n\
                     ST: {st}\r\nUSN: {usn}\r\n\r\n",
                    location(ip, port, &zone),
                    server(r)
                );
                let _ = sock.send_to(msg.as_bytes(), from);
            }
        }
    }
    for z in known.keys() {
        notify(&sock, r, port, z, false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_targets_and_paths() {
        let m = "M-SEARCH * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\nMAN: \"ssdp:discover\"\r\nMX: 2\r\nst: urn:schemas-upnp-org:device:MediaRenderer:1\r\n\r\n";
        assert_eq!(search_target(m).as_deref(), Some(DEVICE_TYPE));
        assert_eq!(search_target("NOTIFY * HTTP/1.1\r\nNT: x\r\n\r\n"), None);
        assert_eq!(path_segment("slim:02/x y"), "slim%3A02%2Fx%20y");
        let t = targets("u");
        assert_eq!(t.len(), 6);
        assert!(t
            .iter()
            .any(|(nt, usn)| nt == AVT && usn == &format!("uuid:u::{AVT}")));
    }
}
