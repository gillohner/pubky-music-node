//! The web UI without the node: a server on `127.0.0.1` that serves the cached web UI and the
//! few routes its offline mode uses, from the device's store. `GET /v1/info` says
//! `offline: true`, and the UI then shows only what the device keeps (`GET /v1/offline`) and
//! plays it itself. Plays and hearts go into the queue for the next sync.

use crate::store::{now, Event, Store};
use axum::body::Body;
use axum::extract::{Path, Query, Request, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode, Uri};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncSeekExt};

fn err(status: StatusCode, msg: &str) -> Response {
    (status, Json(json!({ "error": msg }))).into_response()
}

/// The routes, over a store.
pub fn router(store: Arc<Store>) -> Router {
    Router::new()
        .route("/v1/info", get(info))
        .route("/v1/me", get(me))
        .route("/v1/offline", get(offline))
        .route("/v1/library/tracks/{id}", get(track))
        .route("/v1/library/tracks/{id}/file", get(file))
        .route("/v1/library/albums/{id}/cover", get(cover))
        .route("/v1/library/history", post(play))
        .route("/v1/favourites", get(favourites).put(heart).delete(unheart))
        .route("/v1/zones", get(|| async { Json(json!([])) }))
        .route("/v1/sources", get(|| async { Json(json!([])) }))
        .route(
            "/v1/{*rest}",
            any(|| async {
                err(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Offline: this needs your node. Your downloads still play.",
                )
            }),
        )
        .fallback(ui)
        .with_state(store)
}

/// Whether a request is for this server by its own name: `Host` is `127.0.0.1:<port>` or
/// `localhost:<port>`, and an `Origin`, when sent, is that same server. A page that rebinds its
/// own name to 127.0.0.1, or posts from another site, is refused.
fn is_local(headers: &HeaderMap, port: u16) -> bool {
    let ours = |host: &str| host == format!("127.0.0.1:{port}") || host == format!("localhost:{port}");
    let host_ok = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|h| ours(&h.to_ascii_lowercase()));
    let origin_ok = match headers.get(header::ORIGIN) {
        None => true,
        Some(o) => o
            .to_str()
            .ok()
            .and_then(|o| o.to_ascii_lowercase().strip_prefix("http://").map(str::to_string))
            .is_some_and(|h| ours(&h)),
    };
    host_ok && origin_ok
}

/// Serve on `127.0.0.1` (a free port). Returns the address and the server's task. Only requests
/// addressed to this server by its own name are answered (403 otherwise).
pub async fn serve(
    store: Arc<Store>,
) -> std::io::Result<(std::net::SocketAddr, tokio::task::JoinHandle<()>)> {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await?;
    let addr = listener.local_addr()?;
    let port = addr.port();
    let app = router(store).layer(axum::middleware::from_fn(
        move |req: Request, next: Next| async move {
            if is_local(req.headers(), port) {
                next.run(req).await
            } else {
                err(StatusCode::FORBIDDEN, "not for this address")
            }
        },
    ));
    let task = tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            tracing::warn!("offline server stopped: {e}");
        }
    });
    Ok((addr, task))
}

async fn info(State(s): State<Arc<Store>>) -> Json<Value> {
    let ix = s.index();
    let albums: BTreeSet<i64> = ix
        .tracks
        .values()
        .filter_map(|t| t.track.pointer("/album/id").and_then(Value::as_i64))
        .collect();
    let artists: BTreeSet<i64> = ix
        .tracks
        .values()
        .filter_map(|t| t.track.pointer("/artists/0/id").and_then(Value::as_i64))
        .collect();
    let duration: i64 = ix
        .tracks
        .values()
        .filter_map(|t| t.track.get("duration_ms").and_then(Value::as_i64))
        .sum();
    let node = ix.node.clone().unwrap_or_else(|| json!({}));
    Json(json!({
        "id": node.get("id").cloned().unwrap_or(Value::Null),
        "name": node.get("name").and_then(Value::as_str).unwrap_or("Pubky Music"),
        "version": node.get("version").cloned().unwrap_or(Value::Null),
        "api": "v1",
        "offline": true,
        "synced_at": ix.synced_at,
        "library": {
            "tracks": ix.tracks.len(),
            "albums": albums.len(),
            "artists": artists.len(),
            "files": ix.tracks.len(),
            "missing_files": 0,
            "total_duration_ms": duration,
        },
        "zones": 0,
        "outputs": 0,
        "scanning": false,
    }))
}

