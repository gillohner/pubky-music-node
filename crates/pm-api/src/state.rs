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
    pub spotify: crate::spotify::SpotifyCredentials,
    /// AcoustID application key (acoustid.org/new-application); empty = fingerprints
    /// are computed and cached but not looked up.
    pub acoustid_key: String,
    /// Jamendo developer app; empty = Jamendo not offered.
    pub jamendo: crate::jamendo::JamendoCredentials,
}

pub struct NodeState {
    /// The config file the node was started with (a backup carries it, encrypted).
    config_file: Mutex<Option<PathBuf>>,
    /// The node's one Pubky client, built on first use and shared by every Pubky call.
    pubky_client: std::sync::OnceLock<pm_pubky::homeserver::NodeHomeserver>,
    /// The homeserver bundled with the node (deploy/bundle), managed through its admin API.
    bundled_homeserver: std::sync::OnceLock<crate::bundled::AdminEndpoint>,
    /// The folder shared with cloudflared and the homeserver config wrapper (deploy/bundle).
    tunnel_dir: std::sync::OnceLock<PathBuf>,
    /// The bundled homeserver's files folder, mounted read-only (`<dir>/<user>/<path>`).
    homeserver_files_dir: std::sync::OnceLock<PathBuf>,
    /// What auto-save last did (storage.rs).
    pub storage_save: Mutex<crate::storage::SaveState>,
    /// Uploads from the browser (add_music.rs).
    pub music_uploads: crate::add_music::Uploads,
    /// The background sync after a sign-in (or a restore), for the page.
    pub sync_activity: Mutex<crate::types::SyncActivity>,
    /// The static sign-in return page for the project's apps (`[auth] return_page`).
    return_page: std::sync::RwLock<String>,
    /// What the last scan did (shown when it ends, and in Settings).
    pub last_scan: Mutex<Option<crate::types::LastScan>>,
    pub name: String,
    /// Stable id of this node (a UUID kept in the library), for LAN discovery and controllers
    /// that remember a node across address changes.
    pub node_id: String,
    /// Remote access over iroh (ADR 0013): configuration and the running tunnel.
    pub remote: tokio::sync::Mutex<crate::remote::Remote>,
    /// The tunnel's key once it runs, for `/v1/info`.
    pub remote_id: std::sync::RwLock<Option<String>>,
    /// Keys the pull-stream URLs handed to renderers (`stream_key`), so a TV gets a URL for its
    /// own stream and never the API token. Kept in the library: URLs survive restarts.
    stream_secret: [u8; 32],
    /// Sonos players by output id, so a zone of Sonos players only groups them the Sonos way.
    pub sonos: Mutex<HashMap<OutputId, Arc<pm_out_upnp::UpnpOutput>>>,
    /// HEOS players (Denon, Marantz) among the renderers: a zone of them only is a HEOS group.
    pub heos: Mutex<HashMap<OutputId, Arc<pm_out_upnp::UpnpOutput>>>,
    /// The snapserver whose groups are zones (`[outputs] snapcast`).
    snapcast: Mutex<Option<String>>,
    /// Offer AirPlay 1 receivers (`[outputs] airplay`).
    airplay: std::sync::atomic::AtomicBool,
    /// MPD servers to play to (`[outputs] mpd`).
    mpd: Mutex<Vec<pm_out_mpd::Server>>,
    /// Offer BluOS players (`[outputs] bluos`).
    bluos: std::sync::atomic::AtomicBool,
    /// BluOS players by output id: a zone of them only is a BluOS group.
    pub bluos_players: Mutex<HashMap<OutputId, Arc<pm_out_bluos::BluosOutput>>>,
    /// The zones as UPnP/DLNA MediaRenderers, once `start_renderer` ran.
    pub renderer: std::sync::OnceLock<Arc<pm_renderer::Renderer>>,
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
    /// Controller sign-ins in progress: id → (flow, started).
    /// Bulk uploads to the homeserver's private storage, by job id (files.rs).
    pub upload_jobs: std::sync::Mutex<std::collections::HashMap<String, crate::types::UploadProgress>>,
    pub controller_logins: tokio::sync::Mutex<
        std::collections::HashMap<String, (pm_pubky::homeserver::LoginFlow, std::time::Instant)>,
    >,
    /// The zones' source factory, also the per-output DSP registry.
    pub sources: Arc<NodeSources>,
    pub events: crate::events::EventBus,
    /// Homeserver sync, when the node has a Pubky session.
    pub sync: Mutex<Option<Arc<crate::sync::PubkySync>>>,
    /// Held across one sync of the node's sign-ins with the vault (`vault_cache`), and across
    /// "Sign out everywhere", so two never interleave.
    pub vault_pass: tokio::sync::Mutex<()>,
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
    /// ListenBrainz API origin (tests point it at a mock).
    pub listenbrainz_base: std::sync::RwLock<String>,
    /// TIDAL: developer credentials and the API / auth / login origins (tests point them at a mock);
    /// shared with the source factory so the audio thread can resolve `tidal:track:` URIs.
    pub tidal: crate::tidal::SharedEndpoints,
    pub spotify: crate::spotify::SharedEndpoints,
    /// AcoustID key and origin (tests point it at a mock).
    pub acoustid: std::sync::RwLock<crate::identify::AcoustIdSettings>,
    /// MusicBrainz ws/2 origin for identification suggestions (tests: a mock).
    pub mb_base: std::sync::RwLock<String>,
    /// OpenStreetMap Nominatim origin for the Home city (tests: a mock).
    pub nominatim_base: std::sync::RwLock<String>,
    /// The library identification job's progress.
    pub identify: Mutex<crate::identify::IdentifyProgress>,
    /// Pinakes cards for any link: by-uri, then paced resolves, cached.
    pub pinakes_reader: crate::pinakes_reader::PinakesReader,
    /// Deezer API origin (tests point it at a mock).
    pub deezer_base: crate::deezer::SharedBase,
    /// Jamendo credentials and origin, shared with the audio thread.
    pub jamendo: crate::jamendo::SharedEndpoints,
    /// The user's own (Open)Subsonic server, shared with the audio thread.
    pub opensubsonic: crate::opensubsonic::Shared,
    /// The user's Jellyfin, shared with the audio thread.
    pub jellyfin: crate::jellyfin::Shared,
    /// The user's Plex, shared with the audio thread.
    pub plex: crate::plex::Shared,
    /// WebDAV shares that are library roots, shared with the audio thread.
    pub webdav: crate::webdav::Shared,
    /// Google Drive and OneDrive folders that are library roots, shared with the audio thread.
    pub drives: crate::drives::Shared,
    /// The user's Audiobookshelf, shared with the audio thread.
    pub audiobookshelf: crate::audiobookshelf::Shared,
    /// MusicBrainz / Wikidata / Wikipedia / Commons origins for artist info.
    pub info_bases: std::sync::RwLock<crate::artist_info::InfoBases>,
    /// Shared async HTTP client for upstream lookups, rebuilt when the contact
    /// (and so the User-Agent) changes: `(contact, client)`.
    http: Mutex<Option<(String, reqwest::Client)>>,
    scanning: Mutex<bool>,
    /// Sources the owner switched off (Sources page): hidden everywhere, sign-ins kept.
    sources_off: std::sync::RwLock<std::collections::HashSet<String>>,
}

