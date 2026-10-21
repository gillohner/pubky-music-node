//! Music objects as v1 `PubkyAppPost`s (pubky-app-specs 0.8) whose `content`
//! is a JSON envelope: `{"type":"pubky.music/<kind>","v":2, ...fields}` (the node's working copy; the homeserver gets v1 objects, ADR 0014).
//!
//! Why a post and not an own object type: the v1 specs are what Nexus and
//! every Pubky client already index and render, so a playlist written this
//! way is a post other apps can at least show, reply to and tag, while this
//! node reads the envelope. The playlist-specific social primitives come
//! later; until then everything is written to the **private** posts folder
//! (`/priv/pubky.app/posts/<id>`), readable only with the owner's session,
//! and moving an object public is a path change, not a format change.
//!
//! Post shape: `kind: long` (the envelope is JSON, not prose), no parent or
//! attachments; reviews and activity carry `embed: {kind: link, uri}` with
//! the canonical music URL so generic clients show what the post is about.

use super::{Activity, Playlist, Review, SchemaError, Validatable};
use crate::ids;
use pubky_app_specs::traits::{HasIdPath, Validatable as _};
use pubky_app_specs::{PubkyAppPost, PubkyAppPostEmbed, PubkyAppPostKind};
use serde::{Deserialize, Serialize};

/// Current envelope version. 2 (2026-10-21): review ratings are 1–10 (half stars); a version 1
/// review's 1–5 stars are doubled when read.
pub const ENVELOPE_VERSION: u32 = 2;
pub const TYPE_PLAYLIST: &str = "pubky.music/playlist";
pub const TYPE_REVIEW: &str = "pubky.music/review";
pub const TYPE_ACTIVITY: &str = "pubky.music/activity";

pub const PUBLIC_PATH: &str = "/pub/";
/// Anything outside `/pub/` is private on a homeserver; `/priv/` mirrors the
/// public layout so an object can move with a prefix change.
pub const PRIVATE_PATH: &str = "/priv/";
pub const APP_SPECS_DIR: &str = "pubky.app/posts/";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Visibility {
    /// `/priv/pubky.app/posts/<id>` (the default until the social primitives exist).
    #[default]
    Private,
    /// `/pub/pubky.app/posts/<id>`: a regular pubky.app post.
    Public,
}

impl Visibility {
    pub fn prefix(self) -> &'static str {
        match self {
            Visibility::Private => PRIVATE_PATH,
            Visibility::Public => PUBLIC_PATH,
        }
    }

    /// `/priv/pubky.app/posts/` or `/pub/pubky.app/posts/`.
    pub fn posts_dir(self) -> String {
        format!("{}{APP_SPECS_DIR}", self.prefix())
    }

    pub fn post_path(self, id: &str) -> String {
        format!("{}{id}", self.posts_dir())
    }
}

/// The typed payload inside `PubkyAppPost::content`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum MusicContent {
    #[serde(rename = "pubky.music/playlist")]
    Playlist(Playlist),
    #[serde(rename = "pubky.music/review")]
    Review(Review),
    #[serde(rename = "pubky.music/activity")]
    Activity(Activity),
}

/// The envelope as written: `type`, `v`, then the object's fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Envelope {
    #[serde(flatten)]
    content: MusicContent,
    #[serde(default = "default_version")]
    v: u32,
}

/// Every envelope ever written carries `v`; one without it predates the field.
fn default_version() -> u32 {
    1
}

impl MusicContent {
    pub fn type_name(&self) -> &'static str {
        match self {
            MusicContent::Playlist(_) => TYPE_PLAYLIST,
            MusicContent::Review(_) => TYPE_REVIEW,
            MusicContent::Activity(_) => TYPE_ACTIVITY,
        }
    }

    /// Microseconds the Timestamp ID derives from.
    pub fn created_micros(&self) -> i64 {
        match self {
            MusicContent::Playlist(p) => p.created_at,
            MusicContent::Review(r) => r.created_at,
            MusicContent::Activity(a) => a.played_at,
        }
    }

    /// The single music URL the object is about, for the post embed.
    pub fn subject_url(&self) -> Option<String> {
        match self {
            MusicContent::Playlist(_) => None,
            MusicContent::Review(r) => Some(r.target_uri.clone()),
            MusicContent::Activity(a) => Some(a.uri.clone()),
        }
    }

    fn sanitize(self) -> Self {
        match self {
            MusicContent::Playlist(p) => MusicContent::Playlist(p.sanitize()),
            MusicContent::Review(r) => MusicContent::Review(r.sanitize()),
            MusicContent::Activity(a) => MusicContent::Activity(a.sanitize()),
        }
    }

    fn validate(&self, id: Option<&str>) -> Result<(), SchemaError> {
        match self {
            MusicContent::Playlist(p) => p.validate(id)?,
            MusicContent::Review(r) => r.validate(id)?,
            MusicContent::Activity(a) => a.validate(id)?,
        }
        if let Some(id) = id {
            let expected = ids::timestamp_id(self.created_micros());
            if id != expected {
                return Err(SchemaError::Id {
                    expected,
                    found: id.to_string(),
                });
            }
        }
        Ok(())
    }
}

