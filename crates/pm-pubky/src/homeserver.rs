//! Read and write the music posts on homeservers through the `pubky` SDK
//! (0.16.2). The node writes the user's own posts with their session (to
//! the private folder by default) and reads anyone's public posts without
//! one. Private posts are listed and read only with the owner's session.
//!
//! Sign-in is Pubky Ring's `pubkyauth` flow (M9) and lives outside this crate;
//! here a session is either handed in or restored from an exported grant
//! secret with [`NodeHomeserver::restore_session`].
//!
//! API notes against 0.16.2 (the skill notes describe 0.12): `signin` takes a
//! `ClientId`, `signup` returns `()`, `restore_session(&str)` accepts both
//! grant and legacy cookie secrets, `PublicStorage::list` needs a trailing
//! `/` and returns `PubkyResource { owner, path }` entries, `get_json` is
//! behind the `json` feature.

use crate::config::Network;
use crate::schemas::{Activity, Favourite, MusicPost, Playlist, Review, Visibility};
use pubky::{Pubky, PubkySession, PublicKey};
use serde::Serialize;

/// The homeserver answers 404 for a directory nobody has written to yet.
fn is_not_found(e: &pubky::Error) -> bool {
    matches!(
        e,
        pubky::Error::Request(pubky::errors::RequestError::Server { status, .. })
            if *status == reqwest::StatusCode::NOT_FOUND
    )
}

#[derive(Debug, thiserror::Error)]
pub enum HomeserverError {
    #[error(transparent)]
    Pubky(#[from] pubky::Error),
    #[error("invalid public key: {0}")]
    Key(String),
    #[error("transport: {0}")]
    Transport(String),
    #[error("object at {path} is invalid: {source}")]
    Schema {
        path: String,
        source: crate::schemas::SchemaError,
    },
    #[error("reading private objects needs the owner's session")]
    NeedsSession,
}

/// Reads in flight at once during a sync.
pub const FETCH_AT_ONCE: usize = 8;

/// Default page size for listings; the homeserver may cap it.
pub const LIST_LIMIT: u16 = 100;

#[derive(Clone)]
pub struct NodeHomeserver {
    pubky: Pubky,
}

/// Pubky clients built in this process: each one starts its own DHT node and keeps its own
/// homeserver cache, so a node builds one and shares it (STEWARD-FIX-pubky-signin-speed.md).
static CLIENTS_BUILT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// How many Pubky clients this process has built (tests check it stays at one).
pub fn clients_built() -> usize {
    CLIENTS_BUILT.load(std::sync::atomic::Ordering::Relaxed)
}

impl NodeHomeserver {
    /// A new Pubky client. A node builds one for its lifetime and clones it: clones share the
    /// connection pool, the DHT node and the homeserver cache.
    pub fn new(network: Network) -> Result<Self, HomeserverError> {
        CLIENTS_BUILT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let pubky = match network {
            Network::Mainnet => Pubky::new()?,
            Network::Testnet => Pubky::testnet()?,
        };
        Ok(Self { pubky })
    }

    /// Wrap a client the caller configured (timeouts, testnet host).
    pub fn from_pubky(pubky: Pubky) -> Self {
        Self { pubky }
    }

    pub fn pubky(&self) -> &Pubky {
        &self.pubky
    }

    /// Restore a session from an exported grant secret. The secret works like
    /// a bearer token: keep it in the OS keychain, never log it.
    pub async fn restore_session(&self, secret: &str) -> Result<PubkySession, HomeserverError> {
        Ok(self.pubky.restore_session(secret).await?)
    }

    fn parse_key(z32: &str) -> Result<PublicKey, HomeserverError> {
        PublicKey::try_from(z32).map_err(|e| HomeserverError::Key(e.to_string()))
    }

    /// List a user's posts directory: public through anyone's view, private
    /// only with the owner's session. Follows the cursor to the end.
    async fn list_posts(
        &self,
        user: &PublicKey,
        visibility: Visibility,
        session: Option<&PubkySession>,
    ) -> Result<Vec<String>, HomeserverError> {
        self.list_dir(user, &visibility.posts_dir(), visibility, session)
            .await
    }

