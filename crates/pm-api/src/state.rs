//! Shared node state behind the API: library, outputs, zones, event fan-out.

use crate::sources::NodeSources;
use crate::types::ApiEvent;
use pm_audio::PipelineConfig;
use pm_library::Library;
use pm_zones::{Output, OutputId, OutputRegistry, Zone, ZoneId, Zones};
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
    pub discovery: Option<Arc<pm_pubky::Discovery>>,
    pub events: broadcast::Sender<ApiEvent>,
    /// Homeserver sync, when the node has a Pubky session.
    pub sync: Mutex<Option<Arc<crate::sync::PubkySync>>>,
    /// Folder watcher, kept alive here.
    pub watcher: Mutex<Option<notify::RecommendedWatcher>>,
    /// How to find outputs again (local devices, endpoints); set by the binary.
    #[allow(clippy::type_complexity)]
    pub discover: Mutex<Option<Arc<dyn Fn() -> Vec<Arc<dyn Output>> + Send + Sync>>>,
    scanning: Mutex<bool>,
}

impl NodeState {
    pub fn new(settings: NodeSettings, library: Library, outputs: OutputRegistry) -> Arc<Self> {
        let library = Arc::new(Mutex::new(library));
        let sources = Arc::new(NodeSources::new(library.clone(), settings.pipeline));
        let (events, _) = broadcast::channel(1024);
        let discovery = match settings.pubky.validate_contact() {
            Ok(()) => match (
                pm_pubky::PinakesClient::new(&settings.pubky),
                pm_pubky::NexusClient::new(&settings.pubky),
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
            zones: Zones::new(sources),
            token: settings.token,
            subsonic_user: settings.subsonic_user,
            discovery,
            events,
            sync: Mutex::new(None),
            watcher: Mutex::new(None),
            discover: Mutex::new(None),
            scanning: Mutex::new(false),
        })
    }

    /// Create a zone and forward its state and events to the API stream.
    /// Call from inside a tokio runtime.
    pub fn create_zone(self: &Arc<Self>, name: &str, output: Arc<dyn Output>) -> Arc<Zone> {
        let zone = self.zones.create(name, output);
        let events = self.events.clone();
        let mut state_rx = zone.watch();
        tokio::spawn(async move {
            while state_rx.changed().await.is_ok() {
                let state = state_rx.borrow_and_update().clone();
                let _ = events.send(ApiEvent::Zone { state });
            }
        });
        let events = self.events.clone();
        let mut ev_rx = zone.subscribe();
        let id = zone.id().clone();
        tokio::spawn(async move {
            loop {
                match ev_rx.recv().await {
                    Ok(event) => {
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
    pub fn watch_folders(self: &Arc<Self>, quiet: std::time::Duration) {
        if self.scan_folders.is_empty() {
            return;
        }
        match crate::watch::spawn(self.clone(), self.scan_folders.clone(), quiet) {
            Ok(w) => *self.watcher.lock().expect("watcher lock") = Some(w),
            Err(e) => tracing::warn!("folder watching unavailable: {e}"),
        }
    }

    pub fn set_discover(&self, f: Arc<dyn Fn() -> Vec<Arc<dyn Output>> + Send + Sync>) {
        *self.discover.lock().expect("discover lock") = Some(f);
    }

    /// Run discovery again: register outputs not seen before and give each
    /// a zone. Returns the ids that are new. Blocking (mDNS browse, TCP
    /// connects); call from `spawn_blocking`.
    pub fn rescan_outputs(self: &Arc<Self>) -> Vec<OutputId> {
        let Some(f) = self.discover.lock().expect("discover lock").clone() else {
            return Vec::new();
        };
        let mut new = Vec::new();
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
