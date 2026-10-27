//! Spotify through librespot, unofficial mode (STEWARD-DUAL-MODE.md item 6): the upstream
//! librespot project (github.com/librespot-org/librespot) run as an **external program**, never our
//! own reimplementation. librespot publishes no ready-made builds, so the owner installs it (`cargo
//! install librespot`, a distro package, Homebrew) and the node finds it: `[spotify] librespot`,
//! `PM_LIBRESPOT`, `<data_dir>/bin/librespot`, or the PATH.
//!
//! It shows up in Spotify apps as a Connect device (pairing is picking it there). Two outputs:
//! - **This machine's sound:** librespot's own audio backend, like Soloist;
//! - **A zone:** librespot's `pipe` backend writes 16-bit/44.1 kHz PCM to the node, which plays it
//!   on the zone the owner picked as the live item `librespot:connect`. librespot's events
//!   (`--onevent`) start, pause and stop the zone and carry the track's title.
//!
//! Opt-in behind a warning; a crash restarts it (up to five times in a row) and says why.

use crate::state::NodeState;
use crate::{ApiError, ApiResult};
use axum::extract::State;
use axum::Json;
use pm_model::{AudioBlock, AudioFormat};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};

pub const STATE_KEY: &str = "spotify.librespot";
/// The live item a zone plays while librespot pipes into it.
pub const LIVE_URI: &str = "librespot:connect";

/// The `--bitrate` librespot was last started with (kbps; 0 until it starts).
static REQUESTED_KBPS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
pub const WARNING: &str = "librespot is a community-made Spotify client, not approved by Spotify. It can stop working at any time, and Spotify could act on the account that picks it. It needs Spotify Premium.";
const FORMAT: AudioFormat = AudioFormat::pcm(44_100, 16, 2);
/// Frames per block handed to the zone.
const BLOCK_FRAMES: usize = 1024;
/// At most this much PCM waits for the zone (2 s); older audio is dropped.
const MAX_BUFFER: usize = 44_100 * 4 * 2;
const CACHE_LIMIT: &str = "1G";
/// The events librespot reports and the variables the node keeps from them.
const EVENT_VARS: &[&str] = &[
    "PLAYER_EVENT",
    "TRACK_ID",
    "URI",
    "NAME",
    "ARTISTS",
    "ALBUM",
    "COVERS",
    "DURATION_MS",
    "POSITION_MS",
    "USER_NAME",
    "CLIENT_NAME",
];

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
    pub device_name: String,
    /// `local` (librespot's own audio) or `zone`.
    #[serde(default)]
    pub output: String,
    /// The zone librespot plays into when `output` is `zone`.
    #[serde(default)]
    pub zone_id: String,
    /// Where `credentials.json` came from when the person put it there: `file` or `pasted`;
    /// none when librespot wrote it after a Connect sign-in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credentials_from: Option<String>,
    /// `320`, `160` or `96` kbps (Ogg Vorbis); empty = 320.
    #[serde(default)]
    pub bitrate: String,
    #[serde(default)]
    pub health: Option<Health>,
}

/// The bitrate librespot is asked for, as a `--bitrate` value.
pub fn bitrate_of(st: &ModeState) -> &'static str {
    match st.bitrate.as_str() {
        "160" => "160",
        "96" => "96",
        _ => "320",
    }
}