/// A `PubkyAppPost` carrying a [`MusicContent`] envelope.
#[derive(Debug, Clone)]
pub struct MusicPost {
    pub post: PubkyAppPost,
    pub content: MusicContent,
}

impl MusicPost {
    /// Build the post for `content` (sanitized). Validate before writing.
    pub fn new(content: MusicContent) -> Self {
        let content = content.sanitize();
        let embed = content.subject_url().map(|uri| PubkyAppPostEmbed {
            kind: PubkyAppPostKind::Link,
            uri,
        });
        let envelope = Envelope {
            content: content.clone(),
            v: ENVELOPE_VERSION,
        };
        let post = PubkyAppPost {
            content: serde_json::to_string(&envelope).expect("envelope serializes"),
            kind: PubkyAppPostKind::Long,
            parent: None,
            embed,
            attachments: None,
            lock: None,
        };
        Self { post, content }
    }

    /// Timestamp ID from the object's creation time; reuse it on edits.
    pub fn create_id(&self) -> String {
        ids::timestamp_id(self.content.created_micros())
    }

    /// Session-relative path for `id` under the given visibility.
    pub fn path(id: &str, visibility: Visibility) -> String {
        visibility.post_path(id)
    }

    /// The public (`/pub/`) path as `pubky_app_specs` computes it, for
    /// cross-checking against the spec crate.
    pub fn spec_public_path(id: &str) -> String {
        PubkyAppPost::create_path(id)
    }

    /// Does a post body carry a music envelope? Cheap check before parsing.
    pub fn looks_like_music(post_content: &str) -> bool {
        post_content.trim_start().starts_with('{') && post_content.contains("\"pubky.music/")
    }

    /// Parse a stored post. `Ok(None)` for an ordinary post (not ours);
    /// `Err` for a music envelope that is malformed or invalid.
    pub fn from_post(post: PubkyAppPost, id: Option<&str>) -> Result<Option<Self>, SchemaError> {
        if !Self::looks_like_music(&post.content) {
            return Ok(None);
        }
        let envelope: Envelope =
            serde_json::from_str(&post.content).map_err(|e| SchemaError::Json(e.to_string()))?;
        if envelope.v > ENVELOPE_VERSION {
            return Err(SchemaError::validation(format!(
                "envelope version {} is newer than {ENVELOPE_VERSION}",
                envelope.v
            )));
        }
        let mut content = envelope.content.sanitize();
        if envelope.v < 2 {
            if let MusicContent::Review(r) = &mut content {
                r.rating = r.rating.map(|stars| stars.saturating_mul(2));
            }
        }
        content.validate(id)?;
        Ok(Some(Self { post, content }))
    }

    /// Parse a homeserver body (the ingestion contract).
    pub fn try_from_bytes(blob: &[u8], id: &str) -> Result<Option<Self>, SchemaError> {
        let post: PubkyAppPost =
            serde_json::from_slice(blob).map_err(|e| SchemaError::Json(e.to_string()))?;
        Self::from_post(post, Some(id))
    }

    /// Validate both layers: the v1 post rules (kind, lengths, embed URL)
    /// and the music envelope.
    pub fn validate(&self, id: Option<&str>) -> Result<(), SchemaError> {
        self.post
            .validate(id)
            .map_err(|e| SchemaError::validation(format!("post: {e}")))?;
        self.content.validate(id)
    }

    pub fn as_playlist(&self) -> Option<&Playlist> {
        match &self.content {
            MusicContent::Playlist(p) => Some(p),
            _ => None,
        }
    }

    pub fn as_review(&self) -> Option<&Review> {
        match &self.content {
            MusicContent::Review(r) => Some(r),
            _ => None,
        }
    }

