//! The built-in player: this machine's DACs served to the node as a
//! `pm-endpoint` (bit-perfect ALSA `hw:` on Linux, the system output on
//! macOS and Windows), registered with the node so "<this machine> · <DAC>"
//! is an ordinary zone in the UI, driven by the node like any other.

use crate::settings::Settings;

/// Serve this machine's outputs over the `pm-stream` protocol on the usual
/// endpoint port (another when it is taken). `None` when nothing can be
/// bound; the UI's "This browser" zone still plays then.
pub fn start() -> Option<u16> {
    let outputs = pm_endpoint::local_outputs();
    let listener = std::net::TcpListener::bind(("0.0.0.0", pm_stream::DEFAULT_PORT))
        .or_else(|_| std::net::TcpListener::bind("0.0.0.0:0"))
        .map_err(|e| tracing::warn!("no player: cannot listen: {e}"))
        .ok()?;
    let port = listener.local_addr().ok()?.port();
    let name = pm_endpoint::machine_name();
    tracing::info!(port, %name, outputs = outputs.len(), "player ready");
    let endpoint = pm_endpoint::Endpoint::new(&name, outputs);
    std::thread::Builder::new()
        .name("pm-player".into())
        .spawn(move || {
            if let Err(e) = endpoint.serve(listener) {
                tracing::warn!("player stopped: {e}");
            }
        })
        .ok()?;
    Some(port)
}

/// The address this machine has on the way to the node (the interface the
/// node will connect back through).
fn local_ip_towards(node_url: &str) -> Option<std::net::IpAddr> {
    let u = url::Url::parse(node_url).ok()?;
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

/// Tell the node where this machine's player is. An old address from a
/// previous network is forgotten first. Failures are logged, never fatal.
pub async fn register(mut s: Settings, port: Option<u16>) -> Settings {
    let Some(port) = port else { return s };
    let Some(ip) = local_ip_towards(&s.node_url) else {
        tracing::warn!("player not registered: no route to the node");
        return s;
    };
    let address = format!("{ip}:{port}");
    if !s.player_address.is_empty() && s.player_address != address {
        if let Err(e) = crate::node::remove_endpoint(&s, &s.player_address).await {
            tracing::debug!(old = %s.player_address, "old player address: {e}");
        }
    }
    match crate::node::add_endpoint(&s, &address).await {
        Ok(()) => {
            tracing::info!(%address, "player registered with the node");
            s.player_address = address;
        }
        Err(e) => tracing::warn!(%address, "player not registered (the node must reach this machine): {e}"),
    }
    s
}
