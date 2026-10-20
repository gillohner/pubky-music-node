//! Sync the local music posts with the user's private homeserver folder.
//!
//! Push: every row the library marks unsynced is written to its path
//! (`/priv/pubky.app/posts/<id>` or the public path); tombstones are
//! deleted remotely and purged. Pull: private posts on the homeserver that
//! the node does not have are imported, so a fresh node recovers its
//! playlists. Conflicts resolve by `updated_at`, newest wins.

use crate::playlists::{now_micros, store};
use crate::types::SyncReport;
use pm_library::Library;
use pm_pubky::homeserver::{HomeserverError, NodeHomeserver};
use pm_pubky::schemas::post::TYPE_PLAYLIST;
use pm_pubky::{MusicContent, MusicPost, Network, Visibility};
use pubky::PubkySession;
use std::sync::{Arc, Mutex};

#[derive(Debug, thiserror::Error)]
pub enum SyncError {
    #[error(transparent)]
    Homeserver(#[from] HomeserverError),
    #[error("library: {0}")]
    Library(#[from] pm_library::LibraryError),
    #[error("storing pulled post: {0}")]
    Store(String),
}

pub struct PubkySync {
    homeserver: NodeHomeserver,
    session: PubkySession,
    z32: String,
}

impl PubkySync {
    /// Restore a session from an exported secret and prepare to sync.
    pub async fn connect(network: Network, secret: &str) -> Result<Self, HomeserverError> {
        let homeserver = NodeHomeserver::new(network)?;
        let session = homeserver.restore_session(secret.trim()).await?;
        let z32 = session.public_key().to_string();
        Ok(Self {
            homeserver,
            session,
            z32,
        })
    }

    /// The signed-in user's raw z32 key.
    pub fn z32(&self) -> &str {
        &self.z32
    }

    pub async fn sync(&self, library: &Arc<Mutex<Library>>) -> Result<SyncReport, SyncError> {
        let mut report = SyncReport::default();

        // Push local changes.
        let pending = library.lock().expect("library poisoned").unsynced_music_posts()?;
        for row in pending {
            let visibility = if row.visibility == "public" {
                Visibility::Public
            } else {
                Visibility::Private
            };
            let path = MusicPost::path(&row.id, visibility);
            if row.deleted {
                match self.homeserver.delete(&self.session, &path).await {
                    Ok(()) => report.deleted += 1,
                    // Already gone remotely counts as done.
                    Err(HomeserverError::Pubky(e)) if e.to_string().contains("404") => report.deleted += 1,
                    Err(e) => {
                        report.errors.push(format!("delete {path}: {e}"));
                        continue;
                    }
                }
            } else {
                let post: pubky_app_specs::PubkyAppPost = match serde_json::from_str(&row.json) {
                    Ok(p) => p,
                    Err(e) => {
                        report.errors.push(format!("{}: stored json: {e}", row.id));
                        continue;
                    }
                };
                if let Err(e) = self.homeserver.write_object(&self.session, &path, &post).await {
                    report.errors.push(format!("write {path}: {e}"));
                    continue;
                }
                report.pushed += 1;
            }
            library
                .lock()
                .expect("library poisoned")
                .mark_music_post_synced(&row.id, now_micros())?;
        }

        // Pull what the homeserver has that we do not (or has newer).
        let remote = self
            .homeserver
            .read_music_posts(&self.z32, Visibility::Private, Some(&self.session))
            .await?;
        let lib = library.lock().expect("library poisoned");
        for (path, post) in remote {
            let id = path.rsplit('/').next().unwrap_or("").to_string();
            if post.content.type_name() != TYPE_PLAYLIST {
                continue;
            }
            let remote_updated = match &post.content {
                MusicContent::Playlist(p) => p.updated_at,
                other => other.created_micros(),
            };
            let local = lib.get_music_post(&id)?;
            let take = match &local {
                None => true,
                Some(row) => remote_updated > row.updated_at,
            };
            if take {
                store(&lib, &post, &id, Visibility::Private).map_err(|e| SyncError::Store(e.message))?;
                lib.mark_music_post_synced(&id, now_micros())?;
                report.pulled += 1;
            }
        }
        Ok(report)
    }
}

impl std::fmt::Debug for PubkySync {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PubkySync").field("z32", &self.z32).finish()
    }
}