    /// Every entry path directly under `dir`, following the listing cursor.
    /// A directory that does not exist yet (a fresh homeserver before the
    /// first write) lists as empty rather than failing.
    async fn list_dir(
        &self,
        user: &PublicKey,
        dir: &str,
        visibility: Visibility,
        session: Option<&PubkySession>,
    ) -> Result<Vec<String>, HomeserverError> {
        self.list_paths(user, dir, visibility, session, true).await
    }

    /// Every file path under `dir` at any depth (a v1 post tree is `{id}/{editId}.json`).
    pub async fn list_tree(
        &self,
        user_z32: &str,
        dir: &str,
        visibility: Visibility,
        session: Option<&PubkySession>,
    ) -> Result<Vec<String>, HomeserverError> {
        let user = Self::parse_key(user_z32)?;
        self.list_paths(&user, dir, visibility, session, false).await
    }

    /// The newest version of every v1 post of a user under one root and namespace.
    pub async fn v1_heads(
        &self,
        user_z32: &str,
        root: crate::v1::Root,
        namespace: &str,
        session: Option<&PubkySession>,
    ) -> Result<std::collections::BTreeMap<String, crate::v1::Head>, HomeserverError> {
        let visibility = match root {
            crate::v1::Root::Pub => Visibility::Public,
            crate::v1::Root::Priv => Visibility::Private,
        };
        let dir = crate::v1::posts_dir(root, namespace);
        Ok(crate::v1::heads(
            self.list_tree(user_z32, &dir, visibility, session).await?,
        ))
    }

    /// The body at `path` of a user's storage.
    pub async fn get_bytes(
        &self,
        user_z32: &str,
        path: &str,
        visibility: Visibility,
        session: Option<&PubkySession>,
    ) -> Result<Vec<u8>, HomeserverError> {
        let user = Self::parse_key(user_z32)?;
        self.fetch(&user, path, visibility, session).await
    }

    /// The signed-in user's v1 bookmarks of music URLs: `(path, uri, created_at)`. Bookmarks of
    /// anything else, and invalid ones, are skipped.
    pub async fn read_v1_favourites(
        &self,
        session: &PubkySession,
    ) -> Result<Vec<(String, pm_model::MusicUri, i64)>, HomeserverError> {
        self.read_v1_favourites_except(session, &Default::default()).await
    }

    /// The same, without downloading the bookmarks the caller already knows: `known` maps a
    /// bookmark's file name to its music URL and time, and a listed file of that name is taken
    /// from it. A sync then costs a listing, not one request per favourite.
    pub async fn read_v1_favourites_except(
        &self,
        session: &PubkySession,
        known: &std::collections::HashMap<String, (pm_model::MusicUri, i64)>,
    ) -> Result<Vec<(String, pm_model::MusicUri, i64)>, HomeserverError> {
        let user = session.public_key();
        let mut out = Vec::new();
        let mut unknown = Vec::new();
        for path in self
            .list_dir(
                &user,
                &crate::v1::bookmarks_dir(),
                Visibility::Private,
                Some(session),
            )
            .await?
        {
            match known.get(file_name(&path)) {
                Some((uri, created_at)) => out.push((path, uri.clone(), *created_at)),
                None => unknown.push(path),
            }
        }
        for (path, bytes) in self
            .fetch_many(&user, unknown, Visibility::Private, Some(session))
            .await?
        {
            if let Some((uri, created_at)) = crate::v1::favourite_from_bookmark(&path, &bytes) {
                out.push((path, uri, created_at));
            }
        }
        Ok(out)
    }

    /// Fetch many paths [`FETCH_AT_ONCE`] at a time, results in the order of `paths`: a sync of
    /// hundreds of objects was one request after another (STEWARD-FIX-pubky-signin-speed.md).
    async fn fetch_many(
        &self,
        user: &PublicKey,
        paths: Vec<String>,
        visibility: Visibility,
        session: Option<&PubkySession>,
    ) -> Result<Vec<(String, Vec<u8>)>, HomeserverError> {
        use futures_util::StreamExt;
        let results: Vec<(String, Result<Vec<u8>, HomeserverError>)> = futures_util::stream::iter(paths)
            .map(|path| async move {
                let r = self.fetch(user, &path, visibility, session).await;
                (path, r)
            })
            .buffered(FETCH_AT_ONCE)
            .collect()
            .await;
        results.into_iter().map(|(p, r)| r.map(|b| (p, b))).collect()
    }

