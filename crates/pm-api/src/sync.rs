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
use pm_pubky::schemas::post::{TYPE_PLAYLIST, TYPE_REVIEW};
use pm_pubky::{Favourite, MusicContent, MusicPost, Network, Visibility};
use pubky::PubkySession;
use pubky_app_specs::traits::HasIdPath;
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

    /// Wrap an existing session (tests, or a sign-in flow handled elsewhere).
    pub fn from_session(homeserver: NodeHomeserver, session: PubkySession) -> Self {
        let z32 = session.public_key().to_string();
        Self {
            homeserver,
            session,
            z32,
        }
    }

    pub fn session(&self) -> &PubkySession {
        &self.session
    }

    /// Tell the user's other devices where this node is (ADR 0008).
    pub async fn publish_node(
        &self,
        name: &str,
        api_url: &str,
        token: Option<&str>,
        version: &str,
    ) -> Result<(), SyncError> {
        let record = pm_pubky::homeserver::NodeRecord {
            name: name.to_string(),
            api_url: api_url.to_string(),
            token: token.unwrap_or("").to_string(),
            version: version.to_string(),
            updated_at: now_micros(),
        };
        Ok(self
            .homeserver
            .publish_node_record(&self.session, &record)
            .await?)
    }

    pub async fn list_private_files(&self) -> Result<Vec<pm_pubky::homeserver::RemoteFile>, SyncError> {
        Ok(self.homeserver.list_private_files(&self.session).await?)
    }

    pub async fn upload_private(&self, path: &str, bytes: Vec<u8>) -> Result<(), SyncError> {
        Ok(self.homeserver.upload_private(&self.session, path, bytes).await?)
    }

    pub async fn download_private(&self, path: &str) -> Result<Vec<u8>, SyncError> {
        Ok(self.homeserver.download_private(&self.session, path).await?)
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

        // Favourites: push bookmarks and tombstones the same way.
        let pending = library.lock().expect("library poisoned").unsynced_favourites()?;
        for row in pending {
            let path = Favourite::path(&row.id, Visibility::Private);
            if row.deleted {
                match self.homeserver.delete(&self.session, &path).await {
                    Ok(()) => report.deleted += 1,
                    Err(HomeserverError::Pubky(e)) if e.to_string().contains("404") => report.deleted += 1,
                    Err(e) => {
                        report.errors.push(format!("delete {path}: {e}"));
                        continue;
                    }
                }
            } else {
                let Ok(uri) = pm_model::MusicUri::parse(&row.uri) else {
                    report
                        .errors
                        .push(format!("{}: not a canonical music URL", row.uri));
                    continue;
                };
                let b = Favourite {
                    id: row.id.clone(),
                    uri,
                    created_at: row.created_at,
                }
                .to_bookmark();
                if let Err(e) = self.homeserver.write_object(&self.session, &path, &b).await {
                    report.errors.push(format!("write {path}: {e}"));
                    continue;
                }
                report.pushed += 1;
            }
            library
                .lock()
                .expect("library poisoned")
                .mark_favourite_synced(&row.id, now_micros())?;
        }
        let remote_favs = self
            .homeserver
            .read_favourites(&self.z32, Visibility::Private, Some(&self.session))
            .await?;
        {
            let lib = library.lock().expect("library poisoned");
            for (_, f) in remote_favs {
                let url = f.uri.to_url();
                if lib.favourite_by_uri(&url)?.is_none() {
                    lib.add_favourite(&f.id, &url, f.created_at)?;
                    lib.mark_favourite_synced(&f.id, now_micros())?;
                    report.pulled += 1;
                }
            }
        }

        // Tags: public PubkyAppTags, pushed and pulled the same way.
        let pending = library.lock().expect("library poisoned").unsynced_tags()?;
        for row in pending {
            let path = pubky_app_specs::PubkyAppTag::create_path(&row.id);
            if row.deleted {
                match self.homeserver.delete(&self.session, &path).await {
                    Ok(()) => report.deleted += 1,
                    Err(HomeserverError::Pubky(e)) if e.to_string().contains("404") => report.deleted += 1,
                    Err(e) => {
                        report.errors.push(format!("delete {path}: {e}"));
                        continue;
                    }
                }
            } else {
                let tag = pubky_app_specs::PubkyAppTag {
                    uri: row.uri.clone(),
                    label: row.label.clone(),
                    created_at: row.created_at,
                };
                if let Err(e) = self.homeserver.write_object(&self.session, &path, &tag).await {
                    report.errors.push(format!("write {path}: {e}"));
                    continue;
                }
                report.pushed += 1;
            }
            library
                .lock()
                .expect("library poisoned")
                .mark_tag_synced(&row.id, now_micros())?;
        }
        match self.homeserver.read_music_tags(&self.z32).await {
            Ok(remote_tags) => {
                let lib = library.lock().expect("library poisoned");
                for (path, t) in remote_tags {
                    let id = path.rsplit('/').next().unwrap_or("").to_string();
                    if lib.tag_by_id(&id)?.is_none() {
                        lib.add_tag(&id, &t.uri, &t.label, t.created_at)?;
                        lib.mark_tag_synced(&id, now_micros())?;
                        report.pulled += 1;
                    }
                }
            }
            Err(e) => report.errors.push(format!("reading tags: {e}")),
        }

        // Pull what the homeserver has that we do not (or has newer).
        let remote = self
            .homeserver
            .read_music_posts(&self.z32, Visibility::Private, Some(&self.session))
            .await?;
        let lib = library.lock().expect("library poisoned");
        for (path, post) in remote {
            let id = path.rsplit('/').next().unwrap_or("").to_string();
            if !matches!(post.content.type_name(), t if t == TYPE_PLAYLIST || t == TYPE_REVIEW) {
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
