//! Music on Pubky, on top of the v1 social objects.
//!
//! Music uses the shared social objects for everything that is not musical: a tag on a song is a
//! social `tag` on its canonical URL, a bookmark is a social `bookmark`, a comment is a social
//! `post` whose `parent` is the music URL, and a playlist is a social `collection` post. This
//! crate adds only what is musical:
//!
//! - post kinds of its own on the shared [`PostEnvelope`] under the `music` namespace
//!   (`/{pub,priv}/music/v1/posts/{id}/{editId}.json`), with their content rules:
//!   [`MusicPostKind::Review`], [`MusicPostKind::Listen`] (one play), [`MusicPostKind::Listening`]
//!   (now playing, until it expires) and [`MusicPostKind::Session`] (a listening session others
//!   join). The path decides who sees a post: `/priv/` or `/pub/`;
//! - the `ext.music` hints a playlist carries on its items and on the collection ([`ext`]), and the
//!   readers that treat them as hostile input. `music`, the spec's namespace, is also its key in
//!   `ext`.
//!
//! Times (`*_at`) are Unix microseconds, like the social specs; durations (`*_ms`) are
//! milliseconds. Readers take an older millisecond time as one ([`micros_of`]).
//!
//! Canonical music URLs (what `embed` and collection items point at), in order of preference:
//! the MusicBrainz URL first, then the ISRC URL, then the service URL. This crate does not pick
//! them; it only requires that a review or listen names one.
//!
//! Service names (`source.service`, `source_order`) are an open vocabulary of 1–32 lowercase
//! letters, digits or `-`: a streaming service's own name, hyphenated (`tidal`, `apple-music`,
//! `youtube-music`; [`SERVICE_NAMES`] lists the shared spellings), `library` for the curator's own
//! files wherever they play them from, or `roon`. [`service_name`] folds a spelling like
//! `Apple_Music` into one. Readers that don't know a name treat the source as unknown and fall back
//! to the item's canonical URL. A source is where it played, never where a record came from.
//!
//! The crate is generic: any app can read and write these objects. It holds no app's own data (no
//! settings, credentials, vault or library); each app keeps that under its own namespace. It depends on nothing but the social specs and serde, so it can live in its
//! own repository like the Mapky and Eventky specs.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod content;
pub mod ext;
mod kind;

pub use content::{
    generic_listening_name, generic_session_name, is_service_name, micros_of, service_name,
    MusicListenContent, MusicListeningContent, MusicReviewContent, MusicSessionContent, MusicSource,
    LISTEN_DURATION_MAX_MS, LIVE_NAME_MAX_LENGTH, LIVE_WINDOW_MAX_MICROS, MILLIS_BEFORE, REVIEW_RATING_MAX,
    REVIEW_RATING_MIN, REVIEW_TEXT_MAX_LENGTH, SERVICE_MAX_LENGTH, SERVICE_NAMES, TIME_MIN_MICROS,
};
pub use kind::MusicPostKind;

pub use pubky_social_specs::{MintedVersion, PostEnvelope, PubkyId, Root, ValidationCtx};

/// The namespace music kinds live under: `/{root}/music/v1/…`.
pub const MUSIC_NAMESPACE: &str = "music";

/// A music post: the shared v1 envelope with the music kind set.
pub type MusicPost = PostEnvelope<MusicPostKind>;

/// A review of the music at `embed`: a rating from 1 to 10 (half stars in a five-star UI) and
/// optional text. Trims the text; a blank one becomes absent. Infallible; validate before writing.
pub fn review(rating: u8, text: Option<String>, embed: String) -> MusicPost {
    let content = MusicReviewContent {
        rating,
        text: text
            .map(|t| pubky_social_specs::frozen_trim(&t).to_string())
            .filter(|t| !t.is_empty()),
        extra: Default::default(),
    };
    let content = serde_json::to_string(&content).expect("a review always serializes");
    PostEnvelope::new(content, MusicPostKind::Review, None, Some(embed), Vec::new())
}

/// A listen of the music at `embed`, played at `played_at` (Unix microseconds). A listen
/// validates under either root: `/priv/` for a private history, `/pub/` for a public one.
/// Infallible; validate before writing.
pub fn listen(
    played_at: i64,
    duration_ms: Option<i64>,
    source: Option<MusicSource>,
    embed: String,
) -> MusicPost {
    let content = MusicListenContent {
        played_at,
        duration_ms,
        source,
        extra: Default::default(),
    };
    let content = serde_json::to_string(&content).expect("a listen always serializes");
    PostEnvelope::new(content, MusicPostKind::Listen, None, Some(embed), Vec::new())
}