/// Pick the bitrate (`crate::quality`); librespot restarts when it runs.
pub(crate) async fn set_bitrate(s: &Arc<NodeState>, bitrate: &str) {
    let enabled = {
        let lib = s.library.lock().expect("library poisoned");
        let mut st = load(&lib);
        st.bitrate = bitrate.into();
        save(&lib, &st);
        st.enabled
    };
    if enabled && running() {
        let s2 = s.clone();
        tokio::task::spawn_blocking(move || {
            stop();
            if let Err(e) = start(&s2) {
                tracing::info!("librespot can't start yet: {e}");
            }
        })
        .await
        .ok();
    }
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

pub(crate) fn save(lib: &pm_library::Library, st: &ModeState) {
    let _ = lib.set_setting(STATE_KEY, &serde_json::to_string(st).expect("state serializes"));
}

// ---- where librespot is ------------------------------------------------------------------------

struct Paths {
    config: Option<PathBuf>,
    bin_dir: Option<PathBuf>,
}

fn paths() -> &'static std::sync::RwLock<Paths> {
    static P: std::sync::OnceLock<std::sync::RwLock<Paths>> = std::sync::OnceLock::new();
    P.get_or_init(|| {
        std::sync::RwLock::new(Paths {
            config: std::env::var("PM_LIBRESPOT")
                .ok()
                .filter(|v| !v.trim().is_empty())
                .map(PathBuf::from),
            bin_dir: None,
        })
    })
}

/// `[spotify] librespot` from node.toml, set at start-up.
pub fn set_config_path(p: &str) {
    if !p.trim().is_empty() {
        paths().write().expect("librespot paths").config = Some(PathBuf::from(p.trim()));
    }
}

pub fn set_data_dir(d: &Path) {
    paths().write().expect("librespot paths").bin_dir = Some(d.join("bin"));
}

/// The librespot to run and where it came from: `config`, `data` or `path`.
pub fn binary() -> Option<(PathBuf, &'static str)> {
    let (cfg, bin_dir) = {
        let p = paths().read().expect("librespot paths");
        (p.config.clone(), p.bin_dir.clone())
    };
    if let Some(p) = cfg.filter(|p| p.is_file()) {
        return Some((p, "config"));
    }
    if let Some(p) = bin_dir.map(|d| d.join("librespot")).filter(|p| p.is_file()) {
        return Some((p, "data"));
    }
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|d| d.join("librespot"))
        .find(|p| p.is_file())
        .map(|p| (p, "path"))
}

fn version_of(bin: &Path) -> Option<String> {
    let out = std::process::Command::new(bin).arg("--version").output().ok()?;
    let text = String::from_utf8_lossy(if out.stdout.is_empty() {
        &out.stderr
    } else {
        &out.stdout
    })
    .into_owned();
    out.status.success().then(|| {
        text.lines()
            .next()
            .unwrap_or("")
            .trim()
            .chars()
            .take(80)
            .collect()
    })
}

pub(crate) fn dir_of(s: &NodeState) -> PathBuf {
    s.data_dir().join("librespot")
}

/// librespot's `credentials.json` in its system cache: written by librespot after a Connect
/// sign-in, or put there from a file the person picked (`crate::imports`).
pub(crate) fn credentials_path(s: &NodeState) -> PathBuf {
    dir_of(s).join("credentials.json")
}

/// The username in the credentials file, when there is one (never its auth data).
pub(crate) fn credentials_user(s: &NodeState) -> Option<Option<String>> {
    let text = std::fs::read_to_string(credentials_path(s)).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    Some(v["username"].as_str().map(str::to_string))
}

/// Switch the mode on or off from the Sign in chooser; librespot starts or stops in the
/// background.
pub(crate) fn set_enabled_bg(s: &Arc<NodeState>, enabled: bool) {
    {
        let lib = s.library.lock().expect("library poisoned");
        let mut st = load(&lib);
        st.enabled = enabled;
        if enabled {
            st.health = st.health.filter(|h| h.ok);
        }
        save(&lib, &st);
    }
    // Tests share one librespot runner between them: no restarts from here in a test build
    #[cfg(not(test))]
    {
        let s2 = s.clone();
        std::thread::spawn(move || {
            stop();
            if enabled {
                if let Err(e) = start(&s2) {
                    tracing::info!("librespot can't start yet: {e}");
                }
            }
        });
    }
}

// ---- what plays (from librespot's events) ------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct Now {
    /// `playing`, `paused`, `stopped` or `loading`.
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
}

