//! The user's own ratings and comments on an album, track or artist: a rating (1–10, half stars)
//! with optional text, or text alone. Synced as social v1.1 posts whose `parent` is the music URL
//! (ADR 0014): a `rating`, or a `note` for text alone. One per target, the way social `rating` is
//! one per author per target; editing keeps the post id.
//!
//! Private by default; `public` publishes it like a collection: the private draft is kept and the
//! same version is copied under `/pub/`, where every app and the indexer's counts see it.
//!
//! The node still calls its working copy a review (`pubky.music/review` in the library).
//! `/v1/reviews` is the older name of `/v1/ratings`, for one release.

use crate::bookmarks::canonical_for;
use crate::playlists::{now_micros, store};
use crate::state::NodeState;
use crate::types::*;
use crate::{ApiError, ApiResult};
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::Json;
use pm_library::{Library, MusicPostRow};
use pm_pubky::schemas::post::TYPE_REVIEW;
use pm_pubky::{MusicContent, MusicPost, Review, Visibility};
use std::collections::BTreeMap;
use std::sync::Arc;

fn target(r: &RatingRef) -> ItemRef {
    ItemRef {
        uri: r.uri.clone(),
        track_id: r.track_id,
        album_id: r.album_id,
        artist_id: r.artist_id,
        at: None,
    }
}

fn entry(row: &MusicPostRow, r: &Review) -> RatingEntry {
    RatingEntry {
        id: row.id.clone(),
        uri: r.target_uri.clone(),
        rating: r.rating,
        text: r.text.clone(),
        public: row.visibility == "public",
        created_at: r.created_at,
        updated_at: row.updated_at,
        synced: row.synced_at.is_some(),
    }
}

fn answer(uri: Option<String>, ratings: Vec<RatingEntry>) -> Ratings {
    Ratings {
        uri,
        reviews: ratings.clone(),
        ratings,
    }
}

/// The one rating of each target, newest first. Another app (or another node before a sync)
/// may have written a second one: a rating wins over a comment, then the newest.
pub(crate) fn all_ratings(lib: &Library) -> Result<Vec<(MusicPostRow, Review)>, ApiError> {
    let mut best: BTreeMap<String, (MusicPostRow, Review)> = BTreeMap::new();
    for row in lib.list_music_posts(Some(TYPE_REVIEW))? {
        if row.deleted {
            continue;
        }
        let Ok(Some(post)) = MusicPost::try_from_bytes(row.json.as_bytes(), &row.id) else {
            continue;
        };
        let Some(r) = post.as_review() else {
            continue;
        };
        let rank = |row: &MusicPostRow, r: &Review| (r.rating.is_some(), row.updated_at, row.id.clone());
        let better = best
            .get(&r.target_uri)
            .is_none_or(|(b_row, b)| rank(&row, r) > rank(b_row, b));
        if better {
            best.insert(r.target_uri.clone(), (row.clone(), r.clone()));
        }
    }
    let mut out: Vec<_> = best.into_values().collect();
    out.sort_by_key(|(row, _)| std::cmp::Reverse((row.updated_at, row.id.clone())));
    Ok(out)
}

fn rating_for(lib: &Library, url: &str) -> Result<Option<(MusicPostRow, Review)>, ApiError> {
    Ok(all_ratings(lib)?.into_iter().find(|(_, r)| r.target_uri == url))
}

fn has_target(r: &RatingRef) -> bool {
    r.uri.is_some() || r.track_id.is_some() || r.album_id.is_some() || r.artist_id.is_some()
}

/// `GET /v1/ratings` (all) or `?uri=|track_id=|album_id=|artist_id=` (one).
pub(crate) async fn list(State(s): State<Arc<NodeState>>, Query(r): Query<RatingRef>) -> ApiResult<Ratings> {
    let lib = s.library.lock().expect("library poisoned");
    if !has_target(&r) {
        let all = all_ratings(&lib)?.iter().map(|(row, r)| entry(row, r)).collect();
        return Ok(Json(answer(None, all)));
    }
    let url = canonical_for(&lib, &target(&r))?.to_url();
    let one = rating_for(&lib, &url)?
        .map(|(row, r)| entry(&row, &r))
        .into_iter()
        .collect();
    Ok(Json(answer(Some(url), one)))
}

/// `PUT /v1/ratings {target, rating?, text?, public?}`: create or replace the rating of a target.
/// `public` publishes or unpublishes it; left out, a new one is private and an edit keeps its
/// visibility.
pub(crate) async fn put(State(s): State<Arc<NodeState>>, Json(r): Json<RatingRef>) -> ApiResult<RatingEntry> {
    let text = r.text.clone().unwrap_or_default().trim().to_string();
    if r.rating.is_none() && text.is_empty() {
        return Err(ApiError::bad_request("give a rating, some text, or both"));
    }
    let out = {
        let lib = s.library.lock().expect("library poisoned");
        let uri = canonical_for(&lib, &target(&r))?;
        let url = uri.to_url();
        let existing = rating_for(&lib, &url)?;
        let mut review = Review::new(&uri, r.rating, text);
        if let Some((_, old)) = &existing {
            review.created_at = old.created_at;
        }
        let public = r.public.unwrap_or_else(|| {
            existing
                .as_ref()
                .is_some_and(|(row, _)| row.visibility == "public")
        });
        let post = MusicPost::new(MusicContent::Review(review.clone()));
        let id = post.create_id();
        post.validate(Some(&id))
            .map_err(|e| ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, e.to_string()))?;
        let visibility = if public {
            Visibility::Public
        } else {
            Visibility::Private
        };
        store(&lib, &post, &id, visibility)?;
        let row = lib
            .get_music_post(&id)?
            .ok_or_else(|| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "rating not stored"))?;
        entry(&row, &review)
    };
    s.notify_library();
    Ok(Json(out))
}

/// `DELETE /v1/ratings?target`.
pub(crate) async fn delete(
    State(s): State<Arc<NodeState>>,
    Query(r): Query<RatingRef>,
) -> ApiResult<Ratings> {
    let out = {
        let lib = s.library.lock().expect("library poisoned");
        let url = canonical_for(&lib, &target(&r))?.to_url();
        if let Some((row, _)) = rating_for(&lib, &url)? {
            lib.delete_music_post(&row.id, now_micros())?;
        }
        answer(Some(url), Vec::new())
    };
    s.notify_library();
    Ok(Json(out))
}