/// Library setting: JSON list of the source ids switched off.
pub const SOURCES_OFF_KEY: &str = "sources.off";

fn load_sources_off(lib: &Library) -> std::collections::HashSet<String> {
    lib.get_setting(SOURCES_OFF_KEY)
        .ok()
        .flatten()
        .and_then(|v| serde_json::from_str(&v).ok())
        .unwrap_or_default()
}

/// Library setting: JSON list of renderer addresses added by hand.
const UPNP_ADDRESSES_KEY: &str = "outputs.upnp_addresses";
/// Endpoint addresses (`pm-endpoint`, the desktop app) added by hand.
const ENDPOINT_ADDRESSES_KEY: &str = "outputs.endpoint_addresses";

/// The public page of a Pubky identity, usable as an upstream contact.
pub fn profile_url(z32: &str) -> String {
    format!("https://pubky.app/profile/{z32}")
}

impl NodeState {
    /// Whether a source is switched on. Off means hidden everywhere: search, Home, item pages,
    /// play-from order, menus. Its sign-in and settings stay. `library` is always on.
    pub fn source_enabled(&self, id: &str) -> bool {
        !self.sources_off.read().expect("sources_off lock").contains(id)
    }

    /// Switch a source on or off (persisted).
    pub fn set_source_enabled(&self, id: &str, on: bool) -> Result<(), pm_library::LibraryError> {
        let mut off = self.sources_off.write().expect("sources_off lock");
        if on {
            off.remove(id);
        } else {
            off.insert(id.to_string());
        }
        let mut list: Vec<&String> = off.iter().collect();
        list.sort();
        let json = serde_json::to_string(&list).expect("list serializes");
        self.library
            .lock()
            .expect("library poisoned")
            .set_setting(SOURCES_OFF_KEY, &json)
    }

    /// Read the switched-off sources again (after a restore replaced the settings).
    pub fn reload_sources_off(&self) {
        let fresh = load_sources_off(&self.library.lock().expect("library poisoned"));
        *self.sources_off.write().expect("sources_off lock") = fresh;
    }

