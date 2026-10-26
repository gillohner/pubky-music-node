//! Music kept on this device (STEWARD-DOWNLOADS.md), through `pm-offline`: a sync with the node
//! while it is reachable, and the web UI from a local server while it isn't. Each node keeps its
//! own folder (`offline/<node id>/`), so one node's plays, hearts and keeps never reach another.

use crate::settings::Settings;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// How often the device syncs while connected.
const SYNC_EVERY: Duration = Duration::from_secs(15 * 60);

/// The local server, for one node's store.
struct Server {
    node_id: String,
    origin: String,
    task: tokio::task::JoinHandle<()>,
}

pub struct Offline {
    /// `offline/` in the app's data folder; each node's store is in a folder under it.
    base: PathBuf,
    /// Every store opened in this run, by node id: the sync loop and the local server share
    /// them, so a node's folder is never open twice (each `Store` holds its own queue lock and
    /// index).
    open: Mutex<HashMap<String, Arc<pm_offline::Store>>>,
    server: tokio::sync::Mutex<Option<Server>>,
    /// `http://127.0.0.1:<port>` while the local server runs; the navigation guard reads it.
    origin: Arc<Mutex<Option<String>>>,
    /// Raised by every [`Offline::spawn_sync`]: an older loop sees it and stops.
    generation: AtomicU64,
    /// One sync at a time, also while a replaced loop finishes its last one.
    syncing: tokio::sync::Mutex<()>,
}

impl Offline {
    /// Offline music in `base`, or `None` when the folder can't be made (offline music is then
    /// off; everything else works).
    pub fn open(base: PathBuf, origin: Arc<Mutex<Option<String>>>) -> Option<Arc<Self>> {
        if let Err(e) = std::fs::create_dir_all(&base) {
            tracing::warn!("offline music is off: {e}");
            return None;
        }
        Some(Arc::new(Self {
            base,
            open: Mutex::default(),
            server: Default::default(),
            origin,
            generation: AtomicU64::new(0),
            syncing: Default::default(),
        }))
    }

    /// The store of the node with this id, opened once. `None` without an id (a node that never
    /// answered) or when it can't be opened.
    fn store(&self, node_id: &str) -> Option<Arc<pm_offline::Store>> {
        if node_id.is_empty() {
            return None;
        }
        let mut open = self.open.lock().ok()?;
        if let Some(s) = open.get(node_id) {
            return Some(s.clone());
        }
        match pm_offline::Store::open(pm_offline::node_dir(&self.base, node_id)) {
            Ok(s) => {
                let s = Arc::new(s);
                open.insert(node_id.to_string(), s.clone());
                Some(s)
            }
            Err(e) => {
                tracing::warn!("offline music for this node is off: {e}");
                None
            }
        }
    }

    /// Whether this device keeps music from the node with this id. Doesn't make its folder.
    pub fn has_downloads(&self, node_id: &str) -> bool {
        !node_id.is_empty()
            && pm_offline::node_dir(&self.base, node_id)
                .join("index.json")
                .is_file()
            && self.store(node_id).is_some_and(|s| !s.index().tracks.is_empty())
    }

    /// Start the local server for this node's store (once; again for another node) and give the
    /// address of its Downloads page.
    pub async fn page(&self, node_id: &str) -> Result<String, String> {
        let store = self
            .store(node_id)
            .ok_or("Nothing is downloaded from this node")?;
        let mut server = self.server.lock().await;
        let origin = match server.as_ref() {
            Some(s) if s.node_id == node_id && !s.task.is_finished() => s.origin.clone(),
            _ => {
                if let Some(old) = server.take() {
                    old.task.abort();
                }
                let (addr, task) = pm_offline::server::serve(store)
                    .await
                    .map_err(|e| format!("Can't start offline playback: {e}"))?;
                let origin = format!("http://{addr}");
                *server = Some(Server {
                    node_id: node_id.to_string(),
                    origin: origin.clone(),
                    task,
                });
                origin
            }
        };
        if let Ok(mut cur) = self.origin.lock() {
            *cur = Some(origin.clone());
        }
        Ok(format!("{origin}/downloads"))
    }

    fn device() -> pm_offline::SyncOptions {
        let (name, kind) = match std::env::consts::OS {
            "android" => ("This phone", "android"),
            "ios" => ("This phone", "ios"),
            _ => ("This computer", "desktop"),
        };
        pm_offline::SyncOptions {
            name: name.into(),
            kind: kind.into(),
        }
    }

    /// Sync now and then every [`SYNC_EVERY`], with the settings `current` gives at each round
    /// (the node's address, the tunnel's port and the token may change between rounds). A newer
    /// call replaces this loop; it stops when there is no node any more.
    pub fn spawn_sync(self: &Arc<Self>, current: impl Fn() -> Option<Settings> + Send + 'static) {
        let mine = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        let me = self.clone();
        tauri::async_runtime::spawn(async move {
            loop {
                {
                    let _one = me.syncing.lock().await;
                    if me.generation.load(Ordering::SeqCst) != mine {
                        break;
                    }
                    let Some(s) = current() else { break };
                    me.sync_once(&s).await;
                }
                tokio::time::sleep(SYNC_EVERY).await;
                if me.generation.load(Ordering::SeqCst) != mine {
                    break;
                }
            }
        });
    }

    async fn sync_once(&self, s: &Settings) {
        let Some(store) = self.store(&s.node_id) else {
            tracing::info!("offline: no sync without the node's id");
            return;
        };
        match pm_offline::Node::new(s.base(), &s.token) {
            Ok(node) => match pm_offline::sync(&store, &node, &Self::device()).await {
                Ok(r) => tracing::info!(
                    downloaded = r.downloaded.len(),
                    removed = r.removed.len(),
                    sent = r.sent,
                    failed = r.failed.len(),
                    "offline: synced"
                ),
                Err(e) => tracing::info!("offline: sync stopped: {e}"),
            },
            Err(e) => tracing::warn!("offline: {e}"),
        }
    }
}
