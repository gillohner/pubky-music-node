//! The node's native API (handoff §9): HTTP + WebSocket under `/v1`.
//!
//! Resources: `info`, `outputs`, `zones` (queue, transport, volume, repeat),
//! `library` (stats, tracks, albums, search, scan) and the event stream at
//! `/v1/events`. Bodies are in [`types`]; the same JSON shapes drive the
//! controller. OpenSubsonic (M4) will be a second router next to this one.
//!
//! Auth: an optional bearer token (`NodeState::token`). Without one the API
//! trusts whoever can reach the socket, so bind to localhost. Remote access
//! by pubkey (M5) wraps this API rather than replacing it.

pub mod sources;
pub mod state;
pub mod types;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{middleware, Json, Router};
use futures_util::{SinkExt, StreamExt};
use pm_library::{Library, Scanner};
use pm_model::TrackId;
use pm_zones::{Insert, OutputId, OutputInfo, QueueItem, Zone, ZoneId, ZoneState};
use sources::NodeSources;
pub use state::{NodeSettings, NodeState};
use std::sync::Arc;
use tokio::sync::broadcast;
use types::*;

// ---- errors ----

#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub message: String,
}

impl ApiError {
    fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }
    fn not_found(what: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, what)
    }
    fn bad_request(what: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, what)
    }
}

impl From<pm_library::LibraryError> for ApiError {
    fn from(e: pm_library::LibraryError) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
    }
}

impl From<pm_zones::SinkError> for ApiError {
    fn from(e: pm_zones::SinkError) -> Self {
        Self::new(StatusCode::BAD_REQUEST, e.to_string())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(ErrorBody { error: self.message })).into_response()
    }
}

type ApiResult<T> = Result<Json<T>, ApiError>;

// ---- router ----

pub fn router(state: Arc<NodeState>) -> Router {
    let v1 = Router::new()
        .route("/info", get(info))
        .route("/outputs", get(outputs))
        .route("/zones", get(zones).post(create_zone))
        .route("/zones/{id}", get(zone).delete(delete_zone))
        .route("/zones/{id}/queue", get(queue).post(enqueue).delete(clear_queue))
        .route(
            "/zones/{id}/queue/{item}",
            axum::routing::delete(remove_item).put(move_item),
        )
        .route("/zones/{id}/transport", post(transport))
        .route("/zones/{id}/volume", post(volume))
        .route("/zones/{id}/repeat", axum::routing::put(repeat))
        .route("/library/stats", get(library_stats))
        .route("/library/tracks", get(tracks))
        .route("/library/tracks/{id}", get(track))
        .route("/library/tracks/{id}/file", get(track_file))
        .route("/library/search", get(search))
        .route("/library/albums", get(albums))
        .route("/library/albums/{id}/tracks", get(album_tracks))
        .route("/library/folders", get(folders))
        .route("/library/scan", post(scan))
        .route("/events", get(events_ws))
        .route_layer(middleware::from_fn_with_state(state.clone(), auth))
        .with_state(state);
    Router::new().nest("/v1", v1)
}

/// Bind and serve until the task is cancelled.
pub async fn serve(state: Arc<NodeState>, listen: &str) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(listen).await?;
    tracing::info!(%listen, "API listening");
    serve_listener(state, listener).await
}

/// Serve on an already bound listener (tests bind port 0).
pub async fn serve_listener(state: Arc<NodeState>, listener: tokio::net::TcpListener) -> anyhow::Result<()> {
    axum::serve(listener, router(state)).await?;
    Ok(())
}

async fn auth(
    State(state): State<Arc<NodeState>>,
    headers: HeaderMap,
    query: Query<std::collections::HashMap<String, String>>,
    req: axum::extract::Request,
    next: middleware::Next,
) -> Response {
    if let Some(token) = &state.token {
        let bearer = headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .map(str::to_string)
            // Browsers cannot set headers on WebSocket upgrades: allow `?token=`.
            .or_else(|| query.get("token").cloned());
        if bearer.as_deref() != Some(token.as_str()) {
            return ApiError::new(StatusCode::UNAUTHORIZED, "missing or wrong bearer token").into_response();
        }
    }
    next.run(req).await
}

// ---- handlers: node ----

async fn info(State(s): State<Arc<NodeState>>) -> ApiResult<Info> {
    let library = s.library.lock().expect("library poisoned").stats()?;
    Ok(Json(Info {
        name: s.name.clone(),
        version: s.version.clone(),
        api: API_VERSION.into(),
        library,
        zones: s.zones.list().len(),
        outputs: s.outputs.len(),
    }))
}