async fn me(State(s): State<Arc<Store>>) -> Json<Value> {
    Json(
        s.index()
            .me
            .unwrap_or_else(|| json!({"role": "owner", "zones": []})),
    )
}

/// A kept song's place on its album: disc, track number, song id.
type SongAt = (i64, i64, i64);

/// Everything the offline view shows, in one answer.
async fn offline(State(s): State<Arc<Store>>) -> Json<Value> {
    let ix = s.index();
    let kept: BTreeSet<i64> = ix.tracks.keys().copied().collect();
    // Albums from the kept songs, in track order
    let mut albums: BTreeMap<i64, (Value, Vec<SongAt>)> = BTreeMap::new();
    for (id, t) in &ix.tracks {
        if let Some(a) = t.track.get("album").filter(|a| a.get("id").is_some()) {
            let aid = a.get("id").and_then(Value::as_i64).unwrap_or_default();
            let disc = t.track.get("disc_no").and_then(Value::as_i64).unwrap_or(1);
            let no = t.track.get("track_no").and_then(Value::as_i64).unwrap_or(0);
            albums
                .entry(aid)
                .or_insert_with(|| (a.clone(), Vec::new()))
                .1
                .push((disc, no, *id));
        }
    }
    let mut albums: Vec<Value> = albums
        .into_iter()
        .map(|(aid, (a, mut songs))| {
            songs.sort();
            let mut a = a;
            a["track_ids"] = json!(songs.iter().map(|x| x.2).collect::<Vec<_>>());
            a["cover"] = json!(ix.covers.contains_key(&aid));
            a
        })
        .collect();
    albums.sort_by_key(|a| {
        a.get("title")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_lowercase()
    });
    let playlists: Vec<Value> = ix
        .playlists
        .iter()
        .map(|(id, p)| {
            let ids: Vec<i64> = p
                .get("tracks")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|t| t.get("id").and_then(Value::as_i64))
                        .collect()
                })
                .unwrap_or_default();
            let name = p
                .pointer("/summary/name")
                .or_else(|| p.get("name"))
                .cloned()
                .unwrap_or(Value::Null);
            json!({
                "id": id,
                "name": name,
                "total": ids.len(),
                "track_ids": ids.iter().filter(|i| kept.contains(i)).collect::<Vec<_>>(),
            })
        })
        .collect();
    let favourites: Vec<i64> = kept.iter().copied().filter(|i| ix.is_favourite(*i)).collect();
    let tracks: Vec<Value> = ix
        .tracks
        .values()
        .map(|t| {
            let mut v = t.track.clone();
            v["offline"] = json!({ "bytes": t.bytes, "format": t.format });
            v
        })
        .collect();
    Json(json!({
        "offline": true,
        "device_id": ix.device_id,
        "synced_at": ix.synced_at,
        "bytes": ix.bytes(),
        "queued": s.queued().len(),
        "tracks": tracks,
        "albums": albums,
        "playlists": playlists,
        "favourites": { "track_ids": favourites },
    }))
}

async fn track(State(s): State<Arc<Store>>, Path(id): Path<i64>) -> Response {
    match s.index().tracks.get(&id) {
        Some(t) => Json(t.track.clone()).into_response(),
        None => err(StatusCode::NOT_FOUND, "not downloaded to this device"),
    }
}

fn content_type(path: &std::path::Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()).unwrap_or_default() {
        "flac" => "audio/flac",
        "mp3" => "audio/mpeg",
        "m4a" => "audio/mp4",
        "ogg" | "opus" => "audio/ogg",
        "wav" => "audio/wav",
        "aiff" => "audio/aiff",
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "webp" => "image/webp",
        "html" => "text/html; charset=utf-8",
        "js" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "svg" => "image/svg+xml",
        "webmanifest" => "application/manifest+json",
        _ => "application/octet-stream",
    }
}