    async fn list_paths(
        &self,
        user: &PublicKey,
        dir: &str,
        visibility: Visibility,
        session: Option<&PubkySession>,
        shallow: bool,
    ) -> Result<Vec<String>, HomeserverError> {
        let dir = dir.to_string();
        let mut all = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let page = match (visibility, session) {
                (Visibility::Public, _) => {
                    let public = self.pubky.public_storage();
                    let mut b = public
                        .list((user, dir.as_str()))?
                        .limit(LIST_LIMIT)
                        .shallow(shallow);
                    if let Some(c) = &cursor {
                        b = b.cursor(c);
                    }
                    match b.send().await {
                        Ok(p) => p,
                        Err(e) if is_not_found(&e) => break,
                        Err(e) => return Err(e.into()),
                    }
                }
                (Visibility::Private, Some(session)) => {
                    let storage = session.storage();
                    let mut b = storage.list(dir.as_str())?.limit(LIST_LIMIT).shallow(shallow);
                    if let Some(c) = &cursor {
                        b = b.cursor(c);
                    }
                    match b.send().await {
                        Ok(p) => p,
                        Err(e) if is_not_found(&e) => break,
                        Err(e) => return Err(e.into()),
                    }
                }
                (Visibility::Private, None) => return Err(HomeserverError::NeedsSession),
            };
            let n = page.len();
            cursor = page.last().map(|e| e.to_pubky_url());
            all.extend(page.into_iter().map(|e| e.path.to_string()));
            if n < LIST_LIMIT as usize {
                break;
            }
        }
        Ok(all)
    }

    async fn fetch(
        &self,
        user: &PublicKey,
        path: &str,
        visibility: Visibility,
        session: Option<&PubkySession>,
    ) -> Result<Vec<u8>, HomeserverError> {
        let resp = match (visibility, session) {
            (Visibility::Public, _) => self.pubky.public_storage().get((user, path)).await?,
            (Visibility::Private, Some(session)) => session.storage().get(path).await?,
            (Visibility::Private, None) => return Err(HomeserverError::NeedsSession),
        };
        resp.bytes()
            .await
            .map(|b| b.to_vec())
            .map_err(|e| HomeserverError::Transport(e.to_string()))
    }

    /// Every music post of a user: `(path, post)`. Ordinary posts are
    /// skipped silently; invalid music envelopes are skipped with a warning
    /// (an indexer never trusts a homeserver body). Private posts need the
    /// owner's session.
    pub async fn read_music_posts(
        &self,
        user_z32: &str,
        visibility: Visibility,
        session: Option<&PubkySession>,
    ) -> Result<Vec<(String, MusicPost)>, HomeserverError> {
        let user = Self::parse_key(user_z32)?;
        let mut out = Vec::new();
        let paths = self.list_posts(&user, visibility, session).await?;
        for (path, bytes) in self.fetch_many(&user, paths, visibility, session).await? {
            let id = path.rsplit('/').next().unwrap_or("").to_string();
            match MusicPost::try_from_bytes(&bytes, &id) {
                Ok(Some(post)) => out.push((path, post)),
                Ok(None) => {}
                Err(source) => tracing::warn!(%path, %source, "skipping invalid music post"),
            }
        }
        Ok(out)
    }

    /// The user's favourites (music bookmarks) under `visibility`; other
    /// bookmarks (of posts) are skipped.
    pub async fn read_favourites(
        &self,
        user_z32: &str,
        visibility: Visibility,
        session: Option<&PubkySession>,
    ) -> Result<Vec<(String, Favourite)>, HomeserverError> {
        let user = Self::parse_key(user_z32)?;
        let dir = Favourite::dir(visibility);
        let mut out = Vec::new();
        let paths = self.list_dir(&user, &dir, visibility, session).await?;
        for (path, bytes) in self.fetch_many(&user, paths, visibility, session).await? {
            let id = path.rsplit('/').next().unwrap_or("").to_string();
            match serde_json::from_slice::<pubky_app_specs::PubkyAppBookmark>(&bytes) {
                Ok(b) => {
                    if let Some(f) = Favourite::from_bookmark(&id, &b) {
                        out.push((path, f));
                    }
                }
                Err(e) => tracing::warn!(%path, error = %e, "skipping invalid bookmark"),
            }
        }
        Ok(out)
    }

