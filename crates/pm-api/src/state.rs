//! Shared node state behind the API: library, outputs, zones, event fan-out.

use crate::sources::NodeSources;

/// A running sleep timer on a zone.
pub struct SleepTimer {
    /// Unix time in milliseconds when the zone pauses.
    pub ends_at_ms: u64,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for SleepTimer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
use crate::types::ApiEvent;
use pm_audio::PipelineConfig;
use pm_library::Library;
use pm_zones::{Output, OutputId, OutputRegistry, Zone, ZoneId, Zones};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::sync::broadcast;

/// Static settings for [`NodeState::new`].
#[derive(Debug, Clone)]
pub struct NodeSettings {
    pub name: String,
    pub version: String,
    /// Path of the SQLite file, so a scan can use its own connection.
    pub db_path: PathBuf,
    pub scan_folders: Vec<PathBuf>,
    /// Where extracted cover art is written.
    pub art_dir: PathBuf,
    /// Where files from the homeserver's private storage are mirrored (a library folder).
    pub mirror_dir: PathBuf,
    pub pipeline: PipelineConfig,
    /// Optional bearer token; `None` = no auth (bind to localhost only).
    pub token: Option<String>,
    /// Username OpenSubsonic clients log in with (password = token).
    pub subsonic_user: String,
    /// Pinakes/Nexus settings; discovery is enabled when the contact validates.
    pub pubky: pm_pubky::PmPubkyConfig,
    /// How renderers reach this node's HTTP API (pull stream URLs); empty = `http://<listen>`.
    pub public_url: String,
    /// Discover UPnP/OpenHome renderers and offer them as outputs.
    pub upnp: bool,
    /// Discover Chromecast / Google Cast devices and offer them as outputs.
    pub cast: bool,
    /// Accept Squeezebox / squeezelite players (Slimproto) on this TCP port; 0 = off.
    pub slimproto_port: u16,
    /// Where a Pubky Ring sign-in stores the session secret (also read at start).
    pub session_secret_path: Option<PathBuf>,
    /// TIDAL developer app credentials; empty = TIDAL not offered.
    pub tidal: crate::tidal::TidalCredentials,
}

pub struct NodeState {
    pub name: String,
    pub version: String,
    pub library: Arc<Mutex<Library>>,
    /// Path of the SQLite file, so a scan can use its own connection.
    pub db_path: PathBuf,
    pub scan_folders: Vec<PathBuf>,
    pub art_dir: PathBuf,
    pub mirror_dir: PathBuf,
    pub outputs: OutputRegistry,
    pub zones: Zones,
    /// Optional bearer token; `None` = no auth (bind to localhost only).
    pub token: Option<String>,
    pub subsonic_user: String,
    pub public_url: String,
    pub upnp: bool,
    pub cast: bool,
    pub slimproto_port: u16,
    pub session_secret_path: Option<PathBuf>,
    pub pubky: pm_pubky::PmPubkyConfig,
    /// Pinakes/Nexus discovery; `None` until a valid contact is known. Rebuilt
    /// when the contact is set at runtime.
    pub discovery: std::sync::RwLock<Option<Arc<pm_pubky::Discovery>>>,
    /// The contact for outbound User-Agent strings: from the config, else the
    /// one saved through the API (`node.contact`), else empty.
    pub contact: std::sync::RwLock<String>,
    /// A sign-in in progress (POST /v1/pubky/login).
    pub login: tokio::sync::Mutex<Option<pm_pubky::homeserver::LoginFlow>>,
    /// The zones' source factory, also the per-output DSP registry.
    pub sources: Arc<NodeSources>,
    pub events: broadcast::Sender<ApiEvent>,
    /// Homeserver sync, when the node has a Pubky session.
    pub sync: Mutex<Option<Arc<crate::sync::PubkySync>>>,
    /// Folder watcher, kept alive here.
    pub watcher: Mutex<Option<notify::RecommendedWatcher>>,
    /// Sleep timers: the zone pauses when its deadline passes.
    pub sleep: Mutex<HashMap<ZoneId, SleepTimer>>,
    /// Pull outputs by stream name (they are also in `outputs`).
    pub pulls: Mutex<HashMap<String, Arc<pm_zones::outputs::PullOutput>>>,
    /// How to find outputs again (local devices, endpoints); set by the binary.
    #[allow(clippy::type_complexity)]
    pub discover: Mutex<Option<Arc<dyn Fn() -> Vec<Arc<dyn Output>> + Send + Sync>>>,
    /// Cover Art Archive origin; tests point it at a local server.
    pub caa_base: std::sync::RwLock<String>,
    /// Internet Archive origin; tests point it at a local server.
    pub archive_base: std::sync::RwLock<String>,
    /// Audius discovery node; empty = resolve through api.audius.co.
    pub audius_base: std::sync::RwLock<String>,
    /// TIDAL: developer credentials and the API / auth / login origins (tests point them at a mock);
    /// shared with the source factory so the audio thread can resolve `tidal:track:` URIs.
    pub tidal: crate::tidal::SharedEndpoints,
    /// MusicBrainz / Wikidata / Wikipedia / Commons origins for artist info.
    pub info_bases: std::sync::RwLock<crate::artist_info::InfoBases>,
    /// Shared async HTTP client for upstream lookups, rebuilt when the contact
    /// (and so the User-Agent) changes: `(contact, client)`.
    http: Mutex<Option<(String, reqwest::Client)>>,
    scanning: Mutex<bool>,
}

impl NodeState {
    pub fn new(settings: NodeSettings, library: Library, outputs: OutputRegistry) -> Arc<Self> {
        let library = Arc::new(Mutex::new(library));
        let tidal: crate::tidal::SharedEndpoints =
            Arc::new(std::sync::RwLock::new(crate::tidal::TidalEndpoints {
                creds: settings.tidal.clone(),
                ..Default::default()
            }));
        let sources = Arc::new(NodeSources::new(
            library.clone(),
            settings.pipeline,
            tidal.clone(),
        ));
        let (events, _) = broadcast::channel(1024);
        // A contact saved through the API wins over an unset config value.
        let mut pubky = settings.pubky;
        if pubky.validate_contact().is_err() {
            if let Ok(Some(saved)) = library.lock().expect("library poisoned").get_setting(CONTACT_KEY) {
                if pm_pubky::config::validate_contact(&saved).is_ok() {
                    pubky.user_agent_contact = saved;
                }
            }
        }
        let contact = if pubky.validate_contact().is_ok() {
            pubky.user_agent_contact.clone()
        } else {
            String::new()
        };
        let discovery = match pubky.validate_contact() {
            Ok(()) => match (
                pm_pubky::PinakesClient::new(&pubky),
                pm_pubky::NexusClient::new(&pubky),
            ) {
                (Ok(p), Ok(n)) => Some(Arc::new(pm_pubky::Discovery::new(p, n))),
                _ => None,
            },
            Err(_) => None,
        };
        Arc::new(Self {
            name: settings.name,
            version: settings.version,
            library,
            db_path: settings.db_path,
            scan_folders: settings.scan_folders,
            art_dir: settings.art_dir,
            mirror_dir: settings.mirror_dir,
            outputs,
            zones: Zones::new(sources.clone()),
            sources,
            token: settings.token,
            subsonic_user: settings.subsonic_user,
            public_url: settings.public_url,
            upnp: settings.upnp,
            cast: settings.cast,
            slimproto_port: settings.slimproto_port,
            session_secret_path: settings.session_secret_path,
            pubky,
            discovery: std::sync::RwLock::new(discovery),
            contact: std::sync::RwLock::new(contact),
            login: tokio::sync::Mutex::new(None),
            events,
            sync: Mutex::new(None),
            watcher: Mutex::new(None),
            sleep: Mutex::new(HashMap::new()),
            discover: Mutex::new(None),
            pulls: Mutex::new(HashMap::new()),
            caa_base: std::sync::RwLock::new("https://coverartarchive.org".into()),
            info_bases: std::sync::RwLock::new(crate::artist_info::InfoBases::default()),
            archive_base: std::sync::RwLock::new("https://archive.org".into()),
            audius_base: std::sync::RwLock::new(String::new()),
            tidal,
            http: Mutex::new(None),
            scanning: Mutex::new(false),
        })
    }