#[derive(Default)]
struct Runner {
    generation: u64,
    pid: Option<u32>,
    since: i64,
    stopping: bool,
    tail: Vec<String>,
    now: Now,
    /// Who picked the device (`session_connected`).
    user: Option<String>,
    /// A new title for the live item, taken once per change.
    title_changed: Option<String>,
    /// librespot runs with the pipe backend (zone output).
    piping: bool,
}

fn runner() -> &'static Mutex<Runner> {
    static R: std::sync::OnceLock<Mutex<Runner>> = std::sync::OnceLock::new();
    R.get_or_init(Default::default)
}

pub fn running() -> bool {
    runner().lock().expect("librespot runner").pid.is_some()
}

/// One `--onevent` call's variables, from the line the event script wrote (fields split by
/// `\x1f`, newlines inside a value turned into `\x1e`).
pub fn parse_event(line: &str) -> std::collections::HashMap<String, String> {
    line.split('\u{1f}')
        .filter_map(|f| f.split_once('='))
        .filter(|(_, v)| !v.is_empty())
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

fn list_of(v: Option<&String>) -> Vec<String> {
    v.map(|s| {
        s.split('\u{1e}')
            .map(str::trim)
            .filter(|x| !x.is_empty())
            .map(str::to_string)
            .collect()
    })
    .unwrap_or_default()
}

/// What an event does to the node's view of the playback; returns the zone action to take.
#[derive(Debug, PartialEq, Eq)]
pub enum ZoneAction {
    None,
    Play,
    Pause,
    Stop,
}

fn apply_event(ev: &std::collections::HashMap<String, String>) -> ZoneAction {
    let mut r = runner().lock().expect("librespot runner");
    match ev.get("PLAYER_EVENT").map(String::as_str) {
        Some("track_changed") => {
            let artists = list_of(ev.get("ARTISTS"));
            let title = ev.get("NAME").cloned();
            r.title_changed = title.as_ref().map(|t| {
                if artists.is_empty() {
                    t.clone()
                } else {
                    format!("{} – {t}", artists.join(", "))
                }
            });
            r.now = Now {
                status: if r.now.status.is_empty() {
                    "loading".into()
                } else {
                    r.now.status.clone()
                },
                title,
                artists,
                album: ev.get("ALBUM").cloned(),
                image: list_of(ev.get("COVERS")).into_iter().next(),
                uri: ev.get("URI").cloned(),
                duration_ms: ev.get("DURATION_MS").and_then(|d| d.parse().ok()),
            };
            ZoneAction::None
        }
        Some("playing") => {
            r.now.status = "playing".into();
            ZoneAction::Play
        }
        Some("paused") => {
            r.now.status = "paused".into();
            ZoneAction::Pause
        }
        Some("loading") => {
            r.now.status = "loading".into();
            ZoneAction::None
        }
        Some("stopped") => {
            r.now.status = "stopped".into();
            ZoneAction::Stop
        }
        Some("session_connected") => {
            r.user = ev.get("USER_NAME").cloned();
            ZoneAction::None
        }
        Some("session_disconnected") => {
            r.now = Now {
                status: "stopped".into(),
                ..Default::default()
            };
            ZoneAction::Stop
        }
        _ => ZoneAction::None,
    }
}

/// The zone follows librespot: playing starts (or resumes) the live item, paused pauses it,
/// stopped stops it. Nothing happens to a zone that has since moved on to something else.
fn follow(s: &NodeState, zone_id: &str, action: ZoneAction) {
    if action == ZoneAction::None || zone_id.is_empty() {
        return;
    }
    let Some(z) = s.zones.get(&pm_zones::ZoneId::new(zone_id)) else {
        return;
    };
    let st = z.state();
    let ours = st.now_playing.as_ref().is_some_and(|n| n.item.uri == LIVE_URI);
    match action {
        ZoneAction::Play if !ours => {
            let mut item = pm_zones::QueueItem::new(LIVE_URI);
            item.title = Some("Spotify Connect".into());
            item.artist = Some("librespot".into());
            z.clear();
            z.enqueue(vec![item], pm_zones::Insert::End);
            z.play_index(0);
        }
        ZoneAction::Play if st.transport != pm_zones::Transport::Playing => z.play(),
        ZoneAction::Pause if ours => z.pause(),
        ZoneAction::Stop if ours => z.stop(),
        _ => {}
    }
}

// ---- the PCM pipe ------------------------------------------------------------------------------

struct Bus {
    buf: Mutex<VecDeque<u8>>,
    cv: Condvar,
}

fn bus() -> &'static Bus {
    static B: std::sync::OnceLock<Bus> = std::sync::OnceLock::new();
    B.get_or_init(|| Bus {
        buf: Mutex::new(VecDeque::with_capacity(MAX_BUFFER)),
        cv: Condvar::new(),
    })
}