async fn outputs(State(s): State<Arc<NodeState>>) -> Json<Vec<OutputInfo>> {
    Json(s.outputs.list())
}

// ---- handlers: zones ----

fn zone_of(s: &NodeState, id: &str) -> Result<Arc<Zone>, ApiError> {
    s.zones
        .get(&ZoneId::new(id))
        .ok_or_else(|| ApiError::not_found(format!("no zone {id}")))
}

async fn zones(State(s): State<Arc<NodeState>>) -> Json<Vec<ZoneState>> {
    Json(s.zones.list())
}

async fn create_zone(State(s): State<Arc<NodeState>>, Json(body): Json<CreateZone>) -> ApiResult<ZoneState> {
    let output = s
        .outputs
        .get(&OutputId(body.output.clone()))
        .ok_or_else(|| ApiError::not_found(format!("no output {}", body.output)))?;
    if body.name.trim().is_empty() {
        return Err(ApiError::bad_request("zone name is empty"));
    }
    let zone = s.create_zone(body.name.trim(), output);
    Ok(Json(zone.state()))
}

async fn zone(State(s): State<Arc<NodeState>>, Path(id): Path<String>) -> ApiResult<ZoneState> {
    Ok(Json(zone_of(&s, &id)?.state()))
}

async fn delete_zone(
    State(s): State<Arc<NodeState>>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let zone = zone_of(&s, &id)?;
    zone.stop();
    s.remove_zone(zone.id());
    Ok(StatusCode::NO_CONTENT)
}

async fn queue(State(s): State<Arc<NodeState>>, Path(id): Path<String>) -> ApiResult<Vec<QueueItem>> {
    Ok(Json(zone_of(&s, &id)?.queue()))
}

async fn enqueue(
    State(s): State<Arc<NodeState>>,
    Path(id): Path<String>,
    Json(body): Json<Enqueue>,
) -> ApiResult<Enqueued> {
    let zone = zone_of(&s, &id)?;
    let mut items = Vec::with_capacity(body.track_ids.len() + body.items.len());
    {
        let lib = s.library.lock().expect("library poisoned");
        for tid in &body.track_ids {
            let track = lib
                .get_track(TrackId(*tid))?
                .ok_or_else(|| ApiError::not_found(format!("no track {tid}")))?;
            if let Some(item) = NodeSources::item_for(&track) {
                items.push(item);
            }
        }
    }
    for it in body.items {
        if it.uri.trim().is_empty() {
            return Err(ApiError::bad_request("item uri is empty"));
        }
        items.push(it);
    }
    if items.is_empty() {
        return Err(ApiError::bad_request("nothing to enqueue"));
    }
    if body.replace {
        zone.stop();
        zone.clear();
    }
    let at = match body.at {
        QueueAt::End => Insert::End,
        QueueAt::Next => Insert::Next,
    };
    let ids = zone.enqueue(items, at);
    if body.play {
        if body.replace {
            zone.play_index(0);
        } else {
            zone.play();
        }
    }
    Ok(Json(Enqueued { ids }))
}

async fn clear_queue(
    State(s): State<Arc<NodeState>>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let zone = zone_of(&s, &id)?;
    zone.stop();
    zone.clear();
    Ok(StatusCode::NO_CONTENT)
}

async fn remove_item(
    State(s): State<Arc<NodeState>>,
    Path((id, item)): Path<(String, u64)>,
) -> Result<StatusCode, ApiError> {
    if zone_of(&s, &id)?.remove(item) {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::not_found(format!("no queue item {item}")))
    }
}

async fn move_item(
    State(s): State<Arc<NodeState>>,
    Path((id, item)): Path<(String, u64)>,
    Json(body): Json<MoveItem>,
) -> Result<StatusCode, ApiError> {
    if zone_of(&s, &id)?.move_item(item, body.to_index) {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::not_found(format!("no queue item {item}")))
    }
}

async fn transport(
    State(s): State<Arc<NodeState>>,
    Path(id): Path<String>,
    Json(body): Json<TransportAction>,
) -> ApiResult<ZoneState> {
    let zone = zone_of(&s, &id)?;
    match body {
        TransportAction::Play { index: Some(i) } => zone.play_index(i),
        TransportAction::Play { index: None } => zone.play(),
        TransportAction::Pause => zone.pause(),
        TransportAction::Toggle => zone.toggle_pause(),
        TransportAction::Stop => zone.stop(),
        TransportAction::Next => zone.next(),
        TransportAction::Previous => zone.previous(),
        TransportAction::Seek { position_ms } => zone.seek_ms(position_ms),
    }
    // The engine publishes within one block; give it a moment so the reply reflects the command.
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    Ok(Json(zone.state()))
}

