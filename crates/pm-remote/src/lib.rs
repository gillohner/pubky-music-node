//! Reach a node from anywhere (ADR 0013): an iroh tunnel to its HTTP API.
//!
//! - The node runs a [`NodeTunnel`]: an iroh endpoint with its own key that accepts [`ALPN`] and pipes
//!   every bidirectional stream, byte for byte, to its HTTP listener on loopback. The API, the
//!   WebSocket and audio streams work unchanged, and the bearer token still guards every request.
//! - A controller runs a [`ClientTunnel`]: a TCP listener on `127.0.0.1:<random>` whose connections each
//!   become one stream to the node. A webview pointed at that address sees the node as if it were
//!   local.
//!
//! iroh dials by key, punches through NATs, and falls back to a relay. [`Route`] says which one a
//! connection uses, so the UI can say "direct" or "relayed".

use std::net::{Ipv4Addr, SocketAddr};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use iroh::endpoint::{presets, Connection, RecvStream, RelayMode, SendStream};
use iroh::Endpoint;
pub use iroh::{EndpointAddr, EndpointId, RelayUrl, SecretKey};
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;

/// The one protocol a node's tunnel speaks.
pub const ALPN: &[u8] = b"pubky-music/http/1";

/// Which relays an endpoint uses.
#[derive(Debug, Clone, Default)]
pub enum Relays {
    /// n0's public relays, and n0's DNS/pkarr address lookup, so a node is found by key alone.
    #[default]
    Public,
    /// A self-hosted `iroh-relay` (plus n0's address lookup).
    Custom(RelayUrl),
    /// No relays and no lookup: direct addresses only (tests, LAN).
    Off,
}

async fn bind(key: Option<SecretKey>, relays: &Relays, alpns: Vec<Vec<u8>>) -> anyhow::Result<Endpoint> {
    let builder = match relays {
        Relays::Public => Endpoint::builder(presets::N0),
        Relays::Custom(url) => {
            Endpoint::builder(presets::N0).relay_mode(RelayMode::Custom(url.clone().into()))
        }
        Relays::Off => Endpoint::builder(presets::Minimal).relay_mode(RelayMode::Disabled),
    };
    let builder = match key {
        Some(k) => builder.secret_key(k),
        None => builder,
    };
    Ok(builder.alpns(alpns).bind().await?)
}

/// The node's key, kept in `path` (64 hex characters, mode 600); made on first use.
pub fn load_or_create_key(path: &Path) -> anyhow::Result<SecretKey> {
    if let Ok(text) = std::fs::read_to_string(path) {
        let bytes = hex_decode(text.trim()).with_context(|| format!("{} is not a key", path.display()))?;
        return Ok(SecretKey::from_bytes(&bytes));
    }
    let key = SecretKey::generate();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let hex: String = key.to_bytes().iter().map(|b| format!("{b:02x}")).collect();
    write_private(path, &hex)?;
    Ok(key)
}

fn hex_decode(s: &str) -> anyhow::Result<[u8; 32]> {
    anyhow::ensure!(s.len() == 64, "expected 64 hex characters");
    let mut out = [0u8; 32];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16)?;
    }
    Ok(out)
}

fn write_private(path: &Path, text: &str) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        f.write_all(text.as_bytes())
    }
    #[cfg(not(unix))]
    std::fs::write(path, text)
}

// ---- node side ----

/// The node's end: accepts tunnel streams and pipes them to `target` (its HTTP listener).
pub struct NodeTunnel {
    endpoint: Endpoint,
    task: tokio::task::JoinHandle<()>,
}

impl NodeTunnel {
    pub async fn start(key: SecretKey, relays: &Relays, target: SocketAddr) -> anyhow::Result<Self> {
        let endpoint = bind(Some(key), relays, vec![ALPN.to_vec()]).await?;
        let ep = endpoint.clone();
        let task = tokio::spawn(async move {
            while let Some(incoming) = ep.accept().await {
                tokio::spawn(async move {
                    let conn = match incoming.accept() {
                        Ok(accepting) => match accepting.await {
                            Ok(c) => c,
                            Err(e) => return tracing::debug!("tunnel handshake failed: {e}"),
                        },
                        Err(e) => return tracing::debug!("tunnel connection refused: {e}"),
                    };
                    tracing::info!(peer = %conn.remote_id().fmt_short(), "remote controller connected");
                    serve_connection(conn, target).await;
                });
            }
        });
        Ok(Self { endpoint, task })
    }

    /// The key controllers dial.
    pub fn id(&self) -> EndpointId {
        self.endpoint.id()
    }

