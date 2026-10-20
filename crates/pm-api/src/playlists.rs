//! Playlists: v1 post envelopes (ADR 0006) stored locally in the library and
//! pushed to the private homeserver folder by [`crate::sync`]. Items are
//! canonical music URLs; playing a playlist resolves each item to a library
//! track by that URL.

use crate::sources::NodeSources;
use crate::state::NodeState;
use crate::types::*;
use crate::{ApiError, ApiResult};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use pm_library::{Library, MusicPostRow};
use pm_model::{MusicUri, Track, TrackId};
use pm_pubky::schemas::post::TYPE_PLAYLIST;
use pm_pubky::{MusicContent, MusicPost, Playlist, PlaylistItem, Visibility};
use pm_zones::{Insert, ZoneId};
use std::sync::Arc;

pub(crate) fn now_micros() -> i64 {
    pm_pubky::ids::now_micros()
}

/// Store a validated post, keeping `synced_at` of an earlier row only when
/// the content is unchanged.
pub(crate) fn store(
    lib: &Library,
    post: &MusicPost,
    id: &str,
    visibility: Visibility,
) -> Result<(), ApiError> {
    let json = serde_json::to_string(&post.post).expect("post serializes");
    let (created_at, updated_at) = match &post.content {
        MusicContent::Playlist(p) => (p.created_at, p.updated_at),
        other => (other.created_micros(), other.created_micros()),
    };
    lib.upsert_music_post(&MusicPostRow {
        id: id.to_string(),
        kind: post.content.type_name().to_string(),
        visibility: match visibility {
            Visibility::Private => "private".into(),
            Visibility::Public => "public".into(),
        },
        json,
        created_at,
        updated_at,
        synced_at: None,
        deleted: false,
    })?;
    Ok(())
}

fn load(lib: &Library, id: &str) -> Result<(MusicPostRow, Playlist), ApiError> {
    let row = lib
        .get_music_post(id)?
        .filter(|r| r.kind == TYPE_PLAYLIST)
        .ok_or_else(|| ApiError::not_found(format!("no playlist {id}")))?;
    let post: pubky_app_specs::PubkyAppPost = serde_json::from_str(&row.json)
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let music = MusicPost::from_post(post, Some(id))
        .map_err(|e| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("stored playlist {id}: {e}"),
            )
        })?
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("stored post {id} is not a music post"),
            )
        })?;
    let playlist = music
        .as_playlist()
        .cloned()
        .ok_or_else(|| ApiError::not_found(format!("no playlist {id}")))?;
    Ok((row, playlist))
}

fn summary(row: &MusicPostRow, p: &Playlist) -> PlaylistSummary {
    PlaylistSummary {
        id: row.id.clone(),
        name: p.name.clone(),
        description: p.description.clone(),
        image: p.image.clone(),
        items: p.items.len(),
        created_at: p.created_at,
        updated_at: p.updated_at,
        visibility: row.visibility.clone(),
        synced: row.synced_at.is_some_and(|s| s >= row.updated_at),
    }
}

/// Canonical URLs for library tracks; a track without a canonical identity
/// cannot go into social data (constraint 4).
fn uris_for_tracks(lib: &Library, ids: &[i64]) -> Result<Vec<String>, ApiError> {
    let mut out = Vec::with_capacity(ids.len());
    for id in ids {
        let t = lib
            .get_track(TrackId(*id))?
            .ok_or_else(|| ApiError::not_found(format!("no track {id}")))?;
        let uri = t.publishable_uri().ok_or_else(|| {
            ApiError::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                format!(
                    "track {id} ({}) has no canonical identity yet; tag it with a MusicBrainz id or ISRC",
                    t.title
                ),
            )
        })?;
        out.push(uri.to_url());
    }
    Ok(out)
}

fn parse_uris(uris: &[String]) -> Result<Vec<PlaylistItem>, ApiError> {
    uris.iter()
        .map(|u| {
            let m = MusicUri::parse(u).map_err(|e| ApiError::bad_request(format!("{u}: {e}")))?;
            Ok(PlaylistItem::new(&m))
        })
        .collect()
}

/// The library track for a playlist item, if any.
fn resolve(lib: &Library, item: &PlaylistItem) -> Option<Track> {
    let uri = MusicUri::parse(&item.uri).ok()?;
    lib.tracks_by_canonical(&uri).ok()?.into_iter().next()
}

fn detail(lib: &Library, row: &MusicPostRow, p: &Playlist) -> PlaylistDetail {
    let tracks = p.items.iter().map(|it| resolve(lib, it)).collect();
    PlaylistDetail {
        summary: summary(row, p),
        playlist: p.clone(),
        tracks,
    }
}

fn save_edit(
    lib: &Library,
    id: &str,
    mut playlist: Playlist,
    row: &MusicPostRow,
) -> Result<PlaylistDetail, ApiError> {
    playlist.touch();
    let post = MusicPost::new(MusicContent::Playlist(playlist));
    post.validate(Some(id))
        .map_err(|e| ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, e.to_string()))?;
    let visibility = if row.visibility == "public" {
        Visibility::Public
    } else {
        Visibility::Private
    };
    store(lib, &post, id, visibility)?;
    let (row, p) = load(lib, id)?;
    Ok(detail(lib, &row, &p))
}

// ---- handlers ----

pub(crate) async fn list(State(s): State<Arc<NodeState>>) -> ApiResult<Vec<PlaylistSummary>> {
    let lib = s.library.lock().expect("library poisoned");
    let mut out = Vec::new();
    for row in lib.list_music_posts(Some(TYPE_PLAYLIST))? {
        if let Ok((row, p)) = load(&lib, &row.id) {
            out.push(summary(&row, &p));
        }
    }
    Ok(Json(out))
}