async fn volume(
    State(s): State<Arc<NodeState>>,
    Path(id): Path<String>,
    Json(body): Json<SetVolume>,
) -> Result<StatusCode, ApiError> {
    zone_of(&s, &id)?.set_volume(body.volume).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn repeat(
    State(s): State<Arc<NodeState>>,
    Path(id): Path<String>,
    Json(body): Json<SetRepeat>,
) -> Result<StatusCode, ApiError> {
    zone_of(&s, &id)?.set_repeat(body.repeat);
    Ok(StatusCode::NO_CONTENT)
}

// ---- handlers: library ----

async fn library_stats(State(s): State<Arc<NodeState>>) -> ApiResult<pm_library::LibraryStats> {
    Ok(Json(s.library.lock().expect("library poisoned").stats()?))
}

async fn tracks(State(s): State<Arc<NodeState>>, Query(p): Query<Page>) -> ApiResult<Tracks> {
    let limit = p.limit.unwrap_or(100).min(1000);
    let tracks = s
        .library
        .lock()
        .expect("library poisoned")
        .list_tracks(p.after.map(TrackId), limit)?;
    Ok(Json(Tracks { tracks }))
}

async fn track(State(s): State<Arc<NodeState>>, Path(id): Path<i64>) -> ApiResult<pm_model::Track> {
    let t = s
        .library
        .lock()
        .expect("library poisoned")
        .get_track(TrackId(id))?
        .ok_or_else(|| ApiError::not_found(format!("no track {id}")))?;
    Ok(Json(t))
}

/// The track's audio file as stored, for a controller that plays locally
/// (`pm-app`). Served only to authenticated callers of this node; never a
/// public URL (constraint 2).
async fn track_file(State(s): State<Arc<NodeState>>, Path(id): Path<i64>) -> Result<Response, ApiError> {
    let t = s
        .library
        .lock()
        .expect("library poisoned")
        .get_track(TrackId(id))?
        .ok_or_else(|| ApiError::not_found(format!("no track {id}")))?;
    let bytes = tokio::fs::read(&t.file.path)
        .await
        .map_err(|e| ApiError::not_found(format!("file for track {id}: {e}")))?;
    let mime = match t.file.container.as_str() {
        "flac" => "audio/flac",
        "wav" => "audio/wav",
        "aiff" => "audio/aiff",
        "isomp4" | "mp4" | "m4a" => "audio/mp4",
        "dsf" => "audio/x-dsf",
        "dff" => "audio/x-dff",
        _ => "application/octet-stream",
    };
    let name = t
        .file
        .path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| format!("track-{id}"));
    Ok((
        [
            (header::CONTENT_TYPE, mime.to_string()),
            (
                header::CONTENT_DISPOSITION,
                format!("inline; filename=\"{}\"", name.replace('"', "")),
            ),
        ],
        bytes,
    )
        .into_response())
}

async fn search(State(s): State<Arc<NodeState>>, Query(q): Query<SearchQuery>) -> ApiResult<Tracks> {
    let limit = q.limit.unwrap_or(50).min(500);
    let tracks = s
        .library
        .lock()
        .expect("library poisoned")
        .search_tracks(&q.q, limit)?;
    Ok(Json(Tracks { tracks }))
}

async fn albums(State(s): State<Arc<NodeState>>, Query(p): Query<Page>) -> ApiResult<Vec<pm_model::Album>> {
    let limit = p.limit.unwrap_or(100).min(1000);
    Ok(Json(
        s.library
            .lock()
            .expect("library poisoned")
            .list_albums(p.after, limit)?,
    ))
}

async fn album_tracks(State(s): State<Arc<NodeState>>, Path(id): Path<i64>) -> ApiResult<Tracks> {
    let tracks = s.library.lock().expect("library poisoned").album_tracks(id)?;
    Ok(Json(Tracks { tracks }))
}

async fn folders(State(s): State<Arc<NodeState>>) -> ApiResult<Vec<(String, bool)>> {
    let f = s
        .library
        .lock()
        .expect("library poisoned")
        .list_folders()?
        .into_iter()
        .map(|(p, present)| (p.to_string_lossy().into_owned(), present))
        .collect();
    Ok(Json(f))
}