fn push_pcm(bytes: &[u8]) {
    let b = bus();
    let mut q = b.buf.lock().expect("librespot bus");
    q.extend(bytes);
    if q.len() > MAX_BUFFER {
        let len = q.len();
        let drop = ((len - MAX_BUFFER + 3) & !3).min(len);
        q.drain(..drop);
    }
    b.cv.notify_all();
}

/// 16-bit little-endian stereo bytes as the block layout (sign-extended `i32`).
pub fn pcm_block(bytes: &[u8]) -> AudioBlock {
    let samples = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| i32::from(i16::from_le_bytes(*c)))
        .collect();
    AudioBlock::pcm_i32(FORMAT, samples)
}

/// The live item: whatever librespot pipes, in blocks; silence while the pipe is quiet but
/// librespot plays on (a gap between tracks), the end once librespot has stopped.
pub struct LiveSource {
    title: Option<String>,
}

impl pm_zones::FrameSource for LiveSource {
    fn format(&self) -> AudioFormat {
        FORMAT
    }

    /// Ogg Vorbis from Spotify, at the bitrate librespot was started with. That is the bitrate
    /// *requested*: librespot falls back to a lower one when a track has no file at it.
    fn stream_info(&self) -> Option<pm_zones::StreamInfo> {
        let kbps = REQUESTED_KBPS.load(std::sync::atomic::Ordering::Relaxed);
        Some(pm_zones::StreamInfo {
            codec: "Vorbis".into(),
            bitrate_kbps: (kbps > 0).then_some(kbps),
            lossless: false,
            service: "spotify".into(),
            tier: None,
            preview: false,
        })
    }

    fn duration_ms(&self) -> Option<u64> {
        None
    }

    fn next_block(&mut self) -> Result<Option<AudioBlock>, pm_zones::SourceError> {
        let want = BLOCK_FRAMES * 4;
        let b = bus();
        let mut q = b.buf.lock().expect("librespot bus");
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(60);
        while q.len() < want {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                break;
            }
            q = b.cv.wait_timeout(q, left).expect("librespot bus").0;
        }
        let n = q.len().min(want) & !3;
        if n > 0 {
            let bytes: Vec<u8> = q.drain(..n).collect();
            return Ok(Some(pcm_block(&bytes)));
        }
        drop(q);
        let r = runner().lock().expect("librespot runner");
        if r.pid.is_none() || !r.piping || r.now.status == "stopped" {
            return Ok(None);
        }
        Ok(Some(AudioBlock::pcm_i32(FORMAT, vec![0; BLOCK_FRAMES * 2])))
    }

    fn seek_ms(&mut self, _ms: u64) -> Result<(), pm_zones::SourceError> {
        Ok(())
    }

    fn lossy_source(&self) -> bool {
        true
    }

    fn stream_title(&mut self) -> Option<String> {
        let t = runner().lock().expect("librespot runner").title_changed.take();
        if t.is_some() && t != self.title {
            self.title = t.clone();
            return t;
        }
        None
    }
}

