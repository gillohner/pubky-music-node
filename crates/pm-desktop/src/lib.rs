//! Pubky Music app: one native window around the node's web UI (the same UI
//! on every platform), built with Tauri 2 for macOS, Windows, Linux and
//! Android. The window starts on a small connect page (`ui/`): approving in
//! Pubky Ring lets the app find the node's address and token through the
//! owner's homeserver, or an address and token are typed by hand. Once
//! connected the window shows the node's own page.
//!
//! The app is also a player (`player`): it serves this machine's DACs (on
//! Android, whatever the phone plays through) to the node as a `pm-endpoint`
//! and registers them, so they are zones like any other.

#[cfg(target_os = "android")]
mod android;
mod node;
#[cfg(any(desktop, target_os = "android"))]
mod player;
mod settings;

use serde::Serialize;
use settings::Settings;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tauri::{AppHandle, Emitter, Manager, State, WebviewUrl, WebviewWindowBuilder};
use tauri_plugin_opener::OpenerExt;

const MAIN: &str = "main";

/// The capability the app asks Ring for: read the owner's node record.
const RING_CAPS: &str = "/priv/pubky.music/:r";

struct AppState {
    /// Shared with the navigation guard, which runs outside commands.
    settings: Arc<Mutex<Option<Settings>>>,
    path: PathBuf,
    player_port: Option<u16>,
    /// The iroh tunnel to the node, while the node is reached through it (ADR 0013).
    tunnel: tokio::sync::Mutex<Option<pm_remote::ClientTunnel>>,
}

impl AppState {
    fn current(&self) -> Option<Settings> {
        self.settings.lock().ok()?.clone()
    }

    fn store(&self, s: Option<Settings>) {
        if let Err(e) = settings::save(&self.path, s.as_ref()) {
            tracing::warn!("cannot save the app settings: {e}");
        }
        if let Ok(mut cur) = self.settings.lock() {
            *cur = s;
        }
    }
}

/// Links that would open a second window (`target="_blank"`, a
/// cross-origin `window.open`) become plain navigations, which the
/// navigation guard sends to the system browser. WebKitGTK does not always
/// raise a new-window request for them, and Android never does.
const EXTERNAL_LINKS_JS: &str = "document.addEventListener('click',function(e){var a=e.target&&e.target.closest&&e.target.closest('a[target=_blank]');if(a&&a.href&&!e.defaultPrevented){e.preventDefault();location.assign(a.href);}});var wo=window.open;window.open=function(u){try{var x=new URL(u,location.href);if(x.origin!==location.origin){location.assign(x.href);return null;}}catch(e){}return wo.apply(window,arguments);};";

/// Run before every page: the node's page gets the token in the
/// localStorage key the web UI reads (only on the node's origin), and every
/// page can see it runs inside the app.
fn init_script(s: Option<&Settings>) -> String {
    let (origin, token) = s
        .and_then(|s| Some((s.origin()?, s.token.clone())))
        .unwrap_or_default();
    format!(
        "(function(){{var o={o},t={t};if(o&&location.origin===o){{try{{localStorage.setItem('pm.token',t);}}catch(e){{}}}}window.pmDesktop={{version:{v},platform:{p}}};{EXTERNAL_LINKS_JS}}})();",
        o = serde_json::to_string(&origin).unwrap_or_default(),
        t = serde_json::to_string(&token).unwrap_or_default(),
        v = serde_json::to_string(env!("CARGO_PKG_VERSION")).unwrap_or_default(),
        p = serde_json::to_string(std::env::consts::OS).unwrap_or_default(),
    )
}

/// For a node connected after the window was built: the init script holds
/// the old token, so put the new one in place once and reload.
fn token_sync_script(token: &str) -> String {
    format!(
        "(function(t){{try{{if(localStorage.getItem('pm.token')!==t){{localStorage.setItem('pm.token',t);location.reload();}}}}catch(e){{}}}})({});",
        serde_json::to_string(token).unwrap_or_default()
    )
}

/// The app's own pages: `tauri://localhost` on macOS and Linux,
/// `http(s)://tauri.localhost` on Windows and Android.
fn is_app_page(u: &url::Url) -> bool {
    u.scheme() == "tauri" || u.host_str() == Some("tauri.localhost") || u.scheme() == "about"
}

