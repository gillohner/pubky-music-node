//! Spotify Soloist: Spotify's official headless Connect receiver, run by the node as an external
//! program (STEWARD-SOURCES-AGENT.md, item 5). Nothing of it ships with the node: each person
//! generates their own Soloist API key at developer.spotify.com, and the node downloads Soloist for
//! this machine from Spotify's own CDN when asked (Spotify forbids redistributing it). Once it
//! runs, it shows up in Spotify apps as a device and plays on this machine's sound output (PipeWire
//! or PulseAudio). Its audio never enters the node's zones (DRM): the node only starts, stops and
//! watches it, and reads what plays over Soloist's loopback control (`soloist ctl`).
//!
//! The key goes to Soloist the only way it takes it, on its command line, so it is visible to other
//! users of this machine while Soloist runs (Spotify's limitation, github.com/spotify/soloist/issues/12).

use crate::state::NodeState;
use crate::{ApiError, ApiResult};
use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub const STATE_KEY: &str = "spotify.soloist";
/// The owner's Soloist API key; never returned by the API, backed up as a secret.
pub const KEY_SETTING: &str = "spotify.soloist_key";
/// What the page says about the mode.
pub const NOTE: &str = "Spotify's official headless player. It appears in your Spotify apps as a device and plays on this machine's own sound output (PipeWire or PulseAudio), not on the node's zones. Making the key needs a Spotify Premium account; afterwards Free accounts can pick the device too. Spotify's builds stop working 90 days after they were made; the node tells you when to download a newer one.";
const DOWNLOADS: &str = "https://soloist-builds.spotifycdn.com";
/// Soloist's playback cache, in MB (its default is unlimited; SD cards are small).
const CACHE_MB: u32 = 1000;
/// Health from a failed start clears once the process has stayed up this long.
const SETTLED_SECS: i64 = 15;

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
    /// The Spotify Connect device name; the node's name when empty.
    #[serde(default)]
    pub device_name: String,
    /// `--pipewire-device`: a PipeWire node by name or id; the system default when empty.
    #[serde(default)]
    pub pipewire_device: String,
    #[serde(default)]
    pub health: Option<Health>,
    #[serde(default)]
    pub downloaded_at: Option<i64>,
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

fn api_key(lib: &pm_library::Library) -> Option<String> {
    lib.get_setting(KEY_SETTING)
        .ok()
        .flatten()
        .filter(|k| !k.trim().is_empty())
}

// ---- where Soloist is ----------------------------------------------------------------------------

struct Paths {
    /// `[spotify] soloist` / `PM_SOLOIST`.
    config: Option<PathBuf>,
    /// `<data_dir>/bin`, where a download goes.
    bin_dir: Option<PathBuf>,
    /// Tests: Spotify's CDN stood in.
    downloads: Option<String>,
}

fn paths() -> &'static std::sync::RwLock<Paths> {
    static P: std::sync::OnceLock<std::sync::RwLock<Paths>> = std::sync::OnceLock::new();
    P.get_or_init(|| {
        std::sync::RwLock::new(Paths {
            config: std::env::var("PM_SOLOIST")
                .ok()
                .filter(|v| !v.trim().is_empty())
                .map(PathBuf::from),
            bin_dir: None,
            downloads: None,
        })
    })
}

/// The Soloist named in node.toml (`[spotify] soloist`), set at start-up.
pub fn set_config_path(p: &str) {
    if !p.trim().is_empty() {
        paths().write().expect("soloist paths").config = Some(PathBuf::from(p.trim()));
    }
}

/// Where a downloaded Soloist lives: `<data_dir>/bin`.
pub fn set_data_dir(d: &Path) {
    paths().write().expect("soloist paths").bin_dir = Some(d.join("bin"));
}

/// Tests: download from here instead of Spotify's CDN.
pub fn set_downloads_base(url: &str) {
    paths().write().expect("soloist paths").downloads = Some(url.trim_end_matches('/').to_string());
}

fn downloaded_path() -> Option<PathBuf> {
    paths()
        .read()
        .expect("soloist paths")
        .bin_dir
        .as_ref()
        .map(|d| d.join("soloist"))
}

