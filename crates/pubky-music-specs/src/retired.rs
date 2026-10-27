//! Kinds this spec no longer has, read only to rewrite them.
//!
//! **`review`** (music 1.0 drafts): `{"rating": 1–10, "text"?}` with `embed` the music URL. A
//! rating is the social `rating` kind since pubky-social-specs 1.1: the same scale and text,
//! `parent` the thing rated, one per author per target with the newest version winning, and
//! counted by the indexer the same way for a song, a place or a listing. An old review reads as
//! [`MusicPostKind::Unknown`] and fails validation; [`rating_from_review`] gives the rating to
//! write in its place, under `/{root}/social/v1/posts/` with the same post id, so it keeps its
//! creation time.

use pubky_social_specs::{
    code_point_len, frozen_trim, PubkySocialPost, PubkySocialPostKind, VALIDATION_LIMITS,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::content::{check_extra, parse};
use crate::{MusicPost, MusicPostKind};

/// The wire spelling of the retired review kind.
pub const REVIEW_KIND: &str = "review";

/// The content of a retired `review`.
#[derive(Serialize, Deserialize)]
struct ReviewContent {
    rating: i64,
    #[serde(default)]
    text: Option<String>,
    #[serde(flatten)]
    extra: Map<String, Value>,
}

/// Whether `post` is a retired music `review`.
pub fn is_review(post: &MusicPost) -> bool {
    post.kind == MusicPostKind::Unknown(REVIEW_KIND.to_string())
}

/// The social `rating` a retired music `review` becomes: `parent` is its `embed`, the rating and
/// the text are the same (a blank text is left out), and unknown members of its content and of
/// the post, its attachments and its lock are carried over. Validate before writing: the rating
/// rules are the social spec's.
pub fn rating_from_review(post: &MusicPost) -> Result<PubkySocialPost, String> {
    if !is_review(post) {
        return Err("Validation Error: not a review".into());
    }
    let Some(target) = post.embed.clone() else {
        return Err("Validation Error: a review must embed the music URL it is about".into());
    };
    let review: ReviewContent = parse(&post.content, "review")?;
    check_extra(&review.extra, &["rating", "text"])?;
    let text = review
        .text
        .map(|t| frozen_trim(&t).to_string())
        .filter(|t| !t.is_empty());
    if text
        .as_deref()
        .is_some_and(|t| code_point_len(t) > VALIDATION_LIMITS.rating_text_max_length)
    {
        return Err(format!(
            "Validation Error: review text must be at most {} code points",
            VALIDATION_LIMITS.rating_text_max_length
        ));
    }
    let mut rating = PubkySocialPost::new_rating(target, review.rating, text, post.attachments.clone());
    if !review.extra.is_empty() {
        let mut content: Map<String, Value> =
            serde_json::from_str(&rating.content).expect("the rating was just written");
        content.extend(review.extra);
        rating.content = serde_json::to_string(&content).expect("a rating serializes");
    }
    rating.lock = post.lock.clone();
    rating.extra = post.extra.clone();
    debug_assert_eq!(rating.kind, PubkySocialPostKind::Rating);
    Ok(rating)
}