    pub fn new(settings: NodeSettings, library: Library, outputs: OutputRegistry) -> Arc<Self> {
        // A key entered in Settings wins over the configured one
        let acoustid_key = library
            .get_setting(crate::identify::KEY_SETTING)
            .ok()
            .flatten()
            .filter(|k| !k.is_empty())
            .unwrap_or_else(|| settings.acoustid_key.clone());
        let node_id = node_id(&library);
        let stream_secret = stream_secret(&library);
        let library = Arc::new(Mutex::new(library));
        let tidal: crate::tidal::SharedEndpoints =
            Arc::new(std::sync::RwLock::new(crate::tidal::TidalEndpoints {
                creds: settings.tidal.clone(),
                ..Default::default()
            }));
        let deezer_base = crate::deezer::shared_base();
        let jamendo: crate::jamendo::SharedEndpoints =
            Arc::new(std::sync::RwLock::new(crate::jamendo::JamendoEndpoints {
                creds: settings.jamendo.clone(),
                ..Default::default()
            }));
        let spotify: crate::spotify::SharedEndpoints =
            Arc::new(std::sync::RwLock::new(crate::spotify::SpotifyEndpoints {
                creds: settings.spotify.clone(),
                ..Default::default()
            }));
        let opensubsonic: crate::opensubsonic::Shared = Arc::new(std::sync::RwLock::new(
            crate::opensubsonic::load(&library.lock().expect("library poisoned")),
        ));
        let jellyfin: crate::jellyfin::Shared = Arc::new(std::sync::RwLock::new(crate::jellyfin::load(
            &library.lock().expect("library poisoned"),
        )));
        let plex: crate::plex::Shared = Arc::new(std::sync::RwLock::new(crate::plex::load(
            &library.lock().expect("library poisoned"),
        )));
        let webdav: crate::webdav::Shared = Arc::new(std::sync::RwLock::new(crate::webdav::load(
            &library.lock().expect("library poisoned"),
        )));
        let drives: crate::drives::Shared = Arc::new(std::sync::RwLock::new(crate::drives::load(
            &library.lock().expect("library poisoned"),
            Default::default(),
        )));
        let audiobookshelf: crate::audiobookshelf::Shared = Arc::new(std::sync::RwLock::new(
            crate::audiobookshelf::load(&library.lock().expect("library poisoned")),
        ));
        let sources = Arc::new(NodeSources::new(
            library.clone(),
            settings.pipeline,
            tidal.clone(),
            deezer_base.clone(),
            jamendo.clone(),
            opensubsonic.clone(),
            jellyfin.clone(),
            plex.clone(),
            audiobookshelf.clone(),
            webdav.clone(),
            drives.clone(),
        ));
        let events = crate::events::EventBus::new(1024);
        let sources_off =
            std::sync::RwLock::new(load_sources_off(&library.lock().expect("library poisoned")));
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
            node_id,
            remote: Default::default(),
            remote_id: Default::default(),
            stream_secret,
            sonos: Default::default(),
            heos: Default::default(),
            snapcast: Default::default(),
            airplay: Default::default(),
            mpd: Default::default(),
            bluos: Default::default(),
            bluos_players: Default::default(),
            renderer: Default::default(),
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
            controller_logins: tokio::sync::Mutex::new(std::collections::HashMap::new()),
            upload_jobs: Default::default(),
            events,
            sources_off,
            sync: Mutex::new(None),
            vault_pass: tokio::sync::Mutex::new(()),
            watcher: Mutex::new(None),
            sleep: Mutex::new(HashMap::new()),
            discover: Mutex::new(None),
            pulls: Mutex::new(HashMap::new()),
            caa_base: std::sync::RwLock::new("https://coverartarchive.org".into()),
            info_bases: std::sync::RwLock::new(crate::artist_info::InfoBases::default()),
            archive_base: std::sync::RwLock::new("https://archive.org".into()),
            audius_base: std::sync::RwLock::new(String::new()),
            listenbrainz_base: std::sync::RwLock::new(crate::listenbrainz::API_BASE.to_string()),
            deezer_base,
            jamendo,
            opensubsonic,
            jellyfin,
            plex,
            audiobookshelf,
            webdav,
            drives,
            spotify,
            acoustid: std::sync::RwLock::new(crate::identify::AcoustIdSettings {
                api_key: acoustid_key,
                base_url: "https://api.acoustid.org".into(),
                configured: settings.acoustid_key.clone(),
            }),
            mb_base: std::sync::RwLock::new("https://musicbrainz.org/ws/2".into()),
            nominatim_base: std::sync::RwLock::new("https://nominatim.openstreetmap.org".into()),
            identify: Mutex::new(Default::default()),
            pinakes_reader: Default::default(),
            tidal,
            http: Mutex::new(None),
            scanning: Mutex::new(false),
            config_file: Mutex::new(None),
            pubky_client: std::sync::OnceLock::new(),
            bundled_homeserver: std::sync::OnceLock::new(),
            tunnel_dir: std::sync::OnceLock::new(),
            homeserver_files_dir: std::sync::OnceLock::new(),
            storage_save: Mutex::new(Default::default()),
            music_uploads: Default::default(),
            sync_activity: Mutex::new(Default::default()),
            return_page: std::sync::RwLock::new(crate::project_apps::RETURN_PAGE.to_string()),
            last_scan: Mutex::new(None),
        })
    }

    /// Create a zone and forward its state and events to the API stream.
    /// Call from inside a tokio runtime. The zone's last queue (saved in the
    /// library settings on every queue change) is restored, stopped.
    pub fn create_zone(self: &Arc<Self>, name: &str, output: Arc<dyn Output>) -> Arc<Zone> {
        let zone = self.zones.create(name, output);
        self.restore_queue(&zone);
        // The zone's crossfade (`PUT /v1/zones/{id}/crossfade`), kept across restarts
        if let Ok(Some(ms)) = self
            .library
            .lock()
            .expect("library poisoned")
            .get_setting(&crossfade_key(zone.id()))
        {
            if let Ok(ms) = ms.parse::<u64>() {
                zone.set_crossfade_ms(ms);
            }
        }
        let events = self.events.clone();
        let library = self.library.clone();
        let key = queue_key(zone.id());
        let mut state_rx = zone.watch();
        // Every controller learns about the new zone at once, not on its first change.
        let initial = state_rx.borrow_and_update().clone();
        let _ = self.events.send(ApiEvent::Zone { state: initial });
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
        let node = Arc::downgrade(self);
        tokio::spawn(async move {
            loop {
                match ev_rx.recv().await {
                    Ok(event) => {
                        if let pm_zones::ZoneEvent::TrackStarted { item, .. } = &event {
                            record_play(&library, &id, &item.uri);
                            crate::listens::record_zone_play(
                                &library.lock().expect("library poisoned"),
                                &item.uri,
                                item.duration_ms,
                            );
                            if let Some(n) = node.upgrade() {
                                crate::listenbrainz::track_started(&n, zone_for_radio.clone(), item.clone());
                                crate::lastfm::track_started(&n, zone_for_radio.clone(), item.clone());
                            }
                        }
                        if let pm_zones::ZoneEvent::TrackEnded { item, .. } = &event {
                            if let Some(n) = node.upgrade() {
                                crate::listenbrainz::track_ended(&n, &id, item);
                                crate::lastfm::track_ended(&n, &id, item);
                            }
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
            let mut summary = crate::types::LastScan::default();
            let result = (|| -> Result<(), pm_library::LibraryError> {
                let lib = pm_library::Library::open(&state.db_path)?;
                let scanner = pm_library::Scanner::new(&lib).with_art_dir(&state.art_dir);
                for root in &folders {
                    lib.add_folder(root)?;
                    match scanner.scan(root) {
                        Ok(r) => {
                            tracing::info!(root = %root.display(), added = r.files_added, updated = r.files_updated, removed = r.files_removed, errors = r.errors.len(), "scan done");
                            summary.added += r.files_added;
                            summary.updated += r.files_updated;
                            summary.removed += r.files_removed;
                            summary.error_count += r.errors.len();
                            for (p, why) in r.errors {
                                tracing::warn!(path = %p.display(), "not read: {why}");
                                if summary.errors.len() < 20 {
                                    summary.errors.push((p.to_string_lossy().into_owned(), why));
                                }
                            }
                        }
                        Err(e) => {
                            tracing::warn!(root = %root.display(), "scan failed: {e}");
                            summary.error_count += 1;
                            summary
                                .errors
                                .push((root.to_string_lossy().into_owned(), e.to_string()));
                        }
                    }
                }
                Ok(())
            })();
            if let Err(e) = result {
                tracing::error!("scan: {e}");
            }
            summary.finished_at = time::OffsetDateTime::now_utc().unix_timestamp();
            *state.last_scan.lock().expect("last scan lock") = Some(summary);
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

    /// The pull-stream URL renderers fetch for `name`, with that stream's key (renderers cannot
    /// send a bearer token, and must not get the API token).
    pub fn pull_url(&self, name: &str) -> String {
        format!(
            "{}{}",
            self.public_url.trim_end_matches('/'),
            self.pull_path(name)
        )
    }

    /// `/v1/outputs/pull/{name}/stream.wav?k=<key>`.
    pub fn pull_path(&self, name: &str) -> String {
        format!("/v1/outputs/pull/{name}/stream.wav?k={}", self.stream_key(name))
    }

    /// What a TV or speaker shows for a queue item: the item's own fields, filled from the library
    /// for library tracks, with cover art the device can fetch (a keyed node URL, or the source's
    /// own image).
    pub fn describe_for_renderers(self: &Arc<Self>) -> pm_zones::Describe {
        let state = Arc::downgrade(self);
        Arc::new(move |item: &pm_zones::QueueItem| {
            let mut meta = pm_zones::TrackMeta::from_item(item);
            let Some(s) = state.upgrade() else { return meta };
            let id = item
                .uri
                .strip_prefix(crate::sources::TRACK_SCHEME)
                .and_then(|n| n.parse::<i64>().ok());
            if let Some(id) = id {
                let track = s
                    .library
                    .lock()
                    .ok()
                    .and_then(|l| l.get_track(pm_model::TrackId(id)).ok().flatten());
                if let Some(t) = track {
                    meta.title = meta.title.or(Some(t.title.clone()));
                    if meta.artist.is_none() && !t.artists.is_empty() {
                        meta.artist = Some(
                            t.artists
                                .iter()
                                .map(|a| a.name.as_str())
                                .collect::<Vec<_>>()
                                .join(", "),
                        );
                    }
                    if let Some(album) = &t.album {
                        meta.album = meta.album.or(Some(album.title.clone()));
                        if meta.image.is_none() {
                            meta.image = album.id.map(|aid| s.art_url(aid));
                        }
                    }
                }
            }
            meta
        })
    }

    /// An album cover URL a renderer can fetch without the API token.
    pub fn art_url(&self, album_id: i64) -> String {
        format!(
            "{}/v1/art/{album_id}?k={}",
            self.public_url.trim_end_matches('/'),
            self.stream_key(&format!("art:{album_id}"))
        )
    }

    /// The key that opens one pull stream (and nothing else).
    pub fn stream_key(&self, name: &str) -> String {
        let h = blake3::keyed_hash(&self.stream_secret, name.as_bytes());
        h.to_hex()[..32].to_string()
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
    /// discovery responder. Call from inside the tokio runtime. Each connecting player becomes an output with a
    /// Offer every zone as a UPnP/DLNA MediaRenderer (`pm-renderer`): SSDP on the LAN, the
    /// device and control routes on the API port (merged by `router`, no token). Call from
    /// inside the runtime, before building the router. The advertiser lives as long as the
    /// returned handle.
    pub fn start_renderer(
        self: &Arc<Self>,
        api_port: u16,
        announce: bool,
    ) -> Option<pm_renderer::ssdp::Advertiser> {
        let host = crate::renderer::ZoneRenderers::new(self);
        let r = pm_renderer::Renderer::new(host, &self.node_id, env!("CARGO_PKG_VERSION"));
        r.spawn_notifier();
        let _ = self.renderer.set(r.clone());
        tracing::info!(zones = r.zones().len(), "zones offered as UPnP/DLNA renderers");
        if announce {
            pm_renderer::ssdp::start(r, api_port)
        } else {
            None
        }
    }

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
        let keys = self.clone();
        let stream_path: Arc<dyn Fn(&str) -> String + Send + Sync> =
            Arc::new(move |name| keys.pull_path(name));
        let state = self.clone();
        // Players connect on the server's own threads; zones need the runtime.
        let runtime = tokio::runtime::Handle::current();
        // Once per player: a player that reconnects keeps its output and zone.
        let on_player: Arc<dyn Fn(Arc<pm_out_slimproto::SlimOutput>) + Send + Sync> = Arc::new(move |out| {
            let _rt = runtime.enter();
            let name = out.pull().stream_name().to_string();
            state
                .pulls
                .lock()
                .expect("pulls lock")
                .insert(name, out.pull().clone());
            let zone_name = out.name();
            state.outputs.register(out.clone());
            if state.zones.list().iter().all(|z| z.output != out.id()) {
                state.create_zone(&zone_name, out);
            }
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
                if let Err(e) = pm_out_slimproto::discovery_responder(&name) {
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
        renderers
            .into_iter()
            .filter_map(|r| self.register_renderer(r))
            .collect()
    }

    /// A renderer as an output with its own pull stream and a zone; `None`
    /// when it is already registered.
    pub fn register_renderer(self: &Arc<Self>, r: pm_out_upnp::Renderer) -> Option<OutputId> {
        let name = format!(
            "upnp-{}",
            r.udn
                .trim_start_matches("uuid:")
                .chars()
                .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
                .take(40)
                .collect::<String>()
        );
        let id = OutputId::new("upnp", r.udn.trim_start_matches("uuid:"));
        if self.outputs.get(&id).is_some() {
            return None;
        }
        // What it accepts decides how the stream is offered; a renderer that takes no PCM stream
        // (some TVs play only MP3/AAC files) gets no zone, rather than one that always fails.
        let sinks = pm_out_upnp::dlna::sink_protocols(&r).unwrap_or_else(|e| {
            tracing::debug!(renderer = %r.friendly_name, "GetProtocolInfo failed, offering WAV: {e}");
            Vec::new()
        });
        let Some(kind) = pm_out_upnp::dlna::choose(&sinks) else {
            tracing::info!(renderer = %r.friendly_name, "UPnP renderer accepts no PCM stream (WAV or LPCM); no zone");
            return None;
        };
        let pull = self.pull_output(&name);
        let out = Arc::new(
            pm_out_upnp::UpnpOutput::new(r.clone(), pull, self.pull_url(&name))
                .with_stream_kind(kind.clone())
                .with_describe(self.describe_for_renderers()),
        );
        tracing::info!(renderer = %r.friendly_name, output = %id, stream = ?kind, "UPnP renderer");
        if pm_out_upnp::heos::is_heos(&r) {
            self.heos
                .lock()
                .expect("heos lock")
                .insert(id.clone(), out.clone());
        }
        if pm_out_upnp::sonos::is_sonos(&r) {
            self.sonos
                .lock()
                .expect("sonos lock")
                .insert(id.clone(), out.clone());
        }
        self.outputs.register(out.clone());
        self.create_zone(&r.friendly_name, out);
        Some(id)
    }

    /// Renderers the owner added by address (another subnet, VLAN, or a
    /// device that ignores multicast); kept in the library settings.
    pub fn upnp_addresses(&self) -> Vec<String> {
        let lib = self.library.lock().expect("library poisoned");
        lib.get_setting(UPNP_ADDRESSES_KEY)
            .ok()
            .flatten()
            .and_then(|j| serde_json::from_str(&j).ok())
            .unwrap_or_default()
    }

    fn save_upnp_addresses(&self, addrs: &[String]) {
        let lib = self.library.lock().expect("library poisoned");
        let _ = lib.set_setting(
            UPNP_ADDRESSES_KEY,
            &serde_json::to_string(addrs).expect("serializes"),
        );
    }

    /// Probe `addr` (IP, `ip:port` or description URL), register what answers,
    /// and remember the address for the next start. Blocking.
    pub fn add_upnp_address(self: &Arc<Self>, addr: &str) -> Result<Vec<pm_out_upnp::Renderer>, String> {
        let addr = addr.trim().to_string();
        if addr.is_empty() {
            return Err("address is required".into());
        }
        let found = pm_out_upnp::probe_address(&addr, std::time::Duration::from_secs(2))
            .map_err(|e| e.to_string())?;
        if found.is_empty() {
            return Err(format!(
                "{addr} answered, but nothing there is a MediaRenderer with AVTransport"
            ));
        }
        let mut saved = self.upnp_addresses();
        if !saved.contains(&addr) {
            saved.push(addr);
            self.save_upnp_addresses(&saved);
        }
        for r in &found {
            self.register_renderer(r.clone());
        }
        Ok(found)
    }

    /// Forget a saved address (its outputs stay until restart).
    pub fn remove_upnp_address(&self, addr: &str) -> bool {
        let mut saved = self.upnp_addresses();
        let before = saved.len();
        saved.retain(|a| a != addr.trim());
        if saved.len() != before {
            self.save_upnp_addresses(&saved);
        }
        saved.len() != before
    }

    /// Re-add the saved addresses at start. Blocking.
    pub fn restore_upnp_addresses(self: &Arc<Self>) -> Vec<OutputId> {
        let mut new = Vec::new();
        for addr in self.upnp_addresses() {
            match pm_out_upnp::probe_address(&addr, std::time::Duration::from_secs(2)) {
                Ok(rs) => new.extend(rs.into_iter().filter_map(|r| self.register_renderer(r))),
                Err(e) => tracing::warn!(%addr, "saved renderer address: {e}"),
            }
        }
        new
    }

    /// Endpoints (`pm-endpoint`, the desktop app's player) the owner added
    /// by address; mDNS does not cross subnets and a laptop moves around.
    pub fn endpoint_addresses(&self) -> Vec<String> {
        let lib = self.library.lock().expect("library poisoned");
        lib.get_setting(ENDPOINT_ADDRESSES_KEY)
            .ok()
            .flatten()
            .and_then(|j| serde_json::from_str(&j).ok())
            .unwrap_or_default()
    }

    fn save_endpoint_addresses(&self, addrs: &[String]) {
        let lib = self.library.lock().expect("library poisoned");
        let _ = lib.set_setting(
            ENDPOINT_ADDRESSES_KEY,
            &serde_json::to_string(addrs).expect("serializes"),
        );
    }

    /// Connect to the endpoint at `addr` (`host:port`), give each of its
    /// outputs a zone, and remember the address for the next start. The
    /// outputs it already has keep their zones. Blocking.
    pub fn add_endpoint_address(self: &Arc<Self>, addr: &str) -> Result<Vec<pm_zones::OutputInfo>, String> {
        let addr = addr.trim().to_string();
        if addr.is_empty() {
            return Err("address is required".into());
        }
        let outs = pm_out_net::NetOutput::connect(addr.as_str()).map_err(|e| e.to_string())?;
        let mut saved = self.endpoint_addresses();
        if !saved.contains(&addr) {
            saved.push(addr.clone());
            self.save_endpoint_addresses(&saved);
        }
        let mut infos = Vec::new();
        for o in outs {
            let out: Arc<dyn Output> = Arc::new(o);
            let id = out.id();
            if self.outputs.get(&id).is_none() {
                tracing::info!(endpoint = %addr, output = %id, "endpoint output");
                let name = out.name();
                self.outputs.register(out.clone());
                self.create_zone(&name, out.clone());
            }
            infos.push(out.info());
        }
        Ok(infos)
    }

    /// Forget a saved endpoint address. Its outputs and zones go away too,
    /// so an app that moved to another address leaves nothing behind.
    pub fn remove_endpoint_address(self: &Arc<Self>, addr: &str) -> bool {
        let addr = addr.trim();
        let mut saved = self.endpoint_addresses();
        let before = saved.len();
        saved.retain(|a| a != addr);
        if saved.len() != before {
            self.save_endpoint_addresses(&saved);
        }
        let prefix = format!("net:{addr}/");
        let gone: Vec<OutputId> = self
            .outputs
            .list()
            .into_iter()
            .map(|o| o.id)
            .filter(|id| id.0.starts_with(&prefix))
            .collect();
        for id in &gone {
            for z in self.zones.list() {
                if z.output == *id {
                    self.remove_zone(&z.id);
                }
            }
            self.outputs.remove(id);
        }
        saved.len() != before || !gone.is_empty()
    }

    /// Re-add the saved endpoint addresses at start and on rescan. Blocking.
    pub fn restore_endpoint_addresses(self: &Arc<Self>) -> Vec<OutputId> {
        let mut new = Vec::new();
        for addr in self.endpoint_addresses() {
            let before: std::collections::HashSet<OutputId> =
                self.outputs.list().into_iter().map(|o| o.id).collect();
            match self.add_endpoint_address(&addr) {
                Ok(outs) => new.extend(outs.into_iter().map(|o| o.id).filter(|id| !before.contains(id))),
                Err(e) => tracing::info!(%addr, "saved endpoint is not reachable now: {e}"),
            }
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
            let out = Arc::new(
                pm_out_cast::CastOutput::new(d.clone(), pull, self.pull_url(&name))
                    .with_describe(self.describe_for_renderers()),
            );
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

    pub fn set_airplay(&self, on: bool) {
        self.airplay.store(on, std::sync::atomic::Ordering::Relaxed);
    }

    /// The MPD servers to offer as outputs (`[outputs] mpd`: `host`, `host:port` or
    /// `password@host:port`); entries that don't parse are logged and skipped.
    pub fn set_mpd(&self, entries: &[String]) {
        let servers = entries
            .iter()
            .filter_map(|e| {
                let s = pm_out_mpd::Server::parse(e);
                if s.is_none() {
                    tracing::warn!(
                        "[outputs] mpd: can't read one entry (host, host:port or password@host:port)"
                    );
                }
                s
            })
            .collect();
        *self.mpd.lock().expect("mpd lock") = servers;
    }

    /// The configured MPD servers that answer, as outputs with zones. Blocking.
    pub fn discover_mpd(self: &Arc<Self>) -> Vec<OutputId> {
        let servers = self.mpd.lock().expect("mpd lock").clone();
        let mut new = Vec::new();
        for server in servers {
            let name = format!(
                "mpd-{}",
                server
                    .address()
                    .chars()
                    .filter(|c| c.is_ascii_alphanumeric())
                    .take(40)
                    .collect::<String>()
            );
            let pull = self.pull_output(&name);
            let out = Arc::new(pm_out_mpd::MpdOutput::new(
                server.clone(),
                pull,
                self.pull_url(&name),
            ));
            let id = out.id();
            if self.outputs.get(&id).is_some() {
                continue;
            }
            match pm_out_mpd::Client::connect(&server) {
                Ok(c) => {
                    tracing::info!(server = %server.address(), version = %c.version, output = %id, "MPD server")
                }
                Err(e) => {
                    tracing::info!(server = %server.address(), "MPD server not reachable now: {e}");
                    continue;
                }
            }
            let zone_name = out.name();
            self.outputs.register(out.clone());
            self.create_zone(&zone_name, out);
            new.push(id);
        }
        new
    }

    pub fn set_bluos(&self, on: bool) {
        self.bluos.store(on, std::sync::atomic::Ordering::Relaxed);
    }

    /// BluOS players (Bluesound, NAD) found over mDNS as outputs with zones, each playing its own
    /// pull stream. Blocking.
    pub fn discover_bluos(self: &Arc<Self>, timeout: std::time::Duration) -> Vec<OutputId> {
        if !self.bluos.load(std::sync::atomic::Ordering::Relaxed) {
            return Vec::new();
        }
        let mut new = Vec::new();
        for p in pm_out_bluos::discover(timeout) {
            let name = format!(
                "bluos-{}",
                p.id.chars()
                    .filter(|c| c.is_ascii_alphanumeric())
                    .take(32)
                    .collect::<String>()
            );
            let pull = self.pull_output(&name);
            let out = Arc::new(pm_out_bluos::BluosOutput::new(
                p.clone(),
                pull,
                self.pull_url(&name),
            ));
            let id = out.id();
            if self.outputs.get(&id).is_some() {
                continue;
            }
            tracing::info!(player = %p.name, output = %id, "BluOS player");
            self.bluos_players
                .lock()
                .expect("bluos lock")
                .insert(id.clone(), out.clone());
            let zone_name = out.name();
            self.outputs.register(out.clone());
            self.create_zone(&zone_name, out);
            new.push(id);
        }
        new
    }

    /// AirPlay 1 receivers found over mDNS as outputs with zones; ones the node cannot play to (they
    /// require encryption) are logged and skipped. Blocking.
    pub fn discover_airplay(self: &Arc<Self>, timeout: std::time::Duration) -> Vec<OutputId> {
        if !self.airplay.load(std::sync::atomic::Ordering::Relaxed) {
            return Vec::new();
        }
        let mut new = Vec::new();
        for d in pm_out_airplay::discover(timeout) {
            if let Some(why) = d.unsupported() {
                tracing::info!(receiver = %d.name, "AirPlay receiver skipped: {why}");
                continue;
            }
            let out = Arc::new(
                pm_out_airplay::AirPlayOutput::new(d.clone()).with_describe(self.describe_for_renderers()),
            );
            let id = out.id();
            if self.outputs.get(&id).is_some() {
                continue;
            }
            tracing::info!(receiver = %d.name, output = %id, "AirPlay receiver");
            let zone_name = out.name();
            self.outputs.register(out.clone());
            self.create_zone(&zone_name, out);
            new.push(id);
        }
        new
    }

    /// Use the snapserver at `addr` (`host` or `host:port`) for Snapcast zones.
    pub fn set_snapcast(&self, addr: Option<String>) {
        *self.snapcast.lock().expect("snapcast lock") = addr;
    }

    /// Every Snapcast group as an output with a zone. Blocking.
    pub fn discover_snapcast(self: &Arc<Self>) -> Vec<OutputId> {
        let Some(addr) = self.snapcast.lock().expect("snapcast lock").clone() else {
            return Vec::new();
        };
        let groups = match pm_out_snapcast::discover(&addr) {
            Ok(g) => g,
            Err(e) => {
                tracing::warn!("Snapcast: {e}");
                return Vec::new();
            }
        };
        let mut new = Vec::new();
        for g in groups {
            let out = Arc::new(pm_out_snapcast::SnapcastOutput::new(&addr, g));
            let id = out.id();
            if self.outputs.get(&id).is_some() {
                continue;
            }
            tracing::info!(group = %out.name(), output = %id, "Snapcast group");
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
        new.extend(self.restore_upnp_addresses());
        new.extend(self.restore_endpoint_addresses());
        new.extend(self.discover_cast(std::time::Duration::from_millis(1500)));
        new.extend(self.discover_snapcast());
        new.extend(self.discover_airplay(std::time::Duration::from_millis(1500)));
        new.extend(self.discover_bluos(std::time::Duration::from_millis(1500)));
        new.extend(self.discover_mpd());
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
        let derived = sync.as_ref().map(|s| profile_url(s.z32()));
        *self.sync.lock().expect("sync lock") = sync;
        // The owner's pubky.app profile is a public URL that reaches them,
        // which is all MusicBrainz, Wikipedia and Pinakes ask of a contact:
        // use it when nothing was set, so discovery works without any form.
        if let Some(url) = derived {
            if self.contact().trim().is_empty() {
                match self.set_contact(&url) {
                    Ok(()) => {
                        tracing::info!(contact = %url, "upstream contact derived from the Pubky identity")
                    }
                    Err(e) => tracing::warn!("derived contact refused: {e}"),
                }
            }
        }
    }

    pub fn sync_handle(&self) -> Option<Arc<crate::sync::PubkySync>> {
        self.sync.lock().expect("sync lock").clone()
    }

    /// Tell event listeners that library-side data (tracks, playlists) changed.
    pub fn notify_library(&self) {
        let _ = self.events.send(ApiEvent::LibraryChanged);
    }

    /// Where the project's apps send a sign-in back to (a static page that forwards to this node).
    pub fn return_page(&self) -> String {
        self.return_page.read().expect("return page lock").clone()
    }

    pub fn set_return_page(&self, url: &str) {
        if !url.trim().is_empty() {
            *self.return_page.write().expect("return page lock") = url.trim().to_string();
        }
    }

    /// The node's Pubky client: one for its lifetime (one DHT node, one homeserver cache), shared by
    /// sign-in, session restore, sync, uploads and reads of other people's data.
    pub fn homeserver(
        &self,
    ) -> Result<pm_pubky::homeserver::NodeHomeserver, pm_pubky::homeserver::HomeserverError> {
        if let Some(h) = self.pubky_client.get() {
            return Ok(h.clone());
        }
        let h = pm_pubky::homeserver::NodeHomeserver::new(self.pubky.network)?;
        Ok(self.pubky_client.get_or_init(|| h).clone())
    }

    /// Use this client (tests on the testnet bring their own).
    pub fn set_homeserver(&self, h: pm_pubky::homeserver::NodeHomeserver) {
        let _ = self.pubky_client.set(h);
    }

    /// The config file the node runs with, when there is one.
    pub fn config_file(&self) -> Option<PathBuf> {
        self.config_file.lock().expect("config file lock").clone()
    }

    /// Set once at start when the node runs with a bundled homeserver.
    pub fn set_bundled_homeserver(&self, ep: crate::bundled::AdminEndpoint) {
        let _ = self.bundled_homeserver.set(ep);
    }

    pub fn bundled_homeserver(&self) -> Option<&crate::bundled::AdminEndpoint> {
        self.bundled_homeserver.get()
    }

    /// Set once at start when the install shares a tunnel folder (`PM_TUNNEL_DIR`).
    pub fn set_tunnel_dir(&self, dir: PathBuf) {
        let _ = self.tunnel_dir.set(dir);
    }

    pub fn tunnel_dir(&self) -> Option<&std::path::Path> {
        self.tunnel_dir.get().map(PathBuf::as_path)
    }

    /// The Google Drive and OneDrive apps from the config (or the build's project apps).
    pub fn set_drive_apps(
        &self,
        apps: std::collections::HashMap<crate::drives::Provider, crate::drives::App>,
    ) {
        self.drives.write().expect("drives").apps = apps;
    }

    /// Set once at start with the bundled homeserver's files folder (`PM_HOMESERVER_FILES_DIR`).
    pub fn set_homeserver_files_dir(&self, dir: PathBuf) {
        let _ = self.homeserver_files_dir.set(dir);
    }

    pub fn homeserver_files_dir(&self) -> Option<&std::path::Path> {
        self.homeserver_files_dir.get().map(PathBuf::as_path)
    }

    /// The node's data folder (where the library database lives).
    pub fn data_dir(&self) -> PathBuf {
        self.db_path
            .parent()
            .map(std::path::Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."))
    }

    pub fn set_config_file(&self, path: Option<PathBuf>) {
        *self.config_file.lock().expect("config file lock") = path;
    }

    pub fn is_scanning(&self) -> bool {
        *self.scanning.lock().expect("scan flag poisoned")
    }
}

const CONTACT_KEY: &str = "node.contact";

/// Where a zone's crossfade length (ms) is kept.
pub fn crossfade_key(zone: &ZoneId) -> String {
    format!("crossfade/{}", zone.0)
}

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

const NODE_ID_KEY: &str = "node.id";

/// This node's id: read from the library, or made and saved on first start.
fn node_id(lib: &Library) -> String {
    if let Ok(Some(id)) = lib.get_setting(NODE_ID_KEY) {
        if !id.is_empty() {
            return id;
        }
    }
    let id = uuid::Uuid::new_v4().to_string();
    if let Err(e) = lib.set_setting(NODE_ID_KEY, &id) {
        tracing::warn!(error = %e, "saving the node id");
    }
    id
}

impl NodeState {
    /// What this node announces on the LAN (pm-discovery) when its API listens on `port`.
    pub fn announcement(&self, port: u16) -> pm_discovery::NodeAd {
        pm_discovery::NodeAd {
            id: self.node_id.clone(),
            name: self.name.clone(),
            port,
            version: self.version.clone(),
            token_required: self.token.is_some(),
        }
    }
}

const STREAM_SECRET_KEY: &str = "stream.secret";

/// The secret behind the pull-stream keys: read from the library, or made and saved.
fn stream_secret(lib: &Library) -> [u8; 32] {
    if let Ok(Some(hex)) = lib.get_setting(STREAM_SECRET_KEY) {
        if let Ok(h) = blake3::Hash::from_hex(hex.trim()) {
            return *h.as_bytes();
        }
    }
    let secret: [u8; 32] = rand::random();
    if let Err(e) = lib.set_setting(STREAM_SECRET_KEY, &blake3::Hash::from_bytes(secret).to_hex()) {
        tracing::warn!(error = %e, "saving the stream secret");
    }
    secret
}