/// Navigation stays on the app's pages and the node's origin; anything else
/// (an "Open on …" link, a streaming service's sign-in) opens in the
/// system browser.
fn allow_navigation(app: &AppHandle, current: &Arc<Mutex<Option<Settings>>>, u: &url::Url) -> bool {
    tracing::debug!(url = %u, "navigation");
    if is_app_page(u) {
        return true;
    }
    let node_origin = current.lock().ok().and_then(|s| s.as_ref()?.origin());
    if node_origin.as_deref() == Some(u.origin().ascii_serialization().as_str()) {
        return true;
    }
    if matches!(
        u.scheme(),
        "http" | "https" | "mailto" | "pubkyauth" | "pubkyring"
    ) {
        if let Err(e) = app.opener().open_url(u.as_str(), None::<&str>) {
            tracing::warn!(url = %u, "cannot open externally: {e}");
        }
    }
    false
}

fn show_node(app: &AppHandle, s: &Settings) -> Result<(), String> {
    let w = app
        .get_webview_window(MAIN)
        .ok_or_else(|| "the window is gone".to_string())?;
    let u = url::Url::parse(&format!("{}/", s.base())).map_err(|e| format!("Not an address: {e}"))?;
    w.navigate(u).map_err(|e| e.to_string())
}

/// Connected: remember the node, show it and register the player.
fn connected(app: &AppHandle, state: &AppState, mut s: Settings, info: &node::Info) -> Result<(), String> {
    if !info.id.is_empty() {
        s.node_id = info.id.clone();
    }
    if let Some(old) = state.current() {
        if old.node_url == s.node_url {
            s.player_address = old.player_address;
        }
    }
    state.store(Some(s.clone()));
    show_node(app, &s)?;
    // Through the tunnel the node is far away and cannot reach this machine's player.
    #[cfg(any(desktop, target_os = "android"))]
    if s.via.is_empty() {
        let port = state.player_port;
        #[cfg(target_os = "android")]
        if port.is_some() {
            let node = if info.name.is_empty() {
                "your node"
            } else {
                info.name.as_str()
            };
            android::start_player_service(app, format!("This phone is a zone of {node}"));
        }
        let app = app.clone();
        tauri::async_runtime::spawn(async move {
            let s = player::register(s, port).await;
            let state = app.state::<AppState>();
            if state.current().map(|c| c.node_url) == Some(s.node_url.clone()) {
                state.store(Some(s));
            }
        });
    }
    Ok(())
}

#[derive(Serialize)]
struct Saved {
    node_url: String,
}

/// The node remembered from last time, if any.
#[tauri::command]
fn saved(state: State<'_, AppState>) -> Option<Saved> {
    state.current().map(|s| Saved { node_url: s.node_url })
}

/// Reconnect to the remembered node: check it answers, then show it.
#[tauri::command]
async fn open_saved(app: AppHandle, state: State<'_, AppState>) -> Result<String, String> {
    let s = state.current().ok_or("No node yet")?;
    let (s, info) = reach(&app, &state, s).await?;
    connected(&app, &state, s, &info)?;
    Ok(info.name)
}

/// Reach the node: at its address; else, on the LAN under its id (it may have a new address); else
/// through the iroh tunnel when its record carries a key (away from home).
async fn reach(app: &AppHandle, state: &AppState, mut s: Settings) -> Result<(Settings, node::Info), String> {
    s.via.clear();
    let msg = match node::probe(&s).await {
        Ok(info) => {
            state.tunnel.lock().await.take();
            return Ok((s, info));
        }
        Err(node::ProbeError::Unreachable(msg)) => msg,
        Err(e) => return Err(e.into()),
    };
    if !s.node_id.is_empty() {
        let found = discover(app).await;
        if let Some(moved) = found
            .into_iter()
            .find(|n| n.id == s.node_id && n.url != s.node_url)
        {
            tracing::info!(from = %s.node_url, to = %moved.url, "the node moved; following it");
            s.node_url = moved.url;
            if let Ok(info) = node::probe(&s).await {
                return Ok((s, info));
            }
        }
    }
    if s.iroh.is_empty() {
        return Err(msg);
    }
    let id: pm_remote::EndpointId = s.iroh.parse().map_err(|_| msg.clone())?;
    let tunnel = pm_remote::ClientTunnel::start(id, &pm_remote::Relays::Public)
        .await
        .map_err(|e| format!("{msg}, and remote access failed: {e}"))?;
    let route = tunnel
        .connect()
        .await
        .map_err(|e| format!("{msg}, and the node did not answer remotely either: {e}"))?;
    tracing::info!(?route, "reaching the node over iroh");
    s.via = format!("http://{}", tunnel.local_addr());
    let info = node::probe(&s).await?;
    if let Some(old) = state.tunnel.lock().await.replace(tunnel) {
        old.close().await;
    }
    Ok((s, info))
}

