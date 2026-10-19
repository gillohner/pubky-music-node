//! Read and write the music objects on homeservers through the `pubky` SDK
//! (0.16.2). The node writes the user's own objects with their session and
//! reads anyone's public objects without one.
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
use crate::schemas::{Activity, Playlist, Review, Validatable, APP_NAMESPACE};
use pubky::{Pubky, PubkySession, PublicKey};
use serde::Serialize;

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

    /// List every object path under `/pub/pubky.music/<segment>` for a user,
    /// following the cursor until the end.
    async fn list_segment(
        &self,
        user: &PublicKey,
        segment: &str,
    ) -> Result<Vec<pubky::PubkyResource>, HomeserverError> {
        let public = self.pubky.public_storage();
        let dir = format!("/pub/{APP_NAMESPACE}/{segment}");
        let mut all = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let mut builder = public.list((user, dir.as_str()))?.limit(LIST_LIMIT).shallow(true);
            if let Some(c) = &cursor {
                builder = builder.cursor(c);
            }
            let page = builder.send().await?;
            let n = page.len();
            cursor = page.last().map(|e| e.to_pubky_url());
            all.extend(page);
            if n < LIST_LIMIT as usize {
                break;
            }
        }
        Ok(all)
    }

    async fn read_all<T: Validatable>(
        &self,
        user_z32: &str,
        segment: &str,
    ) -> Result<Vec<(String, T)>, HomeserverError> {
        let user = Self::parse_key(user_z32)?;
        let public = self.pubky.public_storage();
        let mut out = Vec::new();
        for entry in self.list_segment(&user, segment).await? {
            let path = entry.path.as_str().to_string();
            let id = path.rsplit('/').next().unwrap_or_default().to_string();
            let bytes = public
                .get(&entry)
                .await?
                .bytes()
                .await
                .map_err(|e| HomeserverError::Transport(e.to_string()))?;
            match T::try_from(&bytes, &id) {
                Ok(obj) => out.push((path, obj)),
                Err(source) => {
                    tracing::warn!(%path, %source, "skipping invalid object");
                }
            }
        }
        Ok(out)
    }

    /// Another user's playlists, validated; invalid objects are skipped with
    /// a warning (an indexer never trusts a homeserver body).
    pub async fn read_playlists_of(
        &self,
        user_z32: &str,
    ) -> Result<Vec<(String, Playlist)>, HomeserverError> {
        self.read_all::<Playlist>(user_z32, Playlist::PATH_SEGMENT).await
    }

    pub async fn read_reviews_of(&self, user_z32: &str) -> Result<Vec<(String, Review)>, HomeserverError> {
        self.read_all::<Review>(user_z32, Review::PATH_SEGMENT).await
    }

    pub async fn read_activity_of(&self, user_z32: &str) -> Result<Vec<(String, Activity)>, HomeserverError> {
        self.read_all::<Activity>(user_z32, Activity::PATH_SEGMENT).await
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

    pub async fn write_playlist(
        &self,
        session: &PubkySession,
        playlist: &Playlist,
        meta: &crate::schemas::Meta,
    ) -> Result<(), HomeserverError> {
        self.write_object(session, &meta.path, playlist).await
    }

    pub async fn write_review(
        &self,
        session: &PubkySession,
        review: &Review,
        meta: &crate::schemas::Meta,
    ) -> Result<(), HomeserverError> {
        self.write_object(session, &meta.path, review).await
    }

    pub async fn write_activity(
        &self,
        session: &PubkySession,
        activity: &Activity,
        meta: &crate::schemas::Meta,
    ) -> Result<(), HomeserverError> {
        self.write_object(session, &meta.path, activity).await
    }

    #[cfg(feature = "app-specs")]
    pub async fn write_tag(
        &self,
        session: &PubkySession,
        tag: &pubky_app_specs::PubkyAppTag,
        meta: &crate::schemas::Meta,
    ) -> Result<(), HomeserverError> {
        self.write_object(session, &meta.path, tag).await
    }

    /// Delete an object the user wrote (unfollow a playlist, retract a review).
    pub async fn delete(&self, session: &PubkySession, path: &str) -> Result<(), HomeserverError> {
        session.storage().delete(path).await?;
        Ok(())
    }
}
