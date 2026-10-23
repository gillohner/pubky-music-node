//! Listening activity and playlist import for integrations such as the Roon
//! bridge sidecar: callers send what they know (title, artist, album, or a
//! canonical URL) and the node resolves it against the library so that
//! only canonical identities enter social data (constraint 4). Activity
//! posts are private (ADR 0006); a full history never leaves the node.

use crate::playlists::store;
use crate::state::NodeState;
use crate::types::*;
use crate::{ApiError, ApiResult};
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::Json;
use pm_library::Library;
use pm_model::{MusicUri, Track};
use pm_pubky::schemas::post::TYPE_ACTIVITY;
use pm_pubky::{Activity, ActivitySource, MusicContent, MusicPost, Playlist, PlaylistItem, Visibility};
use std::sync::Arc;

/// Find a library track for a described item: by canonical URL when given,
/// else by a title + artist search (first hit whose title matches).
pub(crate) fn resolve_described(lib: &Library, d: &DescribedTrack) -> Result<Option<Track>, ApiError> {
    if let Some(uri) = d.uri.as_deref().filter(|u| !u.trim().is_empty()) {
        let m = MusicUri::parse(uri).map_err(|e| ApiError::bad_request(format!("{uri}: {e}")))?;
        if let Some(t) = lib.tracks_by_canonical(&m)?.into_iter().next() {
            return Ok(Some(t));
        }
    }
    Ok(lib.resolve_import(&pm_library::ImportedTrack {
        title: d.title.clone(),
        artist: d.artist.clone(),
        album: d.album.clone(),
        path: d.path.clone(),
        duration_ms: None,
    })?)
}

fn source_of(s: &str) -> ActivitySource {
    match s.trim().to_lowercase().as_str() {
        "" | "local" => ActivitySource::Local,
        "roon" => ActivitySource::Roon,
        other => ActivitySource::Service {
            name: other.to_string(),
        },
    }
}

/// Record a play. The item must resolve to a canonical identity: a
/// canonical URL in the request, or a library track that has one.
pub(crate) async fn record(
    State(s): State<Arc<NodeState>>,
    Json(body): Json<RecordActivity>,
) -> ApiResult<RecordedActivity> {
    let lib = s.library.lock().expect("library poisoned");
    let track = resolve_described(&lib, &body.track)?;
    let uri = match (&track, body.track.uri.as_deref()) {
        (Some(t), _) if t.publishable_uri().is_some() => t.publishable_uri().cloned(),
        (_, Some(u)) if !u.trim().is_empty() => {
            Some(MusicUri::parse(u).map_err(|e| ApiError::bad_request(e.to_string()))?)
        }
        _ => None,
    };
    let Some(uri) = uri else {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            format!(
                "“{}” has no canonical identity in this library; nothing recorded",
                body.track.title
            ),
        ));
    };
    let activity = Activity::new(&uri, source_of(&body.source), body.duration_ms);
    let post = MusicPost::new(MusicContent::Activity(activity));
    let id = post.create_id();
    post.validate(Some(&id))
        .map_err(|e| ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, e.to_string()))?;
    store(&lib, &post, &id, Visibility::Private)?;
    Ok(Json(RecordedActivity {
        id,
        uri: uri.to_url(),
        track_id: track.and_then(|t| t.id.map(|i| i.0)),
    }))
}

pub(crate) async fn list(
    State(s): State<Arc<NodeState>>,
    Query(p): Query<Page>,
) -> ApiResult<Vec<ActivityEntry>> {
    let lib = s.library.lock().expect("library poisoned");
    let limit = p.limit.unwrap_or(100).min(1000) as usize;
    let mut out = Vec::new();
    for row in lib.list_music_posts(Some(TYPE_ACTIVITY))?.into_iter().take(limit) {
        let Ok(post) = serde_json::from_str::<pubky_app_specs::PubkyAppPost>(&row.json) else {
            continue;
        };
        let Ok(Some(music)) = MusicPost::from_post(post, Some(&row.id)) else {
            continue;
        };
        if let Some(a) = music.as_activity() {
            let track = MusicUri::parse(&a.uri)
                .ok()
                .and_then(|m| lib.tracks_by_canonical(&m).ok())
                .and_then(|v| v.into_iter().next());
            out.push(ActivityEntry {
                id: row.id.clone(),
                uri: a.uri.clone(),
                played_at: a.played_at,
                source: a.source_kind.clone(),
                duration_ms: a.duration_ms,
                track,
                synced: row.synced_at.is_some(),
            });
        }
    }
    Ok(Json(out))
}

