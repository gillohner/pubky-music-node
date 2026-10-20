//! The music node's social objects (handoff §8.3), written as **v1
//! `PubkyAppPost`s** whose `content` is a JSON envelope (see [`post`]):
//!
//! | Object | Envelope `type` | Post path (private by default) |
//! | :-- | :-- | :-- |
//! | [`Playlist`] | `pubky.music/playlist` | `/priv/pubky.app/posts/<id>` |
//! | [`Review`] | `pubky.music/review` | `/priv/pubky.app/posts/<id>` |
//! | [`Activity`] | `pubky.music/activity` | `/priv/pubky.app/posts/<id>` |
//! | music tag | `PubkyAppTag` (reused as-is) | `/pub/pubky.app/tags/<id>` |
//! | favourite | `PubkyAppBookmark` (reused as-is) | `/priv/pubky.app/bookmarks/<hash id>` |
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
pub use bookmark::Favourite;
pub use builder::{Meta, MusicSpecsBuilder};
pub use playlist::{Playlist, PlaylistItem};
pub use post::{MusicContent, MusicPost, Visibility};
pub use review::Review;

use pm_model::ids::IdError;
use pm_model::MusicUri;
use serde::de::DeserializeOwned;

/// The envelope type prefix; handoff §11 leaves the final app name open and
/// this is the one place to change it.
pub const APP_NAMESPACE: &str = "pubky.music";
pub const PROTOCOL: &str = "pubky://";

/// Limits, in one place like `pubky_app_specs::VALIDATION_LIMITS`.
pub mod limits {
    pub const PLAYLIST_NAME_MIN: usize = 1;
    pub const PLAYLIST_NAME_MAX: usize = 100;
    pub const PLAYLIST_DESCRIPTION_MAX: usize = 500;
    pub const PLAYLIST_IMAGE_URL_MAX: usize = 200;
    pub const PLAYLIST_ITEMS_MAX: usize = 500;
    pub const REVIEW_TEXT_MIN: usize = 1;
    pub const REVIEW_TEXT_MAX: usize = 2000;
    pub const REVIEW_RATING_MIN: u8 = 1;
    pub const REVIEW_RATING_MAX: u8 = 5;
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
