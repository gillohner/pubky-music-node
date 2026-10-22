//! Favourites ("hearts"): v1 `PubkyAppBookmark`s of canonical music URLs
//! (ADR 0010), kept locally in the library and pushed to the user's private
//! homeserver folder by the sync. A track, album or artist without a
//! canonical identity cannot be favourited (constraint 4), with a 422 that
//! says why.

use crate::state::NodeState;
use crate::types::*;
use crate::{ApiError, ApiResult};
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::Json;
use pm_library::{FavouriteRow, Library};
use pm_model::{MusicUri, TrackId};
use pm_pubky::Favourite;
use std::sync::Arc;

/// The canonical URL a favourite reference points at.
pub(crate) fn canonical_for(lib: &Library, r: &FavouriteRef) -> Result<MusicUri, ApiError> {
    if let Some(uri) = r.uri.as_deref().filter(|u| !u.trim().is_empty()) {
        return MusicUri::parse(uri).map_err(|e| ApiError::bad_request(format!("{uri}: {e}")));
    }
    if let Some(id) = r.track_id {
        let t = lib
            .get_track(TrackId(id))?
            .ok_or_else(|| ApiError::not_found(format!("track {id}")))?;
        return t.publishable_uri().cloned().ok_or_else(|| {
            ApiError::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                format!(
                    "“{}” is not matched to a song yet: press the ? next to it and pick which song it is",
                    t.title
                ),
            )
        });
    }
    if let Some(id) = r.album_id {
        let a = lib
            .get_album(id)?
            .ok_or_else(|| ApiError::not_found(format!("album {id}")))?;
        return a
            .release_mbid
            .map(|mbid| MusicUri::Release { mbid })
            .ok_or_else(|| {
                ApiError::new(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    format!("“{}” has no MusicBrainz release id yet", a.title),
                )
            });
    }
    if let Some(id) = r.artist_id {
        let a = lib
            .get_artist(id)?
            .ok_or_else(|| ApiError::not_found(format!("artist {id}")))?;
        return a.mbid.map(|mbid| MusicUri::Artist { mbid }).ok_or_else(|| {
            ApiError::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                format!("“{}” has no MusicBrainz artist id yet", a.name),
            )
        });
    }
    Err(ApiError::bad_request(
        "give one of uri, track_id, album_id, artist_id",
    ))
}

pub(crate) fn add(lib: &Library, uri: MusicUri) -> Result<FavouriteRow, pm_library::LibraryError> {
    let f = Favourite::new(uri);
    lib.add_favourite(&f.id, &f.uri.to_url(), f.created_at)
}

/// Every favourite, joined to the library where it is present.
pub(crate) fn collect(lib: &Library) -> Result<Favourites, ApiError> {
    let mut out = Favourites::default();
    for row in lib.list_favourites()? {
        out.uris.push(row.uri.clone());
        let Ok(uri) = MusicUri::parse(&row.uri) else {
            out.other.push(row.uri);
            continue;
        };
        match &uri {
            MusicUri::Recording { .. } | MusicUri::Isrc { .. } | MusicUri::Service(_) => {
                match lib.tracks_by_canonical(&uri)?.into_iter().next() {
                    Some(t) => out.tracks.push(t),
                    None => out.other.push(row.uri),
                }
            }
            MusicUri::Release { mbid } => match lib.album_by_release_mbid(*mbid)? {
                Some(a) => out.albums.push(a),
                None => out.other.push(row.uri),
            },
            MusicUri::Artist { mbid } => match lib.artist_by_mbid(*mbid)? {
                Some(a) => out.artists.push(a),
                None => out.other.push(row.uri),
            },
            MusicUri::ReleaseGroup { .. } => out.other.push(row.uri),
        }
    }
    Ok(out)
}

pub(crate) async fn list(State(s): State<Arc<NodeState>>) -> ApiResult<Favourites> {
    let lib = s.library.lock().expect("library poisoned");
    Ok(Json(collect(&lib)?))
}

pub(crate) async fn put(
    State(s): State<Arc<NodeState>>,
    Json(r): Json<FavouriteRef>,
) -> ApiResult<FavouriteChanged> {
    let uri = {
        let lib = s.library.lock().expect("library poisoned");
        let uri = canonical_for(&lib, &r)?;
        add(&lib, uri.clone())?;
        uri
    };
    s.notify_library();
    Ok(Json(FavouriteChanged {
        uri: uri.to_url(),
        favourite: true,
    }))
}

pub(crate) async fn delete(
    State(s): State<Arc<NodeState>>,
    Query(r): Query<FavouriteRef>,
) -> ApiResult<FavouriteChanged> {
    let uri = {
        let lib = s.library.lock().expect("library poisoned");
        let uri = canonical_for(&lib, &r)?;
        lib.remove_favourite(&uri.to_url())?;
        uri
    };
    s.notify_library();
    Ok(Json(FavouriteChanged {
        uri: uri.to_url(),
        favourite: false,
    }))
}