/// Start a scan in the background on its own SQLite connection; one at a time.
async fn scan(State(s): State<Arc<NodeState>>, Json(body): Json<ScanRequest>) -> ApiResult<ScanStarted> {
    let folders: Vec<std::path::PathBuf> = if body.folders.is_empty() {
        s.scan_folders.clone()
    } else {
        body.folders.iter().map(std::path::PathBuf::from).collect()
    };
    if folders.is_empty() {
        return Err(ApiError::bad_request("no folders to scan"));
    }
    if !s.begin_scan() {
        return Err(ApiError::new(StatusCode::CONFLICT, "a scan is already running"));
    }
    let state = s.clone();
    let list = folders.clone();
    tokio::task::spawn_blocking(move || {
        let result = (|| -> Result<(), pm_library::LibraryError> {
            let lib = Library::open(&state.db_path)?;
            let scanner = Scanner::new(&lib);
            for root in &list {
                lib.add_folder(root)?;
                match scanner.scan(root) {
                    Ok(r) => {
                        tracing::info!(root = %root.display(), added = r.files_added, updated = r.files_updated, removed = r.files_removed, errors = r.errors.len(), "scan done")
                    }
                    Err(e) => tracing::warn!(root = %root.display(), "scan failed: {e}"),
                }
            }
            Ok(())
        })();
        if let Err(e) = result {
            tracing::error!("scan: {e}");
        }
        state.end_scan();
    });
    Ok(Json(ScanStarted {
        folders: folders.iter().map(|p| p.to_string_lossy().into_owned()).collect(),
    }))
}

// ---- events ----

async fn events_ws(State(s): State<Arc<NodeState>>, ws: WebSocketUpgrade) -> Response {
    ws.on_upgrade(move |socket| event_stream(socket, s))
}

