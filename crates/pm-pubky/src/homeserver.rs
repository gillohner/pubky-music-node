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

/// Default page size for listings; the homeserver may cap it.
pub const LIST_LIMIT: u16 = 100;

#[derive(Clone)]
pub struct NodeHomeserver {
    pubky: Pubky,
}

impl NodeHomeserver {
    pub fn new(network: Network) -> Result<Self, HomeserverError> {
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
        let dir = dir.to_string();
        let mut all = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let page = match (visibility, session) {
                (Visibility::Public, _) => {
                    let public = self.pubky.public_storage();
                    let mut b = public.list((user, dir.as_str()))?.limit(LIST_LIMIT).shallow(true);
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
                    let mut b = storage.list(dir.as_str())?.limit(LIST_LIMIT).shallow(true);
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
        for path in self.list_posts(&user, visibility, session).await? {
            let id = path.rsplit('/').next().unwrap_or("").to_string();
            let bytes = self.fetch(&user, &path, visibility, session).await?;
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
        for path in self.list_dir(&user, &dir, visibility, session).await? {
            let id = path.rsplit('/').next().unwrap_or("").to_string();
            let bytes = self.fetch(&user, &path, visibility, session).await?;
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
    pub async fn read_music_tags(
        &self,
        user_z32: &str,
    ) -> Result<Vec<(String, pubky_app_specs::PubkyAppTag)>, HomeserverError> {
        let user = Self::parse_key(user_z32)?;
        let dir = format!("{}pubky.app/tags/", Visibility::Public.prefix());
        let mut out = Vec::new();
        for path in self.list_dir(&user, &dir, Visibility::Public, None).await? {
            let bytes = self.fetch(&user, &path, Visibility::Public, None).await?;
            match serde_json::from_slice::<pubky_app_specs::PubkyAppTag>(&bytes) {
                Ok(t) if pm_model::MusicUri::parse(&t.uri).is_ok() => out.push((path, t)),
                Ok(_) => {}
                Err(e) => tracing::warn!(%path, error = %e, "skipping invalid tag"),
            }
        }
        Ok(out)
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

/// What the node asks for: its own private folders and pubky.app posts/tags.
pub const LOGIN_CAPABILITIES: &str = "/priv/pubky.music/:rw,/priv/pubky.app/:rw,/pub/pubky.app/:rw";

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