fn on_path() -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|d| d.join("soloist"))
        .find(|p| p.is_file())
}

/// The Soloist to run and where it came from: `config`, `downloaded` or `path`.
pub fn binary() -> Option<(PathBuf, &'static str)> {
    let cfg = paths().read().expect("soloist paths").config.clone();
    if let Some(p) = cfg.filter(|p| p.is_file()) {
        return Some((p, "config"));
    }
    if let Some(p) = downloaded_path().filter(|p| p.is_file()) {
        return Some((p, "downloaded"));
    }
    on_path().map(|p| (p, "path"))
}

/// Soloist runs on Linux (and Apple Silicon Macs); the node starts it only there.
fn supported() -> bool {
    cfg!(unix)
}

/// Spotify's archive for this machine (`soloist_release_<arch>.tar.gz`), Linux and macOS.
fn archive() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => Some("soloist_release_x86_64.tar.gz"),
        ("linux", "aarch64") => Some("soloist_release_arm64.tar.gz"),
        ("linux", "arm") => Some("soloist_release_arm32.tar.gz"),
        ("macos", "aarch64") => Some("soloist_release_osx_arm64.tar.gz"),
        _ => None,
    }
}

/// `soloist --version`, its first line.
fn version_of(bin: &Path) -> Option<String> {
    let out = std::process::Command::new(bin).arg("--version").output().ok()?;
    out.status.success().then(|| {
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .next()
            .unwrap_or("")
            .trim()
            .chars()
            .take(80)
            .collect::<String>()
    })
}

/// Soloist's own state (device identity, the paired session, the WebSocket port files).
fn soloist_dir(s: &NodeState) -> PathBuf {
    s.data_dir().join("soloist")
}

// ---- the running process -----------------------------------------------------------------------

#[derive(Default)]
struct Runner {
    /// Bumped by every start and stop; a watcher whose generation moved on neither restarts nor
    /// records anything.
    generation: u64,
    pid: Option<u32>,
    since: i64,
    stopping: bool,
    /// The last lines Soloist wrote, key redacted, for the health detail.
    tail: Vec<String>,
}

fn runner() -> &'static std::sync::Mutex<Runner> {
    static R: std::sync::OnceLock<std::sync::Mutex<Runner>> = std::sync::OnceLock::new();
    R.get_or_init(Default::default)
}

/// Whether the node's Soloist is running right now.
pub fn running() -> bool {
    runner().lock().expect("soloist runner").pid.is_some()
}

fn redact(line: &str, key: &str) -> String {
    if key.is_empty() {
        line.to_string()
    } else {
        line.replace(key, "<api key>")
    }
}

#[cfg(unix)]
fn signal(pid: u32, sig: i32) {
    // Our own child or the pid Soloist left in its data dir; never 0 or negative (process groups).
    if pid > 1 {
        unsafe {
            libc::kill(pid as i32, sig);
        }
    }
}

#[cfg(unix)]
fn alive(pid: u32) -> bool {
    pid > 1 && unsafe { libc::kill(pid as i32, 0) } == 0
}