async fn event_stream(socket: WebSocket, s: Arc<NodeState>) {
    let (mut tx, mut rx) = socket.split();
    let mut events = s.events.subscribe();
    let snapshot = ApiEvent::Snapshot {
        zones: s.zones.list(),
    };
    if send_json(&mut tx, &snapshot).await.is_err() {
        return;
    }
    loop {
        tokio::select! {
            ev = events.recv() => match ev {
                Ok(ev) => {
                    if send_json(&mut tx, &ev).await.is_err() {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::debug!(n, "event stream lagged; resending snapshot");
                    let snap = ApiEvent::Snapshot { zones: s.zones.list() };
                    if send_json(&mut tx, &snap).await.is_err() {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Closed) => break,
            },
            msg = rx.next() => match msg {
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                Some(Ok(Message::Ping(p))) => {
                    if tx.send(Message::Pong(p)).await.is_err() {
                        break;
                    }
                }
                Some(Ok(_)) => {}
            },
        }
    }
}

async fn send_json<T: serde::Serialize>(
    tx: &mut futures_util::stream::SplitSink<WebSocket, Message>,
    v: &T,
) -> Result<(), axum::Error> {
    let text = serde_json::to_string(v).expect("event serializes");
    tx.send(Message::Text(text.into())).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use pm_audio::PipelineConfig;
    use pm_zones::outputs::NullOutput;
    use pm_zones::OutputRegistry;
    use tower::ServiceExt;

    fn write_wav(path: &std::path::Path, frames: u32) {
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: 44_100,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut w = hound::WavWriter::create(path, spec).unwrap();
        for i in 0..frames {
            w.write_sample((i % 100) as i16).unwrap();
            w.write_sample(-((i % 100) as i16)).unwrap();
        }
        w.finalize().unwrap();
    }

    async fn state(dir: &std::path::Path, token: Option<&str>) -> Arc<NodeState> {
        let db = dir.join("lib.sqlite");
        let lib = Library::open(&db).unwrap();
        let outputs = OutputRegistry::new();
        outputs.register(Arc::new(NullOutput::new()));
        let s = NodeState::new(
            NodeSettings {
                name: "test".into(),
                version: "0".into(),
                db_path: db,
                scan_folders: vec![dir.join("music")],
                pipeline: PipelineConfig::default(),
                token: token.map(str::to_string),
            },
            lib,
            outputs,
        );
        s.zones_for_all_outputs();
        s
    }

    async fn call(
        app: &Router,
        method: &str,
        uri: &str,
        body: Option<serde_json::Value>,
    ) -> (StatusCode, serde_json::Value) {
        let req = Request::builder()
            .method(method)
            .uri(uri)
            .header("content-type", "application/json")
            .body(match body {
                Some(b) => Body::from(b.to_string()),
                None => Body::empty(),
            })
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let json = if bytes.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap()
        };
        (status, json)
    }

    #[tokio::test]
    async fn scan_then_queue_and_play_a_library_track() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("music")).unwrap();
        write_wav(&dir.path().join("music/one.wav"), 4410);
        let s = state(dir.path(), None).await;
        let app = router(s.clone());

        let (st, info) = call(&app, "GET", "/v1/info", None).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(info["api"], "v1");
        assert_eq!(info["zones"], 1);

        let (st, _) = call(&app, "POST", "/v1/library/scan", Some(serde_json::json!({}))).await;
        assert_eq!(st, StatusCode::OK);
        for _ in 0..500 {
            if !s.is_scanning() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(!s.is_scanning());
        let (st, tracks) = call(&app, "GET", "/v1/library/tracks?limit=10", None).await;
        assert_eq!(st, StatusCode::OK);
        let id = tracks["tracks"][0]["id"].as_i64().unwrap();
        let (st, found) = call(&app, "GET", "/v1/library/search?q=one", None).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(found["tracks"][0]["id"], id);

        let (st, zones) = call(&app, "GET", "/v1/zones", None).await;
        assert_eq!(st, StatusCode::OK);
        let zid = zones[0]["id"].as_str().unwrap().to_string();
        let (st, q) = call(
            &app,
            "POST",
            &format!("/v1/zones/{zid}/queue"),
            Some(serde_json::json!({"track_ids": [id], "replace": true, "play": true})),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{q}");
        assert_eq!(q["ids"].as_array().unwrap().len(), 1);
        let zone = s.zones.get(&ZoneId::new(&zid)).unwrap();
        let st = zone.wait_stopped().await;
        assert_eq!(st.error, None);
        let (st, q) = call(&app, "GET", &format!("/v1/zones/{zid}/queue"), None).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(q[0]["uri"], format!("track:{id}"));
        assert_eq!(q[0]["title"], "one");

        // Unknown track and unknown zone are 404s.
        let (st, _) = call(
            &app,
            "POST",
            &format!("/v1/zones/{zid}/queue"),
            Some(serde_json::json!({"track_ids": [999]})),
        )
        .await;
        assert_eq!(st, StatusCode::NOT_FOUND);
        let (st, _) = call(&app, "GET", "/v1/zones/nope", None).await;
        assert_eq!(st, StatusCode::NOT_FOUND);
        // Transport on an empty-after-clear zone is accepted and reports state.
        let (st, _) = call(&app, "DELETE", &format!("/v1/zones/{zid}/queue"), None).await;
        assert_eq!(st, StatusCode::NO_CONTENT);
        let (st, z) = call(
            &app,
            "POST",
            &format!("/v1/zones/{zid}/transport"),
            Some(serde_json::json!({"action": "play"})),
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(z["transport"], "stopped");
    }

    #[tokio::test]
    async fn bearer_token_guards_every_route() {
        let dir = tempfile::tempdir().unwrap();
        let s = state(dir.path(), Some("s3cret")).await;
        let app = router(s);
        let (st, _) = call(&app, "GET", "/v1/info", None).await;
        assert_eq!(st, StatusCode::UNAUTHORIZED);
        let req = Request::builder()
            .uri("/v1/info")
            .header("authorization", "Bearer s3cret")
            .body(Body::empty())
            .unwrap();
        assert_eq!(app.clone().oneshot(req).await.unwrap().status(), StatusCode::OK);
        let (st, _) = call(&app, "GET", "/v1/info?token=s3cret", None).await;
        assert_eq!(st, StatusCode::OK);
    }

    #[tokio::test]
    async fn zones_can_be_created_and_removed() {
        let dir = tempfile::tempdir().unwrap();
        let s = state(dir.path(), None).await;
        let app = router(s.clone());
        let mut events = s.events.subscribe();
        let (st, z) = call(
            &app,
            "POST",
            "/v1/zones",
            Some(serde_json::json!({"name": "Kitchen", "output": "null"})),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{z}");
        assert_eq!(z["id"], "kitchen");
        let (st, _) = call(
            &app,
            "POST",
            "/v1/zones",
            Some(serde_json::json!({"name": "X", "output": "nope"})),
        )
        .await;
        assert_eq!(st, StatusCode::NOT_FOUND);
        let (st, _) = call(&app, "DELETE", "/v1/zones/kitchen", None).await;
        assert_eq!(st, StatusCode::NO_CONTENT);
        let mut removed = false;
        while let Ok(ev) = events.try_recv() {
            if matches!(ev, ApiEvent::ZoneRemoved { ref zone } if zone.0 == "kitchen") {
                removed = true;
            }
        }
        assert!(removed);
        assert_eq!(s.zones.list().len(), 1);
    }
}
