//! Music kept on this device (STEWARD-DOWNLOADS.md), through `pm-offline`: a sync with the node
//! while it is reachable, and the web UI from a local server while it isn't.

use crate::settings::Settings;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// How often the device syncs while connected.
const SYNC_EVERY: Duration = Duration::from_secs(15 * 60);

pub struct Offline {
    store: Arc<pm_offline::Store>,
    /// `http://127.0.0.1:<port>` once the local server runs; the navigation guard reads it.
    origin: Arc<Mutex<Option<String>>>,
    syncing: AtomicBool,
}

impl Offline {
    /// The store in the app's data folder, or `None` when it can't be opened (offline music is
    /// then off; everything else works).
    pub fn open(dir: std::path::PathBuf, origin: Arc<Mutex<Option<String>>>) -> Option<Arc<Self>> {
        match pm_offline::Store::open(dir) {
            Ok(store) => Some(Arc::new(Self {
                store: Arc::new(store),
                origin,
                syncing: AtomicBool::new(false),
            })),
            Err(e) => {
                tracing::warn!("offline music is off: {e}");
                None
            }
        }
    }

    pub fn has_downloads(&self) -> bool {
        !self.store.index().tracks.is_empty()
    }

    /// Start the local server (once) and give the address of its Downloads page.
    pub async fn page(&self) -> Result<String, String> {
        let existing = self.origin.lock().ok().and_then(|o| o.clone());
        let origin = match existing {
            Some(o) => o,
            None => {
                let (addr, _task) = pm_offline::server::serve(self.store.clone())
                    .await
                    .map_err(|e| format!("Can't start offline playback: {e}"))?;
                let o = format!("http://{addr}");
                if let Ok(mut cur) = self.origin.lock() {
                    *cur = Some(o.clone());
                }
                o
            }
        };
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

    /// Sync now and then every [`SYNC_EVERY`] while `still(&s)` says this node is the current one.
    /// One sync loop at a time.
    pub fn spawn_sync(self: &Arc<Self>, s: Settings, still: impl Fn(&Settings) -> bool + Send + 'static) {
        if self.syncing.swap(true, Ordering::SeqCst) {
            return;
        }
        let me = self.clone();
        tauri::async_runtime::spawn(async move {
            loop {
                match pm_offline::Node::new(s.base(), &s.token) {
                    Ok(node) => match pm_offline::sync(&me.store, &node, &Self::device()).await {
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
                tokio::time::sleep(SYNC_EVERY).await;
                if !still(&s) {
                    break;
                }
            }
            me.syncing.store(false, Ordering::SeqCst);
        });
    }
}