    /// Create a zone and forward its state and events to the API stream.
    /// Call from inside a tokio runtime. The zone's last queue (saved in the
    /// library settings on every queue change) is restored, stopped.
    pub fn create_zone(self: &Arc<Self>, name: &str, output: Arc<dyn Output>) -> Arc<Zone> {
        let zone = self.zones.create(name, output);
        self.restore_queue(&zone);
        let events = self.events.clone();
        let library = self.library.clone();
        let key = queue_key(zone.id());
        let mut state_rx = zone.watch();
        tokio::spawn(async move {
            let mut saved_version = state_rx.borrow().queue_version;
            while state_rx.changed().await.is_ok() {
                let state = state_rx.borrow_and_update().clone();
                if state.queue_version != saved_version {
                    saved_version = state.queue_version;
                    let saved = SavedQueue {
                        items: state.queue.clone(),
                        repeat: state.repeat,
                        radio: state.radio,
                    };
                    if let Ok(json) = serde_json::to_string(&saved) {
                        let lib = library.lock().expect("library poisoned");
                        if let Err(e) = lib.set_setting(&key, &json) {
                            tracing::warn!(error = %e, "saving queue");
                        }
                    }
                }
                let _ = events.send(ApiEvent::Zone { state });
            }
        });
        let events = self.events.clone();
        let mut ev_rx = zone.subscribe();
        let id = zone.id().clone();
        let library = self.library.clone();
        let zone_for_radio = zone.clone();
        tokio::spawn(async move {
            loop {
                match ev_rx.recv().await {
                    Ok(event) => {
                        if let pm_zones::ZoneEvent::TrackStarted { item, .. } = &event {
                            record_play(&library, &id, &item.uri);
                        }
                        if matches!(event, pm_zones::ZoneEvent::Stopped) {
                            continue_radio(&library, &zone_for_radio);
                        }
                        let _ = events.send(ApiEvent::ZoneEvent {
                            zone: id.clone(),
                            event,
                        });
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        });
        zone
    }

    /// Put the saved queue back into a freshly created zone (no playback).
    fn restore_queue(&self, zone: &Arc<Zone>) {
        let json = {
            let lib = self.library.lock().expect("library poisoned");
            lib.get_setting(&queue_key(zone.id())).ok().flatten()
        };
        let Some(json) = json else {
            return;
        };
        let Ok(saved) = serde_json::from_str::<SavedQueue>(&json) else {
            return;
        };
        if saved.items.is_empty() || !zone.queue().is_empty() {
            return;
        }
        let n = saved.items.len();
        zone.enqueue(saved.items, pm_zones::Insert::End);
        zone.set_repeat(saved.repeat);
        zone.set_radio(saved.radio);
        tracing::info!(zone = %zone.id(), items = n, "queue restored");
    }

    pub fn remove_zone(&self, id: &ZoneId) -> Option<Arc<Zone>> {
        let z = self.zones.remove(id)?;
        let _ = self.events.send(ApiEvent::ZoneRemoved { zone: id.clone() });
        Some(z)
    }

    /// One zone per output, named after the output.
    pub fn zones_for_all_outputs(self: &Arc<Self>) {
        for info in self.outputs.list() {
            if self.zones.for_output(&info.id).is_some() {
                continue;
            }
            if let Some(out) = self.outputs.get(&info.id) {
                self.create_zone(&info.name, out);
            }
        }
    }

    /// Scan `folders` in the background on a separate SQLite connection.
    /// `Ok(false)` when a scan is already running. Call inside a runtime.
    pub fn start_scan(self: &Arc<Self>, folders: Vec<PathBuf>) -> Result<bool, String> {
        if folders.is_empty() {
            return Err("no folders to scan".into());
        }
        if !self.begin_scan() {
            return Ok(false);
        }
        let state = self.clone();
        tokio::task::spawn_blocking(move || {
            let result = (|| -> Result<(), pm_library::LibraryError> {
                let lib = pm_library::Library::open(&state.db_path)?;
                let scanner = pm_library::Scanner::new(&lib).with_art_dir(&state.art_dir);
                for root in &folders {
                    lib.add_folder(root)?;
                    match scanner.scan(root) {
                        Ok(r) => {
                            tracing::info!(root = %root.display(), added = r.files_added, updated = r.files_updated, removed = r.files_removed, errors = r.errors.len(), "scan done")
                        }
                        Err(e) => tracing::warn!(root = %root.display(), "scan failed: {e}"),
                    }
                }
                Ok(())
            })();
            if let Err(e) = result {
                tracing::error!("scan: {e}");
            }
            state.end_scan();
        });
        Ok(true)
    }

    /// Watch the configured folders and rescan after `quiet` without events.
    /// The contact used in outbound User-Agent strings (empty when none is set).
    pub fn contact(&self) -> String {
        self.contact.read().expect("contact lock").clone()
    }

    /// The User-Agent for upstream services: `pubky-music-node/<version> (<contact>)`.
    pub fn user_agent(&self) -> String {
        let contact = self.contact();
        if contact.is_empty() {
            format!("pubky-music-node/{}", env!("CARGO_PKG_VERSION"))
        } else {
            format!("pubky-music-node/{} ({contact})", env!("CARGO_PKG_VERSION"))
        }
    }

    /// An async HTTP client carrying the current User-Agent (20 s timeout);
    /// cached until the contact changes.
    pub fn http_client(&self) -> reqwest::Client {
        let ua = self.user_agent();
        let mut slot = self.http.lock().expect("http client lock");
        if let Some((key, c)) = slot.as_ref() {
            if *key == ua {
                return c.clone();
            }
        }
        let c = reqwest::Client::builder()
            .user_agent(ua.clone())
            .timeout(std::time::Duration::from_secs(20))
            .build()
            .expect("http client");
        *slot = Some((ua, c.clone()));
        c
    }

    pub fn discovery(&self) -> Option<Arc<pm_pubky::Discovery>> {
        self.discovery.read().expect("discovery lock").clone()
    }

    /// Set the contact at runtime: validated, saved in the library settings,
    /// and discovery (Pinakes, Nexus) is built from it.
    pub fn set_contact(&self, contact: &str) -> Result<(), String> {
        let contact = contact.trim().to_string();
        pm_pubky::config::validate_contact(&contact)?;
        let mut cfg = self.pubky.clone();
        cfg.user_agent_contact = contact.clone();
        let discovery = match (
            pm_pubky::PinakesClient::new(&cfg),
            pm_pubky::NexusClient::new(&cfg),
        ) {
            (Ok(p), Ok(n)) => Some(Arc::new(pm_pubky::Discovery::new(p, n))),
            (Err(e), _) => return Err(format!("pinakes client: {e}")),
            (_, Err(e)) => return Err(format!("nexus client: {e}")),
        };
        self.library
            .lock()
            .expect("library poisoned")
            .set_setting(CONTACT_KEY, &contact)
            .map_err(|e| e.to_string())?;
        *self.contact.write().expect("contact lock") = contact;
        *self.discovery.write().expect("discovery lock") = discovery;
        Ok(())
    }

    /// The configured folders plus the ones added through the API, in order,
    /// without duplicates.
    pub fn all_folders(&self) -> Vec<PathBuf> {
        let mut out: Vec<PathBuf> = self.scan_folders.clone();
        if let Ok(extra) = self.library.lock().expect("library poisoned").list_folders() {
            for (p, _) in extra {
                if !out.contains(&p) {
                    out.push(p);
                }
            }
        }
        out
    }

    /// Start watching one more folder (after `watch_folders`); no-op without a watcher.
    pub fn watch_more(&self, folder: &std::path::Path) {
        use notify::Watcher as _;
        if let Some(w) = self.watcher.lock().expect("watcher lock").as_mut() {
            if let Err(e) = w.watch(folder, notify::RecursiveMode::Recursive) {
                tracing::warn!(folder = %folder.display(), "cannot watch: {e}");
            }
        }
    }

    pub fn unwatch(&self, folder: &std::path::Path) {
        use notify::Watcher as _;
        if let Some(w) = self.watcher.lock().expect("watcher lock").as_mut() {
            let _ = w.unwatch(folder);
        }
    }

    pub fn watch_folders(self: &Arc<Self>, quiet: std::time::Duration) {
        let folders = self.all_folders();
        if folders.is_empty() {
            return;
        }
        match crate::watch::spawn(self.clone(), folders, quiet) {
            Ok(w) => *self.watcher.lock().expect("watcher lock") = Some(w),
            Err(e) => tracing::warn!("folder watching unavailable: {e}"),
        }
    }

    pub fn set_discover(&self, f: Arc<dyn Fn() -> Vec<Arc<dyn Output>> + Send + Sync>) {
        *self.discover.lock().expect("discover lock") = Some(f);
    }

    /// The pull-stream URL renderers fetch for `name`.
    pub fn pull_url(&self, name: &str) -> String {
        format!(
            "{}/v1/outputs/pull/{name}/stream.wav",
            self.public_url.trim_end_matches('/')
        )
    }

    /// Create (or reuse) a pull output under `name`, registered for the HTTP route.
    pub fn pull_output(self: &Arc<Self>, name: &str) -> Arc<pm_zones::outputs::PullOutput> {
        let mut pulls = self.pulls.lock().expect("pulls lock");
        pulls
            .entry(name.to_string())
            .or_insert_with(|| Arc::new(pm_zones::outputs::PullOutput::new(name)))
            .clone()
    }

    /// Start the Slimproto server (players connect to us) and the UDP
    /// discovery responder. Each connecting player becomes an output with a
    /// zone; its pull stream is registered for the HTTP route.
    pub fn start_slimproto(self: &Arc<Self>) -> std::io::Result<()> {
        if self.slimproto_port == 0 {
            return Ok(());
        }
        let listener = std::net::TcpListener::bind(("0.0.0.0", self.slimproto_port))?;
        let web_port = url::Url::parse(&self.public_url)
            .ok()
            .and_then(|u| u.port_or_known_default())
            .unwrap_or(7788);
        let token = self.token.clone();
        let stream_path: Arc<dyn Fn(&str) -> String + Send + Sync> = Arc::new(move |name| match &token {
            Some(t) => format!("/v1/outputs/pull/{name}/stream.wav?token={t}"),
            None => format!("/v1/outputs/pull/{name}/stream.wav"),
        });
        let state = self.clone();
        let on_player: Arc<dyn Fn(Arc<pm_out_slimproto::SlimOutput>) + Send + Sync> = Arc::new(move |out| {
            let name = out.pull().stream_name().to_string();
            state
                .pulls
                .lock()
                .expect("pulls lock")
                .insert(name, out.pull().clone());
            let id = out.id();
            if state.outputs.get(&id).is_some() {
                // Reconnecting player: replace the output behind the same zone.
                state.outputs.register(out.clone());
                return;
            }
            let zone_name = out.name();
            state.outputs.register(out.clone());
            state.create_zone(&zone_name, out);
        });
        let name = self.name.clone();
        std::thread::Builder::new()
            .name("slimproto".into())
            .spawn(move || {
                if let Err(e) = pm_out_slimproto::serve(listener, web_port, stream_path, on_player) {
                    tracing::warn!("slimproto server stopped: {e}");
                }
            })?;
        std::thread::Builder::new()
            .name("slimproto-discovery".into())
            .spawn(move || {
                if let Err(e) = pm_out_slimproto::discovery_responder(&name, web_port) {
                    tracing::info!("slimproto discovery responder unavailable: {e}");
                }
            })?;
        tracing::info!(port = self.slimproto_port, "Slimproto server listening");
        Ok(())
    }

    /// SSDP discovery of UPnP/OpenHome renderers; each becomes an output
    /// playing through its own pull stream. Blocking; call from `spawn_blocking`.
    pub fn discover_upnp(self: &Arc<Self>, timeout: std::time::Duration) -> Vec<OutputId> {
        if !self.upnp {
            return Vec::new();
        }
        let renderers = match pm_out_upnp::discover(timeout) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("UPnP discovery failed: {e}");
                return Vec::new();
            }
        };
        let mut new = Vec::new();
        for r in renderers {
            let name = format!(
                "upnp-{}",
                r.udn
                    .trim_start_matches("uuid:")
                    .chars()
                    .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
                    .take(40)
                    .collect::<String>()
            );
            let pull = self.pull_output(&name);
            let out = Arc::new(pm_out_upnp::UpnpOutput::new(
                r.clone(),
                pull,
                self.pull_url(&name),
            ));
            let id = out.id();
            if self.outputs.get(&id).is_some() {
                continue;
            }
            tracing::info!(renderer = %r.friendly_name, output = %id, "UPnP renderer");
            self.outputs.register(out.clone());
            self.create_zone(&r.friendly_name, out);
            new.push(id);
        }
        new
    }

    /// mDNS discovery of Cast devices; each becomes an output playing
    /// through its own pull stream. Blocking; call from `spawn_blocking`.
    pub fn discover_cast(self: &Arc<Self>, timeout: std::time::Duration) -> Vec<OutputId> {
        if !self.cast {
            return Vec::new();
        }
        let mut new = Vec::new();
        for d in pm_out_cast::discover(timeout) {
            let name = format!(
                "cast-{}",
                d.id.chars()
                    .filter(|c| c.is_ascii_alphanumeric())
                    .take(32)
                    .collect::<String>()
            );
            let pull = self.pull_output(&name);
            let out = Arc::new(pm_out_cast::CastOutput::new(
                d.clone(),
                pull,
                self.pull_url(&name),
            ));
            let id = out.id();
            if self.outputs.get(&id).is_some() {
                continue;
            }
            tracing::info!(device = %d.friendly_name, output = %id, "Cast device");
            let zone_name = out.name();
            self.outputs.register(out.clone());
            self.create_zone(&zone_name, out);
            new.push(id);
        }
        new
    }

    /// Run discovery again: register outputs not seen before and give each
    /// a zone. Returns the ids that are new. Blocking (mDNS browse, TCP
    /// connects, SSDP); call from `spawn_blocking`.
    pub fn rescan_outputs(self: &Arc<Self>) -> Vec<OutputId> {
        let mut new = self.discover_upnp(std::time::Duration::from_secs(3));
        new.extend(self.discover_cast(std::time::Duration::from_millis(1500)));
        let Some(f) = self.discover.lock().expect("discover lock").clone() else {
            return new;
        };
        for out in f() {
            let id = out.id();
            if self.outputs.get(&id).is_some() {
                continue;
            }
            let name = out.name();
            self.outputs.register(out.clone());
            new.push(id);
            self.create_zone(&name, out);
        }
        new
    }

    /// Pause `zone` after `seconds` (`None` cancels). Returns when the zone
    /// will pause, as unix milliseconds.
    pub fn set_sleep(self: &Arc<Self>, zone: Arc<Zone>, seconds: Option<u64>) -> Option<u64> {
        let mut timers = self.sleep.lock().expect("sleep lock");
        timers.remove(zone.id());
        let secs = seconds?;
        let ends_at_ms = unix_ms() + secs * 1000;
        let state = Arc::clone(self);
        let id = zone.id().clone();
        let key = id.clone();
        let task = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(secs)).await;
            zone.pause();
            tracing::info!(zone = %zone.name(), "sleep timer: paused");
            if let Ok(mut t) = state.sleep.lock() {
                t.remove(&id);
            }
        });
        timers.insert(key, SleepTimer { ends_at_ms, task });
        Some(ends_at_ms)
    }

    /// When the zone's sleep timer fires, if one is set.
    pub fn sleep_ends_at(&self, zone: &ZoneId) -> Option<u64> {
        self.sleep
            .lock()
            .expect("sleep lock")
            .get(zone)
            .map(|t| t.ends_at_ms)
            .filter(|ms| *ms > unix_ms())
    }

    /// Claim the scan slot; false if a scan is already running.
    pub(crate) fn begin_scan(&self) -> bool {
        let mut s = self.scanning.lock().expect("scan flag poisoned");
        if *s {
            return false;
        }
        *s = true;
        true
    }

    pub(crate) fn end_scan(&self) {
        *self.scanning.lock().expect("scan flag poisoned") = false;
        let _ = self.events.send(ApiEvent::LibraryChanged);
    }

    pub fn set_sync(&self, sync: Option<Arc<crate::sync::PubkySync>>) {
        *self.sync.lock().expect("sync lock") = sync;
    }

    pub fn sync_handle(&self) -> Option<Arc<crate::sync::PubkySync>> {
        self.sync.lock().expect("sync lock").clone()
    }

    /// Tell event listeners that library-side data (tracks, playlists) changed.
    pub fn notify_library(&self) {
        let _ = self.events.send(ApiEvent::LibraryChanged);
    }

    pub fn is_scanning(&self) -> bool {
        *self.scanning.lock().expect("scan flag poisoned")
    }
}

