//! Pubky Music desktop: one native window around the node's web UI (the
//! same UI on every platform), with the connection made by Pubky Ring. On
//! first start it shows a connect page; approving in Ring lets the app find
//! the node's address and token through the owner's homeserver, store them,
//! and open the UI.
//!
//! The app is also a player: it serves this machine's DACs to the node as
//! a `pm-endpoint` (bit-perfect ALSA `hw:` on Linux, the system output
//! elsewhere) and registers its address with the node, so "<this machine> ·
//! <DAC>" is an ordinary zone in the UI, driven by the node like any other.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy};
use tao::window::WindowBuilder;
use wry::WebViewBuilder;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct Settings {
    node_url: String,
    token: String,
    /// The `host:port` this machine's player was last registered under.
    #[serde(default)]
    player_address: String,
}

fn settings_path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("APPDATA").map(PathBuf::from))
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("pubky-music").join("desktop.json")
}

fn load_settings() -> Option<Settings> {
    let s: Settings = serde_json::from_slice(&std::fs::read(settings_path()).ok()?).ok()?;
    (!s.node_url.trim().is_empty()).then_some(s)
}

fn save_settings(s: &Settings) -> anyhow::Result<()> {
    let path = settings_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&path, serde_json::to_vec_pretty(s)?)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

/// Messages from the background (Ring flow) to the window.
#[derive(Debug)]
enum UserEvent {
    /// Show the Ring URL on the connect page.
    RingUrl(String),
    /// The node was found (or given): open its UI.
    Connected(Settings),
    /// The player's address at the node changed; remember it.
    PlayerRegistered(Settings),
    Failed(String),
}

/// JavaScript run before the node's page: the token the UI expects in
/// localStorage, and a flag the UI can use to adapt (window controls etc.).
fn init_script(s: &Settings) -> String {
    format!(
        "try {{ localStorage.setItem('pm.token', {token}); }} catch (e) {{}} window.pmDesktop = {{ version: {ver} }};",
        token = serde_json::to_string(&s.token).unwrap_or_default(),
        ver = serde_json::to_string(env!("CARGO_PKG_VERSION")).unwrap_or_default(),
    )
}

const CONNECT_PAGE: &str = include_str!("connect.html");

fn main() -> anyhow::Result<()> {
    // WebKitGTK's DMA-BUF renderer crashes the web process on some Linux
    // GPU stacks (NVIDIA, some Wayland compositors); the software path is
    // reliable and fast enough for this UI. An explicit setting wins.
    #[cfg(target_os = "linux")]
    if std::env::var_os("WEBKIT_DISABLE_DMABUF_RENDERER").is_none() {
        std::env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "1");
    }
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let rt = tokio::runtime::Runtime::new()?;
    let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();
    let window = WindowBuilder::new()
        .with_title("Pubky Music")
        .with_inner_size(tao::dpi::LogicalSize::new(1280.0, 840.0))
        .with_min_inner_size(tao::dpi::LogicalSize::new(420.0, 600.0))
        .build(&event_loop)?;

    let settings = std::env::var("PM_NODE").ok().map(|node_url| Settings {
        node_url,
        token: std::env::var("PM_TOKEN").unwrap_or_default(),
    });
    let settings = settings.or_else(load_settings);
    let player_port = start_player();
    if let Some(s) = settings.clone() {
        let proxy = proxy.clone();
        rt.spawn(async move {
            let s = register_player(s, player_port).await;
            let _ = proxy.send_event(UserEvent::PlayerRegistered(s));
        });
    }

    let ipc_proxy = proxy.clone();
    let rt_handle = rt.handle().clone();
    let mut builder = WebViewBuilder::new().with_ipc_handler(move |req: wry::http::Request<String>| {
        handle_ipc(req.body(), &ipc_proxy, &rt_handle, player_port);
    });
    builder = match &settings {
        Some(s) => builder
            .with_initialization_script(&init_script(s))
            .with_url(ui_url(&s.node_url)),
        None => builder.with_html(CONNECT_PAGE),
    };
    #[cfg(not(target_os = "linux"))]
    let webview = builder.build(&window)?;
    #[cfg(target_os = "linux")]
    let webview = {
        use tao::platform::unix::WindowExtUnix;
        use wry::WebViewBuilderExtUnix;
        let vbox = window
            .default_vbox()
            .ok_or_else(|| anyhow::anyhow!("no gtk box"))?;
        builder.build_gtk(vbox)?
    };

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        match event {
            Event::WindowEvent {
                event: WindowEvent::CloseRequested,
                ..
            } => *control_flow = ControlFlow::Exit,
            Event::UserEvent(UserEvent::RingUrl(url)) => {
                let js = format!(
                    "window.pmRingUrl && window.pmRingUrl({});",
                    serde_json::to_string(&url).unwrap_or_default()
                );
                let _ = webview.evaluate_script(&js);
            }
            Event::UserEvent(UserEvent::Connected(s)) => {
                if let Err(e) = save_settings(&s) {
                    tracing::warn!("cannot save desktop settings: {e}");
                }
                // The token must be in localStorage before the UI runs: set it
                // on the connect page's origin is useless, so navigate to the
                // node and set it there through the init script on next start;
                // for this session pass it in the URL, which the UI accepts.
                let _ = webview.load_url(&format!("{}?token={}", ui_url(&s.node_url), s.token));
            }
            Event::UserEvent(UserEvent::PlayerRegistered(s)) => {
                if let Err(e) = save_settings(&s) {
                    tracing::warn!("cannot save desktop settings: {e}");
                }
            }
            Event::UserEvent(UserEvent::Failed(e)) => {
                let js = format!(
                    "window.pmFailed && window.pmFailed({});",
                    serde_json::to_string(&e).unwrap_or_default()
                );
                let _ = webview.evaluate_script(&js);
            }
            _ => {}
        }
    });
}

