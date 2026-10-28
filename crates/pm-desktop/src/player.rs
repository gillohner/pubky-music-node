//! The built-in player: this machine's DACs served to the node as a
//! `pm-endpoint` (bit-perfect ALSA `hw:` on Linux, the system output on
//! macOS and Windows), registered with the node so "<this machine> · <DAC>"
//! is an ordinary zone in the UI, driven by the node like any other.
//!
//! Only that node may use it: the player makes a random key each time the
//! app starts and registers it together with the address; it answers no one
//! without it. It listens where the node can reach it, and nowhere else it
//! need not: on loopback only for a node on this machine.

use crate::settings::Settings;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, Mutex};

pub struct Player {
    endpoint: Arc<pm_endpoint::Endpoint>,
    key: pm_stream::Key,
    /// The listener, once the node is known; replaced when it needs another address.
    serving: Mutex<Option<pm_endpoint::Serving>>,
}

/// This machine's outputs with a new key. It listens once it is registered with a node.
pub fn start() -> Option<Arc<Player>> {
    // The null output only for scripted tests (PM_NULL_OUTPUT).
    let outputs = if std::env::var_os("PM_NULL_OUTPUT").is_some() {
        pm_endpoint::local_outputs()
    } else {
        pm_endpoint::device_outputs()
    };
    Some(Player::new(outputs))
}

impl Player {
    fn new(outputs: pm_endpoint::OutputRegistry) -> Arc<Self> {
        let name = pm_endpoint::machine_name();
        tracing::info!(%name, outputs = outputs.len(), "player ready");
        let key = pm_stream::Key::random();
        Arc::new(Player {
            endpoint: pm_endpoint::Endpoint::new(&name, outputs, key.clone()),
            key,
            serving: Mutex::new(None),
        })
    }

    /// Listen on `ip`, on the usual endpoint port (another when it is taken); the listener on
    /// another address stops first. `None` when nothing can be bound; the UI's "This browser"
    /// zone still plays then.
    fn listen(&self, ip: IpAddr) -> Option<u16> {
        let mut serving = self.serving.lock().expect("player listener");
        if let Some(s) = serving.as_ref().filter(|s| s.addr().ip() == ip) {
            return Some(s.addr().port());
        }
        *serving = None;
        let listener = std::net::TcpListener::bind((ip, pm_stream::DEFAULT_PORT))
            .or_else(|_| std::net::TcpListener::bind((ip, 0)))
            .map_err(|e| tracing::warn!(%ip, "no player: cannot listen: {e}"))
            .ok()?;
        let s = self
            .endpoint
            .spawn(listener)
            .map_err(|e| tracing::warn!("no player: {e}"))
            .ok()?;
        let port = s.addr().port();
        tracing::info!(addr = %s.addr(), "player listening");
        *serving = Some(s);
        Some(port)
    }
}

/// Where the player listens and the address the node is given, for a node at `node_url`: a node
/// on this machine (a loopback address or `localhost`) gets loopback only; another gets every
/// interface and this machine's address on the way to it.
fn plan(node_url: &str) -> Option<(IpAddr, IpAddr)> {
    let u = url::Url::parse(node_url).ok()?;
    let loopback = match u.host()? {
        url::Host::Ipv4(ip) => ip.is_loopback(),
        url::Host::Ipv6(ip) => ip.is_loopback(),
        url::Host::Domain(d) => d.eq_ignore_ascii_case("localhost"),
    };
    if loopback {
        let lo = match u.host()? {
            url::Host::Ipv6(_) => IpAddr::V6(Ipv6Addr::LOCALHOST),
            _ => IpAddr::V4(Ipv4Addr::LOCALHOST),
        };
        return Some((lo, lo));
    }
    Some((IpAddr::V4(Ipv4Addr::UNSPECIFIED), local_ip_towards(&u)?))
}

/// The address this machine has on the way to the node (the interface the
/// node will connect back through).
fn local_ip_towards(u: &url::Url) -> Option<IpAddr> {
    let host = u.host_str()?;
    let port = u.port_or_known_default().unwrap_or(80);
    let sock = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    // UDP connect sends nothing; it only picks the route.
    sock.connect((host, port))
        .or_else(|_| sock.connect("1.1.1.1:53"))
        .ok()?;
    let ip = sock.local_addr().ok()?.ip();
    (!ip.is_loopback() && !ip.is_unspecified()).then_some(ip)
}

