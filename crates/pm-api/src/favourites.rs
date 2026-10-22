//! Favourites ("hearts"): v1 `PubkyAppBookmark`s of canonical music URLs
//! (ADR 0010), kept locally in the library and pushed to the user's private
//! homeserver folder by the sync. A track, album or artist without a
//! canonical identity is kept as a favourite **on this node only** (never synced; constraint 4)
//! and becomes an ordinary favourite once it is identified. Albums and artists without an id
//! answer 422 with the reason.

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

/// Library songs without a MusicBrainz id or ISRC, favourited on this node only (mn-infra:
/// Music Assistant's `star` failed on every other song of an untagged library).
const LOCAL_KEY: &str = "favourites.local";

pub(crate) fn local_ids(lib: &Library) -> Vec<i64> {
    lib.get_setting(LOCAL_KEY)
        .ok()
        .flatten()
        .and_then(|v| serde_json::from_str(&v).ok())
        .unwrap_or_default()
}

fn store_local(lib: &Library, ids: &[i64]) -> Result<(), ApiError> {
    if ids.is_empty() {
        lib.delete_setting(LOCAL_KEY)?;
    } else {
        lib.set_setting(LOCAL_KEY, &serde_json::to_string(ids).expect("ids serialize"))?;
    }
    Ok(())
}

/// The library track a reference names when it has no identity yet.
fn unidentified_track(lib: &Library, r: &FavouriteRef) -> Result<Option<i64>, ApiError> {
    if r.uri.as_deref().is_some_and(|u| !u.trim().is_empty()) {
        return Ok(None);
    }
    let Some(id) = r.track_id else { return Ok(None) };
    let t = lib
        .get_track(TrackId(id))?
        .ok_or_else(|| ApiError::not_found(format!("track {id}")))?;
    Ok(t.publishable_uri().is_none().then_some(id))
}

/// Heart or unheart: a published favourite, or (a song not identified yet) one kept on this
/// node. Returns the key the page checks (`uri`, or `track:<id>` for a node-only one).
pub(crate) fn set(lib: &Library, r: &FavouriteRef, on: bool) -> Result<String, ApiError> {
    if let Some(id) = unidentified_track(lib, r)? {
        let mut ids = local_ids(lib);
        ids.retain(|x| *x != id);
        if on {
            ids.push(id);
        }
        store_local(lib, &ids)?;
        return Ok(format!("track:{id}"));
    }
    let uri = canonical_for(lib, r)?;
    if on {
        add(lib, uri.clone())?;
    } else {
        lib.remove_favourite(&uri.to_url())?;
        // It may have been a node-only favourite before it was identified
        if let Some(id) = r.track_id {
            let mut ids = local_ids(lib);
            let n = ids.len();
            ids.retain(|x| *x != id);
            if ids.len() != n {
                store_local(lib, &ids)?;
            }
        }
    }
    Ok(uri.to_url())
}

/// Node-only favourites identified since become ordinary ones (synced with the next sync).
fn promote_local(lib: &Library) -> Result<(), ApiError> {
    let ids = local_ids(lib);
    if ids.is_empty() {
        return Ok(());
    }
    let mut keep = Vec::new();
    for id in &ids {
        if let Some(t) = lib.get_track(TrackId(*id))? {
            match t.publishable_uri() {
                Some(u) => {
                    if lib.favourite_by_uri(&u.to_url())?.is_none() {
                        add(lib, u.clone())?;
                    }
                }
                None => keep.push(*id),
            }
        }
    }
    if keep.len() != ids.len() {
        store_local(lib, &keep)?;
    }
    Ok(())
}

pub(crate) fn add(lib: &Library, uri: MusicUri) -> Result<FavouriteRow, pm_library::LibraryError> {
    let f = Favourite::new(uri);
    lib.add_favourite(&f.id, &f.uri.to_url(), f.created_at)
}

/// Every favourite, joined to the library where it is present.
pub(crate) fn collect(lib: &Library) -> Result<Favourites, ApiError> {
    promote_local(lib)?;
    let mut out = Favourites::default();
    for id in local_ids(lib) {
        if let Some(t) = lib.get_track(TrackId(id))? {
            out.uris.push(format!("track:{id}"));
            out.tracks.push(t);
        }
    }
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
    let uri = set(&s.library.lock().expect("library poisoned"), &r, true)?;
    s.notify_library();
    Ok(Json(FavouriteChanged { uri, favourite: true }))
}

pub(crate) async fn delete(
    State(s): State<Arc<NodeState>>,
    Query(r): Query<FavouriteRef>,
) -> ApiResult<FavouriteChanged> {
    let uri = set(&s.library.lock().expect("library poisoned"), &r, false)?;
    s.notify_library();
    Ok(Json(FavouriteChanged {
        uri,
        favourite: false,
    }))
}
