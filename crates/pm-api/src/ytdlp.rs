//! YouTube for the node's zones, unofficial mode (STEWARD-DUAL-MODE.md): the audio comes through
//! **yt-dlp as an external program**, never our own reimplementation of YouTube's signature
//! handling. The owner points the node at a yt-dlp they installed, or lets the node download it
//! from yt-dlp's official GitHub releases (checked against the release's SHA2-256SUMS). Opt-in
//! behind a warning; This browser keeps YouTube's official player.
//!
//! A `youtube:video:<id>` on a zone asks yt-dlp for the audio URL (AAC in MP4, which the decoder
//! reads) and streams it like any HTTP source.

use crate::state::NodeState;
use crate::{ApiError, ApiResult};
use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub const STATE_KEY: &str = "youtube.unofficial";
/// What the page says before the mode is switched on.
pub const WARNING: &str = "This plays YouTube on your speakers through yt-dlp, outside YouTube's own player. YouTube does not allow that in its terms, it can stop working at any time, and there are no ads to support the artists.";
const RELEASES: &str = "https://github.com/yt-dlp/yt-dlp/releases/latest/download";
/// The audio formats the decoder reads: AAC in MP4 first.
const FORMAT: &str = "bestaudio[ext=m4a]/bestaudio[acodec^=mp4a]";
const RESOLVE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(45);

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Health {
    pub ok: bool,
    pub detail: String,
    pub at: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ModeState {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub health: Option<Health>,
}

fn now() -> i64 {
    pm_library::db::now_unix()
}

pub fn load(lib: &pm_library::Library) -> ModeState {
    lib.get_setting(STATE_KEY)
        .ok()
        .flatten()
        .and_then(|v| serde_json::from_str(&v).ok())
        .unwrap_or_default()
}

fn save(lib: &pm_library::Library, st: &ModeState) {
    let _ = lib.set_setting(STATE_KEY, &serde_json::to_string(st).expect("state serializes"));
}

// ---- where yt-dlp is ------------------------------------------------------------------------------

struct Paths {
    /// `[youtube] ytdlp` / `PM_YTDLP`.
    config: Option<PathBuf>,
    /// `<data_dir>/bin`, where a download goes.
    bin_dir: Option<PathBuf>,
    /// Tests: GitHub's release downloads stood in.
    releases: Option<String>,
}

fn paths() -> &'static std::sync::RwLock<Paths> {
    static P: std::sync::OnceLock<std::sync::RwLock<Paths>> = std::sync::OnceLock::new();
    P.get_or_init(|| {
        std::sync::RwLock::new(Paths {
            config: std::env::var("PM_YTDLP")
                .ok()
                .filter(|v| !v.trim().is_empty())
                .map(PathBuf::from),
            bin_dir: None,
            releases: None,
        })
    })
}

/// The yt-dlp named in node.toml (`[youtube] ytdlp`), set at start-up.
pub fn set_config_path(p: &str) {
    if !p.trim().is_empty() {
        paths().write().expect("ytdlp paths").config = Some(PathBuf::from(p.trim()));
    }
}

/// Where a downloaded yt-dlp lives: `<data_dir>/bin`.
pub fn set_data_dir(d: &Path) {
    paths().write().expect("ytdlp paths").bin_dir = Some(d.join("bin"));
}

/// Tests: download from here instead of GitHub.
pub fn set_releases_base(url: &str) {
    paths().write().expect("ytdlp paths").releases = Some(url.trim_end_matches('/').to_string());
}

fn downloaded_path() -> Option<PathBuf> {
    let name = if cfg!(windows) { "yt-dlp.exe" } else { "yt-dlp" };
    paths()
        .read()
        .expect("ytdlp paths")
        .bin_dir
        .as_ref()
        .map(|d| d.join(name))
}

fn on_path() -> Option<PathBuf> {
    let name = if cfg!(windows) { "yt-dlp.exe" } else { "yt-dlp" };
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|d| d.join(name))
        .find(|p| p.is_file())
}

/// The yt-dlp to run and where it came from: `config`, `downloaded` or `path`.
pub fn binary() -> Option<(PathBuf, &'static str)> {
    let cfg = paths().read().expect("ytdlp paths").config.clone();
    if let Some(p) = cfg.filter(|p| p.is_file()) {
        return Some((p, "config"));
    }
    if let Some(p) = downloaded_path().filter(|p| p.is_file()) {
        return Some((p, "downloaded"));
    }
    on_path().map(|p| (p, "path"))
}