/// `librespot:connect` for a zone.
pub(crate) fn open_live() -> Result<Box<dyn pm_zones::FrameSource>, pm_zones::SourceError> {
    let r = runner().lock().expect("librespot runner");
    if r.pid.is_none() || !r.piping {
        return Err(pm_zones::SourceError::Open {
            uri: LIVE_URI.into(),
            reason: "librespot isn't playing into a zone (Sources › Spotify › librespot)".into(),
        });
    }
    let title = r.now.title.clone();
    drop(r);
    // Start from what plays now, not from what waited
    bus().buf.lock().expect("librespot bus").clear();
    Ok(Box::new(LiveSource { title }))
}

// ---- the process -------------------------------------------------------------------------------

fn record_health(lib: &Arc<Mutex<pm_library::Library>>, ok: bool, detail: String) {
    let lib = lib.lock().expect("library poisoned");
    let mut st = load(&lib);
    st.health = Some(Health {
        ok,
        detail,
        at: now(),
    });
    save(&lib, &st);
}

#[cfg(unix)]
fn signal(pid: u32, sig: i32) {
    if pid > 1 {
        unsafe {
            libc::kill(pid as i32, sig);
        }
    }
}

/// The `--onevent` program: writes each event's variables as one line the node reads.
fn write_event_script(dir: &Path) -> Result<PathBuf, String> {
    let events = dir.join("events");
    let ev = events.to_string_lossy();
    if ev.contains('\'') {
        return Err(
            "the node's data folder has a quote in its path; librespot's events can't be read".into(),
        );
    }
    let vars = EVENT_VARS.join(" ");
    let script = format!(
        "#!/bin/sh\n# Written by the node: librespot's events, one line each\n{{ for v in {vars}; do eval \"x=\\${{$v-}}\"; printf '%s=%s\\037' \"$v\" \"$(printf '%s' \"$x\" | tr '\\n\\037' '\\036 ')\"; done; printf '\\n'; }} >> '{ev}'\n"
    );
    let path = dir.join("onevent.sh");
    std::fs::write(&path, script).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).map_err(|e| e.to_string())?;
    }
    Ok(path)
}