    /// A user's public tags on music URLs: `(path, tag)`; other tags skipped.
    /// Reads the node's namespace and the legacy `pubky.app/tags/` (until
    /// the sync has moved those).
    pub async fn read_music_tags(
        &self,
        user_z32: &str,
    ) -> Result<Vec<(String, pubky_app_specs::PubkyAppTag)>, HomeserverError> {
        self.read_music_tags_except(user_z32, &Default::default()).await
    }

    /// The music tags whose id (file name) is not in `known`: the ones the caller does not have
    /// yet. Known tags are listed but not downloaded.
    pub async fn read_music_tags_except(
        &self,
        user_z32: &str,
        known: &std::collections::HashSet<String>,
    ) -> Result<Vec<(String, pubky_app_specs::PubkyAppTag)>, HomeserverError> {
        self.read_music_tags_in(user_z32, known, true).await
    }

    /// The same; `legacy`: also the old `pubky.app/tags/` folder, which once the node has moved
    /// its tags out holds only other apps' tags (hundreds for an active user: skip it then).
    pub async fn read_music_tags_in(
        &self,
        user_z32: &str,
        known: &std::collections::HashSet<String>,
        legacy: bool,
    ) -> Result<Vec<(String, pubky_app_specs::PubkyAppTag)>, HomeserverError> {
        let user = Self::parse_key(user_z32)?;
        let mut out = Vec::new();
        let mut paths = self
            .list_dir(&user, crate::schemas::TAG_DIR, Visibility::Public, None)
            .await
            .unwrap_or_default();
        if legacy {
            paths.extend(
                self.list_dir(&user, crate::schemas::LEGACY_TAG_DIR, Visibility::Public, None)
                    .await?,
            );
        }
        paths.retain(|p| !known.contains(file_name(p)));
        for (path, bytes) in self.fetch_many(&user, paths, Visibility::Public, None).await? {
            match serde_json::from_slice::<pubky_app_specs::PubkyAppTag>(&bytes) {
                Ok(t) if pm_model::MusicUri::parse(&t.uri).is_ok() => out.push((path, t)),
                Ok(_) => {}
                Err(e) => tracing::warn!(%path, error = %e, "skipping invalid tag"),
            }
        }
        Ok(out)
    }

    /// The public paths under `dir` of a user (`/pub/...`), e.g. to check
    /// where tags live.
    pub async fn public_paths(&self, user_z32: &str, dir: &str) -> Result<Vec<String>, HomeserverError> {
        let user = Self::parse_key(user_z32)?;
        self.list_dir(&user, dir, Visibility::Public, None).await
    }

    /// Move the user's tags on external links from the legacy
    /// `/pub/pubky.app/tags/` to `/pub/pubky.music/tags/` (same id, same
    /// body), so Nexus indexes them. Tags on `pubky://` objects stay. Safe to
    /// run again: a tag already at the new path is only deleted at the old.
    /// Returns (moved, kept).
    pub async fn migrate_music_tags(
        &self,
        session: &PubkySession,
    ) -> Result<(usize, usize), HomeserverError> {
        let user = session.public_key();
        let legacy = self
            .list_dir(&user, crate::schemas::LEGACY_TAG_DIR, Visibility::Public, None)
            .await?;
        let (mut moved, mut kept) = (0, 0);
        // The old folder also holds other apps' tags: read 8 at a time (a node runs this once)
        for (path, bytes) in self.fetch_many(&user, legacy, Visibility::Public, None).await? {
            let Ok(tag) = serde_json::from_slice::<pubky_app_specs::PubkyAppTag>(&bytes) else {
                kept += 1;
                continue;
            };
            if tag.uri.starts_with("pubky://") {
                kept += 1;
                continue;
            }
            let id = path.rsplit('/').next().unwrap_or("").to_string();
            self.write_object(session, &crate::schemas::music_tag_path(&id), &tag)
                .await?;
            self.delete(session, &path).await?;
            moved += 1;
        }
        Ok((moved, kept))
    }

    /// Another user's public playlists.
    pub async fn read_playlists_of(
        &self,
        user_z32: &str,
    ) -> Result<Vec<(String, Playlist)>, HomeserverError> {
        Ok(self
            .read_music_posts(user_z32, Visibility::Public, None)
            .await?
            .into_iter()
            .filter_map(|(p, post)| post.as_playlist().cloned().map(|pl| (p, pl)))
            .collect())
    }