/// Tell the node where this machine's player is, and its key. An old address from a previous
/// network is forgotten first. Failures are logged, never fatal.
pub async fn register(mut s: Settings, player: Option<Arc<Player>>) -> Settings {
    let Some(player) = player else { return s };
    let Some((bind, ip)) = plan(&s.node_url) else {
        tracing::warn!("player not registered: no route to the node");
        return s;
    };
    let Some(port) = player.listen(bind) else { return s };
    let address = SocketAddr::new(ip, port).to_string();
    if !s.player_address.is_empty() && s.player_address != address {
        if let Err(e) = crate::node::remove_endpoint(&s, &s.player_address).await {
            tracing::debug!(old = %s.player_address, "old player address: {e}");
        }
    }
    match crate::node::add_endpoint(&s, &address, &player.key).await {
        Ok(()) => {
            tracing::info!(%address, "player registered with the node");
            s.player_address = address;
        }
        Err(e) => tracing::warn!(%address, "player not registered (the node must reach this machine): {e}"),
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read, Write};

    #[test]
    fn a_node_on_this_machine_gets_a_player_on_loopback_only() {
        let lo4 = IpAddr::V4(Ipv4Addr::LOCALHOST);
        for url in [
            "http://127.0.0.1:7788",
            "http://localhost:7788/",
            "http://127.0.0.2",
        ] {
            assert_eq!(plan(url), Some((lo4, lo4)), "{url}");
        }
        let lo6 = IpAddr::V6(Ipv6Addr::LOCALHOST);
        assert_eq!(plan("http://[::1]:7788"), Some((lo6, lo6)));
        assert_eq!(plan("not a url"), None);
        // Another machine: every interface, and the address towards it (none from a sandbox
        // without a route, which leaves the player unregistered)
        if let Some((bind, ip)) = plan("http://192.0.2.10:7788") {
            assert!(bind.is_unspecified());
            assert!(!ip.is_loopback() && !ip.is_unspecified());
        }
    }

    /// One HTTP request to a mock node: the path, the headers and the body.
    fn one_request(l: &std::net::TcpListener) -> (String, String, String) {
        let (stream, _) = l.accept().unwrap();
        let mut r = BufReader::new(stream.try_clone().unwrap());
        let mut line = String::new();
        r.read_line(&mut line).unwrap();
        let (mut headers, mut len) = (String::new(), 0);
        loop {
            let mut h = String::new();
            r.read_line(&mut h).unwrap();
            if h.trim().is_empty() {
                break;
            }
            if let Some(v) = h.to_ascii_lowercase().strip_prefix("content-length:") {
                len = v.trim().parse().unwrap();
            }
            headers.push_str(&h);
        }
        let mut body = vec![0; len];
        r.read_exact(&mut body).unwrap();
        let mut w = stream;
        w.write_all(b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 2\r\nconnection: close\r\n\r\n[]")
            .unwrap();
        (line, headers, String::from_utf8(body).unwrap())
    }

    #[tokio::test]
    async fn registration_sends_the_key_and_the_player_answers_only_it() {
        let node = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let s = Settings::new(&format!("http://{}", node.local_addr().unwrap()), "node-token");
        let player = Player::new(pm_endpoint::local_outputs());
        let mock = std::thread::spawn(move || one_request(&node));
        let s = register(s, Some(player.clone())).await;
        let (line, headers, body) = mock.join().unwrap();
        assert!(line.starts_with("POST /v1/outputs/endpoints "), "{line}");
        assert!(headers
            .to_ascii_lowercase()
            .contains("authorization: bearer node-token"));
        let body: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(body["key"], player.key.expose());
        // A node on loopback: the player listens there only
        let addr: SocketAddr = body["address"].as_str().unwrap().parse().unwrap();
        assert!(addr.ip().is_loopback(), "{addr}");
        assert_eq!(s.player_address, addr.to_string());
        let bound = player.serving.lock().unwrap().as_ref().unwrap().addr();
        assert_eq!(bound, addr);
        // The key works there, and nothing else does
        let wrong = hello_answer(addr, &pm_stream::Key::random());
        assert!(matches!(wrong, pm_stream::Frame::Error { .. }), "{wrong:?}");
        let right = hello_answer(addr, &player.key);
        assert!(matches!(right, pm_stream::Frame::Welcome { .. }), "{right:?}");
    }

    /// What the player answers a node's Hello with this key.
    fn hello_answer(addr: SocketAddr, key: &pm_stream::Key) -> pm_stream::Frame {
        let s = std::net::TcpStream::connect(addr).unwrap();
        s.set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        pm_stream::write_frame(
            &mut s.try_clone().unwrap(),
            &pm_stream::Frame::Hello {
                client: "test".into(),
                version: pm_stream::PROTOCOL_VERSION,
                key: key.clone(),
            },
        )
        .unwrap();
        match pm_stream::read_frame(&mut BufReader::new(s)).unwrap() {
            pm_stream::Incoming::Control(f) => f,
            pm_stream::Incoming::Audio(_) => panic!("audio"),
        }
    }
}