/// The mode is on and yt-dlp is there: zones can play YouTube.
pub fn zones_on_lib(lib: &pm_library::Library) -> bool {
    load(lib).enabled && binary().is_some()
}

pub fn zones_on(s: &NodeState) -> bool {
    zones_on_lib(&s.library.lock().expect("library poisoned"))
}

fn version_of(bin: &Path) -> Option<String> {
    let out = std::process::Command::new(bin).arg("--version").output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .filter(|v| !v.is_empty())
}

// ---- playback -------------------------------------------------------------------------------------

/// Ask yt-dlp for the audio URL of a video. Blocking (the audio thread).
pub fn resolve_blocking(bin: &Path, id: &str) -> Result<String, String> {
    if id.len() != 11
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err("bad YouTube video id".into());
    }
    let mut child = std::process::Command::new(bin)
        .args([
            "--no-playlist",
            "--no-warnings",
            "--quiet",
            "-f",
            FORMAT,
            "-g",
            "--",
        ])
        .arg(format!("https://www.youtube.com/watch?v={id}"))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("yt-dlp did not start: {e}"))?;
    let started = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if started.elapsed() > RESOLVE_TIMEOUT => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("yt-dlp took too long".into());
            }
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(100)),
            Err(e) => return Err(format!("yt-dlp: {e}")),
        }
    }
    let out = child.wait_with_output().map_err(|e| format!("yt-dlp: {e}"))?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    let url = stdout
        .lines()
        .map(str::trim)
        .find(|l| l.starts_with("https://") || l.starts_with("http://"));
    match (out.status.success(), url) {
        (true, Some(u)) => Ok(u.to_string()),
        _ => {
            let err = String::from_utf8_lossy(&out.stderr);
            let last = err
                .lines()
                .rev()
                .find(|l| !l.trim().is_empty())
                .unwrap_or("no audio URL")
                .trim();
            Err(format!("yt-dlp: {}", last.trim_start_matches("ERROR: ")))
        }
    }
}

fn record(library: &std::sync::Mutex<pm_library::Library>, r: &Result<String, String>) {
    let Ok(lib) = library.lock() else { return };
    let mut st = load(&lib);
    st.health = Some(Health {
        ok: r.is_ok(),
        detail: match r {
            Ok(_) => "YouTube is playing on the speakers".into(),
            Err(e) => e.clone(),
        },
        at: now(),
    });
    save(&lib, &st);
}

/// A `youtube:video:<id>` for a zone.
pub(crate) fn open_blocking(
    library: &std::sync::Mutex<pm_library::Library>,
    id: &str,
) -> Result<Box<dyn pm_zones::FrameSource>, pm_zones::SourceError> {
    let fail = |reason: String| pm_zones::SourceError::Open {
        uri: format!("{}{id}", crate::youtube::VIDEO_URI_PREFIX),
        reason,
    };
    let (bin, _) =
        binary().ok_or_else(|| fail("yt-dlp is not on this node any more (Sources › YouTube)".into()))?;
    let r = resolve_blocking(&bin, id);
    record(library, &r);
    crate::sources::open_http_stream(&r.map_err(fail)?)
        .map_err(|_| fail("the YouTube stream did not open".into()))
}

// ---- status, settings, download --------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct YtDlp {
    pub path: String,
    /// `config`, `downloaded` or `path`.
    pub from: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ZonesStatus {
    pub enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ytdlp: Option<YtDlp>,
    /// The node can download yt-dlp for this machine.
    pub can_download: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub health: Option<Health>,
    pub warning: String,
}

/// The official release asset for this machine.
fn asset() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => Some("yt-dlp_linux"),
        ("linux", "aarch64") => Some("yt-dlp_linux_aarch64"),
        ("linux", "arm") => Some("yt-dlp_linux_armv7l"),
        ("macos", _) => Some("yt-dlp_macos"),
        ("windows", "x86_64") => Some("yt-dlp.exe"),
        _ => None,
    }
}

async fn view(s: &NodeState) -> ZonesStatus {
    set_data_dir(&s.data_dir());
    let st = load(&s.library.lock().expect("library poisoned"));
    let found = binary();
    let ytdlp = match found {
        Some((p, from)) => {
            let pc = p.clone();
            let version = tokio::task::spawn_blocking(move || version_of(&pc))
                .await
                .ok()
                .flatten();
            Some(YtDlp {
                path: p.display().to_string(),
                from: from.into(),
                version,
            })
        }
        None => None,
    };
    ZonesStatus {
        enabled: st.enabled,
        ytdlp,
        can_download: asset().is_some(),
        health: st.health,
        warning: WARNING.into(),
    }
}