    /// Addresses and relay this node is reachable on right now.
    pub fn addr(&self) -> EndpointAddr {
        self.endpoint.addr()
    }

    /// Wait until the endpoint has a relay connection (or `timeout`): it is then findable by key.
    pub async fn online(&self, timeout: Duration) -> bool {
        tokio::time::timeout(timeout, self.endpoint.online())
            .await
            .is_ok()
    }

    pub async fn close(self) {
        self.task.abort();
        self.endpoint.close().await;
    }
}

async fn serve_connection(conn: Connection, target: SocketAddr) {
    while let Ok((send, recv)) = conn.accept_bi().await {
        tokio::spawn(async move {
            match TcpStream::connect(target).await {
                Ok(tcp) => pipe(tcp, send, recv).await,
                Err(e) => tracing::warn!(%target, "tunnel cannot reach the node's API: {e}"),
            }
        });
    }
}

/// Copy both ways until both sides are done.
async fn pipe(tcp: TcpStream, mut send: SendStream, mut recv: RecvStream) {
    let _ = tcp.set_nodelay(true);
    let (mut r, mut w) = tcp.into_split();
    let up = async {
        let _ = tokio::io::copy(&mut recv, &mut w).await;
        let _ = w.shutdown().await;
    };
    let down = async {
        let _ = tokio::io::copy(&mut r, &mut send).await;
        let _ = send.finish();
    };
    tokio::join!(up, down);
}

// ---- controller side ----

/// How a tunnel reaches the node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    Direct,
    Relayed,
}

/// The controller's end: `http://{local_addr}` is the node.
pub struct ClientTunnel {
    local: SocketAddr,
    endpoint: Endpoint,
    conn: Arc<Mutex<Option<Connection>>>,
    node: EndpointAddr,
    task: tokio::task::JoinHandle<()>,
}

impl ClientTunnel {
    /// Listen on `127.0.0.1:<random>` and connect to `node` (dialed by key; addresses help when known).
    pub async fn start(node: impl Into<EndpointAddr>, relays: &Relays) -> anyhow::Result<Self> {
        let node = node.into();
        let endpoint = bind(None, relays, vec![]).await?;
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        let local = listener.local_addr()?;
        let conn: Arc<Mutex<Option<Connection>>> = Arc::new(Mutex::new(None));
        let (ep, c, n) = (endpoint.clone(), conn.clone(), node.clone());
        let task = tokio::spawn(async move {
            while let Ok((tcp, _)) = listener.accept().await {
                let (ep, c, n) = (ep.clone(), c.clone(), n.clone());
                tokio::spawn(async move {
                    let conn = match connection(&ep, &c, &n).await {
                        Ok(conn) => conn,
                        Err(e) => return tracing::warn!("cannot reach the node over the tunnel: {e:#}"),
                    };
                    match conn.open_bi().await {
                        Ok((send, recv)) => pipe(tcp, send, recv).await,
                        Err(e) => {
                            tracing::debug!("tunnel stream failed: {e}");
                            c.lock().await.take();
                        }
                    }
                });
            }
        });
        Ok(Self {
            local,
            endpoint,
            conn,
            node,
            task,
        })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local
    }

    /// Connect now (rather than on the first request) and say how.
    pub async fn connect(&self) -> anyhow::Result<Route> {
        let conn = connection(&self.endpoint, &self.conn, &self.node).await?;
        Ok(route(&conn))
    }

    /// The current route, if connected.
    pub async fn route(&self) -> Option<Route> {
        self.conn.lock().await.as_ref().map(route)
    }

    pub async fn close(self) {
        self.task.abort();
        self.endpoint.close().await;
    }
}

fn route(conn: &Connection) -> Route {
    let paths = conn.paths();
    let selected = paths.iter().find(|p| p.is_selected());
    match selected {
        Some(p) if p.is_ip() => Route::Direct,
        _ => Route::Relayed,
    }
}

/// The live connection, or a new one when there is none or it closed.
async fn connection(
    ep: &Endpoint,
    slot: &Mutex<Option<Connection>>,
    node: &EndpointAddr,
) -> anyhow::Result<Connection> {
    let mut slot = slot.lock().await;
    if let Some(c) = slot.as_ref() {
        if c.close_reason().is_none() {
            return Ok(c.clone());
        }
    }
    let c = tokio::time::timeout(Duration::from_secs(15), ep.connect(node.clone(), ALPN))
        .await
        .context("timed out")??;
    *slot = Some(c.clone());
    Ok(c)
}
