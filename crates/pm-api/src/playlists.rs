//! Playlists: v1 post envelopes (ADR 0006) stored locally in the library and
//! pushed to the private homeserver folder by [`crate::sync`]. Items are
//! canonical music URLs; playing a playlist resolves each item to a library
//! track by that URL.

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

/// A stored playlist by id.
pub(crate) fn load_playlist(s: &NodeState, id: &str) -> Result<Playlist, ApiError> {
    let lib = s.library.lock().expect("library poisoned");
    Ok(load(&lib, id)?.1)
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

/// The canonical identity behind one queue entry, if it has one.
fn canonical_for_queue_uri(lib: &Library, uri: &str) -> Option<MusicUri> {
    if let Some(id) = uri
        .strip_prefix(crate::sources::TRACK_SCHEME)
        .and_then(|r| r.parse::<i64>().ok())
    {
        return lib
            .get_track(TrackId(id))
            .ok()
            .flatten()
            .and_then(|t| t.publishable_uri().cloned());
    }
    if let Ok(m) = MusicUri::parse(uri) {
        return Some(m);
    }
    let path = std::path::Path::new(uri);
    if path.is_absolute() {
        let row = lib.file_by_path(path).ok().flatten()?;
        return lib
            .get_track(row.track_id)
            .ok()
            .flatten()
            .and_then(|t| t.publishable_uri().cloned());
    }
    None
}

/// Save a queue as a new private playlist. Entries without a canonical
/// identity are reported in `skipped` rather than failing the whole save.
pub(crate) async fn from_queue(
    State(s): State<Arc<NodeState>>,
    Json(body): Json<PlaylistFromQueue>,
) -> ApiResult<PlaylistFromQueueResult> {
    let (entries, default_name): (Vec<(String, String)>, String) = match &body.zone {
        Some(z) => {
            let zone = s
                .zones
                .get(&ZoneId::new(z))
                .ok_or_else(|| ApiError::not_found(format!("no zone {z}")))?;
            let q = zone.queue();
            (
                q.iter().map(|it| (it.uri.clone(), it.label())).collect(),
                format!("Queue · {}", zone.name()),
            )
        }
        None => (
            body.uris.iter().map(|u| (u.clone(), u.clone())).collect(),
            "Queue".to_string(),
        ),
    };
    let lib = s.library.lock().expect("library poisoned");
    let mut items = Vec::with_capacity(entries.len());
    let mut skipped = Vec::new();
    for (uri, label) in &entries {
        match canonical_for_queue_uri(&lib, uri) {
            Some(m) => items.push(PlaylistItem::new(&m)),
            None => skipped.push(label.clone()),
        }
    }
    if items.is_empty() {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            format!(
                "{} track{} in the queue {} not identified yet: identify them in Settings › Identify library",
                skipped.len(),
                if skipped.len() == 1 { "" } else { "s" },
                if skipped.len() == 1 { "is" } else { "are" }
            ),
        ));
    }
    let name = body.name.filter(|n| !n.trim().is_empty()).unwrap_or(default_name);
    let playlist = Playlist::new(name, None, None, items);
    let post = MusicPost::new(MusicContent::Playlist(playlist));
    let id = post.create_id();
    post.validate(Some(&id))
        .map_err(|e| ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, e.to_string()))?;
    store(&lib, &post, &id, Visibility::Private)?;
    let (row, p) = load(&lib, &id)?;
    let d = detail(&lib, &row, &p);
    drop(lib);
    s.notify_library();
    Ok(Json(PlaylistFromQueueResult { playlist: d, skipped }))
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
        // Keep `added_at` and the pin for items that stay (each copy keeps its own).
        let mut old = p.items.clone();
        p.items = parse_uris(&uris)?
            .into_iter()
            .map(|mut it| {
                if let Some(k) = old.iter().position(|o| o.uri == it.uri) {
                    let prev = old.remove(k);
                    it.added_at = prev.added_at;
                    it.source = prev.source;
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

/// `PUT /v1/playlists/{id}/items/{index}/source`: pin where this item plays from, or Auto.
pub(crate) async fn pin_source(
    State(s): State<Arc<NodeState>>,
    Path((id, index)): Path<(String, usize)>,
    Json(body): Json<PinItemSource>,
) -> ApiResult<PlaylistDetail> {
    let lib = s.library.lock().expect("library poisoned");
    let (row, mut p) = load(&lib, &id)?;
    let item = p
        .items
        .get_mut(index)
        .ok_or_else(|| ApiError::not_found(format!("no item {index}")))?;
    item.source = body
        .service
        .filter(|sv| !sv.trim().is_empty() && sv != "auto")
        .map(|service| pm_pubky::ItemSource {
            service,
            uri: body.uri.filter(|u| !u.trim().is_empty()),
        });
    let d = save_edit(&lib, &id, p, &row)?;
    drop(lib);
    s.notify_library();
    Ok(Json(d))
}

/// `PUT /v1/playlists/{id}/order {order}`: the playlist's own play order, or `null` to follow
/// the node's.
pub(crate) async fn set_order(
    State(s): State<Arc<NodeState>>,
    Path(id): Path<String>,
    Json(body): Json<SetPlaylistOrder>,
) -> ApiResult<PlaylistDetail> {
    let order = match body.order {
        Some(o) => Some(crate::play_from::checked_order(&o)?),
        None => None,
    };
    let lib = s.library.lock().expect("library poisoned");
    let (row, mut p) = load(&lib, &id)?;
    p.source_order = order;
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

/// Queue the playlist into a zone, each item from where it plays (its pin, else the
/// playlist's order, else the node's; see [`crate::play_from`]). Items that play only on a
/// Spotify device, or nowhere here, are reported in `missing`.
pub(crate) async fn play(
    State(s): State<Arc<NodeState>>,
    Path(id): Path<String>,
    Json(body): Json<PlayPlaylist>,
) -> ApiResult<PlayedPlaylist> {
    let zone = s
        .zones
        .get(&ZoneId::new(&body.zone))
        .ok_or_else(|| ApiError::not_found(format!("no zone {}", body.zone)))?;
    let p = load_playlist(&s, &id)?;
    let (_, _, resolved) = crate::play_from::resolve_playlist(&s, &p).await;
    let mut items = Vec::new();
    let mut missing = Vec::new();
    for r in resolved {
        match r.queue {
            Some(q) => items.push(q),
            None => missing.push(r.view.uri),
        }
    }
    if items.is_empty() {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "none of the playlist's items plays on this node's connected services",
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

/// A queue item for a playlist entry that is a directly streamable service
/// file (today: `archive.org/download/<item>/<file>`), with the file name as
/// its title and the item as its album.
pub(crate) fn stream_item(uri: &str) -> Option<pm_zones::QueueItem> {
    let link = match pm_model::MusicUri::parse(uri).ok()? {
        pm_model::MusicUri::Service(l) => l,
        _ => return None,
    };
    if link.service != pm_model::ids::Service::InternetArchive
        || link.kind != pm_model::ids::ServiceLinkKind::Track
    {
        return None;
    }
    let file = link.url.rsplit('/').next().unwrap_or("");
    let title = percent_decode(file)
        .rsplit_once('.')
        .map(|(s, _)| s.to_string())
        .unwrap_or_else(|| file.to_string())
        .replace('_', " ");
    let mut q = pm_zones::QueueItem::new(link.url.clone());
    q.title = Some(title);
    q.album = link.id.clone();
    q.artist = Some("Internet Archive".into());
    Some(q)
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod stream_item_tests {
    use super::stream_item;

    #[test]
    fn archive_files_become_queue_items() {
        let q = stream_item("https://archive.org/download/cornellgem/1.%20Minglewood%20Blues.mp3").unwrap();
        assert_eq!(q.title.as_deref(), Some("1. Minglewood Blues"));
        assert_eq!(q.album.as_deref(), Some("cornellgem"));
        assert!(
            stream_item("https://archive.org/details/cornellgem").is_none(),
            "items are not playable as one file"
        );
        assert!(
            stream_item("https://musicbrainz.org/recording/b1a9c0e9-d987-4042-ae91-78d6a3267d69").is_none()
        );
    }
}
