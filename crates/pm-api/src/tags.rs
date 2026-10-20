//! The user's music tags: v1 `PubkyAppTag`s on canonical music URLs
//! (ADR 0011). Tags are public by nature in pubky.app, so the node writes
//! one only on an explicit action, keeps the working copy in the library's
//! `tags` table and pushes it with the sync. Pinakes indexes them, which is
//! what Discover reads back.

use crate::favourites::canonical_for;
use crate::state::NodeState;
use crate::types::*;
use crate::{ApiError, ApiResult};
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::Json;
use pm_library::{Library, TagRow};
use pm_model::MusicUri;
use pubky_app_specs::traits::{HashId, Validatable};
use pubky_app_specs::PubkyAppTag;
use std::sync::Arc;

/// Sanitize and validate a label the way pubky.app does; returns the tag
/// object (with the normalized label) and its id.
pub(crate) fn make_tag(uri: &MusicUri, label: &str) -> Result<(PubkyAppTag, String), ApiError> {
    let tag = PubkyAppTag::new(uri.to_url(), label.to_string());
    let id = tag.create_id();
    tag.validate(Some(&id))
        .map_err(|e| ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, e))?;
    Ok((tag, id))
}

fn target(r: &TagRef) -> FavouriteRef {
    FavouriteRef {
        uri: r.uri.clone(),
        track_id: r.track_id,
        album_id: r.album_id,
        artist_id: r.artist_id,
    }
}

fn rows_for(lib: &Library, uri: Option<&str>) -> Result<Vec<TagEntry>, ApiError> {
    Ok(lib
        .list_tags(uri)?
        .into_iter()
        .map(|t: TagRow| TagEntry {
            uri: t.uri,
            label: t.label,
            created_at: t.created_at,
            synced: t.synced_at.is_some(),
        })
        .collect())
}

/// `GET /v1/tags` (all of mine) or `?uri=|track_id=|album_id=|artist_id=` (one object).
pub(crate) async fn list(State(s): State<Arc<NodeState>>, Query(r): Query<TagRef>) -> ApiResult<Tags> {
    let lib = s.library.lock().expect("library poisoned");
    let has_target = r.uri.is_some() || r.track_id.is_some() || r.album_id.is_some() || r.artist_id.is_some();
    if !has_target {
        return Ok(Json(Tags {
            uri: None,
            tags: rows_for(&lib, None)?,
        }));
    }
    let uri = canonical_for(&lib, &target(&r))?;
    let url = uri.to_url();
    Ok(Json(Tags {
        tags: rows_for(&lib, Some(&url))?,
        uri: Some(url),
    }))
}

/// `PUT /v1/tags {uri|track_id|album_id|artist_id, label}`.
pub(crate) async fn put(State(s): State<Arc<NodeState>>, Json(r): Json<TagRef>) -> ApiResult<TagChanged> {
    let label = r.label.clone().unwrap_or_default();
    let (url, label) = {
        let lib = s.library.lock().expect("library poisoned");
        let uri = canonical_for(&lib, &target(&r))?;
        let (tag, id) = make_tag(&uri, &label)?;
        lib.add_tag(&id, &tag.uri, &tag.label, tag.created_at)?;
        (tag.uri, tag.label)
    };
    s.notify_library();
    Ok(Json(TagChanged {
        uri: url,
        label,
        tagged: true,
    }))
}

/// `DELETE /v1/tags?...&label=`.
pub(crate) async fn delete(
    State(s): State<Arc<NodeState>>,
    Query(r): Query<TagRef>,
) -> ApiResult<TagChanged> {
    let label = r.label.clone().unwrap_or_default().trim().to_lowercase();
    if label.is_empty() {
        return Err(ApiError::bad_request("label is required"));
    }
    let url = {
        let lib = s.library.lock().expect("library poisoned");
        let uri = canonical_for(&lib, &target(&r))?;
        let url = uri.to_url();
        lib.remove_tag(&url, &label)?;
        url
    };
    s.notify_library();
    Ok(Json(TagChanged {
        uri: url,
        label,
        tagged: false,
    }))
}
