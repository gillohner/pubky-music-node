//! Music on Pubky, on top of the v1 social objects.
//!
//! Music uses the shared social objects for everything that is not musical: a tag on a song is a
//! social `tag` on its canonical URL, a favourite is a social `bookmark`, a comment is a social
//! `post` that embeds the music URL, and a playlist is a social `collection` post. This crate adds
//! only what is musical:
//!
//! - two post kinds of its own, [`MusicPostKind::Review`] and [`MusicPostKind::Listen`], on the
//!   shared [`PostEnvelope`] under the `music` namespace
//!   (`/{pub,priv}/music/v1/posts/{id}/{editId}.json`), with their content rules;
//! - the `ext.music` hints a playlist carries on its items and on the collection ([`ext`]), and the
//!   readers that treat them as hostile input.
//!
//! Canonical music URLs (what `embed` and collection items point at) follow the node's ADR 0001:
//! the MusicBrainz URL first, then the ISRC URL, then the service URL. This crate does not pick
//! them; it only requires that a review or listen names one.
//!
//! The crate depends on nothing but the social specs and serde, so it can live in its own
//! repository like the Mapky and Eventky specs.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod content;
pub mod ext;
mod kind;

pub use content::{
    MusicListenContent, MusicReviewContent, MusicSource, LISTEN_DURATION_MAX_MS, LISTEN_PLAYED_AT_MIN_MS,
    REVIEW_RATING_MAX, REVIEW_RATING_MIN, REVIEW_TEXT_MAX_LENGTH, SERVICE_MAX_LENGTH,
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

/// A listen of the music at `embed`, played at `played_at_ms` (Unix milliseconds). Listens are
/// private: they validate only under the `priv` root. Infallible; validate before writing.
pub fn listen(
    played_at_ms: i64,
    duration_ms: Option<i64>,
    source: Option<MusicSource>,
    embed: String,
) -> MusicPost {
    let content = MusicListenContent {
        played_at: played_at_ms,
        duration_ms,
        source,
        extra: Default::default(),
    };
    let content = serde_json::to_string(&content).expect("a listen always serializes");
    PostEnvelope::new(content, MusicPostKind::Listen, None, Some(embed), Vec::new())
}

/// The review inside a `kind = review` post, checked against the review rules.
pub fn read_review(post: &MusicPost) -> Result<MusicReviewContent, String> {
    if post.kind != MusicPostKind::Review {
        return Err("Validation Error: not a review".into());
    }
    content::parse_review(&post.content)
}

/// The listen inside a `kind = listen` post, checked against the listen rules.
pub fn read_listen(post: &MusicPost) -> Result<MusicListenContent, String> {
    if post.kind != MusicPostKind::Listen {
        return Err("Validation Error: not a listen".into());
    }
    content::parse_listen(&post.content)
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