/// `bytes=a-b`, `bytes=a-` or `bytes=-n` against a length: the inclusive range, or `None` when
/// it can't be served.
fn parse_range(h: &str, len: u64) -> Option<(u64, u64)> {
    let spec = h.strip_prefix("bytes=")?;
    if spec.contains(',') || len == 0 {
        return None;
    }
    let (a, b) = spec.split_once('-')?;
    let (start, end) = if a.is_empty() {
        let n: u64 = b.parse().ok()?;
        (len.saturating_sub(n), len - 1)
    } else {
        let start: u64 = a.parse().ok()?;
        let end = if b.is_empty() {
            len - 1
        } else {
            b.parse::<u64>().ok()?.min(len - 1)
        };
        (start, end)
    };
    (start <= end && start < len).then_some((start, end))
}

/// A file, with ranges (the audio element seeks with them).
async fn send_file(path: &std::path::Path, headers: &HeaderMap) -> Response {
    let Ok(meta) = tokio::fs::metadata(path).await else {
        return err(StatusCode::NOT_FOUND, "not on this device");
    };
    let len = meta.len();
    let Ok(mut f) = tokio::fs::File::open(path).await else {
        return err(StatusCode::NOT_FOUND, "not on this device");
    };
    let range = headers
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .map(|h| parse_range(h, len));
    let (status, start, end) = match range {
        None => (StatusCode::OK, 0, len.saturating_sub(1)),
        Some(Some((a, b))) => (StatusCode::PARTIAL_CONTENT, a, b),
        Some(None) => {
            return (
                StatusCode::RANGE_NOT_SATISFIABLE,
                [(header::CONTENT_RANGE, format!("bytes */{len}"))],
            )
                .into_response()
        }
    };
    if start > 0 && f.seek(std::io::SeekFrom::Start(start)).await.is_err() {
        return err(StatusCode::INTERNAL_SERVER_ERROR, "the file can't be read");
    }
    let count = if len == 0 { 0 } else { end - start + 1 };
    let body = Body::from_stream(tokio_util::io::ReaderStream::new(f.take(count)));
    let mut r = Response::new(body);
    *r.status_mut() = status;
    let h = r.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type(path)));
    h.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    h.insert(header::CONTENT_LENGTH, HeaderValue::from(count));
    if status == StatusCode::PARTIAL_CONTENT {
        if let Ok(v) = HeaderValue::from_str(&format!("bytes {start}-{end}/{len}")) {
            h.insert(header::CONTENT_RANGE, v);
        }
    }
    r
}

async fn file(State(s): State<Arc<Store>>, Path(id): Path<i64>, headers: HeaderMap) -> Response {
    match s.track_file(id) {
        Some(p) => send_file(&p, &headers).await,
        None => err(StatusCode::NOT_FOUND, "not downloaded to this device"),
    }
}

async fn cover(State(s): State<Arc<Store>>, Path(id): Path<i64>, headers: HeaderMap) -> Response {
    match s.cover_file(id) {
        Some(p) => send_file(&p, &headers).await,
        None => err(StatusCode::NOT_FOUND, "no cover on this device"),
    }
}

#[derive(Deserialize)]
struct Played {
    track_id: i64,
}