fn ui_url(node_url: &str) -> String {
    format!("{}/", node_url.trim_end_matches('/'))
}

#[derive(Debug, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
enum Ipc {
    /// Sign in with Pubky Ring and find the node through the homeserver.
    Ring,
    /// A node address and token typed by hand.
    Manual { node_url: String, token: String },
}

fn handle_ipc(
    body: &str,
    proxy: &EventLoopProxy<UserEvent>,
    rt: &tokio::runtime::Handle,
    player_port: Option<u16>,
) {
    let Ok(msg) = serde_json::from_str::<Ipc>(body) else {
        tracing::warn!(%body, "unknown ipc message");
        return;
    };
    match msg {
        Ipc::Manual { node_url, token } => {
            let s = Settings {
                node_url,
                token,
                player_address: String::new(),
            };
            let _ = proxy.send_event(UserEvent::Connected(s.clone()));
            let proxy = proxy.clone();
            rt.spawn(async move {
                let s = register_player(s, player_port).await;
                let _ = proxy.send_event(UserEvent::PlayerRegistered(s));
            });
        }
        Ipc::Ring => {
            let proxy = proxy.clone();
            rt.spawn(async move {
                match ring_connect(&proxy).await {
                    Ok(s) => {
                        let _ = proxy.send_event(UserEvent::Connected(s.clone()));
                        let s = register_player(s, player_port).await;
                        let _ = proxy.send_event(UserEvent::PlayerRegistered(s));
                    }
                    Err(e) => {
                        let _ = proxy.send_event(UserEvent::Failed(e.to_string()));
                    }
                }
            });
        }
    }
}

/// Start a Ring sign-in asking only to read the node's private folder, show
/// the URL, wait for approval, then read the owner's node record.
async fn ring_connect(proxy: &EventLoopProxy<UserEvent>) -> anyhow::Result<Settings> {
    let hs = pm_pubky::homeserver::NodeHomeserver::new(pm_pubky::Network::Mainnet)?;
    let flow = hs.start_login_with("pubky-music-desktop", "/priv/pubky.music/:r")?;
    let _ = proxy.send_event(UserEvent::RingUrl(flow.url().to_string()));
    let session = flow.await_approval().await?;
    let record = hs.read_node_record(&session).await?.ok_or_else(|| {
        anyhow::anyhow!(
            "this identity has no node yet: sign the node in once on its web page (Settings → Pubky)"
        )
    })?;
    Ok(Settings {
        node_url: record.api_url,
        token: record.token,
        player_address: String::new(),
    })
}

/// Serve this machine's outputs over the `pm-stream` protocol on the usual
/// endpoint port (another when it is taken). `None` when nothing can be
/// bound; the UI's "This browser" zone still plays then.
fn start_player() -> Option<u16> {
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

/// Tell the node where this machine's player is; it becomes a zone per
/// DAC. An old address from a previous network is forgotten first so the
/// zone list does not fill with ghosts. Failures are logged, never fatal.
async fn register_player(mut s: Settings, port: Option<u16>) -> Settings {
    let Some(port) = port else { return s };
    let Some(ip) = local_ip_towards(&s.node_url) else {
        tracing::warn!("player not registered: no route to the node");
        return s;
    };
    let address = format!("{ip}:{port}");
    let client = match pm_client::NodeClient::new(&s.node_url, Some(s.token.clone())) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("player not registered: {e}");
            return s;
        }
    };
    if !s.player_address.is_empty() && s.player_address != address {
        if let Err(e) = client.remove_endpoint(&s.player_address).await {
            tracing::debug!(old = %s.player_address, "old player address: {e}");
        }
    }
    match client.add_endpoint(&address).await {
        Ok(outs) => {
            tracing::info!(%address, outputs = outs.len(), "player registered with the node");
            s.player_address = address;
        }
        Err(e) => tracing::warn!(%address, "player not registered (the node must reach this machine): {e}"),
    }
    s
}
