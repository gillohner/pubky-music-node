//! Bookmarks (called favourites or hearts before 2026-10-26): social v1 bookmarks of canonical
//! music URLs (ADR 0010, 0014), kept locally in the library and pushed to the user's private
//! homeserver folder (`/priv/social/v1/bookmarks/`) by the sync. A track, album or artist without
//! a canonical identity is bookmarked **on this node only** (never synced; constraint 4) and
//! becomes an ordinary bookmark once it is identified. Albums and artists without an id answer
//! 422 with the reason.
//!
//! A public Pubky post can be bookmarked too, by its `pubky://…/posts/<id>` link: someone's
//! collection, say (answers list those in `posts`).
//!
//! `/v1/bookmarks` is the API; `/v1/favourites` answers the same for one release (deprecated).

use crate::state::NodeState;
use crate::types::*;
use crate::{ApiError, ApiResult};
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::Json;
use pm_library::{BookmarkRow, Library};
use pm_model::{MusicUri, TrackId};
use pm_pubky::Bookmark;
use std::sync::Arc;

/// The canonical URL an item reference points at (a bookmark, a tag or a review target).
pub(crate) fn canonical_for(lib: &Library, r: &ItemRef) -> Result<MusicUri, ApiError> {
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

/// Library songs without a MusicBrainz id or ISRC, bookmarked on this node only (mn-infra:
/// Music Assistant's `star` failed on every other song of an untagged library). Albums and
/// artists MusicBrainz doesn't know yet are kept the same way (second review: bookmarking such an
/// album failed with "no MusicBrainz release id"). Library migration 15 renamed the keys from
/// `favourites.local*`.
const LOCAL_KEY: &str = "bookmarks.local";
const LOCAL_ALBUMS: &str = "bookmarks.local.albums";
const LOCAL_ARTISTS: &str = "bookmarks.local.artists";

fn local_of(lib: &Library, key: &str) -> Vec<i64> {
    lib.get_setting(key)
        .ok()
        .flatten()
        .and_then(|v| serde_json::from_str(&v).ok())
        .unwrap_or_default()
}

pub(crate) fn local_ids(lib: &Library) -> Vec<i64> {
    local_of(lib, LOCAL_KEY)
}

fn store_of(lib: &Library, key: &str, ids: &[i64]) -> Result<(), ApiError> {
    if ids.is_empty() {
        lib.delete_setting(key)?;
    } else {
        lib.set_setting(key, &serde_json::to_string(ids).expect("ids serialize"))?;
    }
    Ok(())
}

fn store_local(lib: &Library, ids: &[i64]) -> Result<(), ApiError> {
    store_of(lib, LOCAL_KEY, ids)
}

/// What a reference names when it has no identity yet: `(setting key, page key prefix, id)`.
fn unidentified(lib: &Library, r: &ItemRef) -> Result<Option<(&'static str, &'static str, i64)>, ApiError> {
    if r.uri.as_deref().is_some_and(|u| !u.trim().is_empty()) {
        return Ok(None);
    }
    if let Some(id) = r.track_id {
        let t = lib
            .get_track(TrackId(id))?
            .ok_or_else(|| ApiError::not_found(format!("track {id}")))?;
        return Ok(t.publishable_uri().is_none().then_some((LOCAL_KEY, "track", id)));
    }
    if let Some(id) = r.album_id {
        let a = lib
            .get_album(id)?
            .ok_or_else(|| ApiError::not_found(format!("album {id}")))?;
        return Ok(a.release_mbid.is_none().then_some((LOCAL_ALBUMS, "album", id)));
    }
    if let Some(id) = r.artist_id {
        let a = lib
            .get_artist(id)?
            .ok_or_else(|| ApiError::not_found(format!("artist {id}")))?;
        return Ok(a.mbid.is_none().then_some((LOCAL_ARTISTS, "artist", id)));
    }
    Ok(None)
}

/// Bookmark or unbookmark: a published bookmark, or (a song, album or artist not identified yet)
/// one kept on this node. Returns the key the page checks (`uri`, or `track:<id>`, `album:<id>`,
/// `artist:<id>` for a node-only one).
pub(crate) fn set(lib: &Library, r: &ItemRef, on: bool) -> Result<String, ApiError> {
    // A public Pubky post (someone's collection): bookmarked by its link
    if let Some(post) = r.uri.as_deref().and_then(pm_pubky::schemas::post_uri) {
        if on {
            add_uri(lib, &post)?;
        } else {
            lib.remove_bookmark(&post)?;
        }
        return Ok(post);
    }
    if let Some((key, kind, id)) = unidentified(lib, r)? {
        let mut ids = local_of(lib, key);
        ids.retain(|x| *x != id);
        if on {
            ids.push(id);
        }
        store_of(lib, key, &ids)?;
        return Ok(format!("{kind}:{id}"));
    }
    let uri = canonical_for(lib, r)?;
    if on {
        add(lib, uri.clone())?;
    } else {
        lib.remove_bookmark(&uri.to_url())?;
        // It may have been a node-only bookmark before it was identified
        for (key, id) in [
            (LOCAL_KEY, r.track_id),
            (LOCAL_ALBUMS, r.album_id),
            (LOCAL_ARTISTS, r.artist_id),
        ] {
            let Some(id) = id else { continue };
            let mut ids = local_of(lib, key);
            let n = ids.len();
            ids.retain(|x| *x != id);
            if ids.len() != n {
                store_of(lib, key, &ids)?;
            }
        }
    }
    Ok(uri.to_url())
}

/// [`set`] for a change a device made at `at` (Unix seconds) and sends later (an offline
/// bookmark). For a published bookmark, a clear older than the bookmark the node has is skipped
/// (the song was bookmarked again since), and a new bookmark keeps the time it was made.
/// Node-only bookmarks and bookmarks of posts have no time: the last change to arrive wins.
/// Returns the key and whether it is bookmarked now.
pub(crate) fn set_at(
    lib: &Library,
    r: &ItemRef,
    on: bool,
    at: Option<i64>,
) -> Result<(String, bool), ApiError> {
    let is_post = r.uri.as_deref().and_then(pm_pubky::schemas::post_uri).is_some();
    if let Some(at) = at.filter(|_| !is_post) {
        if unidentified(lib, r)?.is_none() {
            let uri = canonical_for(lib, r)?;
            let url = uri.to_url();
            let live = lib.bookmark_by_uri(&url)?.filter(|f| !f.deleted);
            match (on, live) {
                // Bookmarked after the second the clear was made in (within it, the clear wins)
                (false, Some(f)) if f.created_at >= at.saturating_add(1).saturating_mul(1_000_000) => {
                    return Ok((url, true))
                }
                (true, None) => {
                    let f = Bookmark {
                        created_at: at.clamp(0, pm_library::db::now_unix()) * 1_000_000,
                        ..Bookmark::new(uri)
                    };
                    lib.add_bookmark(&f.id, &url, f.created_at)?;
                    return Ok((url, true));
                }
                _ => {}
            }
        }
    }
    Ok((set(lib, r, on)?, on))
}

/// Node-only bookmarks identified since become ordinary ones (synced with the next sync).
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
                    if lib.bookmark_by_uri(&u.to_url())?.is_none() {
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
    for (key, album) in [(LOCAL_ALBUMS, true), (LOCAL_ARTISTS, false)] {
        let ids = local_of(lib, key);
        if ids.is_empty() {
            continue;
        }
        let mut keep = Vec::new();
        for id in &ids {
            let uri = if album {
                match lib.get_album(*id)? {
                    Some(a) => a.release_mbid.map(|mbid| MusicUri::Release { mbid }),
                    None => continue,
                }
            } else {
                match lib.get_artist(*id)? {
                    Some(a) => a.mbid.map(|mbid| MusicUri::Artist { mbid }),
                    None => continue,
                }
            };
            match uri {
                Some(u) => {
                    if lib.bookmark_by_uri(&u.to_url())?.is_none() {
                        add(lib, u)?;
                    }
                }
                None => keep.push(*id),
            }
        }
        if keep.len() != ids.len() {
            store_of(lib, key, &keep)?;
        }
    }
    Ok(())
}

pub(crate) fn add(lib: &Library, uri: MusicUri) -> Result<BookmarkRow, pm_library::LibraryError> {
    let f = Bookmark::new(uri);
    lib.add_bookmark(&f.id, &f.uri.to_url(), f.created_at)
}

/// Bookmark a public post by its stored link ([`pm_pubky::schemas::post_uri`]).
fn add_uri(lib: &Library, uri: &str) -> Result<BookmarkRow, pm_library::LibraryError> {
    lib.add_bookmark(&Bookmark::id_of(uri), uri, pm_pubky::ids::now_micros())
}

/// Every bookmark, joined to the library where it is present.
pub(crate) fn collect(lib: &Library) -> Result<Bookmarks, ApiError> {
    promote_local(lib)?;
    let mut out = Bookmarks::default();
    for id in local_ids(lib) {
        if let Some(t) = lib.get_track(TrackId(id))? {
            out.uris.push(format!("track:{id}"));
            out.tracks.push(t);
        }
    }
    for id in local_of(lib, LOCAL_ALBUMS) {
        if let Some(a) = lib.get_album(id)? {
            out.uris.push(format!("album:{id}"));
            out.albums.push(a);
        }
    }
    for id in local_of(lib, LOCAL_ARTISTS) {
        if let Some(a) = lib.get_artist(id)? {
            out.uris.push(format!("artist:{id}"));
            out.artists.push(a);
        }
    }
    for row in lib.list_bookmarks()? {
        out.uris.push(row.uri.clone());
        let Ok(uri) = MusicUri::parse(&row.uri) else {
            if pm_pubky::schemas::post_uri(&row.uri).is_some() {
                out.posts.push(row.uri);
            } else {
                out.other.push(row.uri);
            }
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

/// `GET /v1/bookmarks` (and the deprecated `/v1/favourites`).
pub(crate) async fn list(State(s): State<Arc<NodeState>>) -> ApiResult<Bookmarks> {
    let lib = s.library.lock().expect("library poisoned");
    Ok(Json(collect(&lib)?))
}

/// `PUT /v1/bookmarks {uri | track_id | album_id | artist_id}`.
pub(crate) async fn put(
    State(s): State<Arc<NodeState>>,
    Json(r): Json<ItemRef>,
) -> ApiResult<BookmarkChanged> {
    let (uri, on) = set_at(&s.library.lock().expect("library poisoned"), &r, true, r.at)?;
    s.notify_library();
    Ok(Json(BookmarkChanged::new(uri, on)))
}

/// `DELETE /v1/bookmarks?uri=|track_id=|album_id=|artist_id=`.
pub(crate) async fn delete(
    State(s): State<Arc<NodeState>>,
    Query(r): Query<ItemRef>,
) -> ApiResult<BookmarkChanged> {
    let (uri, on) = set_at(&s.library.lock().expect("library poisoned"), &r, false, r.at)?;
    s.notify_library();
    Ok(Json(BookmarkChanged::new(uri, on)))
}