    pub fn as_activity(&self) -> Option<&Activity> {
        match &self.content {
            MusicContent::Activity(a) => Some(a),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schemas::{ActivitySource, PlaylistItem};
    use pm_model::{Mbid, MusicUri};

    fn rec() -> MusicUri {
        MusicUri::recording(Mbid::parse("b1a9c0e9-d987-4042-ae91-78d6a3267d69").unwrap())
    }

    #[test]
    fn playlist_round_trips_through_a_v1_post() {
        let p = Playlist::new(
            "Late night",
            Some("Quiet ones".into()),
            None,
            vec![PlaylistItem::new(&rec())],
        );
        let post = MusicPost::new(MusicContent::Playlist(p.clone()));
        let id = post.create_id();
        post.validate(Some(&id)).unwrap();
        assert_eq!(post.post.kind, PubkyAppPostKind::Long);
        assert!(post.post.embed.is_none());
        let json: serde_json::Value = serde_json::from_str(&post.post.content).unwrap();
        assert_eq!(json["type"], TYPE_PLAYLIST);
        assert_eq!(json["v"], ENVELOPE_VERSION);
        assert_eq!(json["name"], "Late night");
        assert_eq!(json["items"][0]["uri"], rec().to_url());

        let bytes = serde_json::to_vec(&post.post).unwrap();
        let back = MusicPost::try_from_bytes(&bytes, &id).unwrap().unwrap();
        assert_eq!(back.as_playlist(), Some(&p));
        assert_eq!(
            MusicPost::path(&id, Visibility::Private),
            format!("/priv/pubky.app/posts/{id}")
        );
        assert_eq!(
            MusicPost::path(&id, Visibility::Public),
            MusicPost::spec_public_path(&id)
        );
    }

    #[test]
    fn review_and_activity_embed_their_subject() {
        let r = Review::new(&rec(), Some(5), "Lovely");
        let post = MusicPost::new(MusicContent::Review(r));
        assert_eq!(post.post.embed.as_ref().unwrap().uri, rec().to_url());
        assert_eq!(post.post.embed.as_ref().unwrap().kind, PubkyAppPostKind::Link);
        post.validate(Some(&post.create_id())).unwrap();
        let a = Activity::new(&rec(), ActivitySource::Local, Some(1000));
        let post = MusicPost::new(MusicContent::Activity(a));
        post.validate(Some(&post.create_id())).unwrap();
        let v: serde_json::Value = serde_json::from_str(&post.post.content).unwrap();
        assert_eq!(v["type"], TYPE_ACTIVITY);
        assert_eq!(v["source_kind"]["kind"], "local");
    }

    #[test]
    fn version_1_reviews_read_with_their_stars_doubled() {
        let r = Review::new(&rec(), Some(4), "Good");
        let id = r.create_id();
        let mut post = MusicPost::new(MusicContent::Review(r.clone()));
        // As a node wrote it before ratings were 1–10
        let mut v: serde_json::Value = serde_json::from_str(&post.post.content).unwrap();
        assert_eq!(v["v"], ENVELOPE_VERSION);
        v["v"] = 1.into();
        post.post.content = v.to_string();
        let back = MusicPost::from_post(post.post.clone(), Some(&id))
            .unwrap()
            .unwrap();
        assert_eq!(back.as_review().unwrap().rating, Some(8));
        // Version 2 keeps the value; 10 is the top
        let ten = MusicPost::new(MusicContent::Review(Review {
            rating: Some(10),
            ..r.clone()
        }));
        let back = MusicPost::from_post(ten.post, Some(&id)).unwrap().unwrap();
        assert_eq!(back.as_review().unwrap().rating, Some(10));
        let eleven = MusicPost::new(MusicContent::Review(Review {
            rating: Some(11),
            ..r
        }));
        assert!(eleven.validate(Some(&id)).is_err());
    }

    #[test]
    fn ordinary_posts_are_not_ours_and_bad_envelopes_fail() {
        let plain = PubkyAppPost {
            content: "hello world".into(),
            kind: PubkyAppPostKind::Short,
            parent: None,
            embed: None,
            attachments: None,
            lock: None,
        };
        assert!(MusicPost::from_post(plain, None).unwrap().is_none());
        let bad = PubkyAppPost {
            content:
                r#"{"type":"pubky.music/playlist","v":1,"name":"","items":[],"created_at":1,"updated_at":1}"#
                    .into(),
            kind: PubkyAppPostKind::Long,
            parent: None,
            embed: None,
            attachments: None,
            lock: None,
        };
        assert!(MusicPost::from_post(bad, None).is_err());
        let future = PubkyAppPost {
            content:
                r#"{"type":"pubky.music/playlist","v":9,"name":"x","items":[],"created_at":1,"updated_at":1}"#
                    .into(),
            kind: PubkyAppPostKind::Long,
            parent: None,
            embed: None,
            attachments: None,
            lock: None,
        };
        assert!(MusicPost::from_post(future, None).is_err());
    }

    #[test]
    fn id_mismatch_is_rejected() {
        let p = Playlist::new("Mix", None, None, vec![]);
        let post = MusicPost::new(MusicContent::Playlist(p));
        assert!(post.validate(Some("0000000000000")).is_err());
    }
}