    /// The signed-in user's private playlists.
    pub async fn read_private_playlists(
        &self,
        session: &PubkySession,
    ) -> Result<Vec<(String, Playlist)>, HomeserverError> {
        let me = session.public_key().to_string();
        Ok(self
            .read_music_posts(&me, Visibility::Private, Some(session))
            .await?
            .into_iter()
            .filter_map(|(p, post)| post.as_playlist().cloned().map(|pl| (p, pl)))
            .collect())
    }

    pub async fn read_reviews_of(&self, user_z32: &str) -> Result<Vec<(String, Review)>, HomeserverError> {
        Ok(self
            .read_music_posts(user_z32, Visibility::Public, None)
            .await?
            .into_iter()
            .filter_map(|(p, post)| post.as_review().cloned().map(|r| (p, r)))
            .collect())
    }

    pub async fn read_activity_of(&self, user_z32: &str) -> Result<Vec<(String, Activity)>, HomeserverError> {
        Ok(self
            .read_music_posts(user_z32, Visibility::Public, None)
            .await?
            .into_iter()
            .filter_map(|(p, post)| post.as_activity().cloned().map(|a| (p, a)))
            .collect())
    }

    /// Write any object to its session-relative path (`Meta::path`).
    pub async fn write_object<T: Serialize + Sync + ?Sized>(
        &self,
        session: &PubkySession,
        path: &str,
        object: &T,
    ) -> Result<(), HomeserverError> {
        session.storage().put_json(path, object).await?;
        Ok(())
    }

    /// Write a music post where its `Meta` says (private or public folder).
    pub async fn write_post(
        &self,
        session: &PubkySession,
        post: &MusicPost,
        meta: &crate::schemas::Meta,
    ) -> Result<(), HomeserverError> {
        self.write_object(session, &meta.path, &post.post).await
    }

    pub async fn write_tag(
        &self,
        session: &PubkySession,
        tag: &pubky_app_specs::PubkyAppTag,
        meta: &crate::schemas::Meta,
    ) -> Result<(), HomeserverError> {
        self.write_object(session, &meta.path, tag).await
    }

    /// Delete an object the user wrote (remove a playlist, retract a review).
    pub async fn delete(&self, session: &PubkySession, path: &str) -> Result<(), HomeserverError> {
        session.storage().delete(path).await?;
        Ok(())
    }

    // ---- private files (audio the user keeps on their homeserver) ----

    /// Upload bytes to a private path (`/priv/…`). Audio never goes under
    /// `/pub/` (constraint 2); this refuses public paths outright.
    pub async fn upload_private(
        &self,
        session: &PubkySession,
        path: &str,
        bytes: Vec<u8>,
    ) -> Result<(), HomeserverError> {
        if !path.starts_with(PRIVATE_FILES_DIR) {
            return Err(HomeserverError::Transport(format!(
                "refusing to upload audio outside {PRIVATE_FILES_DIR}: {path}"
            )));
        }
        session.storage().put(path, bytes).await?;
        Ok(())
    }

    /// The same, sent no faster than `bytes_per_sec` (`None`: as fast as the line allows). The
    /// node's uplink also carries its pages and the music it streams to a browser away from home:
    /// a copy at full speed starved both (owner, 2026-10-22).
    pub async fn upload_private_paced(
        &self,
        session: &PubkySession,
        path: &str,
        bytes: Vec<u8>,
        bytes_per_sec: Option<u64>,
    ) -> Result<(), HomeserverError> {
        let Some(rate) = bytes_per_sec.filter(|r| *r > 0) else {
            return self.upload_private(session, path, bytes).await;
        };
        if !path.starts_with(PRIVATE_FILES_DIR) {
            return Err(HomeserverError::Transport(format!(
                "refusing to upload audio outside {PRIVATE_FILES_DIR}: {path}"
            )));
        }
        let len = bytes.len() as u64;
        let data = bytes::Bytes::from(bytes);
        let start = tokio::time::Instant::now();
        const CHUNK: usize = 64 * 1024;
        let stream = futures_util::stream::unfold(0usize, move |sent| {
            let data = data.clone();
            async move {
                if sent >= data.len() {
                    return None;
                }
                // Wait until this chunk is due at the chosen rate
                let due = start + std::time::Duration::from_secs_f64(sent as f64 / rate as f64);
                tokio::time::sleep_until(due).await;
                let end = (sent + CHUNK).min(data.len());
                Some((Ok::<_, std::io::Error>(data.slice(sent..end)), end))
            }
        });
        let body = reqwest::Body::wrap_stream(stream);
        tracing::debug!(path, len, rate, "paced upload");
        session.storage().put(path, body).await?;
        Ok(())
    }