/// A Soloist left behind by an earlier node process would keep the data dir locked ("another
/// process already using the data directory"): ask it to stop.
#[cfg(unix)]
fn stop_stale(dir: &Path) {
    let Some(pid) = std::fs::read_to_string(dir.join("soloist.pid"))
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
    else {
        return;
    };
    if Some(pid) == runner().lock().expect("soloist runner").pid || !alive(pid) {
        return;
    }
    tracing::info!(pid, "stopping a Soloist left by an earlier run");
    signal(pid, libc::SIGTERM);
    for _ in 0..30 {
        if !alive(pid) {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

/// Soloist's own words for why it stopped, from its last lines.
fn tail_detail(tail: &[String]) -> String {
    let lines: Vec<&str> = tail
        .iter()
        .rev()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .take(3)
        .collect();
    if lines.is_empty() {
        String::new()
    } else {
        format!(": {}", lines.into_iter().rev().collect::<Vec<_>>().join(" · "))
    }
}

fn record_health(lib: &Arc<std::sync::Mutex<pm_library::Library>>, ok: bool, detail: String) {
    let lib = lib.lock().expect("library poisoned");
    let mut st = load(&lib);
    st.health = Some(Health {
        ok,
        detail,
        at: now(),
    });
    save(&lib, &st);
}

/// Start Soloist for this node, supervised: the watcher thread restarts it after a crash (with a
/// pause, up to five times in a row), stops on an expired build, and records why it stopped.
pub fn start(s: &NodeState) -> Result<(), String> {
    if !supported() {
        return Err("Spotify Soloist runs on Linux and macOS only".into());
    }
    set_data_dir(&s.data_dir());
    let (key, st) = {
        let lib = s.library.lock().expect("library poisoned");
        (api_key(&lib), load(&lib))
    };
    let key = key.ok_or("Save your Spotify Soloist API key first")?;
    let (bin, _) = binary().ok_or("Download Soloist first")?;
    let name = if st.device_name.trim().is_empty() {
        s.name.clone()
    } else {
        st.device_name.trim().to_string()
    };
    let dir = soloist_dir(s);
    std::fs::create_dir_all(dir.join("cache")).map_err(|e| format!("Making {}: {e}", dir.display()))?;
    #[cfg(unix)]
    stop_stale(&dir);
    let generation = {
        let mut r = runner().lock().expect("soloist runner");
        r.generation += 1;
        r.stopping = false;
        r.tail.clear();
        r.generation
    };
    let lib = s.library.clone();
    let pipewire = st.pipewire_device.trim().to_string();
    std::thread::Builder::new()
        .name("soloist".into())
        .spawn(move || watch(generation, bin, key, name, pipewire, dir, lib))
        .map_err(|e| format!("Starting the Soloist watcher: {e}"))?;
    // Give the watcher a moment to spawn it, so the answer to the request already says so
    for _ in 0..20 {
        if runner().lock().expect("soloist runner").pid.is_some() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn watch(
    generation: u64,
    bin: PathBuf,
    key: String,
    name: String,
    pipewire: String,
    dir: PathBuf,
    lib: Arc<std::sync::Mutex<pm_library::Library>>,
) {
    use std::io::BufRead;
    let mut failures = 0u32;
    loop {
        let mut cmd = std::process::Command::new(&bin);
        cmd.arg("--device-name")
            .arg(&name)
            .arg("--api-key")
            .arg(&key)
            .arg("--data-dir")
            .arg(&dir)
            .arg("--cache-dir")
            .arg(dir.join("cache"))
            .arg("--cache-size")
            .arg(CACHE_MB.to_string())
            .arg("--ws")
            .arg("127.0.0.1:0")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped());
        if !pipewire.is_empty() {
            cmd.arg("--pipewire-device").arg(&pipewire);
        }
        #[cfg(target_os = "linux")]
        {
            use std::os::unix::process::CommandExt;
            // Soloist dies with the node, so no orphan keeps the data dir and the speaker.
            unsafe {
                cmd.pre_exec(|| {
                    libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
                    Ok(())
                });
            }
        }
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                record_health(&lib, false, format!("Starting Soloist: {e}"));
                return;
            }
        };
        let pid = child.id();
        {
            let mut r = runner().lock().expect("soloist runner");
            if r.generation != generation {
                let _ = child.kill();
                let _ = child.wait();
                return;
            }
            r.pid = Some(pid);
            r.since = now();
            r.tail.clear();
        }
        tracing::info!(pid, device = %name, "Spotify Soloist started");
        if let Some(err) = child.stderr.take() {
            for line in std::io::BufReader::new(err).lines().map_while(Result::ok) {
                let line = redact(&line, &key);
                tracing::debug!(target: "soloist", "{line}");
                let mut r = runner().lock().expect("soloist runner");
                if r.tail.len() >= 30 {
                    r.tail.remove(0);
                }
                r.tail.push(line);
            }
        }
        let status = child.wait();
        let code = status.as_ref().ok().and_then(|s| s.code());
        let (mine, stopping, tail, ran_for) = {
            let mut r = runner().lock().expect("soloist runner");
            let ran_for = now() - r.since;
            if r.pid == Some(pid) {
                r.pid = None;
            }
            (
                r.generation == generation,
                r.stopping,
                std::mem::take(&mut r.tail),
                ran_for,
            )
        };
        if !mine || stopping {
            return;
        }
        let detail = match code {
            Some(10) => "This Soloist build has expired (Spotify's builds stop working 90 days after they were made). Download a newer build, then switch it on again.".to_string(),
            Some(c) => format!("Soloist stopped (exit {c}){}", tail_detail(&tail)),
            None => format!("Soloist was killed{}", tail_detail(&tail)),
        };
        tracing::warn!(pid, ?code, "Spotify Soloist stopped: {detail}");
        record_health(&lib, false, detail);
        if code == Some(10) {
            return;
        }
        // A run that lasted a while was fine until now: count failures afresh.
        failures = if ran_for > 60 { 1 } else { failures + 1 };
        if failures > 5 {
            record_health(
                &lib,
                false,
                format!(
                    "Soloist stopped five times in a row (last: exit {}){}. Switch it on again to retry.",
                    code.map(|c| c.to_string()).unwrap_or_else(|| "none".into()),
                    tail_detail(&tail)
                ),
            );
            return;
        }
        std::thread::sleep(std::time::Duration::from_secs(5 * u64::from(failures)));
        let (enabled, mine) = (
            load(&lib.lock().expect("library poisoned")).enabled,
            runner().lock().expect("soloist runner").generation == generation,
        );
        if !enabled || !mine {
            return;
        }
    }
}

/// Stop the node's Soloist and wait (briefly) for it to go.
pub fn stop() {
    let pid = {
        let mut r = runner().lock().expect("soloist runner");
        r.stopping = true;
        r.generation += 1;
        r.pid
    };
    let Some(pid) = pid else { return };
    #[cfg(unix)]
    {
        signal(pid, libc::SIGTERM);
        for _ in 0..30 {
            if runner().lock().expect("soloist runner").pid != Some(pid) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        signal(pid, libc::SIGKILL);
    }
    for _ in 0..20 {
        if runner().lock().expect("soloist runner").pid != Some(pid) {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

/// At node start: run Soloist again if it was on.
pub fn autostart(s: &NodeState) {
    set_data_dir(&s.data_dir());
    if !load(&s.library.lock().expect("library poisoned")).enabled {
        return;
    }
    if let Err(e) = start(s) {
        tracing::info!("Spotify Soloist is on but can't start yet: {e}");
    }
}

// ---- what plays, over soloist ctl --------------------------------------------------------------

/// `soloist ctl -D <dir> <args>`: stdout on success, else ctl's exit code (2: the daemon's control
/// port isn't up, 3: not paired / no answer) and its last line.
fn ctl(bin: &Path, dir: &Path, args: &[&str]) -> Result<String, (i32, String)> {
    let out = std::process::Command::new(bin)
        .arg("ctl")
        .arg("-D")
        .arg(dir)
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| (-1, e.to_string()))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        let err = String::from_utf8_lossy(&out.stderr);
        Err((
            out.status.code().unwrap_or(-1),
            err.lines().last().unwrap_or("").trim().to_string(),
        ))
    }
}

/// What Soloist plays, from its `playback_state`.
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct Now {
    /// `idle`, `playing`, `paused` or `buffering`.
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub artists: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub album: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uri: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub position_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub volume: Option<u64>,
    /// Soloist is the active Spotify Connect device.
    pub is_active: bool,
}

fn entity_name(e: &Value) -> Option<String> {
    e["decorations"]["identity"]["name"]
        .as_str()
        .filter(|n| !n.is_empty())
        .map(str::to_string)
}

/// Soloist's `playback_state` as the page shows it.
pub fn now_from(v: &Value) -> Now {
    let item = &v["item"];
    let deco = &item["decorations"];
    let covers = deco["visual_identity"]["cover"].as_array();
    let image = covers.and_then(|c| {
        ["default", "large", "small", "xlarge"]
            .iter()
            .find_map(|want| {
                c.iter()
                    .find(|x| x["size"] == *want)
                    .and_then(|x| x["url"].as_str())
            })
            .or_else(|| c.first().and_then(|x| x["url"].as_str()))
            .map(str::to_string)
    });
    Now {
        status: v["status"].as_str().unwrap_or("idle").to_string(),
        title: entity_name(item),
        artists: deco["creators"]
            .as_array()
            .map(|a| a.iter().filter_map(entity_name).collect())
            .unwrap_or_default(),
        album: entity_name(&deco["parent"]["entity"]),
        image,
        uri: item["uri"].as_str().filter(|u| !u.is_empty()).map(str::to_string),
        duration_ms: deco["playback"]["duration_ms"].as_u64(),
        position_ms: v["position"]["position_ms"].as_u64(),
        volume: v["volume"].as_u64(),
        is_active: v["is_active"].as_bool().unwrap_or(false),
    }
}

/// `(paired, now)`: paired is `Some(false)` when Soloist answers that nobody has picked the device
/// yet, `None` when its control port isn't up.
fn now_playing(bin: &Path, dir: &Path) -> (Option<bool>, Option<Now>) {
    match ctl(bin, dir, &["now", "--json"]) {
        Ok(out) => match serde_json::from_str::<Value>(&out) {
            Ok(v) => (Some(true), Some(now_from(&v))),
            Err(_) => (Some(true), None),
        },
        Err((3, _)) => (Some(false), None),
        Err(_) => (None, None),
    }
}

// ---- the API -----------------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct Binary {
    pub path: String,
    /// `config`, `downloaded` or `path`.
    pub from: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SoloistStatus {
    /// This machine can run Soloist (Linux, macOS).
    pub supported: bool,
    pub enabled: bool,
    /// The owner's API key is saved (it never comes back).
    pub key_set: bool,
    pub device_name: String,
    pub pipewire_device: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub soloist: Option<Binary>,
    /// The node can download Soloist for this machine.
    pub can_download: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub downloaded_at: Option<i64>,
    pub running: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    /// On, but not running: what is still missing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub waiting_for: Option<String>,
    /// Somebody picked the device in a Spotify app (Soloist keeps that session).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub paired: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub now: Option<Now>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub health: Option<Health>,
    pub note: String,
}

async fn view(s: &NodeState) -> SoloistStatus {
    set_data_dir(&s.data_dir());
    let (mut st, key_set) = {
        let lib = s.library.lock().expect("library poisoned");
        (load(&lib), api_key(&lib).is_some())
    };
    let found = binary();
    let (running, pid, since) = {
        let r = runner().lock().expect("soloist runner");
        (r.pid.is_some(), r.pid, r.since)
    };
    if running && now() - since > SETTLED_SECS && st.health.as_ref().is_some_and(|h| !h.ok) {
        // A failure from before this run; it has stayed up since.
        st.health = None;
        save(&s.library.lock().expect("library poisoned"), &st);
    }
    let dir = soloist_dir(s);
    let (soloist, paired, now_playing) = match found {
        Some((p, from)) => {
            let pc = p.clone();
            let (version, paired, np) = tokio::task::spawn_blocking(move || {
                let v = version_of(&pc);
                let (paired, np) = if running {
                    now_playing(&pc, &dir)
                } else {
                    (None, None)
                };
                (v, paired, np)
            })
            .await
            .unwrap_or((None, None, None));
            (
                Some(Binary {
                    path: p.display().to_string(),
                    from: from.into(),
                    version,
                }),
                paired,
                np,
            )
        }
        None => (None, None, None),
    };
    let waiting_for = if st.enabled && !running {
        if !supported() {
            Some("a Linux or macOS machine: Soloist runs only there".to_string())
        } else if !key_set {
            Some("your Spotify Soloist API key".to_string())
        } else if soloist.is_none() {
            Some("Soloist itself (download it below)".to_string())
        } else {
            None
        }
    } else {
        None
    };
    SoloistStatus {
        supported: supported(),
        enabled: st.enabled,
        key_set,
        device_name: if st.device_name.trim().is_empty() {
            s.name.clone()
        } else {
            st.device_name.clone()
        },
        pipewire_device: st.pipewire_device.clone(),
        soloist,
        can_download: archive().is_some(),
        downloaded_at: st.downloaded_at,
        running,
        pid,
        waiting_for,
        paired,
        now: now_playing,
        health: st.health,
        note: NOTE.into(),
    }
}

/// `GET /v1/sources/spotify/soloist`.
pub(crate) async fn get_status(State(s): State<Arc<NodeState>>) -> ApiResult<SoloistStatus> {
    Ok(Json(view(&s).await))
}

#[derive(Debug, Deserialize)]
pub struct SoloistUpdate {
    pub enabled: Option<bool>,
    /// Empty forgets it.
    pub api_key: Option<String>,
    pub device_name: Option<String>,
    pub pipewire_device: Option<String>,
}

/// `PUT /v1/sources/spotify/soloist {enabled?, api_key?, device_name?, pipewire_device?}`: the
/// switch, the owner's key (checked for shape only: Soloist itself says whether Spotify takes it),
/// the device name and output. A change while it runs restarts it.
pub(crate) async fn put(
    State(s): State<Arc<NodeState>>,
    Json(u): Json<SoloistUpdate>,
) -> ApiResult<SoloistStatus> {
    let mut restart = false;
    let enabled = {
        let lib = s.library.lock().expect("library poisoned");
        let mut st = load(&lib);
        if let Some(k) = u.api_key.as_deref() {
            let k = k.trim();
            if k.is_empty() {
                lib.delete_setting(KEY_SETTING)?;
            } else {
                if k.len() < 16 || k.len() > 512 || !k.bytes().all(|b| b.is_ascii_graphic()) {
                    return Err(ApiError::bad_request(
                        "That doesn't look like a Spotify Soloist API key: paste the whole key from developer.spotify.com/dashboard/soloist",
                    ));
                }
                lib.set_setting(KEY_SETTING, k)?;
            }
            restart = true;
        }
        if let Some(n) = u.device_name.as_deref() {
            let n = n.trim();
            if n.chars().count() > 64 || n.chars().any(char::is_control) {
                return Err(ApiError::bad_request("A device name is up to 64 characters"));
            }
            if n != st.device_name {
                st.device_name = n.to_string();
                restart = true;
            }
        }
        if let Some(p) = u.pipewire_device.as_deref() {
            let p = p.trim();
            if p.chars().count() > 200 || p.chars().any(char::is_control) {
                return Err(ApiError::bad_request("That PipeWire device name is too long"));
            }
            if p != st.pipewire_device {
                st.pipewire_device = p.to_string();
                restart = true;
            }
        }
        if let Some(e) = u.enabled {
            if e && !st.enabled {
                st.health = None;
            }
            if e {
                // On again is also the retry after a failure
                st.health = st.health.filter(|h| h.ok);
                restart = true;
            }
            st.enabled = e;
        }
        save(&lib, &st);
        st.enabled
    };
    let s2 = s.clone();
    tokio::task::spawn_blocking(move || {
        if enabled {
            if restart || !running() {
                stop();
                if let Err(e) = start(&s2) {
                    tracing::info!("Spotify Soloist can't start yet: {e}");
                }
            }
        } else {
            stop();
        }
    })
    .await
    .ok();
    Ok(Json(view(&s).await))
}

#[derive(Debug, Deserialize)]
pub struct Control {
    /// `play`, `pause`, `next` or `prev`.
    pub command: String,
}

/// `POST /v1/sources/spotify/soloist/control {command}`: play, pause, next, prev through
/// `soloist ctl`.
pub(crate) async fn control(
    State(s): State<Arc<NodeState>>,
    Json(c): Json<Control>,
) -> ApiResult<SoloistStatus> {
    let cmd = match c.command.as_str() {
        "play" => "play",
        "pause" => "pause",
        "next" => "next",
        "prev" => "prev",
        _ => return Err(ApiError::bad_request("command: play, pause, next or prev")),
    };
    if !running() {
        return Err(ApiError::new(StatusCode::CONFLICT, "Soloist isn't running"));
    }
    let (bin, _) = binary().ok_or_else(|| ApiError::new(StatusCode::CONFLICT, "Soloist isn't installed"))?;
    let dir = soloist_dir(&s);
    tokio::task::spawn_blocking(move || ctl(&bin, &dir, &[cmd]))
        .await
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .map_err(|(code, line)| {
            ApiError::new(
                StatusCode::BAD_GATEWAY,
                match code {
                    3 => "Nobody has picked this device in a Spotify app yet".to_string(),
                    2 => "Soloist's control port isn't up yet; try again in a moment".to_string(),
                    _ => format!("Soloist: {line}"),
                },
            )
        })?;
    Ok(Json(view(&s).await))
}

/// `POST /v1/sources/spotify/soloist/binary`: download Soloist for this machine from Spotify's CDN
/// into `<data_dir>/bin`, because the owner pressed the button. Spotify publishes no checksums; the
/// download is checked by running it once (`--version`). An update replaces it and restarts.
pub(crate) async fn install(State(s): State<Arc<NodeState>>) -> ApiResult<SoloistStatus> {
    set_data_dir(&s.data_dir());
    let name = archive().ok_or_else(|| {
        ApiError::bad_request(
            "Spotify offers no Soloist build for this machine (Linux x86_64, ARM, or Apple Silicon): install it yourself and point [spotify] soloist at it",
        )
    })?;
    let base = paths()
        .read()
        .expect("soloist paths")
        .downloads
        .clone()
        .unwrap_or_else(|| DOWNLOADS.into());
    let up = |e: String| ApiError::new(StatusCode::BAD_GATEWAY, format!("Downloading Soloist: {e}"));
    let url = format!("{base}/{name}");
    let r = s
        .http_client()
        .get(&url)
        .send()
        .await
        .map_err(|e| up(e.without_url().to_string()))?;
    if !r.status().is_success() {
        return Err(up(format!("{name} answered {}", r.status())));
    }
    let bytes = r.bytes().await.map_err(|e| up(e.without_url().to_string()))?;
    let dest = downloaded_path().expect("data dir set");
    let bin_dir = dest.parent().expect("bin dir").to_path_buf();
    let was_running = running();
    let unpack = move || -> Result<(), String> {
        std::fs::create_dir_all(&bin_dir).map_err(|e| e.to_string())?;
        let work = bin_dir.join("soloist.download");
        let _ = std::fs::remove_dir_all(&work);
        std::fs::create_dir_all(&work).map_err(|e| e.to_string())?;
        let archive = work.join("soloist.tar.gz");
        std::fs::write(&archive, &bytes).map_err(|e| e.to_string())?;
        let out = std::process::Command::new("tar")
            .arg("-xzf")
            .arg(&archive)
            .arg("-C")
            .arg(&work)
            .arg("--no-same-owner")
            .output()
            .map_err(|e| format!("running tar: {e}"))?;
        if !out.status.success() {
            let _ = std::fs::remove_dir_all(&work);
            return Err(format!(
                "the archive didn't unpack: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        // The executable sits at the top of the archive, or one folder down
        let found = std::fs::read_dir(&work)
            .map_err(|e| e.to_string())?
            .flatten()
            .flat_map(|e| {
                let p = e.path();
                if p.is_dir() {
                    std::fs::read_dir(&p)
                        .map(|d| d.flatten().map(|e| e.path()).collect::<Vec<_>>())
                        .unwrap_or_default()
                } else {
                    vec![p]
                }
            })
            .find(|p| p.is_file() && p.file_name().is_some_and(|n| n == "soloist"));
        let Some(exe) = found else {
            let _ = std::fs::remove_dir_all(&work);
            return Err("the archive holds no `soloist` executable".into());
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755))
                .map_err(|e| e.to_string())?;
        }
        let version = version_of(&exe)
            .ok_or("the downloaded Soloist doesn't run on this machine (`soloist --version` failed)")?;
        if was_running {
            stop();
        }
        std::fs::rename(&exe, &dest).map_err(|e| e.to_string())?;
        let _ = std::fs::remove_dir_all(&work);
        tracing::info!(path = %dest.display(), %version, "Spotify Soloist downloaded from Spotify's CDN");
        Ok(())
    };
    tokio::task::spawn_blocking(unpack)
        .await
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .map_err(up)?;
    let enabled = {
        let lib = s.library.lock().expect("library poisoned");
        let mut st = load(&lib);
        st.downloaded_at = Some(now());
        if st.health.as_ref().is_some_and(|h| h.detail.contains("expired")) {
            st.health = None;
        }
        save(&lib, &st);
        st.enabled
    };
    if enabled && (was_running || !running()) {
        let s2 = s.clone();
        tokio::task::spawn_blocking(move || {
            if let Err(e) = start(&s2) {
                tracing::info!("Spotify Soloist can't start yet: {e}");
            }
        })
        .await
        .ok();
    }
    Ok(Json(view(&s).await))
}

/// `DELETE /v1/sources/spotify/soloist/binary`: remove the downloaded Soloist (one the owner
/// installed elsewhere stays). Stops it first when that is the one running.
pub(crate) async fn uninstall(State(s): State<Arc<NodeState>>) -> ApiResult<SoloistStatus> {
    set_data_dir(&s.data_dir());
    if let Some(p) = downloaded_path().filter(|p| p.is_file()) {
        if binary().is_some_and(|(_, from)| from == "downloaded") {
            tokio::task::spawn_blocking(stop).await.ok();
        }
        std::fs::remove_file(&p).map_err(|e| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Removing Soloist: {e}"),
            )
        })?;
        let lib = s.library.lock().expect("library poisoned");
        let mut st = load(&lib);
        st.downloaded_at = None;
        save(&lib, &st);
    }
    Ok(Json(view(&s).await))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn now_reads_soloists_playback_state() {
        let v: Value = serde_json::json!({
            "type": "playback_state", "status": "playing", "volume": 50, "is_active": true,
            "position": {"position_ms": 45000, "timestamp_ms": 1, "speed": 1.0},
            "item": {"uri": "spotify:track:69kOkLUCkxIZYexIgSG8rq", "entity_type": "track", "decorations": {
                "identity": {"name": "Get Lucky"},
                "creators": [{"decorations": {"identity": {"name": "Daft Punk"}}}, {"decorations": {"identity": {"name": "Pharrell Williams"}}}],
                "parent": {"entity": {"decorations": {"identity": {"name": "Random Access Memories"}}}},
                "visual_identity": {"cover": [{"url": "https://i.scdn.co/s", "size": "small"}, {"url": "https://i.scdn.co/d", "size": "default"}]},
                "playback": {"duration_ms": 369000}
            }}
        });
        let n = now_from(&v);
        assert_eq!(n.title.as_deref(), Some("Get Lucky"));
        assert_eq!(n.artists, vec!["Daft Punk", "Pharrell Williams"]);
        assert_eq!(n.album.as_deref(), Some("Random Access Memories"));
        assert_eq!(n.image.as_deref(), Some("https://i.scdn.co/d"));
        assert_eq!(
            (n.duration_ms, n.position_ms, n.volume),
            (Some(369000), Some(45000), Some(50))
        );
        assert!(n.is_active && n.status == "playing");
        // Idle, nothing loaded
        let n = now_from(
            &serde_json::json!({"type": "playback_state", "status": "idle", "item": {"uri": "", "entity_type": "unknown"}}),
        );
        assert_eq!((n.status.as_str(), n.title, n.artists.len()), ("idle", None, 0));
    }

    #[test]
    fn the_key_never_reaches_the_health_detail() {
        assert_eq!(
            redact("started with --api-key sk-123", "sk-123"),
            "started with --api-key <api key>"
        );
        assert_eq!(tail_detail(&["".into(), "a".into(), "b".into()]), ": a · b");
        assert_eq!(tail_detail(&[]), "");
    }
}
