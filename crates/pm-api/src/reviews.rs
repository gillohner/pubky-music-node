//! The user's own reviews: a rating (1–10, half stars) and/or notes on an album, track or
//! artist, kept by the node and synced as music `review` posts (ADR 0014). One review per target; editing keeps the post id.

use crate::favourites::canonical_for;
use crate::playlists::{now_micros, store};
use crate::state::NodeState;
use crate::types::*;
use crate::{ApiError, ApiResult};
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::Json;
use pm_library::Library;
use pm_pubky::schemas::post::TYPE_REVIEW;
use pm_pubky::{MusicContent, MusicPost, Review, Visibility};
use std::sync::Arc;

fn target(r: &ReviewRef) -> FavouriteRef {
    FavouriteRef {
        uri: r.uri.clone(),
        track_id: r.track_id,
        album_id: r.album_id,
        artist_id: r.artist_id,
        at: None,
    }
}

fn entry(row: &pm_library::MusicPostRow, r: &Review) -> ReviewEntry {
    ReviewEntry {
        id: row.id.clone(),
        uri: r.target_uri.clone(),
        rating: r.rating,
        text: r.text.clone(),
        created_at: r.created_at,
        updated_at: row.updated_at,
        synced: row.synced_at.is_some(),
    }
}

/// Every live review, newest first.
pub(crate) fn all_reviews(lib: &Library) -> Result<Vec<(pm_library::MusicPostRow, Review)>, ApiError> {
    let mut out = Vec::new();
    for row in lib.list_music_posts(Some(TYPE_REVIEW))? {
        if row.deleted {
            continue;
        }
        if let Ok(Some(post)) = MusicPost::try_from_bytes(row.json.as_bytes(), &row.id) {
            if let Some(r) = post.as_review() {
                out.push((row.clone(), r.clone()));
            }
        }
    }
    Ok(out)
}

fn review_for(lib: &Library, url: &str) -> Result<Option<(pm_library::MusicPostRow, Review)>, ApiError> {
    Ok(all_reviews(lib)?.into_iter().find(|(_, r)| r.target_uri == url))
}

fn has_target(r: &ReviewRef) -> bool {
    r.uri.is_some() || r.track_id.is_some() || r.album_id.is_some() || r.artist_id.is_some()
}

/// `GET /v1/reviews` (all) or `?uri=|track_id=|album_id=|artist_id=` (one).
pub(crate) async fn list(State(s): State<Arc<NodeState>>, Query(r): Query<ReviewRef>) -> ApiResult<Reviews> {
    let lib = s.library.lock().expect("library poisoned");
    if !has_target(&r) {
        return Ok(Json(Reviews {
            uri: None,
            reviews: all_reviews(&lib)?.iter().map(|(row, r)| entry(row, r)).collect(),
        }));
    }
    // An item with no identity yet has no review yet (writing one still says why it can't)
    let url = match canonical_for(&lib, &target(&r)) {
        Ok(u) => u.to_url(),
        Err(e) if e.status == StatusCode::UNPROCESSABLE_ENTITY => {
            return Ok(Json(Reviews {
                uri: None,
                reviews: Vec::new(),
            }))
        }
        Err(e) => return Err(e),
    };
    let reviews = review_for(&lib, &url)?
        .map(|(row, r)| entry(&row, &r))
        .into_iter()
        .collect();
    Ok(Json(Reviews {
        uri: Some(url),
        reviews,
    }))
}

/// `PUT /v1/reviews {target, rating?, text?}`: create or replace the review of a target.
pub(crate) async fn put(State(s): State<Arc<NodeState>>, Json(r): Json<ReviewRef>) -> ApiResult<ReviewEntry> {
    let text = r.text.clone().unwrap_or_default().trim().to_string();
    if r.rating.is_none() && text.is_empty() {
        return Err(ApiError::bad_request("give a rating, some text, or both"));
    }
    let out = {
        let lib = s.library.lock().expect("library poisoned");
        let uri = canonical_for(&lib, &target(&r))?;
        let url = uri.to_url();
        let existing = review_for(&lib, &url)?;
        let mut review = Review::new(&uri, r.rating, text);
        if let Some((_, old)) = &existing {
            review.created_at = old.created_at;
        }
        let post = MusicPost::new(MusicContent::Review(review.clone()));
        let id = post.create_id();
        post.validate(Some(&id))
            .map_err(|e| ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, e.to_string()))?;
        store(&lib, &post, &id, Visibility::Private)?;
        let row = lib
            .get_music_post(&id)?
            .ok_or_else(|| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "review not stored"))?;
        entry(&row, &review)
    };
    s.notify_library();
    Ok(Json(out))
}

/// `DELETE /v1/reviews?target`.
pub(crate) async fn delete(
    State(s): State<Arc<NodeState>>,
    Query(r): Query<ReviewRef>,
) -> ApiResult<Reviews> {
    let out = {
        let lib = s.library.lock().expect("library poisoned");
        let url = canonical_for(&lib, &target(&r))?.to_url();
        if let Some((row, _)) = review_for(&lib, &url)? {
            lib.delete_music_post(&row.id, now_micros())?;
        }
        Reviews {
            uri: Some(url),
            reviews: Vec::new(),
        }
    };
    s.notify_library();
    Ok(Json(out))
}
