//! Library › My music and Node library (the owner's): what is on the owner's homeserver as songs,
//! and editing the node's library without touching files.
//!
//! - `GET /v1/me/music`: the homeserver's private music files joined to the library songs they
//!   are (by file name: the bundled homeserver's folder, or the node's "From homeserver" copy),
//!   so a page lists songs, not hashed file names.
//! - `PUT/DELETE /v1/library/tracks/{id}/edit`: the owner's edit of a song's title, artist,
//!   album and album artist, kept on the node and applied whenever the file is read again; DELETE
//!   undoes it. `PUT /v1/library/albums/{id}/edit` renames an album (its id and cover stay).
//! - `DELETE /v1/library/tracks/{id}`: take a song out of the library (the file stays, scans skip
//!   it); `GET /v1/library/hidden` and `POST /v1/library/hidden/restore` bring it back.

use crate::accounts::{Caller, Role};
use crate::state::NodeState;
use crate::types::ApiEvent;
use crate::{ApiError, ApiResult};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::{Extension, Json};
use pm_library::TrackOverride;
use pm_model::{Track, TrackId};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

fn owner(c: &Caller) -> Result<(), ApiError> {
    if c.role == Role::Owner {
        Ok(())
    } else {
        Err(crate::accounts::refused(
            "only the node's owner changes the node's library",
        ))
    }
}

/// A library song as a My music row.
#[derive(Debug, Clone, Serialize)]
pub struct FileSong {
    pub id: i64,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artist: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub album: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub album_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    /// The owner edited its tags on the node.
    pub edited: bool,
}

impl FileSong {
    fn of(t: &Track, edited: bool) -> Option<Self> {
        Some(Self {
            id: t.id?.0,
            title: t.title.clone(),
            artist: Some(
                t.artists
                    .iter()
                    .map(|a| a.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
            )
            .filter(|a| !a.is_empty()),
            album: t.album.as_ref().map(|a| a.title.clone()),
            album_id: t.album.as_ref().and_then(|a| a.id),
            duration_ms: t.duration_ms,
            edited,
        })
    }
}

/// One private file on the homeserver.
#[derive(Debug, Clone, Serialize)]
pub struct MyFile {
    /// Its path on the homeserver (what `DELETE /v1/pubky/files` takes).
    pub path: String,
    /// The name it was uploaded with (the hash prefix the homeserver needs left out).
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    /// The library song it is, once the library has read it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub song: Option<FileSong>,
}

#[derive(Debug, Clone, Serialize)]
pub struct MyMusic {
    pub files: Vec<MyFile>,
    pub count: usize,
    pub bytes: u64,
}

/// `GET /v1/me/music`.
pub(crate) async fn my_music(
    State(s): State<Arc<NodeState>>,
    Extension(c): Extension<Caller>,
) -> ApiResult<MyMusic> {
    owner(&c)?;
    let sync = s.sync_handle().ok_or_else(|| {
        ApiError::new(
            StatusCode::PRECONDITION_FAILED,
            "Sign in with Pubky to keep music on your homeserver",
        )
    })?;
    let remote = sync
        .list_private_files()
        .await
        .map_err(|e| ApiError::new(StatusCode::BAD_GATEWAY, e.to_string()))?;
    let s2 = s.clone();
    let out = tokio::task::spawn_blocking(move || -> Result<MyMusic, ApiError> {
        let lib = s2.library.lock().expect("library poisoned");
        let ids = lib.track_ids_by_file_name()?;
        let mut files = Vec::with_capacity(remote.len());
        let mut bytes = 0;
        for f in remote {
            let file_name = f.path.rsplit('/').next().unwrap_or(&f.path).to_string();
            let name = crate::files::split_remote_name(&file_name)
                .map(|(_, n)| n.to_string())
                .unwrap_or_else(|| file_name.clone());
            let song = match ids.get(&file_name) {
                Some(id) => lib.get_track(*id)?.and_then(|t| {
                    let edited = lib.track_override(&t.file.path).ok().flatten().is_some();
                    FileSong::of(&t, edited)
                }),
                None => None,
            };
            bytes += f.size.unwrap_or(0);
            files.push(MyFile {
                path: f.path,
                name,
                size: f.size,
                song,
            });
        }
        // Songs first, by artist and title; files the library doesn't know yet after, by name
        files.sort_by(|a, b| match (&a.song, &b.song) {
            (Some(x), Some(y)) => (
                x.artist.as_deref().unwrap_or("").to_lowercase(),
                x.title.to_lowercase(),
            )
                .cmp(&(
                    y.artist.as_deref().unwrap_or("").to_lowercase(),
                    y.title.to_lowercase(),
                )),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
        });
        Ok(MyMusic {
            count: files.len(),
            files,
            bytes,
        })
    })
    .await
    .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))??;
    Ok(Json(out))
}

