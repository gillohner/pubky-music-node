//! Shared node state behind the API: library, outputs, zones, event fan-out.

use crate::sources::NodeSources;
use crate::types::ApiEvent;
use pm_audio::PipelineConfig;
use pm_library::Library;
use pm_zones::{Output, OutputRegistry, Zone, ZoneId, Zones};
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
    pub pipeline: PipelineConfig,
    /// Optional bearer token; `None` = no auth (bind to localhost only).
    pub token: Option<String>,
}

pub struct NodeState {
    pub name: String,
    pub version: String,
    pub library: Arc<Mutex<Library>>,
    /// Path of the SQLite file, so a scan can use its own connection.
    pub db_path: PathBuf,
    pub scan_folders: Vec<PathBuf>,
    pub outputs: OutputRegistry,
    pub zones: Zones,
    /// Optional bearer token; `None` = no auth (bind to localhost only).
    pub token: Option<String>,
    pub events: broadcast::Sender<ApiEvent>,
    /// Homeserver sync, when the node has a Pubky session.
    pub sync: Mutex<Option<Arc<crate::sync::PubkySync>>>,
    scanning: Mutex<bool>,
}

impl NodeState {
    pub fn new(settings: NodeSettings, library: Library, outputs: OutputRegistry) -> Arc<Self> {
        let library = Arc::new(Mutex::new(library));
        let sources = Arc::new(NodeSources::new(library.clone(), settings.pipeline));
        let (events, _) = broadcast::channel(1024);
        Arc::new(Self {
            name: settings.name,
            version: settings.version,
            library,
            db_path: settings.db_path,
            scan_folders: settings.scan_folders,
            outputs,
            zones: Zones::new(sources),
            token: settings.token,
            events,
            sync: Mutex::new(None),
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