    /// Every file under the private files folder, with size when the
    /// homeserver reports it.
    pub async fn list_private_files(
        &self,
        session: &PubkySession,
    ) -> Result<Vec<RemoteFile>, HomeserverError> {
        let storage = session.storage();
        let mut out = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let mut b = storage.list(PRIVATE_FILES_DIR)?.limit(LIST_LIMIT).shallow(true);
            if let Some(c) = &cursor {
                b = b.cursor(c);
            }
            let page = match b.send().await {
                Ok(p) => p,
                Err(e) if is_not_found(&e) => break,
                Err(e) => return Err(e.into()),
            };
            let n = page.len();
            cursor = page.last().map(|e| e.to_pubky_url());
            for e in page {
                let path = e.path.to_string();
                let size = storage
                    .stats(path.as_str())
                    .await
                    .ok()
                    .flatten()
                    .and_then(|s| s.content_length);
                out.push(RemoteFile { path, size });
            }
            if n < LIST_LIMIT as usize {
                break;
            }
        }
        Ok(out)
    }

    /// The size of one private file, as the homeserver reports it (`None`: not there).
    pub async fn stat_private(
        &self,
        session: &PubkySession,
        path: &str,
    ) -> Result<Option<u64>, HomeserverError> {
        Ok(session
            .storage()
            .stats(path)
            .await?
            .map(|s| s.content_length.unwrap_or(0)))
    }

    /// Remove one private file (only under the files folder).
    pub async fn delete_private(&self, session: &PubkySession, path: &str) -> Result<(), HomeserverError> {
        if !path.starts_with(PRIVATE_FILES_DIR) {
            return Err(HomeserverError::Transport(format!(
                "refusing to delete outside {PRIVATE_FILES_DIR}: {path}"
            )));
        }
        session.storage().delete(path).await?;
        Ok(())
    }

    /// Download a private file into `dest` as it arrives (no whole file in memory), no faster
    /// than `bytes_per_sec` when set; `progress` hears each chunk's size and returns `false` to
    /// stop (the partial file is removed). Returns the bytes written.
    pub async fn download_private_to(
        &self,
        session: &PubkySession,
        path: &str,
        dest: &std::path::Path,
        bytes_per_sec: Option<u64>,
        mut progress: impl FnMut(u64) -> bool,
    ) -> Result<u64, HomeserverError> {
        use tokio::io::AsyncWriteExt;
        let io = |e: std::io::Error| HomeserverError::Transport(format!("{}: {e}", dest.display()));
        let mut resp = session.storage().get(path).await?;
        let mut file = tokio::fs::File::create(dest).await.map_err(io)?;
        let start = tokio::time::Instant::now();
        let mut written: u64 = 0;
        while let Some(chunk) = resp
            .chunk()
            .await
            .map_err(|e| HomeserverError::Transport(e.to_string()))?
        {
            file.write_all(&chunk).await.map_err(io)?;
            written += chunk.len() as u64;
            if !progress(chunk.len() as u64) {
                drop(file);
                let _ = tokio::fs::remove_file(dest).await;
                return Err(HomeserverError::Transport("cancelled".into()));
            }
            if let Some(rate) = bytes_per_sec.filter(|r| *r > 0) {
                let due = start + std::time::Duration::from_secs_f64(written as f64 / rate as f64);
                tokio::time::sleep_until(due).await;
            }
        }
        file.flush().await.map_err(io)?;
        file.sync_all().await.map_err(io)?;
        Ok(written)
    }

    pub async fn download_private(
        &self,
        session: &PubkySession,
        path: &str,
    ) -> Result<Vec<u8>, HomeserverError> {
        let resp = session.storage().get(path).await?;
        resp.bytes()
            .await
            .map(|b| b.to_vec())
            .map_err(|e| HomeserverError::Transport(e.to_string()))
    }
}