const CONTACT_KEY: &str = "node.contact";

fn queue_key(id: &ZoneId) -> String {
    format!("queue.{}", id.0)
}

/// What survives a restart per zone: the items and the repeat mode.
#[derive(serde::Serialize, serde::Deserialize)]
struct SavedQueue {
    items: Vec<pm_zones::QueueItem>,
    repeat: pm_zones::Repeat,
    #[serde(default)]
    radio: bool,
}

/// Library radio: when a zone with `radio` on runs out of queue, add tracks
/// similar to the last library track it played and keep going.
fn continue_radio(library: &Mutex<Library>, zone: &Arc<Zone>) {
    let state = zone.state();
    if !state.radio || state.queue.is_empty() || state.repeat != pm_zones::Repeat::Off {
        return;
    }
    let Some(last) = state.queue.last() else {
        return;
    };
    let Some(seed_id) = last
        .uri
        .strip_prefix(crate::sources::TRACK_SCHEME)
        .and_then(|v| v.parse::<i64>().ok())
    else {
        return;
    };
    let queued: Vec<pm_model::TrackId> = state
        .queue
        .iter()
        .filter_map(|q| q.uri.strip_prefix(crate::sources::TRACK_SCHEME))
        .filter_map(|v| v.parse::<i64>().ok())
        .map(pm_model::TrackId)
        .collect();
    let picks = {
        let lib = library.lock().expect("library poisoned");
        let Ok(Some(seed)) = lib.get_track(pm_model::TrackId(seed_id)) else {
            return;
        };
        lib.similar_tracks(&seed, &queued, 10).unwrap_or_default()
    };
    let items: Vec<pm_zones::QueueItem> = picks
        .iter()
        .filter_map(crate::sources::NodeSources::item_for)
        .collect();
    if items.is_empty() {
        return;
    }
    let start = state.queue.len();
    tracing::info!(zone = %zone.id(), added = items.len(), "radio: continuing with similar tracks");
    zone.enqueue(items, pm_zones::Insert::End);
    zone.play_index(start);
}

/// Library tracks (`track:<id>`) go into the play history; other URIs do not.
fn record_play(library: &Mutex<Library>, zone: &ZoneId, uri: &str) {
    let Some(id) = uri
        .strip_prefix(crate::sources::TRACK_SCHEME)
        .and_then(|s| s.parse::<i64>().ok())
    else {
        return;
    };
    let lib = library.lock().expect("library poisoned");
    if let Err(e) = lib.record_play(pm_model::TrackId(id), Some(zone.0.as_str())) {
        tracing::warn!(error = %e, track = id, "recording play");
    }
}