/// Nodes on the LAN (mDNS and SSDP, `pm-discovery`).
async fn discover(app: &AppHandle) -> Vec<pm_discovery::FoundNode> {
    #[cfg(target_os = "android")]
    let lock = android::MulticastLock::acquire(app).await;
    #[cfg(not(target_os = "android"))]
    let _ = app;
    let found =
        tauri::async_runtime::spawn_blocking(|| pm_discovery::browse(std::time::Duration::from_millis(2500)))
            .await
            .unwrap_or_default();
    #[cfg(target_os = "android")]
    drop(lock);
    found
}

/// The connect page's "On this network" list.
#[tauri::command]
async fn discover_nodes(app: AppHandle) -> Vec<pm_discovery::FoundNode> {
    discover(&app).await
}

/// An address and token typed by hand.
#[tauri::command]
async fn connect_manual(
    app: AppHandle,
    state: State<'_, AppState>,
    node_url: String,
    token: String,
) -> Result<String, String> {
    if node_url.trim().is_empty() {
        return Err("Enter the node's address".into());
    }
    let s = Settings::new(&node_url, &token);
    let info = node::probe(&s).await?;
    connected(&app, &state, s, &info)?;
    Ok(info.name)
}

#[derive(Clone, Serialize)]
struct RingUrl {
    url: String,
    /// The URL as a QR code, for Ring on a phone.
    qr_svg: String,
}

fn qr_svg(data: &str) -> String {
    qrcode::QrCode::new(data.as_bytes())
        .map(|c| {
            c.render::<qrcode::render::svg::Color>()
                .min_dimensions(220, 220)
                .quiet_zone(true)
                .build()
        })
        .unwrap_or_default()
}

/// Sign in with Pubky Ring, asking only to read the node's private folder;
/// the URL goes to the page as a `ring-url` event; on approval the owner's
/// node record gives the address and token.
#[tauri::command]
async fn ring_connect(app: AppHandle, state: State<'_, AppState>) -> Result<String, String> {
    let hs =
        pm_pubky::homeserver::NodeHomeserver::new(pm_pubky::Network::Mainnet).map_err(|e| e.to_string())?;
    let flow = hs
        .start_login_with("pubky-music-desktop", RING_CAPS)
        .map_err(|e| e.to_string())?;
    let url = flow.url().to_string();
    let _ = app.emit(
        "ring-url",
        RingUrl {
            qr_svg: qr_svg(&url),
            url,
        },
    );
    let session = flow.await_approval().await.map_err(|e| e.to_string())?;
    let record = hs
        .read_node_record(&session)
        .await
        .map_err(|e| e.to_string())?
        .ok_or("This identity has no node yet: sign the node in once on its web page (Settings → Pubky)")?;
    let mut s = Settings::new(&record.api_url, &record.token);
    s.iroh = record.iroh;
    let (s, info) = reach(&app, &state, s).await?;
    connected(&app, &state, s, &info)?;
    Ok(info.name)
}

/// Hand a `pubkyauth://` link to Pubky Ring on this device.
#[tauri::command]
fn open_ring(app: AppHandle, url: String) -> Result<(), String> {
    if !url.starts_with("pubkyauth://") {
        return Err("not a Pubky Ring link".into());
    }
    app.opener()
        .open_url(url, None::<&str>)
        .map_err(|e| e.to_string())
}