/// Where the user's own audio files live on the homeserver: private, never `/pub/`.
pub const PRIVATE_FILES_DIR: &str = "/priv/pubky.music/files/";
/// Where a node publishes how to reach it, readable only by the owner (ADR 0008).
pub const NODE_RECORD_PATH: &str = "/priv/pubky.music/node.json";
/// The node's settings (contact, folders, streaming sessions, renderers, DSP)
/// so a reinstalled or replaced node restores them after one Ring sign-in.
pub const NODE_SETTINGS_PATH: &str = "/priv/pubky.music/settings.json";

/// How the user's own devices find their node (ADR 0008).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct NodeRecord {
    pub name: String,
    /// HTTP(S) origin of the `/v1` API as the user's devices can reach it.
    pub api_url: String,
    /// Bearer token; fine here because the folder is private to the owner.
    #[serde(default)]
    pub token: String,
    pub version: String,
    /// Microseconds.
    pub updated_at: i64,
    /// The node's iroh key when remote access is on (ADR 0013): dial it from anywhere.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub iroh: String,
}

impl NodeHomeserver {
    pub async fn publish_node_record(
        &self,
        session: &PubkySession,
        record: &NodeRecord,
    ) -> Result<(), HomeserverError> {
        self.write_object(session, NODE_RECORD_PATH, record).await
    }

    /// The owner's node settings backup, written by the node after every sync.
    pub async fn write_node_settings(
        &self,
        session: &PubkySession,
        settings: &serde_json::Value,
    ) -> Result<(), HomeserverError> {
        self.write_object(session, NODE_SETTINGS_PATH, settings).await
    }

    pub async fn read_node_settings(
        &self,
        session: &PubkySession,
    ) -> Result<Option<serde_json::Value>, HomeserverError> {
        let storage = session.storage();
        if !storage.exists(NODE_SETTINGS_PATH).await? {
            return Ok(None);
        }
        let v: serde_json::Value = storage.get_json(NODE_SETTINGS_PATH).await?;
        Ok(Some(v))
    }