/// `PUT /v1/library/tracks/{id}/edit {title?, artist?, album?, album_artist?}`: blank fields
/// keep the file's value; an edit with nothing in it undoes the song's edit.
pub(crate) async fn edit_track(
    State(s): State<Arc<NodeState>>,
    Extension(c): Extension<Caller>,
    Path(id): Path<i64>,
    Json(body): Json<TrackOverride>,
) -> ApiResult<Track> {
    owner(&c)?;
    let body = body.cleaned();
    let t = {
        let lib = s.library.lock().expect("library poisoned");
        lib.edit_track(TrackId(id), &body)?
    };
    let t = t.ok_or_else(|| ApiError::not_found("no such song"))?;
    let _ = s.events.send(ApiEvent::LibraryChanged);
    Ok(Json(t))
}

/// `DELETE /v1/library/tracks/{id}/edit`: back to what the file says.
pub(crate) async fn undo_edit(
    State(s): State<Arc<NodeState>>,
    Extension(c): Extension<Caller>,
    Path(id): Path<i64>,
) -> ApiResult<Track> {
    owner(&c)?;
    let t = s
        .library
        .lock()
        .expect("library poisoned")
        .undo_edit(TrackId(id))?;
    let t = t.ok_or_else(|| ApiError::not_found("no such song"))?;
    let _ = s.events.send(ApiEvent::LibraryChanged);
    Ok(Json(t))
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct AlbumEdit {
    #[serde(default)]
    pub title: Option<String>,
    /// `""` clears it.
    #[serde(default)]
    pub album_artist: Option<String>,
}

/// `PUT /v1/library/albums/{id}/edit {title?, album_artist?}`.
pub(crate) async fn edit_album(
    State(s): State<Arc<NodeState>>,
    Extension(c): Extension<Caller>,
    Path(id): Path<i64>,
    Json(body): Json<AlbumEdit>,
) -> ApiResult<serde_json::Value> {
    owner(&c)?;
    let title = body.title.map(|t| t.trim().to_string()).filter(|t| !t.is_empty());
    let artist = body.album_artist.map(|a| a.trim().to_string());
    if title.is_none() && artist.is_none() {
        return Err(ApiError::bad_request("Give the album a title or an album artist"));
    }
    let done = {
        let lib = s.library.lock().expect("library poisoned");
        lib.edit_album(
            id,
            title.as_deref(),
            artist
                .as_ref()
                .map(|a| Some(a.as_str()).filter(|a| !a.is_empty())),
        )?
    };
    if !done {
        return Err(ApiError::not_found("no such album"));
    }
    let _ = s.events.send(ApiEvent::LibraryChanged);
    Ok(Json(serde_json::json!({ "id": id })))
}

/// `DELETE /v1/library/tracks/{id}`: out of the library; the file stays.
pub(crate) async fn hide_track(
    State(s): State<Arc<NodeState>>,
    Extension(c): Extension<Caller>,
    Path(id): Path<i64>,
) -> Result<StatusCode, ApiError> {
    owner(&c)?;
    if !s
        .library
        .lock()
        .expect("library poisoned")
        .hide_track(TrackId(id))?
    {
        return Err(ApiError::not_found("no such song"));
    }
    let _ = s.events.send(ApiEvent::LibraryChanged);
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Clone, Serialize)]
pub struct HiddenFile {
    pub path: String,
    pub hidden_at: i64,
}

/// `GET /v1/library/hidden`.
pub(crate) async fn hidden(
    State(s): State<Arc<NodeState>>,
    Extension(c): Extension<Caller>,
) -> ApiResult<Vec<HiddenFile>> {
    owner(&c)?;
    let rows = s.library.lock().expect("library poisoned").hidden_files()?;
    Ok(Json(
        rows.into_iter()
            .map(|(path, hidden_at)| HiddenFile { path, hidden_at })
            .collect(),
    ))
}

#[derive(Debug, Clone, Deserialize)]
pub struct Restore {
    pub path: String,
}

/// `POST /v1/library/hidden/restore {path}`: back in; the folder holding it is read again.
pub(crate) async fn restore(
    State(s): State<Arc<NodeState>>,
    Extension(c): Extension<Caller>,
    Json(body): Json<Restore>,
) -> ApiResult<serde_json::Value> {
    owner(&c)?;
    if !s.library.lock().expect("library poisoned").unhide(&body.path)? {
        return Err(ApiError::not_found("That file was not taken out of the library"));
    }
    let file = std::path::Path::new(&body.path);
    let folder = s.all_folders().into_iter().find(|f| file.starts_with(f));
    let scan_started = match folder {
        Some(f) => s.start_scan(vec![f]).unwrap_or(false),
        None => false,
    };
    Ok(Json(
        serde_json::json!({ "restored": true, "scan_started": scan_started }),
    ))
}