/// Forget the node (the connect page's "Connect to another node").
#[tauri::command]
fn forget(state: State<'_, AppState>) {
    state.store(None);
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // WebKitGTK's DMA-BUF renderer crashes the web process on some Linux
    // GPU stacks (NVIDIA, some Wayland compositors); the software path is
    // reliable and fast enough for this UI. An explicit setting wins.
    #[cfg(target_os = "linux")]
    if std::env::var_os("WEBKIT_DISABLE_DMABUF_RENDERER").is_none() {
        std::env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "1");
    }
    let filter = || tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into());
    #[cfg(not(target_os = "android"))]
    let _ = tracing_subscriber::fmt().with_env_filter(filter()).try_init();
    #[cfg(target_os = "android")]
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter())
        .with_writer(android::Logcat)
        .with_ansi(false)
        .without_time()
        .try_init();

    let app = tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![
            saved,
            open_saved,
            connect_manual,
            discover_nodes,
            ring_connect,
            open_ring,
            forget
        ])
        .setup(|app| {
            let path = app.path().app_config_dir()?.join("node.json");
            // PM_NODE/PM_TOKEN for scripts, then this app's file, then the
            // wry shell's file from before.
            let initial = std::env::var("PM_NODE")
                .ok()
                .map(|n| Settings::new(&n, &std::env::var("PM_TOKEN").unwrap_or_default()))
                .or_else(|| settings::load(&path))
                .or_else(|| settings::legacy_path().and_then(|p| settings::load(&p)));
            #[cfg(any(desktop, target_os = "android"))]
            let player_port = player::start();
            #[cfg(not(any(desktop, target_os = "android")))]
            let player_port = None;
            let shared = Arc::new(Mutex::new(initial.clone()));
            app.manage(AppState {
                settings: shared.clone(),
                path,
                player_port,
                tunnel: Default::default(),
            });

            let handle = app.handle().clone();
            let guard = shared.clone();
            let loads = shared;
            #[allow(unused_mut)]
            let mut builder = WebviewWindowBuilder::new(app, MAIN, WebviewUrl::App("index.html".into()))
                .initialization_script(init_script(initial.as_ref()))
                .on_navigation(move |u| allow_navigation(&handle, &guard, u))
                .on_page_load(move |w, payload| {
                    if !matches!(payload.event(), tauri::webview::PageLoadEvent::Finished) {
                        return;
                    }
                    let Some(s) = loads.lock().ok().and_then(|s| s.clone()) else {
                        return;
                    };
                    if s.origin().as_deref() == Some(payload.url().origin().ascii_serialization().as_str()) {
                        let _ = w.eval(token_sync_script(&s.token));
                    }
                });
            #[cfg(desktop)]
            {
                // `target="_blank"` and `window.open` (every "Open on …" link
                // in the UI): the system browser, never a second app window.
                // Android has no such request; its WebView navigates instead,
                // which the navigation guard above handles.
                let handle = app.handle().clone();
                builder = builder.on_new_window(move |u, _features| {
                    tracing::debug!(url = %u, "new window requested");
                    if matches!(u.scheme(), "http" | "https" | "mailto") {
                        if let Err(e) = handle.opener().open_url(u.as_str(), None::<&str>) {
                            tracing::warn!(url = %u, "cannot open externally: {e}");
                        }
                    }
                    tauri::webview::NewWindowResponse::Deny
                });
                builder = builder
                    .title("Pubky Music")
                    .inner_size(1280.0, 840.0)
                    .min_inner_size(420.0, 600.0);
            }
            #[allow(unused_variables)]
            let window = builder.build()?;
            #[cfg(target_os = "android")]
            android::init_tls(&window);
            Ok(())
        })
        .build(tauri::generate_context!());
    match app {
        Ok(app) => app.run(|_, _| {}),
        Err(e) => {
            tracing::error!("cannot start: {e}");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_token_goes_only_to_the_node_origin() {
        let s = Settings::new("http://nas.local:7788", "se\"cret");
        let js = init_script(Some(&s));
        assert!(
            js.contains(r#"var o="http://nas.local:7788",t="se\"cret""#),
            "{js}"
        );
        assert!(js.contains("location.origin===o"));
        // Nothing to hand over before the first connection.
        assert!(init_script(None).contains(r#"var o="",t="""#));
    }

    #[test]
    fn app_pages_are_recognised() {
        for u in [
            "tauri://localhost/index.html",
            "http://tauri.localhost/index.html",
            "about:blank",
        ] {
            assert!(is_app_page(&url::Url::parse(u).unwrap()), "{u}");
        }
        assert!(!is_app_page(&url::Url::parse("https://tidal.com/").unwrap()));
    }
}