/// `GET /v1/sources/youtube/unofficial`.
pub(crate) async fn get_status(State(s): State<Arc<NodeState>>) -> ApiResult<ZonesStatus> {
    Ok(Json(view(&s).await))
}

#[derive(Debug, Deserialize)]
pub struct ZonesUpdate {
    pub enabled: Option<bool>,
}

/// `PUT /v1/sources/youtube/unofficial {enabled}`.
pub(crate) async fn put(
    State(s): State<Arc<NodeState>>,
    Json(u): Json<ZonesUpdate>,
) -> ApiResult<ZonesStatus> {
    if let Some(e) = u.enabled {
        let lib = s.library.lock().expect("library poisoned");
        let mut st = load(&lib);
        st.enabled = e;
        if e {
            st.health = None;
        }
        save(&lib, &st);
    }
    Ok(Json(view(&s).await))
}

fn sha256_hex(b: &[u8]) -> String {
    use sha2::Digest;
    sha2::Sha256::digest(b)
        .iter()
        .map(|x| format!("{x:02x}"))
        .collect()
}

/// `POST /v1/sources/youtube/unofficial/ytdlp`: download yt-dlp for this machine from its
/// official GitHub releases into `<data_dir>/bin`, after checking it against the release's
/// SHA2-256SUMS. The owner asked for it on the page; nothing downloads on its own.
pub(crate) async fn install(State(s): State<Arc<NodeState>>) -> ApiResult<ZonesStatus> {
    set_data_dir(&s.data_dir());
    let name = asset().ok_or_else(|| {
        ApiError::bad_request(
            "yt-dlp has no ready-made download for this machine: install it yourself and restart",
        )
    })?;
    let base = paths()
        .read()
        .expect("ytdlp paths")
        .releases
        .clone()
        .unwrap_or_else(|| RELEASES.into());
    let fetch = |url: String| {
        let c = s.http_client();
        async move {
            let r = c
                .get(&url)
                .send()
                .await
                .map_err(|e| e.without_url().to_string())?;
            if !r.status().is_success() {
                return Err(format!(
                    "{} answered {}",
                    url.rsplit('/').next().unwrap_or(""),
                    r.status()
                ));
            }
            r.bytes().await.map_err(|e| e.without_url().to_string())
        }
    };
    let up = |e: String| ApiError::new(StatusCode::BAD_GATEWAY, format!("Downloading yt-dlp: {e}"));
    let sums = fetch(format!("{base}/SHA2-256SUMS")).await.map_err(up)?;
    let want = String::from_utf8_lossy(&sums)
        .lines()
        .find_map(|l| {
            let (h, f) = l.split_once(char::is_whitespace)?;
            (f.trim().trim_start_matches('*') == name).then(|| h.trim().to_ascii_lowercase())
        })
        .ok_or_else(|| up(format!("the release lists no checksum for {name}")))?;
    let bin = fetch(format!("{base}/{name}")).await.map_err(up)?;
    if sha256_hex(&bin) != want {
        return Err(up(
            "the download doesn't match the release's checksum; nothing was installed".into(),
        ));
    }
    let dest = downloaded_path().expect("data dir set");
    let part = dest.with_extension("part");
    let write = || -> std::io::Result<()> {
        std::fs::create_dir_all(dest.parent().expect("bin dir"))?;
        std::fs::write(&part, &bin)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&part, std::fs::Permissions::from_mode(0o755))?;
        }
        std::fs::rename(&part, &dest)
    };
    write().map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, format!("Saving yt-dlp: {e}")))?;
    tracing::info!(path = %dest.display(), "yt-dlp downloaded from its official releases");
    Ok(Json(view(&s).await))
}

/// `DELETE /v1/sources/youtube/unofficial/ytdlp`: remove the downloaded yt-dlp (one the owner
/// installed elsewhere stays).
pub(crate) async fn uninstall(State(s): State<Arc<NodeState>>) -> ApiResult<ZonesStatus> {
    set_data_dir(&s.data_dir());
    if let Some(p) = downloaded_path().filter(|p| p.is_file()) {
        std::fs::remove_file(&p)
            .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, format!("Removing yt-dlp: {e}")))?;
    }
    Ok(Json(view(&s).await))
}
