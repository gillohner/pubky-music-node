//! The node's working copy of its music objects, and the **legacy** pubky.app form they had
//! before ADR 0014. What the homeserver gets is pubky-social-specs v1 ([`crate::v1`] converts at
//! the boundary). The library keeps each object as a pubky.app `PubkyAppPost` whose `content` is a
//! JSON envelope (see [`post`]); the paths below are only **read**, once per node, for the move to
//! v1 (and deleted with an object):
//!
//! | Object | Envelope `type` | Legacy path (read for the move) | Written today |
//! | :-- | :-- | :-- | :-- |
//! | [`Playlist`] (a collection) | `pubky.music/playlist` | `/priv/pubky.app/posts/<id>` | social `collection` |
//! | [`Review`] | `pubky.music/review` | `/priv/pubky.app/posts/<id>` | music `review` or social note |
//! | [`Activity`] (a play) | `pubky.music/activity` | `/priv/pubky.app/posts/<id>` | music `listen` |
//! | music tag | — | `/pub/pubky.app/tags/<id>`, then [`TAG_DIR`] | social v1 tag ([`crate::v1::music_tag`]) |
//! | [`Bookmark`] | `PubkyAppBookmark` | `/priv/pubky.app/bookmarks/<hash id>` | social v1 bookmark |
//!
//! Ids are Timestamp IDs like every post. Objects follow the pubky-app-specs
//! conventions: sanitize, then validate; lengths count Unicode scalar
//! values; optional fields are omitted when `None`; readers accept `null`.
//! Every URI inside is a canonical `pm_model::MusicUri` rendered with
//! `to_url()`; `music:` URIs are rejected at validation.

mod activity;
mod bookmark;
mod builder;
mod playlist;
pub mod post;
mod review;

pub use activity::{Activity, ActivitySource};
pub use bookmark::Bookmark;
pub use builder::{Meta, MusicSpecsBuilder};
pub use playlist::{item_uri, post_uri, web_link, ItemKind, ItemSource, Playlist, PlaylistItem, LAYOUTS};
pub use post::{MusicContent, MusicPost, Visibility};
pub use review::Review;

use pm_model::ids::IdError;
use pm_model::MusicUri;
use serde::de::DeserializeOwned;

/// The envelope type prefix; handoff §11 leaves the final app name open and
/// this is the one place to change it.
pub const APP_NAMESPACE: &str = pm_appdata::paths::APP_NAMESPACE;
pub const PROTOCOL: &str = "pubky://";

/// Limits, in one place like `pubky_app_specs::VALIDATION_LIMITS`.
pub mod limits {
    pub const PLAYLIST_NAME_MIN: usize = 1;
    pub const PLAYLIST_NAME_MAX: usize = 100;
    pub const PLAYLIST_DESCRIPTION_MAX: usize = 500;
    pub const PLAYLIST_IMAGE_URL_MAX: usize = 200;
    /// The node's own cap (a v1 collection is not capped for playlists, see `v1::validate_social`).
    pub const PLAYLIST_ITEMS_MAX: usize = 10_000;
    pub const REVIEW_TEXT_MIN: usize = 1;
    pub const REVIEW_TEXT_MAX: usize = 2000;
    pub const REVIEW_RATING_MIN: u8 = 1;
    /// Ten half stars (envelope version 2).
    pub const REVIEW_RATING_MAX: u8 = 10;
    pub const URI_MAX: usize = 2048;
    pub const SERVICE_NAME_MAX: usize = 50;
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SchemaError {
    #[error("Validation Error: {0}")]
    Validation(String),
    #[error("Validation Error: invalid music URI {uri}: {source}")]
    Uri { uri: String, source: IdError },
    #[error("Invalid ID: expected {expected}, found {found}")]
    Id { expected: String, found: String },
    #[error("JSON error: {0}")]
    Json(String),
}

impl SchemaError {
    pub fn validation(msg: impl Into<String>) -> Self {
        SchemaError::Validation(msg.into())
    }
}

/// The ingestion contract, as `pubky_app_specs::traits::Validatable`:
/// parse a body, sanitize it, validate it against its path id.
pub trait Validatable: Sized + DeserializeOwned {
    fn try_from(blob: &[u8], id: &str) -> Result<Self, SchemaError> {
        let instance: Self = serde_json::from_slice(blob).map_err(|e| SchemaError::Json(e.to_string()))?;
        let instance = instance.sanitize();
        instance.validate(Some(id))?;
        Ok(instance)
    }

    fn sanitize(self) -> Self {
        self
    }

    fn validate(&self, id: Option<&str>) -> Result<(), SchemaError>;
}

/// Parse a URI as a canonical music URI and return its normalized form.
pub(crate) fn canonical_uri(uri: &str) -> Result<String, SchemaError> {
    if uri.chars().count() > limits::URI_MAX {
        return Err(SchemaError::validation(format!(
            "URI exceeds {} characters",
            limits::URI_MAX
        )));
    }
    MusicUri::parse(uri)
        .map(|u| u.to_url())
        .map_err(|source| SchemaError::Uri {
            uri: uri.to_string(),
            source,
        })
}

/// Sanitize a URI: trim and normalize if it parses, keep trimmed otherwise so
/// validation reports it.
pub(crate) fn sanitize_uri(uri: &str) -> String {
    canonical_uri(uri).unwrap_or_else(|_| uri.trim().to_string())
}

pub(crate) fn chars(s: &str) -> usize {
    s.chars().count()
}

pub(crate) fn validate_image_url(url: &str, max: usize) -> Result<(), SchemaError> {
    if url.is_empty() {
        return Err(SchemaError::validation("image URL cannot be empty"));
    }
    if chars(url) > max {
        return Err(SchemaError::validation(format!(
            "image URL exceeds {max} characters"
        )));
    }
    let parsed =
        url::Url::parse(url).map_err(|_| SchemaError::validation(format!("invalid image URL: {url}")))?;
    match parsed.scheme() {
        "http" | "https" | "pubky" => Ok(()),
        s => Err(SchemaError::validation(format!(
            "image URL scheme must be http, https or pubky, got {s}"
        ))),
    }
}

/// The app namespace the node wrote public tags under until 2026-10-26.
pub const TAG_NAMESPACE: &str = pm_appdata::paths::APP_NAMESPACE;
/// `/pub/pubky.music/tags/`: where the node wrote tags (`PubkyAppTag`s) from 2026-10-21 to
/// 2026-10-26, while the Nexus behind Pinakes indexed tags on external links only in an app's own
/// namespace. The sync moves them to `/pub/social/v1/tags/` once ([`crate::v1::music_tag`]) and
/// reads this folder until then.
pub const TAG_DIR: &str = pm_appdata::paths::TAG_DIR;
/// Where the node wrote tags before 2026-10-21 (Nexus never indexed these;
/// the sync moves them to social v1 tags).
pub const LEGACY_TAG_DIR: &str = "/pub/pubky.app/tags/";

/// The path of a tag with this id in [`TAG_DIR`] (the `PubkyAppTag` id rule).
pub fn app_tag_path(id: &str) -> String {
    format!("{TAG_DIR}{id}")
}