    /// The owner's node record, if a node published one.
    pub async fn read_node_record(
        &self,
        session: &PubkySession,
    ) -> Result<Option<NodeRecord>, HomeserverError> {
        let storage = session.storage();
        if !storage.exists(NODE_RECORD_PATH).await? {
            return Ok(None);
        }
        let record: NodeRecord = storage.get_json(NODE_RECORD_PATH).await?;
        Ok(Some(record))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RemoteFile {
    /// Owner-relative path, e.g. `/priv/pubky.music/files/<hash>-<name>`.
    pub path: String,
    pub size: Option<u64>,
}

impl RemoteFile {
    pub fn name(&self) -> &str {
        self.path.rsplit('/').next().unwrap_or(&self.path)
    }
}

/// Sign-in through Pubky Ring (M9): start a grant auth flow, show the URL
/// (or a QR of it) to the user, await approval, keep the exported secret.
pub struct LoginFlow {
    flow: pubky::PubkyGrantAuthFlow,
    url: String,
}

/// What the node asks for: its own private folders, pubky.app objects, its
/// public tag namespace (`/pub/pubky.music/`, where Nexus indexes tags on
/// external links today), and the v1 social and music namespaces it moves to
/// (`social/v1`, `music/v1`; STEWARD-PLAN step 3), so one approval covers both.
pub const LOGIN_CAPABILITIES: &str = "/priv/pubky.music/:rw,/pub/pubky.music/:rw,/priv/pubky.app/:rw,/pub/pubky.app/:rw,/pub/social/v1/:rw,/priv/social/v1/:rw,/pub/music/v1/:rw,/priv/music/v1/:rw";

/// The capabilities in `wanted` (a list like [`LOGIN_CAPABILITIES`]) that `granted` does not
/// cover. A granted `scope:actions` covers a wanted one when its scope is the same folder or a
/// parent of it and it has every wanted action.
pub fn missing_capabilities<S: AsRef<str>>(granted: &[S], wanted: &str) -> Vec<String> {
    let split = |c: &str| -> Option<(String, String)> {
        let (scope, actions) = c.trim().rsplit_once(':')?;
        Some((scope.to_string(), actions.to_string()))
    };
    let have: Vec<(String, String)> = granted.iter().filter_map(|g| split(g.as_ref())).collect();
    wanted
        .split(',')
        .filter(|w| !w.trim().is_empty())
        .filter(|w| {
            let Some((scope, actions)) = split(w) else {
                return false;
            };
            !have.iter().any(|(hs, ha)| {
                let parent =
                    hs == "/" || (hs.ends_with('/') && scope.starts_with(hs.as_str())) || *hs == scope;
                parent && actions.chars().all(|a| ha.contains(a))
            })
        })
        .map(|w| w.trim().to_string())
        .collect()
}

/// The capabilities this session was granted, as `scope:actions` strings.
pub fn session_capabilities(session: &PubkySession) -> Vec<String> {
    session
        .info()
        .capabilities()
        .iter()
        .map(|c| c.to_string())
        .collect()
}

/// Whether a homeserver error says the session lacks access to a path (a session approved for
/// fewer folders than the node now uses).
pub fn is_capability_error(msg: &str) -> bool {
    let lower = msg.to_lowercase();
    lower.contains("does not have") && lower.contains("access")
        || lower.contains("403")
        || lower.contains("forbidden")
}

/// What a controller asks for: read access to the user's public pubky.app
/// data only, enough to prove who is signing in. The node never keeps this
/// session.
pub const CONTROLLER_CAPABILITIES: &str = "/pub/pubky.app/:r";

impl NodeHomeserver {
    pub fn start_login(&self, client_id: &str) -> Result<LoginFlow, HomeserverError> {
        self.start_login_with(client_id, LOGIN_CAPABILITIES)
    }

    /// A sign-in asking for `capabilities` (see [`CONTROLLER_CAPABILITIES`]).
    pub fn start_login_with(
        &self,
        client_id: &str,
        capabilities: &str,
    ) -> Result<LoginFlow, HomeserverError> {
        let caps = capabilities
            .parse::<pubky::Capabilities>()
            .map_err(|e| HomeserverError::Transport(e.to_string()))?;
        let client_id =
            pubky::ClientId::new(client_id).map_err(|e| HomeserverError::Transport(e.to_string()))?;
        let flow = self
            .pubky
            .start_grant_auth_flow(&caps, pubky::AuthFlowKind::signin(), client_id)?;
        let url = flow.authorization_url().to_string();
        Ok(LoginFlow { flow, url })
    }
}

impl LoginFlow {
    /// The `pubkyauth://` URL for Pubky Ring (scan or paste).
    pub fn url(&self) -> &str {
        &self.url
    }

    /// One poll: `Ok(None)` while the user has not approved yet.
    pub async fn poll(&self) -> Result<Option<PubkySession>, HomeserverError> {
        Ok(self.flow.try_poll_once().await?)
    }

    /// Block until approved.
    pub async fn await_approval(self) -> Result<PubkySession, HomeserverError> {
        Ok(self.flow.await_approval().await?)
    }
}

/// The restorable secret of a session (grant or legacy cookie), for
/// `pubky.session_secret_file`.
#[allow(deprecated)]
pub async fn export_session_secret(session: &PubkySession) -> Option<String> {
    if let Some(g) = session.as_grant() {
        if let Some(s) = g.export_local_secret().await {
            return Some(s);
        }
    }
    session.as_cookie().and_then(|c| c.export_secret())
}

/// The last segment of a path (a file name, which for tags and bookmarks is the object's id).
fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

#[cfg(test)]
mod capability_tests {
    use super::*;

    #[test]
    fn an_old_session_misses_the_new_folders() {
        let old = [
            "/priv/pubky.music/:rw",
            "/pub/pubky.app/:rw",
            "/priv/pubky.app/:rw",
        ];
        let missing = missing_capabilities(&old, LOGIN_CAPABILITIES);
        assert!(missing.contains(&"/pub/social/v1/:rw".to_string()));
        assert!(missing.contains(&"/pub/pubky.music/:rw".to_string()));
        assert!(!missing.contains(&"/priv/pubky.app/:rw".to_string()));
        let full: Vec<&str> = LOGIN_CAPABILITIES.split(',').collect();
        assert!(missing_capabilities(&full, LOGIN_CAPABILITIES).is_empty());
        assert!(missing_capabilities(&["/:rw"], LOGIN_CAPABILITIES).is_empty());
        assert_eq!(
            missing_capabilities(&["/pub/social/v1/:r"], "/pub/social/v1/:rw"),
            vec!["/pub/social/v1/:rw".to_string()],
            "read-only does not cover write"
        );
        assert!(is_capability_error(
            "Server responded with an error: 403 Forbidden - Session does not have read access to path"
        ));
        assert!(!is_capability_error("connection reset"));
    }
}