async fn play(State(s): State<Arc<Store>>, Json(p): Json<Played>) -> Response {
    if !s.index().tracks.contains_key(&p.track_id) {
        return err(StatusCode::NOT_FOUND, "not downloaded to this device");
    }
    match s.push(Event::Play {
        track_id: p.track_id,
        at: now(),
    }) {
        Ok(()) => Json(json!({ "plays": [] })).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

async fn favourites(State(s): State<Arc<Store>>) -> Json<Value> {
    let ix = s.index();
    let ids: Vec<i64> = ix
        .tracks
        .keys()
        .copied()
        .filter(|i| ix.is_favourite(*i))
        .collect();
    let tracks: Vec<Value> = ids
        .iter()
        .filter_map(|i| ix.tracks.get(i))
        .map(|t| t.track.clone())
        .collect();
    let uris: Vec<String> = ids.iter().map(|i| format!("track:{i}")).collect();
    Json(json!({"tracks": tracks, "albums": [], "artists": [], "other": [], "uris": uris}))
}

#[derive(Deserialize)]
struct Heart {
    track_id: Option<i64>,
}

async fn set_heart(s: &Store, id: Option<i64>, on: bool) -> Response {
    let Some(id) = id else {
        return err(
            StatusCode::SERVICE_UNAVAILABLE,
            "Offline: only downloaded songs can be hearted until your node is back.",
        );
    };
    if !s.index().tracks.contains_key(&id) {
        return err(StatusCode::NOT_FOUND, "not downloaded to this device");
    }
    match s.push(Event::Favourite {
        track_id: id,
        on,
        at: now(),
    }) {
        Ok(()) => Json(json!({"uri": format!("track:{id}"), "favourite": on})).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

async fn heart(State(s): State<Arc<Store>>, Json(h): Json<Heart>) -> Response {
    set_heart(&s, h.track_id, true).await
}

async fn unheart(State(s): State<Arc<Store>>, Query(h): Query<Heart>) -> Response {
    set_heart(&s, h.track_id, false).await
}

/// The cached web UI: its files by name, every other path (the UI's own routes) the page. Only
/// the known names are served; nothing from the path is joined to a folder.
async fn ui(State(s): State<Arc<Store>>, uri: Uri) -> Response {
    let asked = uri.path().trim_start_matches('/');
    let name = crate::sync::UI_FILES
        .iter()
        .map(|(_, f)| *f)
        .find(|f| *f == asked)
        .unwrap_or("index.html");
    let dir = s.ui_dir();
    let file = dir.join(if dir.join(name).is_file() {
        name
    } else {
        "index.html"
    });
    if !file.is_file() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
            "<!doctype html><meta charset=utf-8><title>Offline</title><p>Offline, and nothing is downloaded yet. Connect to your node once to keep music on this device.</p>",
        )
            .into_response();
    }
    send_file(&file, &HeaderMap::new()).await
}

#[cfg(test)]
mod tests {
    use super::{is_local, parse_range};
    use axum::http::{header, HeaderMap};

    #[test]
    fn only_requests_for_this_server() {
        let h = |host: &str, origin: Option<&str>| {
            let mut m = HeaderMap::new();
            m.insert(header::HOST, host.parse().unwrap());
            if let Some(o) = origin {
                m.insert(header::ORIGIN, o.parse().unwrap());
            }
            m
        };
        assert!(is_local(&h("127.0.0.1:4000", None), 4000));
        assert!(is_local(&h("localhost:4000", None), 4000));
        assert!(is_local(
            &h("127.0.0.1:4000", Some("http://127.0.0.1:4000")),
            4000
        ));
        // DNS rebinding: the page's own name
        assert!(!is_local(&h("evil.example:4000", None), 4000));
        assert!(!is_local(&h("127.0.0.1:4001", None), 4000));
        assert!(!is_local(&h("127.0.0.1", None), 4000));
        // Another site posting to the server
        assert!(!is_local(
            &h("127.0.0.1:4000", Some("https://evil.example")),
            4000
        ));
        assert!(!is_local(&h("127.0.0.1:4000", Some("null")), 4000));
        assert!(!is_local(&HeaderMap::new(), 4000));
    }

    #[test]
    fn ranges() {
        assert_eq!(parse_range("bytes=0-", 10), Some((0, 9)));
        assert_eq!(parse_range("bytes=2-4", 10), Some((2, 4)));
        assert_eq!(parse_range("bytes=-3", 10), Some((7, 9)));
        assert_eq!(parse_range("bytes=5-100", 10), Some((5, 9)));
        assert_eq!(parse_range("bytes=10-", 10), None);
        assert_eq!(parse_range("bytes=0-1,3-4", 10), None);
        assert_eq!(parse_range("items=0-1", 10), None);
    }
}