pub(crate) async fn create(
    State(s): State<Arc<NodeState>>,
    Json(body): Json<CreatePlaylist>,
) -> ApiResult<PlaylistDetail> {
    let lib = s.library.lock().expect("library poisoned");
    let mut items = parse_uris(&body.uris)?;
    for u in uris_for_tracks(&lib, &body.track_ids)? {
        items.push(PlaylistItem::new(&MusicUri::parse(&u).expect("canonical")));
    }
    let playlist = Playlist::new(body.name, body.description, body.image, items);
    let post = MusicPost::new(MusicContent::Playlist(playlist));
    let id = post.create_id();
    post.validate(Some(&id))
        .map_err(|e| ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, e.to_string()))?;
    store(&lib, &post, &id, Visibility::Private)?;
    let (row, p) = load(&lib, &id)?;
    let d = detail(&lib, &row, &p);
    drop(lib);
    s.notify_library();
    Ok(Json(d))
}

pub(crate) async fn get(
    State(s): State<Arc<NodeState>>,
    Path(id): Path<String>,
) -> ApiResult<PlaylistDetail> {
    let lib = s.library.lock().expect("library poisoned");
    let (row, p) = load(&lib, &id)?;
    Ok(Json(detail(&lib, &row, &p)))
}

pub(crate) async fn update(
    State(s): State<Arc<NodeState>>,
    Path(id): Path<String>,
    Json(body): Json<UpdatePlaylist>,
) -> ApiResult<PlaylistDetail> {
    let lib = s.library.lock().expect("library poisoned");
    let (row, mut p) = load(&lib, &id)?;
    if let Some(n) = body.name {
        p.name = n;
    }
    if let Some(d) = body.description {
        p.description = Some(d).filter(|d| !d.trim().is_empty());
    }
    if let Some(i) = body.image {
        p.image = Some(i).filter(|i| !i.trim().is_empty());
    }
    if let Some(uris) = body.uris {
        // Keep `added_at` for items that stay.
        let old = p.items.clone();
        p.items = parse_uris(&uris)?
            .into_iter()
            .map(|mut it| {
                if let Some(prev) = old.iter().find(|o| o.uri == it.uri) {
                    it.added_at = prev.added_at;
                }
                it
            })
            .collect();
    }
    let d = save_edit(&lib, &id, p, &row)?;
    drop(lib);
    s.notify_library();
    Ok(Json(d))
}

pub(crate) async fn add_items(
    State(s): State<Arc<NodeState>>,
    Path(id): Path<String>,
    Json(body): Json<AddPlaylistItems>,
) -> ApiResult<PlaylistDetail> {
    let lib = s.library.lock().expect("library poisoned");
    let (row, mut p) = load(&lib, &id)?;
    let mut items = parse_uris(&body.uris)?;
    for u in uris_for_tracks(&lib, &body.track_ids)? {
        items.push(PlaylistItem::new(&MusicUri::parse(&u).expect("canonical")));
    }
    if items.is_empty() {
        return Err(ApiError::bad_request("nothing to add"));
    }
    let at = body.at.unwrap_or(p.items.len()).min(p.items.len());
    p.items.splice(at..at, items);
    let d = save_edit(&lib, &id, p, &row)?;
    drop(lib);
    s.notify_library();
    Ok(Json(d))
}

pub(crate) async fn remove_item(
    State(s): State<Arc<NodeState>>,
    Path((id, index)): Path<(String, usize)>,
) -> ApiResult<PlaylistDetail> {
    let lib = s.library.lock().expect("library poisoned");
    let (row, mut p) = load(&lib, &id)?;
    if index >= p.items.len() {
        return Err(ApiError::not_found(format!("no item {index}")));
    }
    p.items.remove(index);
    let d = save_edit(&lib, &id, p, &row)?;
    drop(lib);
    s.notify_library();
    Ok(Json(d))
}

pub(crate) async fn delete(
    State(s): State<Arc<NodeState>>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let lib = s.library.lock().expect("library poisoned");
    load(&lib, &id)?;
    lib.delete_music_post(&id, now_micros())?;
    drop(lib);
    s.notify_library();
    Ok(StatusCode::NO_CONTENT)
}

/// Queue the playlist's resolvable tracks into a zone.
pub(crate) async fn play(
    State(s): State<Arc<NodeState>>,
    Path(id): Path<String>,
    Json(body): Json<PlayPlaylist>,
) -> ApiResult<PlayedPlaylist> {
    let zone = s
        .zones
        .get(&ZoneId::new(&body.zone))
        .ok_or_else(|| ApiError::not_found(format!("no zone {}", body.zone)))?;
    let (items, missing) = {
        let lib = s.library.lock().expect("library poisoned");
        let (_, p) = load(&lib, &id)?;
        let mut items = Vec::new();
        let mut missing = Vec::new();
        for it in &p.items {
            match resolve(&lib, it).and_then(|t| NodeSources::item_for(&t)) {
                Some(q) => items.push(q),
                None => missing.push(it.uri.clone()),
            }
        }
        (items, missing)
    };
    if items.is_empty() {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "none of the playlist's items is in the library",
        ));
    }
    let replace = body.replace.unwrap_or(true);
    if replace {
        zone.stop();
        zone.clear();
    }
    let queued = items.len();
    zone.enqueue(items, Insert::End);
    if replace {
        zone.play_index(0);
    }
    Ok(Json(PlayedPlaylist { queued, missing }))
}