/// Start librespot, supervised.
pub fn start(s: &Arc<NodeState>) -> Result<(), String> {
    if !cfg!(unix) {
        return Err("The node runs librespot on Linux and macOS only".into());
    }
    set_data_dir(&s.data_dir());
    let st = load(&s.library.lock().expect("library poisoned"));
    let (bin, _) = binary().ok_or("Install librespot first")?;
    let piping = st.output == "zone";
    if piping && s.zones.get(&pm_zones::ZoneId::new(&st.zone_id)).is_none() {
        return Err("Pick the zone librespot plays into".into());
    }
    let name = if st.device_name.trim().is_empty() {
        s.name.clone()
    } else {
        st.device_name.trim().to_string()
    };
    let dir = dir_of(s);
    std::fs::create_dir_all(dir.join("cache")).map_err(|e| format!("Making {}: {e}", dir.display()))?;
    let script = write_event_script(&dir)?;
    let events = dir.join("events");
    let _ = std::fs::write(&events, "");
    let generation = {
        let mut r = runner().lock().expect("librespot runner");
        r.generation += 1;
        r.stopping = false;
        r.tail.clear();
        r.piping = piping;
        r.now = Now::default();
        r.user = None;
        r.generation
    };
    let s2 = s.clone();
    let zone = st.zone_id.clone();
    std::thread::Builder::new()
        .name("librespot".into())
        .spawn(move || watch(generation, bin, name, piping, dir, script, s2, zone))
        .map_err(|e| format!("Starting the librespot watcher: {e}"))?;
    let s3 = s.clone();
    let zone = st.zone_id.clone();
    std::thread::Builder::new()
        .name("librespot-events".into())
        .spawn(move || read_events(generation, events, s3, zone))
        .map_err(|e| format!("Starting the librespot events reader: {e}"))?;
    for _ in 0..20 {
        if running() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    Ok(())
}

fn read_events(generation: u64, path: PathBuf, s: Arc<NodeState>, zone: String) {
    use std::io::{Read, Seek};
    let mut offset = 0u64;
    let mut pending = String::new();
    loop {
        if runner().lock().expect("librespot runner").generation != generation {
            return;
        }
        if let Ok(mut f) = std::fs::File::open(&path) {
            let len = f.metadata().map(|m| m.len()).unwrap_or(0);
            if len < offset {
                offset = 0;
            }
            if len > offset && f.seek(std::io::SeekFrom::Start(offset)).is_ok() {
                let mut chunk = String::new();
                if let Ok(n) = f.read_to_string(&mut chunk) {
                    offset += n as u64;
                    pending.push_str(&chunk);
                    while let Some(i) = pending.find('\n') {
                        let line: String = pending.drain(..=i).collect();
                        let ev = parse_event(line.trim_end_matches('\n'));
                        let action = apply_event(&ev);
                        follow(&s, &zone, action);
                    }
                }
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
}

#[allow(clippy::too_many_arguments)]
fn watch(
    generation: u64,
    bin: PathBuf,
    name: String,
    piping: bool,
    dir: PathBuf,
    script: PathBuf,
    s: Arc<NodeState>,
    zone: String,
) {
    use std::io::{BufRead, Read};
    let lib = s.library.clone();
    let mut failures = 0u32;
    loop {
        let mut cmd = std::process::Command::new(&bin);
        cmd.arg("--name")
            .arg(&name)
            .arg("--device-type")
            .arg("speaker")
            .arg("--bitrate")
            .arg({
                // Use the same selection for the process and the player's label. Keep the
                // bitrate from this spawn rather than reading settings on every publish.
                let bitrate = bitrate_of(&load(&lib.lock().expect("library poisoned")));
                REQUESTED_KBPS.store(
                    bitrate.parse().expect("known librespot bitrate"),
                    std::sync::atomic::Ordering::Relaxed,
                );
                bitrate
            })
            .arg("--cache")
            .arg(dir.join("cache"))
            .arg("--system-cache")
            .arg(&dir)
            .arg("--cache-size-limit")
            .arg(CACHE_LIMIT)
            .arg("--onevent")
            .arg(&script)
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped());
        if piping {
            // Raw PCM on stdout into the node; the zone keeps its own volume
            cmd.args([
                "--backend",
                "pipe",
                "--format",
                "S16",
                "--mixer",
                "softvol",
                "--volume-ctrl",
                "fixed",
                "--initial-volume",
                "100",
            ])
            .stdout(std::process::Stdio::piped());
        } else {
            cmd.stdout(std::process::Stdio::null());
        }
        #[cfg(target_os = "linux")]
        {
            use std::os::unix::process::CommandExt;
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
                record_health(&lib, false, format!("Starting librespot: {e}"));
                return;
            }
        };
        let pid = child.id();
        {
            let mut r = runner().lock().expect("librespot runner");
            if r.generation != generation {
                let _ = child.kill();
                let _ = child.wait();
                return;
            }
            r.pid = Some(pid);
            r.since = now();
        }
        tracing::info!(pid, device = %name, piping, "librespot started");
        if let Some(mut out) = child.stdout.take() {
            std::thread::spawn(move || {
                let mut buf = vec![0u8; 16 * 1024];
                while let Ok(n) = out.read(&mut buf) {
                    if n == 0 {
                        break;
                    }
                    push_pcm(&buf[..n]);
                }
            });
        }
        if let Some(err) = child.stderr.take() {
            for line in std::io::BufReader::new(err).lines().map_while(Result::ok) {
                tracing::debug!(target: "librespot", "{line}");
                let mut r = runner().lock().expect("librespot runner");
                if r.tail.len() >= 30 {
                    r.tail.remove(0);
                }
                r.tail.push(line);
            }
        }
        let code = child.wait().ok().and_then(|st| st.code());
        let (mine, stopping, tail, ran_for) = {
            let mut r = runner().lock().expect("librespot runner");
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
        follow(&s, &zone, ZoneAction::Stop);
        if !mine || stopping {
            return;
        }
        let last: Vec<&str> = tail
            .iter()
            .rev()
            .map(|l| l.trim())
            .filter(|l| !l.is_empty())
            .take(2)
            .collect();
        let why = if last.is_empty() {
            String::new()
        } else {
            format!(": {}", last.into_iter().rev().collect::<Vec<_>>().join(" · "))
        };
        let detail = match code {
            Some(c) => format!("librespot stopped (exit {c}){why}"),
            None => format!("librespot was killed{why}"),
        };
        tracing::warn!(pid, ?code, "librespot stopped: {detail}");
        record_health(&lib, false, detail.clone());
        failures = if ran_for > 60 { 1 } else { failures + 1 };
        if failures > 5 {
            record_health(
                &lib,
                false,
                format!("{detail}. It stopped five times in a row; switch it on again to retry."),
            );
            return;
        }
        std::thread::sleep(std::time::Duration::from_secs(3 * u64::from(failures)));
        let enabled = load(&lib.lock().expect("library poisoned")).enabled;
        if !enabled || runner().lock().expect("librespot runner").generation != generation {
            return;
        }
    }
}

pub fn stop() {
    let pid = {
        let mut r = runner().lock().expect("librespot runner");
        r.stopping = true;
        r.generation += 1;
        r.pid
    };
    let Some(pid) = pid else { return };
    #[cfg(unix)]
    {
        signal(pid, libc::SIGTERM);
        for _ in 0..30 {
            if runner().lock().expect("librespot runner").pid != Some(pid) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        signal(pid, libc::SIGKILL);
    }
    for _ in 0..20 {
        if runner().lock().expect("librespot runner").pid != Some(pid) {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

pub fn autostart(s: &Arc<NodeState>) {
    set_data_dir(&s.data_dir());
    if !load(&s.library.lock().expect("library poisoned")).enabled {
        return;
    }
    if let Err(e) = start(s) {
        tracing::info!("librespot is on but can't start yet: {e}");
    }
}

// ---- the API -----------------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct Binary {
    pub path: String,
    pub from: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ZoneChoice {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct LibrespotStatus {
    pub supported: bool,
    pub enabled: bool,
    pub device_name: String,
    /// `local` or `zone`.
    pub output: String,
    pub zone_id: String,
    /// `320`, `160` or `96` kbps.
    pub bitrate: String,
    pub zones: Vec<ZoneChoice>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub librespot: Option<Binary>,
    pub running: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub waiting_for: Option<String>,
    /// The Spotify account that picked the device.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub now: Option<Now>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub health: Option<Health>,
    pub warning: String,
}

async fn view(s: &NodeState) -> LibrespotStatus {
    set_data_dir(&s.data_dir());
    let mut st = load(&s.library.lock().expect("library poisoned"));
    let (running, pid, since, user, now_playing) = {
        let r = runner().lock().expect("librespot runner");
        (
            r.pid.is_some(),
            r.pid,
            r.since,
            r.user.clone(),
            (!r.now.status.is_empty()).then(|| r.now.clone()),
        )
    };
    if running && now() - since > 15 && st.health.as_ref().is_some_and(|h| !h.ok) {
        st.health = None;
        save(&s.library.lock().expect("library poisoned"), &st);
    }
    let librespot = match binary() {
        Some((p, from)) => {
            let pc = p.clone();
            let version = tokio::task::spawn_blocking(move || version_of(&pc))
                .await
                .ok()
                .flatten();
            Some(Binary {
                path: p.display().to_string(),
                from: from.into(),
                version,
            })
        }
        None => None,
    };
    let output = if st.output == "zone" { "zone" } else { "local" }.to_string();
    let zones: Vec<ZoneChoice> = s
        .zones
        .list()
        .into_iter()
        .map(|z| ZoneChoice {
            id: z.id.0.clone(),
            name: z.name.clone(),
        })
        .collect();
    let waiting_for = (st.enabled && !running)
        .then(|| {
            if !cfg!(unix) {
                Some("a Linux or macOS machine".to_string())
            } else if librespot.is_none() {
                Some("librespot itself (install it on this machine)".to_string())
            } else if output == "zone" && !zones.iter().any(|z| z.id == st.zone_id) {
                Some("a zone to play into".to_string())
            } else {
                None
            }
        })
        .flatten();
    LibrespotStatus {
        supported: cfg!(unix),
        enabled: st.enabled,
        device_name: if st.device_name.trim().is_empty() {
            s.name.clone()
        } else {
            st.device_name.clone()
        },
        output,
        zone_id: st.zone_id.clone(),
        bitrate: bitrate_of(&st).into(),
        zones,
        librespot,
        running,
        pid,
        waiting_for,
        user,
        now: now_playing,
        health: st.health,
        warning: WARNING.into(),
    }
}

/// `GET /v1/sources/spotify/librespot`.
pub(crate) async fn get_status(State(s): State<Arc<NodeState>>) -> ApiResult<LibrespotStatus> {
    Ok(Json(view(&s).await))
}

#[derive(Debug, Deserialize)]
pub struct LibrespotUpdate {
    pub enabled: Option<bool>,
    pub device_name: Option<String>,
    /// `local` or `zone`.
    pub output: Option<String>,
    pub zone_id: Option<String>,
}

/// `PUT /v1/sources/spotify/librespot {enabled?, device_name?, output?, zone_id?}`; a change while it
/// runs restarts it.
pub(crate) async fn put(
    State(s): State<Arc<NodeState>>,
    Json(u): Json<LibrespotUpdate>,
) -> ApiResult<LibrespotStatus> {
    let mut restart = false;
    let enabled = {
        let lib = s.library.lock().expect("library poisoned");
        let mut st = load(&lib);
        if let Some(n) = u.device_name.as_deref() {
            let n = n.trim();
            if n.chars().count() > 64 || n.chars().any(char::is_control) {
                return Err(ApiError::bad_request("A device name is up to 64 characters"));
            }
            restart |= n != st.device_name;
            st.device_name = n.into();
        }
        if let Some(o) = u.output.as_deref() {
            if o != "local" && o != "zone" {
                return Err(ApiError::bad_request("output is local or zone"));
            }
            restart |= o != st.output;
            st.output = o.into();
        }
        if let Some(z) = u.zone_id.as_deref() {
            if !z.is_empty() && s.zones.get(&pm_zones::ZoneId::new(z)).is_none() {
                return Err(ApiError::bad_request("No such zone"));
            }
            restart |= z != st.zone_id;
            st.zone_id = z.into();
        }
        if let Some(e) = u.enabled {
            if e {
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
                    tracing::info!("librespot can't start yet: {e}");
                }
            }
        } else {
            stop();
        }
    })
    .await
    .ok();
    if !enabled {
        let zone = load(&s.library.lock().expect("library poisoned")).zone_id;
        follow(&s, &zone, ZoneAction::Stop);
    }
    Ok(Json(view(&s).await))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_parse_and_pcm_becomes_blocks() {
        let ev = parse_event("PLAYER_EVENT=track_changed\u{1f}NAME=Get Lucky\u{1f}ARTISTS=Daft Punk\u{1e}Pharrell Williams\u{1f}ALBUM=RAM\u{1f}COVERS=https://i.scdn.co/a\u{1e}https://i.scdn.co/b\u{1f}DURATION_MS=369000\u{1f}POSITION_MS=\u{1f}");
        assert_eq!(ev.get("NAME").map(String::as_str), Some("Get Lucky"));
        assert!(!ev.contains_key("POSITION_MS"));
        assert_eq!(list_of(ev.get("ARTISTS")), vec!["Daft Punk", "Pharrell Williams"]);
        let b = pcm_block(&[0x01, 0x00, 0xff, 0xff, 0x00, 0x80, 0xff, 0x7f]);
        assert_eq!(b.format, FORMAT);
        assert_eq!(b.samples, pm_model::Samples::PcmI32(vec![1, -1, -32768, 32767]));
    }
}