/// Now playing the music at `embed`, from `started_at` until `expires_at` (Unix microseconds),
/// called `name`. Trims the name; a blank one becomes absent. With no name from the person, pass
/// [`generic_listening_name`]. Infallible; validate before writing.
pub fn listening(
    started_at: i64,
    expires_at: i64,
    name: Option<String>,
    duration_ms: Option<i64>,
    source: Option<MusicSource>,
    embed: String,
) -> MusicPost {
    let content = MusicListeningContent {
        started_at,
        expires_at,
        name: content::trimmed_name(name),
        duration_ms,
        source,
        extra: Default::default(),
    };
    let content = serde_json::to_string(&content).expect("a listening always serializes");
    PostEnvelope::new(content, MusicPostKind::Listening, None, Some(embed), Vec::new())
}

/// A listening session hosted by `host` (`pubky://<key>`), playing the music at `embed` now,
/// called `name`. Trims the name; a blank one becomes absent. With no name from the person, pass
/// [`generic_session_name`]. Infallible; validate before writing.
///
/// To say where others join, set [`MusicSessionContent::join`] with [`session_joined_at`].
pub fn session(
    started_at: i64,
    expires_at: Option<i64>,
    host: String,
    name: Option<String>,
    source: Option<MusicSource>,
    embed: String,
) -> MusicPost {
    let content = MusicSessionContent {
        started_at,
        expires_at,
        host,
        name: content::trimmed_name(name),
        source,
        join: None,
        extra: Default::default(),
    };
    let content = serde_json::to_string(&content).expect("a session always serializes");
    PostEnvelope::new(content, MusicPostKind::Session, None, Some(embed), Vec::new())
}

/// The session `post` with `join` (an `http(s)` address others join it at) set, its other members
/// kept. Infallible; validate before writing.
pub fn session_joined_at(post: &MusicPost, join: Option<String>) -> MusicPost {
    let mut value: serde_json::Value = serde_json::from_str(&post.content).unwrap_or_default();
    if let Some(o) = value.as_object_mut() {
        match join {
            Some(j) => {
                o.insert("join".into(), serde_json::Value::String(j));
            }
            None => {
                o.remove("join");
            }
        }
    }
    let mut out = post.clone();
    out.content = value.to_string();
    out
}

/// The review inside a `kind = review` post, checked against the review rules.
pub fn read_review(post: &MusicPost) -> Result<MusicReviewContent, String> {
    if post.kind != MusicPostKind::Review {
        return Err("Validation Error: not a review".into());
    }
    content::parse_review(&post.content)
}

/// The listen inside a `kind = listen` post, checked against the listen rules, with `played_at`
/// in microseconds (an older millisecond value converted).
pub fn read_listen(post: &MusicPost) -> Result<MusicListenContent, String> {
    if post.kind != MusicPostKind::Listen {
        return Err("Validation Error: not a listen".into());
    }
    content::parse_listen(&post.content)
}

/// The content of a `kind = listening` post, checked.
pub fn read_listening(post: &MusicPost) -> Result<MusicListeningContent, String> {
    if post.kind != MusicPostKind::Listening {
        return Err("Validation Error: not a listening".into());
    }
    content::parse_listening(&post.content)
}

/// The content of a `kind = session` post, checked.
pub fn read_session(post: &MusicPost) -> Result<MusicSessionContent, String> {
    if post.kind != MusicPostKind::Session {
        return Err("Validation Error: not a session".into());
    }
    content::parse_session(&post.content)
}

/// Mints a new post's id and its path under the music namespace.
pub fn create_version(post: &MusicPost, root: Root, owner: &PubkyId) -> Result<MintedVersion, String> {
    post.create_version_under(MUSIC_NAMESPACE, root, owner, None)
}

/// Mints an edit of the post `id` whose newest version is `head`.
pub fn edit_version(
    post: &MusicPost,
    id: &str,
    head: &str,
    root: Root,
    owner: &PubkyId,
) -> Result<MintedVersion, String> {
    post.edit_version_under(MUSIC_NAMESPACE, id, head, root, owner, None)
}