/// Create a playlist from described tracks (a Roon playlist, a CSV, …):
/// every track that resolves to a canonical identity becomes an item,
/// the rest are reported back so the caller can show what was lost.
pub(crate) async fn import_playlist(
    State(s): State<Arc<NodeState>>,
    Json(body): Json<ImportPlaylist>,
) -> ApiResult<ImportedPlaylist> {
    import_described(&s, body.name, body.description, body.tracks).map(Json)
}

/// Import a playlist file (M3U/M3U8, PLS, CSV, Roon's Excel export) sent as the
/// raw body. Entries resolve by path, file name, then title and artist.
pub(crate) async fn import_playlist_file(
    State(s): State<Arc<NodeState>>,
    Query(q): Query<ImportFileQuery>,
    body: axum::body::Bytes,
) -> ApiResult<ImportedPlaylist> {
    let format = match q.format.as_deref().map(str::to_ascii_lowercase).as_deref() {
        Some("m3u") | Some("m3u8") => Some(pm_library::ImportFormat::M3u),
        Some("pls") => Some(pm_library::ImportFormat::Pls),
        Some("csv") | Some("tsv") => Some(pm_library::ImportFormat::Csv),
        Some("xlsx") => Some(pm_library::ImportFormat::Xlsx),
        Some(other) => return Err(ApiError::bad_request(format!("unknown format {other}"))),
        None => pm_library::import::detect_format(q.file.as_deref(), &body),
    };
    let format = format
        .ok_or_else(|| ApiError::bad_request(pm_library::import::ImportError::UnknownFormat.to_string()))?;
    let list = pm_library::import::parse(&body, format, None)
        .map_err(|e| ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, e.to_string()))?;
    let name = q
        .name
        .filter(|n| !n.trim().is_empty())
        .or(list.name)
        .or_else(|| {
            q.file
                .as_deref()
                .map(|f| pm_library::import::name_from_file(std::path::Path::new(f)))
        })
        .unwrap_or_else(|| "Imported playlist".into());
    let tracks: Vec<DescribedTrack> = list.tracks.into_iter().map(Into::into).collect();
    import_described(&s, name, None, tracks).map(Json)
}

/// A playlist from described tracks (the Roon import uses it too).
pub(crate) fn import_tracks(
    s: &Arc<NodeState>,
    name: String,
    description: Option<String>,
    tracks: Vec<DescribedTrack>,
) -> Result<ImportedPlaylist, ApiError> {
    import_described(s, name, description, tracks)
}

fn import_described(
    s: &Arc<NodeState>,
    name: String,
    description: Option<String>,
    tracks: Vec<DescribedTrack>,
) -> Result<ImportedPlaylist, ApiError> {
    let lib = s.library.lock().expect("library poisoned");
    let parsed = tracks.len();
    let mut items = Vec::new();
    let mut unresolved = Vec::new();
    for d in &tracks {
        let uri = match resolve_described(&lib, d)? {
            Some(t) => t.publishable_uri().cloned(),
            None => d
                .uri
                .as_deref()
                .filter(|u| !u.trim().is_empty())
                .and_then(|u| MusicUri::parse(u).ok()),
        };
        match uri {
            Some(u) => items.push(PlaylistItem::new(&u)),
            None => unresolved.push(d.clone()),
        }
    }
    if items.is_empty() {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "none of the tracks resolved to a canonical identity",
        ));
    }
    let playlist = Playlist::new(name.clone(), description, None, items);
    let post = MusicPost::new(MusicContent::Playlist(playlist));
    let id = post.create_id();
    post.validate(Some(&id))
        .map_err(|e| ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, e.to_string()))?;
    store(&lib, &post, &id, Visibility::Private)?;
    let imported = post.as_playlist().map(|p| p.items.len()).unwrap_or(0);
    drop(lib);
    s.notify_library();
    Ok(ImportedPlaylist {
        id,
        name,
        parsed,
        imported,
        unresolved,
    })
}
