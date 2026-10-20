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

pub mod activity;
pub mod archive;
pub mod art;
pub mod artist_info;
pub mod audius;
pub mod deezer;
pub mod discover;
pub mod favourites;
pub mod files;
pub mod hls;
pub mod new_releases;
pub mod playlists;
pub mod radio;
pub mod reviews;
pub mod sources;
pub mod state;
pub mod streaming;
pub mod subsonic;
pub mod sync;
pub mod tags;
pub mod tidal;
pub mod types;
pub mod watch;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{middleware, Json, Router};
use futures_util::{SinkExt, StreamExt};
use pm_model::TrackId;
use pm_zones::{Insert, Output, OutputId, OutputInfo, QueueItem, Zone, ZoneId, ZoneState};
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
    pub(crate) fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }
    pub(crate) fn not_found(what: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, what)
    }
    pub(crate) fn bad_request(what: impl Into<String>) -> Self {
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

pub(crate) type ApiResult<T> = Result<Json<T>, ApiError>;

// ---- router ----

pub fn router(state: Arc<NodeState>) -> Router {
    let v1 = Router::new()
        .route("/info", get(info))
        .route("/node/settings", get(node_settings).put(update_node_settings))
        .route("/outputs", get(outputs))
        .route("/outputs/rescan", post(rescan_outputs))
        .route("/outputs/discovery", get(discovery_report))
        .route("/outputs/upnp", post(add_upnp_output))
        .route("/outputs/upnp/{address}", delete(remove_upnp_address))
        .route("/outputs/pull", post(create_pull))
        .route("/outputs/pull/{name}/stream.wav", get(pull_stream))
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
        .route("/zones/{id}/radio", axum::routing::put(radio))
        .route("/zones/{id}/shuffle", post(shuffle_queue))
        .route("/zones/{id}/transfer", post(transfer))
        .route("/zones/{id}/sleep", get(sleep_get).put(sleep_set))
        .route("/zones/{id}/dsp", get(dsp_get).put(dsp_put).delete(dsp_reset))
        .route("/library/stats", get(library_stats))
        .route("/library/tracks", get(tracks))
        .route("/library/tracks/{id}", get(track))
        .route("/library/tracks/{id}/lyrics", get(lyrics))
        .route("/library/tracks/{id}/versions", get(track_versions))
        .route("/library/albums/{id}/versions", get(album_versions))
        .route("/backup", get(backup))
        .route("/library/tracks/{id}/file", get(track_file))
        .route("/library/search", get(search))
        .route("/library/artists", get(artists))
        .route("/library/artists/{id}", get(artist))
        .route("/library/artists/{id}/tracks", get(artist_tracks))
        .route("/library/artists/{id}/info", get(artist_info_handler))
        .route("/library/artists/{id}/image", get(artist_image))
        .route("/library/genres", get(genres))
        .route("/library/genres/{id}", get(genre))
        .route("/library/albums", get(albums))
        .route("/library/albums/formats", get(album_formats))
        .route("/library/new-releases", get(new_releases))
        .route("/library/new-releases/refresh", post(new_releases_refresh))
        .route("/library/albums/{id}/tracks", get(album_tracks))
        .route("/library/albums/{id}/cover", get(album_cover))
        .route("/library/history", get(history).post(record_play))
        .route("/library/home", get(home))
        .route(
            "/library/folders",
            get(folders).post(add_folder).delete(remove_folder),
        )
        .route("/library/scan", post(scan))
        .route("/playlists", get(playlists::list).post(playlists::create))
        .route("/playlists/from-queue", post(playlists::from_queue))
        .route("/playlists/import", post(activity::import_playlist))
        .route("/playlists/import/file", post(activity::import_playlist_file))
        .route("/activity", get(activity::list).post(activity::record))
        .route(
            "/favourites",
            get(favourites::list)
                .put(favourites::put)
                .delete(favourites::delete),
        )
        .route("/tags", get(tags::list).put(tags::put).delete(tags::delete))
        .route(
            "/reviews",
            get(reviews::list).put(reviews::put).delete(reviews::delete),
        )
        .route(
            "/playlists/{id}",
            get(playlists::get)
                .put(playlists::update)
                .delete(playlists::delete),
        )
        .route("/playlists/{id}/items", post(playlists::add_items))
        .route(
            "/playlists/{id}/items/{index}",
            axum::routing::delete(playlists::remove_item),
        )
        .route("/playlists/{id}/play", post(playlists::play))
        .route("/discover/trends", get(discover::trends))
        .route("/discover/uri", get(discover::on_pubky))
        .route("/discover/tag/{label}", get(discover::by_tag))
        .route("/discover/friends/{viewer}", get(discover::friends))
        .route("/sources", get(streaming::list))
        .route("/sources/{source}/search", get(streaming::search))
        .route("/sources/{source}/items/{id}", get(streaming::item))
        .route("/sources/{source}/artists/{id}", get(streaming::artist))
        .route("/sources/tidal/login", get(tidal::login))
        .route("/sources/tidal/status", get(tidal::status))
        .route("/sources/tidal/session", axum::routing::delete(tidal::logout))
        .route("/sources/tidal/collection", get(tidal::collection))
        .route("/sources/tidal/stream/{id}", get(tidal::stream))
        .route("/sources/deezer/stream/{id}", get(deezer::stream))
        .route("/sources/tidal/collection/import", post(tidal::import_collection))
        .route("/sources/tidal/playlists", get(tidal::playlists))
        .route(
            "/sources/tidal/playlists/{id}/import",
            post(tidal::import_playlist),
        )
        .route("/radio/search", get(radio::search))
        .route("/radio/stations", get(radio::saved).put(radio::save))
        .route("/radio/stations/{id}", axum::routing::delete(radio::remove))
        .route("/radio/play", post(radio::play))
        .route("/pubky", get(pubky_status))
        .route("/pubky/sync", post(pubky_sync))
        .route("/pubky/login", post(pubky_login_start).get(pubky_login_poll))
        .route("/pubky/files", get(files::list).post(files::upload))
        .route("/pubky/files/mirror", post(files::mirror))
        .route("/events", get(events_ws))
        .route_layer(middleware::from_fn_with_state(state.clone(), auth))
        .with_state(state.clone());
    Router::new()
        .route("/", get(web_ui))
        .route("/app", get(web_ui))
        .route("/app/", get(web_ui))
        .route("/app.css", get(web_css))
        .route("/app.js", get(web_js))
        .route("/manifest.webmanifest", get(web_manifest))
        .route("/icon.svg", get(web_icon))
        .nest("/v1", v1)
        // OAuth redirect target: the browser arrives without a bearer token.
        .merge(
            Router::new()
                .route("/tidal/callback", get(tidal::callback))
                .with_state(state.clone()),
        )
        .merge(subsonic::router(state))
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

// ---- web UI ----

/// The built-in browser controller: one static page over `/v1`. It asks for
/// the token itself, so the page is served without auth.
async fn web_css() -> Response {
    (
        [
            (header::CONTENT_TYPE, "text/css; charset=utf-8"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        include_str!("../static/app.css"),
    )
        .into_response()
}

async fn web_js() -> Response {
    (
        [
            (header::CONTENT_TYPE, "text/javascript; charset=utf-8"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        include_str!("../static/app.js"),
    )
        .into_response()
}

async fn web_manifest() -> Response {
    (
        [
            (header::CONTENT_TYPE, "application/manifest+json"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        include_str!("../static/manifest.webmanifest"),
    )
        .into_response()
}

async fn web_icon() -> Response {
    (
        [
            (header::CONTENT_TYPE, "image/svg+xml"),
            (header::CACHE_CONTROL, "public, max-age=86400"),
        ],
        include_str!("../static/icon.svg"),
    )
        .into_response()
}

async fn web_ui() -> Response {
    (
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        include_str!("../static/index.html"),
    )
        .into_response()
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
        scanning: s.is_scanning(),
    }))
}

async fn outputs(State(s): State<Arc<NodeState>>) -> Json<Vec<OutputInfo>> {
    Json(s.outputs.list())
}

/// Create a pull output: a zone that plays to it streams WAV at
/// `/v1/outputs/pull/{name}/stream.wav` for any renderer or browser to fetch.
async fn create_pull(State(s): State<Arc<NodeState>>, Json(body): Json<CreatePull>) -> ApiResult<OutputInfo> {
    let name = body.name.trim();
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(ApiError::bad_request("name must be letters, digits, '-' or '_'"));
    }
    let out = Arc::new(pm_zones::outputs::PullOutput::new(name));
    if s.outputs.get(&out.id()).is_some() {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            format!("pull output {name} exists"),
        ));
    }
    let info = out.info();
    s.pulls
        .lock()
        .expect("pulls lock")
        .insert(name.to_string(), out.clone());
    s.outputs.register(out.clone());
    s.create_zone(&format!("Stream {name}"), out);
    Ok(Json(info))
}

/// The live WAV stream of a pull output. Waits up to 15 s for a zone to
/// start playing into it; a second listener replaces the first.
async fn pull_stream(
    State(s): State<Arc<NodeState>>,
    Path(name): Path<String>,
) -> Result<Response, ApiError> {
    let out = s
        .pulls
        .lock()
        .expect("pulls lock")
        .get(&name)
        .cloned()
        .ok_or_else(|| ApiError::not_found(format!("no pull output {name}")))?;
    let Some(live) = out.take_stream(pm_zones::outputs::LISTENER_TIMEOUT).await else {
        return Err(ApiError::new(
            StatusCode::REQUEST_TIMEOUT,
            "no zone is playing into this stream; start playback to the Stream zone first",
        ));
    };
    let stream = tokio_stream::wrappers::ReceiverStream::new(live.rx).map(Ok::<_, std::convert::Infallible>);
    Ok((
        [
            (header::CONTENT_TYPE, "audio/wav".to_string()),
            (header::CACHE_CONTROL, "no-store".to_string()),
            (header::ACCEPT_RANGES, "none".to_string()),
        ],
        axum::body::Body::from_stream(stream),
    )
        .into_response())
}

/// Discover outputs again (local devices, endpoints over mDNS and configured
/// addresses); new ones get a zone. Returns the full list.
/// `GET /v1/outputs/discovery`: scan the network and explain what was found,
/// for setups where a renderer does not show up (Docker bridge, VLANs).
async fn discovery_report(
    State(s): State<Arc<NodeState>>,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> ApiResult<DiscoveryReport> {
    let st = s.clone();
    let quick = q.get("quick").is_some_and(|v| v == "1");
    let (upnp, cast) = tokio::task::spawn_blocking(move || {
        if quick {
            return (Vec::new(), Vec::new());
        }
        let upnp = if st.upnp {
            pm_out_upnp::ssdp_probe(std::time::Duration::from_secs(3)).unwrap_or_default()
        } else {
            Vec::new()
        };
        let cast = if st.cast {
            pm_out_cast::discover(std::time::Duration::from_millis(1500))
        } else {
            Vec::new()
        };
        (upnp, cast)
    })
    .await
    .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let outputs = s.outputs.list();
    let have = |loc: &str| {
        outputs
            .iter()
            .any(|o| o.id.0.contains(loc) || o.name.contains(loc))
    };
    let local_address = std::net::UdpSocket::bind("0.0.0.0:0")
        .and_then(|u| u.connect("1.1.1.1:80").map(|_| u))
        .and_then(|u| u.local_addr())
        .ok()
        .map(|a| a.ip().to_string());
    let mut notes = Vec::new();
    if !s.upnp && !s.cast {
        notes.push("UPnP and Cast discovery are both off in the node config.".into());
    }
    if quick {
        // No scan ran; only the configuration notes below apply.
    } else if s.upnp && upnp.is_empty() {
        notes.push("No UPnP device answered at all: multicast is not reaching this node. In Docker, run the container on the host network; on a VLAN, SSDP must be relayed.".into());
    } else if s.upnp && !upnp.iter().any(|a| a.renderer) {
        notes.push("Devices answered, but none describes itself as a MediaRenderer with AVTransport. OpenHome-only players (some Linn models) need the UPnP AV service enabled, or are reached through another renderer front-end.".into());
    }
    if s.public_url.contains("0.0.0.0")
        || s.public_url.contains("127.0.0.1")
        || s.public_url.contains("localhost")
    {
        notes.push(format!(
            "public_url is {}, which renderers cannot reach; set [api] public_url (PM_PUBLIC_URL) to http://{}:7788.",
            s.public_url,
            local_address.as_deref().unwrap_or("<node ip>")
        ));
    }
    Ok(Json(DiscoveryReport {
        upnp: upnp
            .into_iter()
            .map(|a| DiscoveredUpnp {
                output: have(&a.location),
                from: a.from,
                location: a.location,
                server: a.server,
                friendly_name: a.friendly_name,
                renderer: a.renderer,
            })
            .collect(),
        cast: cast
            .into_iter()
            .map(|d| DiscoveredCast {
                id: d.id,
                friendly_name: d.friendly_name,
                addr: d.addr.to_string(),
            })
            .collect(),
        public_url: s.public_url.clone(),
        local_address,
        notes,
        upnp_enabled: s.upnp,
        cast_enabled: s.cast,
        saved_addresses: s.upnp_addresses(),
    }))
}

/// `POST /v1/outputs/upnp {address}`: add a renderer multicast cannot find
/// (another subnet or VLAN); the address is kept for the next start.
async fn add_upnp_output(
    State(s): State<Arc<NodeState>>,
    Json(body): Json<AddRenderer>,
) -> ApiResult<Vec<OutputInfo>> {
    let st = s.clone();
    let found = tokio::task::spawn_blocking(move || st.add_upnp_address(&body.address))
        .await
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .map_err(ApiError::bad_request)?;
    let names: Vec<&str> = found.iter().map(|r| r.friendly_name.as_str()).collect();
    tracing::info!(?names, "renderers added by address");
    Ok(Json(s.outputs.list()))
}

/// `DELETE /v1/outputs/upnp/{address}`: stop re-adding it at start.
async fn remove_upnp_address(
    State(s): State<Arc<NodeState>>,
    Path(address): Path<String>,
) -> ApiResult<Vec<String>> {
    if !s.remove_upnp_address(&address) {
        return Err(ApiError::not_found(format!("{address} is not a saved address")));
    }
    Ok(Json(s.upnp_addresses()))
}

async fn rescan_outputs(State(s): State<Arc<NodeState>>) -> ApiResult<Vec<OutputInfo>> {
    let st = s.clone();
    let new = tokio::task::spawn_blocking(move || st.rescan_outputs())
        .await
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    if !new.is_empty() {
        tracing::info!(?new, "new outputs");
    }
    Ok(Json(s.outputs.list()))
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
    if body.name.trim().is_empty() {
        return Err(ApiError::bad_request("zone name is empty"));
    }
    let mut ids: Vec<String> = body.outputs.clone();
    if let Some(o) = body.output.clone() {
        if !ids.contains(&o) {
            ids.insert(0, o);
        }
    }
    if ids.is_empty() {
        return Err(ApiError::bad_request("give `output` or `outputs`"));
    }
    let mut members = Vec::with_capacity(ids.len());
    for id in &ids {
        let out = s
            .outputs
            .get(&OutputId(id.clone()))
            .ok_or_else(|| ApiError::not_found(format!("no output {id}")))?;
        members.push(out);
    }
    let output: Arc<dyn pm_zones::Output> = if members.len() == 1 {
        members.remove(0)
    } else {
        Arc::new(
            pm_zones::outputs::GroupOutput::new(body.name.trim(), members)
                .map_err(|e| ApiError::bad_request(e.to_string()))?,
        )
    };
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
    settle(&zone).await;
    Ok(Json(Enqueued { ids }))
}

async fn clear_queue(
    State(s): State<Arc<NodeState>>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let zone = zone_of(&s, &id)?;
    zone.stop();
    zone.clear();
    settle(&zone).await;
    Ok(StatusCode::NO_CONTENT)
}

/// Wait (bounded) until the engine has applied every command sent so far,
/// so that a read right after a write sees the new state.
async fn settle(zone: &Zone) {
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), zone.wait_acked()).await;
}

async fn remove_item(
    State(s): State<Arc<NodeState>>,
    Path((id, item)): Path<(String, u64)>,
) -> Result<StatusCode, ApiError> {
    let zone = zone_of(&s, &id)?;
    if zone.remove(item) {
        settle(&zone).await;
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
    let zone = zone_of(&s, &id)?;
    if zone.move_item(item, body.to_index) {
        settle(&zone).await;
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
    // Reply with the state after the engine applied the command (bounded wait).
    Ok(Json(
        tokio::time::timeout(std::time::Duration::from_secs(2), zone.wait_acked())
            .await
            .unwrap_or_else(|_| zone.state()),
    ))
}

async fn volume(
    State(s): State<Arc<NodeState>>,
    Path(id): Path<String>,
    Json(body): Json<SetVolume>,
) -> Result<StatusCode, ApiError> {
    let zone = zone_of(&s, &id)?;
    // A zone's volume limit (DSP settings) caps absolute requests.
    let limit = s.sources.dsp_for(&zone.output().id()).max_volume_db;
    zone.set_volume(cap_volume(body.volume, limit)).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Apply a zone's volume limit to an absolute dB request.
fn cap_volume(v: pm_model::Volume, limit: Option<f32>) -> pm_model::Volume {
    match (v, limit) {
        (pm_model::Volume::Db(db), Some(max)) if db > max => pm_model::Volume::Db(max),
        (v, _) => v,
    }
}

async fn repeat(
    State(s): State<Arc<NodeState>>,
    Path(id): Path<String>,
    Json(body): Json<SetRepeat>,
) -> Result<StatusCode, ApiError> {
    let zone = zone_of(&s, &id)?;
    zone.set_repeat(body.repeat);
    settle(&zone).await;
    Ok(StatusCode::NO_CONTENT)
}

/// Library radio: keep the zone playing with similar music after the last item.
async fn radio(
    State(s): State<Arc<NodeState>>,
    Path(id): Path<String>,
    Json(body): Json<SetRadio>,
) -> Result<StatusCode, ApiError> {
    let zone = zone_of(&s, &id)?;
    zone.set_radio(body.on);
    settle(&zone).await;
    Ok(StatusCode::NO_CONTENT)
}

/// Shuffle the upcoming part of the queue; the playing item is untouched.
async fn shuffle_queue(
    State(s): State<Arc<NodeState>>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let zone = zone_of(&s, &id)?;
    zone.shuffle_upcoming();
    settle(&zone).await;
    Ok(StatusCode::NO_CONTENT)
}

fn sleep_view(s: &NodeState, zone: &ZoneId) -> SleepView {
    let ends_at_ms = s.sleep_ends_at(zone);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    SleepView {
        ends_at_ms,
        remaining_ms: ends_at_ms.map(|e| e.saturating_sub(now)),
    }
}

/// The zone's sleep timer, if any.
async fn sleep_get(State(s): State<Arc<NodeState>>, Path(id): Path<String>) -> ApiResult<SleepView> {
    let zone = zone_of(&s, &id)?;
    Ok(Json(sleep_view(&s, zone.id())))
}

/// Pause the zone after `seconds` (`null` cancels any timer).
async fn sleep_set(
    State(s): State<Arc<NodeState>>,
    Path(id): Path<String>,
    Json(body): Json<SetSleep>,
) -> ApiResult<SleepView> {
    let zone = zone_of(&s, &id)?;
    if body.seconds == Some(0) {
        return Err(ApiError::bad_request(
            "seconds must be positive, or null to cancel",
        ));
    }
    s.set_sleep(zone.clone(), body.seconds);
    Ok(Json(sleep_view(&s, zone.id())))
}

/// Move the queue and playback position to another zone. The target stops
/// whatever it was doing; the source is left stopped and empty.
async fn transfer(
    State(s): State<Arc<NodeState>>,
    Path(id): Path<String>,
    Json(body): Json<TransferRequest>,
) -> ApiResult<ZoneState> {
    if body.to == id {
        return Err(ApiError::bad_request("cannot transfer a zone to itself"));
    }
    let from = zone_of(&s, &id)?;
    let to = zone_of(&s, &body.to)?;
    let st = from.state();
    if st.queue.is_empty() {
        return Err(ApiError::bad_request("nothing to transfer: the queue is empty"));
    }
    let index = st
        .now_playing
        .as_ref()
        .map(|n| n.index)
        .or(st.queue_index)
        .filter(|i| *i < st.queue.len());
    let position_ms = st.now_playing.as_ref().map(|n| n.position_ms).unwrap_or(0);
    from.stop();
    from.clear();
    to.stop();
    to.clear();
    to.set_repeat(st.repeat);
    to.set_radio(st.radio);
    let mut events = to.subscribe();
    to.enqueue(st.queue, Insert::End);
    if let (Some(i), true) = (index, st.transport != pm_zones::Transport::Stopped) {
        to.play_index(i);
        // Seeking needs the track open; wait for it (bounded) before resuming where it was.
        let started = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                match events.recv().await {
                    Ok(pm_zones::ZoneEvent::TrackStarted { .. }) => break true,
                    Ok(pm_zones::ZoneEvent::Error(_)) | Err(_) => break false,
                    Ok(_) => {}
                }
            }
        })
        .await
        .unwrap_or(false);
        if started {
            if position_ms > 1_000 {
                to.seek_ms(position_ms);
            }
            if st.transport == pm_zones::Transport::Paused {
                to.pause();
            }
        }
    }
    let settle = std::time::Duration::from_secs(5);
    let _ = tokio::time::timeout(settle, from.wait_acked()).await;
    Ok(Json(
        tokio::time::timeout(settle, to.wait_acked())
            .await
            .unwrap_or_else(|_| to.state()),
    ))
}

// ---- handlers: DSP per zone ----

/// The pipeline settings for the zone's output (EQ, headroom, software
/// volume, resampling policy). Changes apply at the next track start.
async fn dsp_get(
    State(s): State<Arc<NodeState>>,
    Path(id): Path<String>,
) -> ApiResult<pm_audio::PipelineConfig> {
    let zone = zone_of(&s, &id)?;
    Ok(Json(s.sources.dsp_for(&zone.output().id())))
}

async fn dsp_put(
    State(s): State<Arc<NodeState>>,
    Path(id): Path<String>,
    Json(cfg): Json<pm_audio::PipelineConfig>,
) -> ApiResult<pm_audio::PipelineConfig> {
    let zone = zone_of(&s, &id)?;
    for b in &cfg.eq {
        // Validate against a common rate; the real plan validates again per track.
        b.validate(48_000).map_err(ApiError::bad_request)?;
    }
    if cfg.headroom_db.is_some_and(|d| !(-30.0..=0.0).contains(&d)) {
        return Err(ApiError::bad_request("headroom_db must be between -30 and 0"));
    }
    if cfg.volume_db.is_some_and(|d| !(-90.0..=0.0).contains(&d)) {
        return Err(ApiError::bad_request("volume_db must be between -90 and 0"));
    }
    if cfg.max_volume_db.is_some_and(|d| !(-90.0..=0.0).contains(&d)) {
        return Err(ApiError::bad_request("max_volume_db must be between -90 and 0"));
    }
    let mut cfg = cfg;
    if let (Some(v), Some(max)) = (cfg.volume_db, cfg.max_volume_db) {
        if v > max {
            cfg.volume_db = Some(max);
        }
    }
    s.sources.set_dsp(&zone.output().id(), Some(cfg.clone()))?;
    Ok(Json(cfg))
}

async fn dsp_reset(State(s): State<Arc<NodeState>>, Path(id): Path<String>) -> Result<StatusCode, ApiError> {
    let zone = zone_of(&s, &id)?;
    s.sources.set_dsp(&zone.output().id(), None)?;
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

/// A consistent snapshot of the library database as a download
/// (`pm-node restore <file>` brings it back).
async fn backup(State(s): State<Arc<NodeState>>) -> Result<axum::response::Response, ApiError> {
    let tmp = std::env::temp_dir().join(format!(
        "pubky-music-backup-{}-{}.db",
        std::process::id(),
        pm_library::db::now_unix()
    ));
    let bytes = {
        let lib = s.library.lock().expect("library poisoned");
        lib.backup_to(&tmp)?;
        let bytes = std::fs::read(&tmp);
        let _ = std::fs::remove_file(&tmp);
        bytes.map_err(|e| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("reading the backup snapshot: {e}"),
            )
        })?
    };
    let date = time::OffsetDateTime::now_utc().date();
    let name = format!("pubky-music-{date}.db");
    Ok((
        [
            (
                axum::http::header::CONTENT_TYPE,
                "application/vnd.sqlite3".to_string(),
            ),
            (
                axum::http::header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{name}\""),
            ),
        ],
        bytes,
    )
        .into_response())
}

/// Where else a library track streams from (TIDAL and Deezer by ISRC); empty when nothing is known.
async fn track_versions(State(s): State<Arc<NodeState>>, Path(id): Path<i64>) -> ApiResult<Vec<Version>> {
    let track = s
        .library
        .lock()
        .expect("library poisoned")
        .get_track(TrackId(id))?
        .ok_or_else(|| ApiError::not_found(format!("no track {id}")))?;
    let (t, d) = tokio::join!(
        tidal::track_versions(&s, &track),
        deezer::track_versions(&s, &track)
    );
    let mut out = t.unwrap_or_default();
    out.extend(d.unwrap_or_default());
    Ok(Json(out))
}

/// Where else a library album streams from (TIDAL and Deezer by UPC).
async fn album_versions(State(s): State<Arc<NodeState>>, Path(id): Path<i64>) -> ApiResult<Vec<Version>> {
    let album = s
        .library
        .lock()
        .expect("library poisoned")
        .get_album(id)?
        .ok_or_else(|| ApiError::not_found(format!("no album {id}")))?;
    let (t, d) = tokio::join!(
        tidal::album_versions(&s, &album),
        deezer::album_versions(&s, &album)
    );
    let mut out = t.unwrap_or_default();
    out.extend(d.unwrap_or_default());
    Ok(Json(out))
}

/// Lyrics from the file's tags or a sidecar `.lrc`; 404 when the track has none.
async fn lyrics(State(s): State<Arc<NodeState>>, Path(id): Path<i64>) -> ApiResult<Lyrics> {
    let found = s.library.lock().expect("library poisoned").lyrics(TrackId(id))?;
    let Some((text, source)) = found else {
        return Err(ApiError::not_found(format!("no lyrics for track {id}")));
    };
    let (lines, synced) = pm_library::lyrics::parse(&text);
    Ok(Json(Lyrics {
        track_id: id,
        source,
        synced,
        text,
        lines,
    }))
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
        "mp3" => "audio/mpeg",
        "aac" => "audio/aac",
        "vorbis" => "audio/ogg",
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

async fn search(State(s): State<Arc<NodeState>>, Query(q): Query<SearchQuery>) -> ApiResult<SearchResults> {
    let limit = q.limit.unwrap_or(50).min(500);
    let needle = q.q.trim().to_lowercase();
    // Streaming services answer alongside the library (their part is bounded in time).
    let sources = if q.sources.unwrap_or(1) != 0 && !needle.is_empty() {
        streaming::unified_search(&s, q.q.trim(), 12).await
    } else {
        Vec::new()
    };
    let lib = s.library.lock().expect("library poisoned");
    let tracks = lib.search_tracks(&q.q, limit)?;
    let albums = if needle.is_empty() {
        Vec::new()
    } else {
        lib.search_albums(&q.q, 24)?
    };
    let contains = |name: &str| !needle.is_empty() && name.to_lowercase().contains(&needle);
    let artists = lib
        .list_artists()?
        .into_iter()
        .filter(|a| contains(&a.name))
        .take(24)
        .collect();
    let genres = lib
        .list_genres()?
        .into_iter()
        .filter(|g| contains(&g.name))
        .take(12)
        .collect();
    Ok(Json(SearchResults {
        tracks,
        albums,
        artists,
        genres,
        sources,
    }))
}

async fn artists(State(s): State<Arc<NodeState>>) -> ApiResult<Vec<pm_library::ArtistSummary>> {
    Ok(Json(s.library.lock().expect("library poisoned").list_artists()?))
}

async fn artist(State(s): State<Arc<NodeState>>, Path(id): Path<i64>) -> ApiResult<ArtistDetail> {
    let lib = s.library.lock().expect("library poisoned");
    let artist = lib
        .get_artist(id)?
        .ok_or_else(|| ApiError::not_found(format!("no artist {id}")))?;
    let albums = lib.artist_albums(id)?;
    Ok(Json(ArtistDetail { artist, albums }))
}

/// Biography, links and whether a portrait is cached (fetched on first request).
async fn artist_info_handler(State(s): State<Arc<NodeState>>, Path(id): Path<i64>) -> ApiResult<ArtistInfo> {
    if s.library
        .lock()
        .expect("library poisoned")
        .get_artist(id)?
        .is_none()
    {
        return Err(ApiError::not_found(format!("no artist {id}")));
    }
    Ok(Json(artist_info::artist_info(&s, id).await?))
}

async fn artist_image(State(s): State<Arc<NodeState>>, Path(id): Path<i64>) -> Result<Response, ApiError> {
    let path = artist_info::image_path(&s, id);
    if !path.is_file() {
        // The first request may have to fetch it.
        let info = artist_info::artist_info(&s, id).await?;
        if !info.image {
            return Err(ApiError::not_found(format!("no portrait for artist {id}")));
        }
    }
    let bytes = tokio::fs::read(&path)
        .await
        .map_err(|e| ApiError::not_found(format!("portrait for artist {id}: {e}")))?;
    let mime = match bytes.get(..4) {
        Some([0x89, b'P', b'N', b'G']) => "image/png",
        Some([b'R', b'I', b'F', b'F']) => "image/webp",
        Some([b'G', b'I', b'F', _]) => "image/gif",
        _ => "image/jpeg",
    };
    Ok((
        [
            (header::CONTENT_TYPE, mime.to_string()),
            (header::CACHE_CONTROL, "private, max-age=86400".to_string()),
        ],
        bytes,
    )
        .into_response())
}

async fn artist_tracks(State(s): State<Arc<NodeState>>, Path(id): Path<i64>) -> ApiResult<Tracks> {
    let tracks = s.library.lock().expect("library poisoned").artist_tracks(id)?;
    Ok(Json(Tracks { tracks }))
}

async fn albums(State(s): State<Arc<NodeState>>, Query(p): Query<Page>) -> ApiResult<Vec<pm_model::Album>> {
    let limit = p.limit.unwrap_or(100).min(1000);
    Ok(Json(
        s.library
            .lock()
            .expect("library poisoned")
            .list_albums_focused(p.focus, p.after, limit)?,
    ))
}

/// Release groups MusicBrainz lists for the library's artists from the last
/// year; a background refresh starts when any artist's list is stale.
async fn new_releases(State(s): State<Arc<NodeState>>) -> ApiResult<NewReleasesPage> {
    Ok(Json(new_releases::page(&s)?))
}

/// Force a refresh of stale artists now (one MusicBrainz request per second).
async fn new_releases_refresh(State(s): State<Arc<NodeState>>) -> ApiResult<NewReleasesPage> {
    if s.contact().is_empty() {
        return Err(ApiError::bad_request(
            "set a contact first (Settings): MusicBrainz requires one in the User-Agent",
        ));
    }
    new_releases::kick_refresh(s.clone());
    Ok(Json(new_releases::page(&s)?))
}

/// The best file format per album, for badges and Focus in clients.
async fn album_formats(State(s): State<Arc<NodeState>>) -> ApiResult<Vec<pm_library::AlbumFormat>> {
    Ok(Json(s.library.lock().expect("library poisoned").album_formats()?))
}

async fn genres(State(s): State<Arc<NodeState>>) -> ApiResult<Vec<pm_library::GenreSummary>> {
    Ok(Json(s.library.lock().expect("library poisoned").list_genres()?))
}

async fn genre(State(s): State<Arc<NodeState>>, Path(id): Path<i64>) -> ApiResult<GenreDetail> {
    let lib = s.library.lock().expect("library poisoned");
    let genre = lib
        .get_genre(id)?
        .ok_or_else(|| ApiError::not_found(format!("no genre {id}")))?;
    Ok(Json(GenreDetail {
        albums: lib.genre_albums(id)?,
        tracks: lib.genre_tracks(id, 500)?,
        genre,
    }))
}

async fn album_tracks(State(s): State<Arc<NodeState>>, Path(id): Path<i64>) -> ApiResult<Tracks> {
    let tracks = s.library.lock().expect("library poisoned").album_tracks(id)?;
    Ok(Json(Tracks { tracks }))
}

async fn history(State(s): State<Arc<NodeState>>, Query(p): Query<Limit>) -> ApiResult<History> {
    let limit = p.limit.unwrap_or(100).min(1000);
    let plays = s.library.lock().expect("library poisoned").history(limit)?;
    Ok(Json(History { plays }))
}

/// Clients that play by themselves (the native app's local zone, a browser)
/// report their plays here; node zones are recorded automatically.
async fn record_play(State(s): State<Arc<NodeState>>, Json(req): Json<RecordPlay>) -> ApiResult<History> {
    let lib = s.library.lock().expect("library poisoned");
    if lib.get_track(TrackId(req.track_id))?.is_none() {
        return Err(ApiError::not_found(format!("track {}", req.track_id)));
    }
    lib.record_play(TrackId(req.track_id), req.zone.as_deref())?;
    let plays = lib.history(1)?;
    Ok(Json(History { plays }))
}

async fn home(State(s): State<Arc<NodeState>>, Query(p): Query<Limit>) -> ApiResult<HomePage> {
    let limit = p.limit.unwrap_or(12).min(100);
    let lib = s.library.lock().expect("library poisoned");
    let favourite_albums = favourites::collect(&lib)?
        .albums
        .into_iter()
        .take(limit as usize)
        .collect();
    Ok(Json(HomePage {
        favourite_albums,
        recently_added: lib.recent_albums(0, limit)?,
        recently_played: lib.recently_played_albums(limit)?,
        most_played_albums: lib.most_played_albums(limit)?,
        most_played: lib.most_played_tracks(limit)?,
        history: lib.history(limit)?,
        stats: lib.stats()?,
    }))
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

fn settings_view(s: &NodeState) -> NodeSettingsView {
    let jukebox_zone = s
        .library
        .lock()
        .expect("library poisoned")
        .get_setting(subsonic::JUKEBOX_ZONE_KEY)
        .ok()
        .flatten()
        .filter(|z| !z.is_empty());
    NodeSettingsView {
        contact: s.contact(),
        discovery: s.discovery().is_some(),
        jukebox_zone,
    }
}

async fn node_settings(State(s): State<Arc<NodeState>>) -> ApiResult<NodeSettingsView> {
    Ok(Json(settings_view(&s)))
}

/// `PUT /v1/node/settings {contact}`: set the contact without touching the
/// config file; discovery comes alive at once.
async fn update_node_settings(
    State(s): State<Arc<NodeState>>,
    Json(req): Json<NodeSettingsUpdate>,
) -> ApiResult<NodeSettingsView> {
    if let Some(c) = req.contact {
        s.set_contact(&c).map_err(ApiError::bad_request)?;
    }
    if let Some(z) = req.jukebox_zone {
        if !z.is_empty() {
            zone_of(&s, &z)?;
        }
        s.library
            .lock()
            .expect("library poisoned")
            .set_setting(subsonic::JUKEBOX_ZONE_KEY, &z)?;
    }
    Ok(Json(settings_view(&s)))
}

fn folder_list(s: &NodeState) -> Result<Vec<(String, bool)>, ApiError> {
    Ok(s.library
        .lock()
        .expect("library poisoned")
        .list_folders()?
        .into_iter()
        .map(|(p, present)| (p.to_string_lossy().into_owned(), present))
        .collect())
}

/// `POST /v1/library/folders {path}`: add a folder, watch it, scan it.
async fn add_folder(
    State(s): State<Arc<NodeState>>,
    Json(req): Json<FolderRequest>,
) -> ApiResult<FoldersChanged> {
    let raw = req.path.trim();
    if raw.is_empty() {
        return Err(ApiError::bad_request("path is required"));
    }
    let path = std::path::PathBuf::from(raw);
    let path = tokio::fs::canonicalize(&path)
        .await
        .map_err(|e| ApiError::bad_request(format!("{}: {e}", path.display())))?;
    if !path.is_dir() {
        return Err(ApiError::bad_request(format!(
            "{} is not a directory",
            path.display()
        )));
    }
    s.library.lock().expect("library poisoned").add_folder(&path)?;
    s.watch_more(&path);
    let scan_started = matches!(s.start_scan(vec![path]), Ok(true));
    Ok(Json(FoldersChanged {
        folders: folder_list(&s)?,
        scan_started,
        removed_files: 0,
        removed_tracks: 0,
    }))
}

/// `DELETE /v1/library/folders?path=`: forget a folder and everything scanned from it.
async fn remove_folder(
    State(s): State<Arc<NodeState>>,
    Query(req): Query<FolderRequest>,
) -> ApiResult<FoldersChanged> {
    let path = std::path::PathBuf::from(req.path.trim());
    if s.is_scanning() {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "a scan is running; try again when it is done",
        ));
    }
    s.unwatch(&path);
    let (removed_files, removed_tracks) = s.library.lock().expect("library poisoned").remove_folder(&path)?;
    s.notify_library();
    Ok(Json(FoldersChanged {
        folders: folder_list(&s)?,
        scan_started: false,
        removed_files,
        removed_tracks,
    }))
}

/// Start a scan in the background on its own SQLite connection; one at a time.
async fn scan(State(s): State<Arc<NodeState>>, Json(body): Json<ScanRequest>) -> ApiResult<ScanStarted> {
    let folders: Vec<std::path::PathBuf> = if body.folders.is_empty() {
        s.all_folders()
    } else {
        body.folders.iter().map(std::path::PathBuf::from).collect()
    };
    match s.start_scan(folders.clone()) {
        Ok(true) => Ok(Json(ScanStarted {
            folders: folders.iter().map(|p| p.to_string_lossy().into_owned()).collect(),
        })),
        Ok(false) => Err(ApiError::new(StatusCode::CONFLICT, "a scan is already running")),
        Err(e) => Err(ApiError::bad_request(e)),
    }
}

/// The album's cover image (extracted at scan time or found in the folder).
async fn album_cover(State(s): State<Arc<NodeState>>, Path(id): Path<i64>) -> Result<Response, ApiError> {
    let known = s.library.lock().expect("library poisoned").album_art(id)?;
    let art = match known {
        Some(a) => a,
        None => art::fetch_missing_cover(&s, id)
            .await?
            .ok_or_else(|| ApiError::not_found(format!("no cover for album {id}")))?,
    };
    let bytes = tokio::fs::read(&art.file)
        .await
        .map_err(|e| ApiError::not_found(format!("cover file for album {id}: {e}")))?;
    Ok((
        [
            (header::CONTENT_TYPE, art.mime),
            (header::CACHE_CONTROL, "private, max-age=86400".to_string()),
        ],
        bytes,
    )
        .into_response())
}

// ---- pubky ----

async fn pubky_status(State(s): State<Arc<NodeState>>) -> ApiResult<PubkyStatus> {
    let unsynced = s
        .library
        .lock()
        .expect("library poisoned")
        .unsynced_music_posts()?
        .len();
    Ok(Json(PubkyStatus {
        user: s.sync_handle().map(|h| h.z32().to_string()),
        unsynced,
    }))
}

/// Start a Pubky Ring sign-in: returns the `pubkyauth://` URL to scan or
/// paste into Pubky Ring. Poll with GET until approved.
async fn pubky_login_start(State(s): State<Arc<NodeState>>) -> ApiResult<LoginStarted> {
    let hs = pm_pubky::homeserver::NodeHomeserver::new(s.pubky.network)
        .map_err(|e| ApiError::new(StatusCode::BAD_GATEWAY, e.to_string()))?;
    let flow = hs
        .start_login("pubky-music-node")
        .map_err(|e| ApiError::new(StatusCode::BAD_GATEWAY, e.to_string()))?;
    let url = flow.url().to_string();
    *s.login.lock().await = Some(flow);
    Ok(Json(LoginStarted { url }))
}

/// Poll the sign-in. When approved: the session is kept, its secret written
/// to the configured file, the node record published, playlists synced.
async fn pubky_login_poll(State(s): State<Arc<NodeState>>) -> ApiResult<LoginStatus> {
    if let Some(sync) = s.sync_handle() {
        if s.login.lock().await.is_none() {
            return Ok(Json(LoginStatus {
                status: "signed_in".into(),
                user: Some(sync.z32().to_string()),
                url: None,
            }));
        }
    }
    let mut guard = s.login.lock().await;
    let Some(flow) = guard.as_ref() else {
        return Ok(Json(LoginStatus {
            status: "none".into(),
            user: None,
            url: None,
        }));
    };
    let url = flow.url().to_string();
    match flow.poll().await {
        Ok(None) => Ok(Json(LoginStatus {
            status: "pending".into(),
            user: None,
            url: Some(url),
        })),
        Ok(Some(session)) => {
            *guard = None;
            drop(guard);
            let secret = pm_pubky::homeserver::export_session_secret(&session).await;
            if let (Some(path), Some(secret)) = (&s.session_secret_path, &secret) {
                if let Some(parent) = path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                match std::fs::write(path, secret) {
                    Ok(()) => {
                        #[cfg(unix)]
                        {
                            use std::os::unix::fs::PermissionsExt;
                            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
                        }
                    }
                    Err(e) => tracing::warn!(path = %path.display(), "cannot store the session secret: {e}"),
                }
            }
            let hs = pm_pubky::homeserver::NodeHomeserver::new(s.pubky.network)
                .map_err(|e| ApiError::new(StatusCode::BAD_GATEWAY, e.to_string()))?;
            let sync = Arc::new(sync::PubkySync::from_session(hs, session));
            let user = sync.z32().to_string();
            s.set_sync(Some(sync.clone()));
            if let Err(e) = sync
                .publish_node(&s.name, &s.public_url, s.token.as_deref(), &s.version)
                .await
            {
                tracing::warn!("publishing the node record failed: {e}");
            }
            if let Err(e) = sync.sync(&s.library).await {
                tracing::warn!("first sync failed: {e}");
            }
            s.notify_library();
            Ok(Json(LoginStatus {
                status: "signed_in".into(),
                user: Some(user),
                url: None,
            }))
        }
        Err(e) => {
            *guard = None;
            Err(ApiError::new(StatusCode::BAD_GATEWAY, e.to_string()))
        }
    }
}

/// Push local changes to the private homeserver folder and pull what is missing.
async fn pubky_sync(State(s): State<Arc<NodeState>>) -> ApiResult<SyncReport> {
    let Some(sync) = s.sync_handle() else {
        return Err(ApiError::new(
            StatusCode::PRECONDITION_FAILED,
            "no Pubky session: set `pubky.session_secret_file` in the node config",
        ));
    };
    let report = sync
        .sync(&s.library)
        .await
        .map_err(|e| ApiError::new(StatusCode::BAD_GATEWAY, e.to_string()))?;
    if report.pulled > 0 {
        s.notify_library();
    }
    Ok(Json(report))
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
    use pm_library::Library;
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
                art_dir: dir.join("art"),
                mirror_dir: dir.join("homeserver"),
                pipeline: PipelineConfig::default(),
                token: token.map(str::to_string),
                subsonic_user: "pubky".into(),
                pubky: Default::default(),
                public_url: "http://127.0.0.1:7788".into(),
                upnp: false,
                cast: false,
                slimproto_port: 0,
                session_secret_path: None,
                tidal: Default::default(),
            },
            lib,
            outputs,
        );
        // Never reach the real Deezer API from tests; a closed port fails fast.
        *s.deezer_base.write().unwrap() = "http://127.0.0.1:9".into();
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
            serde_json::from_slice(&bytes).unwrap_or_else(|e| {
                panic!(
                    "{method} {uri}: {status}: not JSON ({e}): {}",
                    String::from_utf8_lossy(&bytes)
                )
            })
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

        // The play was recorded by the zone event hook; a client can add its own.
        let mut recorded = false;
        for _ in 0..200 {
            let (_, h) = call(&app, "GET", "/v1/library/history", None).await;
            if !h["plays"].as_array().unwrap().is_empty() {
                assert_eq!(h["plays"][0]["track"]["id"], id);
                assert_eq!(h["plays"][0]["zone"], zid);
                recorded = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(recorded, "node zone play not recorded");
        let (st, h) = call(
            &app,
            "POST",
            "/v1/library/history",
            Some(serde_json::json!({"track_id": id, "zone": "local"})),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{h}");
        assert_eq!(h["plays"][0]["zone"], "local");
        let (st, _) = call(
            &app,
            "POST",
            "/v1/library/history",
            Some(serde_json::json!({"track_id": 999_999})),
        )
        .await;
        assert_eq!(st, StatusCode::NOT_FOUND);
        let (st, home) = call(&app, "GET", "/v1/library/home", None).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(home["history"].as_array().unwrap().len(), 2);
        assert_eq!(home["most_played"][0]["plays"], 2);
        assert_eq!(
            home["recently_added"].as_array().unwrap().len(),
            home["stats"]["albums"]
        );

        // Without a contact, the Pubky view of an object is a clear 412; the
        // contact can be set through the API and discovery comes alive.
        let (st, e) = call(&app, "GET", &format!("/v1/discover/uri?track_id={id}"), None).await;
        assert_eq!(st, StatusCode::PRECONDITION_FAILED, "{e}");
        let (st, v) = call(&app, "GET", "/v1/node/settings", None).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(v["discovery"], false);
        let (st, _) = call(
            &app,
            "PUT",
            "/v1/node/settings",
            Some(serde_json::json!({"contact": "github.com/x"})),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        let (st, v) = call(
            &app,
            "PUT",
            "/v1/node/settings",
            Some(serde_json::json!({"contact": " me@example.org "})),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{v}");
        assert_eq!(v["contact"], "me@example.org");
        assert_eq!(v["discovery"], true);
        assert_eq!(
            s.library
                .lock()
                .unwrap()
                .get_setting("node.contact")
                .unwrap()
                .as_deref(),
            Some("me@example.org")
        );

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

    /// Serve the FLAC fixture over HTTP (directly and behind a PLS playlist)
    /// and play both through a zone: the HTTP stream source end to end.
    #[tokio::test]
    async fn http_streams_play_like_files() {
        let flac = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../pm-audio/tests/fixtures/60_mono_audio.flac"
        ))
        .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let pls =
            format!("[playlist]\nNumberOfEntries=1\nFile1=http://127.0.0.1:{port}/a.flac\nTitle1=Fixture\n");
        let files = Router::new()
            .route(
                "/a.flac",
                get(move || {
                    let flac = flac.clone();
                    async move { ([(header::CONTENT_TYPE, "audio/flac")], flac) }
                }),
            )
            .route(
                "/list.pls",
                get(move || {
                    let pls = pls.clone();
                    async move { ([(header::CONTENT_TYPE, "audio/x-scpls")], pls) }
                }),
            );
        tokio::spawn(async move { axum::serve(listener, files).await.unwrap() });

        let dir = tempfile::tempdir().unwrap();
        let s = state(dir.path(), None).await;
        let app = router(s.clone());
        let (_, zones) = call(&app, "GET", "/v1/zones", None).await;
        let zid = zones[0]["id"].as_str().unwrap().to_string();
        for uri in [
            format!("http://127.0.0.1:{port}/a.flac"),
            format!("http://127.0.0.1:{port}/list.pls"),
        ] {
            let (st, q) = call(
                &app,
                "POST",
                &format!("/v1/zones/{zid}/queue"),
                Some(serde_json::json!({"items": [{"uri": uri, "title": "Fixture radio"}], "replace": true, "play": true})),
            )
            .await;
            assert_eq!(st, StatusCode::OK, "{q}");
            let zone = s.zones.get(&ZoneId::new(&zid)).unwrap();
            let st = zone.wait_stopped().await;
            assert_eq!(st.error, None, "{uri}");
        }
        // Saved stations round trip; playing one goes through the same path.
        let (st, saved) = call(
            &app,
            "PUT",
            "/v1/radio/stations",
            Some(serde_json::json!({"id": "", "name": "Fixture FM", "url": format!("http://127.0.0.1:{port}/a.flac"), "tags": ["test"]})),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{saved}");
        let id = saved["stations"][0]["id"].as_str().unwrap().to_string();
        assert!(id.starts_with("url-"));
        let (st, played) = call(
            &app,
            "POST",
            "/v1/radio/play",
            Some(serde_json::json!({"zone": zid, "station": saved["stations"][0]})),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{played}");
        let zone = s.zones.get(&ZoneId::new(&zid)).unwrap();
        assert_eq!(zone.wait_stopped().await.error, None);
        let (_, q) = call(&app, "GET", &format!("/v1/zones/{zid}/queue"), None).await;
        assert_eq!(q[0]["title"], "Fixture FM");
        assert_eq!(q[0]["album"], "Internet radio");
        let (st, _) = call(&app, "DELETE", &format!("/v1/radio/stations/{id}"), None).await;
        assert_eq!(st, StatusCode::OK);
        let (st, _) = call(&app, "DELETE", &format!("/v1/radio/stations/{id}"), None).await;
        assert_eq!(st, StatusCode::NOT_FOUND);
        let (_, left) = call(&app, "GET", "/v1/radio/stations", None).await;
        assert!(left["stations"].as_array().unwrap().is_empty());
        let (st, _) = call(&app, "GET", "/v1/radio/search", None).await;
        assert_eq!(
            st,
            StatusCode::BAD_REQUEST,
            "search without terms is refused before any network call"
        );
    }

    #[tokio::test]
    async fn folders_can_be_added_and_removed_through_the_api() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("music")).unwrap();
        let extra = dir.path().join("more");
        std::fs::create_dir(&extra).unwrap();
        write_wav(&extra.join("extra.wav"), 4410);
        let s = state(dir.path(), None).await;
        let app = router(s.clone());
        let (st, _) = call(
            &app,
            "POST",
            "/v1/library/folders",
            Some(serde_json::json!({"path": "/nope/missing"})),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        let (st, r) = call(
            &app,
            "POST",
            "/v1/library/folders",
            Some(serde_json::json!({"path": extra.to_string_lossy()})),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{r}");
        assert_eq!(r["scan_started"], true);
        assert!(r["folders"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f[0] == extra.to_string_lossy().as_ref()));
        for _ in 0..500 {
            if !s.is_scanning() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let (_, tracks) = call(&app, "GET", "/v1/library/tracks?limit=10", None).await;
        assert_eq!(tracks["tracks"].as_array().unwrap().len(), 1);
        let (st, r) = call(
            &app,
            "DELETE",
            &format!("/v1/library/folders?path={}", extra.to_string_lossy()),
            None,
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{r}");
        assert_eq!(r["removed_tracks"], 1);
        assert!(r["folders"].as_array().unwrap().is_empty());
        let (_, tracks) = call(&app, "GET", "/v1/library/tracks?limit=10", None).await;
        assert!(tracks["tracks"].as_array().unwrap().is_empty());
    }

    /// A zone's queue is saved on every change and comes back after a restart.
    #[tokio::test]
    async fn queues_survive_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("music")).unwrap();
        write_wav(&dir.path().join("music/one.wav"), 4410);
        let zid;
        {
            let s = state(dir.path(), None).await;
            let app = router(s.clone());
            call(&app, "POST", "/v1/library/scan", Some(serde_json::json!({}))).await;
            for _ in 0..500 {
                if !s.is_scanning() {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            let (_, tracks) = call(&app, "GET", "/v1/library/tracks?limit=10", None).await;
            let id = tracks["tracks"][0]["id"].as_i64().unwrap();
            let (_, zones) = call(&app, "GET", "/v1/zones", None).await;
            zid = zones[0]["id"].as_str().unwrap().to_string();
            let (st, _) = call(
                &app,
                "POST",
                &format!("/v1/zones/{zid}/queue"),
                Some(serde_json::json!({"track_ids": [id], "items": [{"uri": "https://radio.example/live", "title": "Live"}], "play": false})),
            )
            .await;
            assert_eq!(st, StatusCode::OK);
            let (st, _) = call(
                &app,
                "PUT",
                &format!("/v1/zones/{zid}/repeat"),
                Some(serde_json::json!({"repeat": "all"})),
            )
            .await;
            assert_eq!(st, StatusCode::NO_CONTENT);
            // The save happens on the state watch; give it a moment.
            let key = format!("queue.{zid}");
            let mut saved = None;
            for _ in 0..200 {
                saved = s.library.lock().unwrap().get_setting(&key).unwrap();
                if saved
                    .as_deref()
                    .is_some_and(|j| j.contains("radio.example") && j.contains("\"all\""))
                {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            assert!(saved.is_some(), "queue was not saved");
        }
        // "Restart": a new node state over the same database and outputs.
        let s2 = state(dir.path(), None).await;
        let app2 = router(s2.clone());
        let (st, q) = call(&app2, "GET", &format!("/v1/zones/{zid}/queue"), None).await;
        assert_eq!(st, StatusCode::OK, "{q}");
        let q = q.as_array().unwrap();
        assert_eq!(q.len(), 2, "{q:?}");
        assert_eq!(q[0]["title"], "one");
        assert_eq!(q[1]["uri"], "https://radio.example/live");
        let (_, z) = call(&app2, "GET", &format!("/v1/zones/{zid}"), None).await;
        assert_eq!(z["repeat"], "all");
        assert_eq!(z["transport"], "stopped", "restored queues do not start playing");
    }

    /// The Internet Archive source against a local mock: search, item
    /// tracks with the best format, and the tracks play through a zone.
    #[tokio::test]
    async fn internet_archive_source_searches_and_plays() {
        let flac = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../pm-audio/tests/fixtures/60_mono_audio.flac"
        ))
        .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let mock = Router::new()
            .route(
                "/advancedsearch.php",
                get(|| async {
                    Json(serde_json::json!({"response": {"docs": [
                        {"identifier": "fixture-set", "title": "Fixture Set", "creator": "Tester", "year": "2020", "downloads": 7}
                    ]}}))
                }),
            )
            .route(
                "/metadata/{id}",
                get(|| async {
                    Json(serde_json::json!({
                        "metadata": {"title": "Fixture Set", "creator": "Tester", "year": "2020"},
                        "files": [
                            {"name": "one.flac", "format": "Flac", "source": "original", "title": "One", "track": "1", "length": "5.15"},
                            {"name": "one.mp3", "format": "VBR MP3", "source": "derivative", "original": "one.flac", "title": "One", "track": "1"}
                        ]
                    }))
                }),
            )
            .route(
                "/download/{id}/{file}",
                get(move || {
                    let flac = flac.clone();
                    async move { ([(header::CONTENT_TYPE, "audio/flac")], flac) }
                }),
            );
        tokio::spawn(async move { axum::serve(listener, mock).await.unwrap() });

        let dir = tempfile::tempdir().unwrap();
        let s = state(dir.path(), None).await;
        *s.archive_base.write().unwrap() = format!("http://127.0.0.1:{port}");
        let app = router(s.clone());
        let (st, sources) = call(&app, "GET", "/v1/sources", None).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(sources[0]["id"], "archive");
        let (st, _) = call(&app, "GET", "/v1/sources/archive/search?q=", None).await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        let (st, r) = call(&app, "GET", "/v1/sources/archive/search?q=fixture", None).await;
        assert_eq!(st, StatusCode::OK, "{r}");
        assert_eq!(r["items"][0]["id"], "fixture-set");
        let (st, it) = call(&app, "GET", "/v1/sources/archive/items/fixture-set", None).await;
        assert_eq!(st, StatusCode::OK, "{it}");
        assert_eq!(
            it["tracks"].as_array().unwrap().len(),
            1,
            "one file per recording"
        );
        assert_eq!(it["tracks"][0]["format"], "Flac");
        let url = it["tracks"][0]["url"].as_str().unwrap().to_string();
        assert!(
            url.starts_with(&format!("http://127.0.0.1:{port}/download/fixture-set/")),
            "{url}"
        );
        let (_, zones) = call(&app, "GET", "/v1/zones", None).await;
        let zid = zones[0]["id"].as_str().unwrap().to_string();
        let (st, q) = call(
            &app,
            "POST",
            &format!("/v1/zones/{zid}/queue"),
            Some(serde_json::json!({"items": [{"uri": url, "title": "One", "album": "Fixture Set"}], "replace": true, "play": true})),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{q}");
        let zone = s.zones.get(&ZoneId::new(&zid)).unwrap();
        assert_eq!(zone.wait_stopped().await.error, None);
    }

    /// Audius against a local mock: the discovery host is configured, a search
    /// yields track hits, an item has one streamable track, and it plays.
    #[tokio::test]
    async fn audius_source_searches_and_plays() {
        let flac = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../pm-audio/tests/fixtures/60_mono_audio.flac"
        ))
        .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let track = serde_json::json!({"id": "W0NJ0", "title": "Ambient Piano Interlude", "user": {"name": "Techwipe"}, "duration": 5,
            "permalink": "/techwipe/ambient-piano-interlude-353852", "artwork": {"480x480": "https://img/480"}, "play_count": 3});
        let t1 = track.clone();
        let t2 = track.clone();
        let mock = Router::new()
            .route(
                "/v1/tracks/search",
                get(move || {
                    let t = t1.clone();
                    async move { Json(serde_json::json!({"data": [t]})) }
                }),
            )
            .route(
                "/v1/tracks/{id}",
                get(move || {
                    let t = t2.clone();
                    async move { Json(serde_json::json!({"data": t})) }
                }),
            )
            .route(
                "/v1/tracks/{id}/stream",
                get(move || {
                    let flac = flac.clone();
                    async move { ([(header::CONTENT_TYPE, "audio/flac")], flac) }
                }),
            );
        tokio::spawn(async move { axum::serve(listener, mock).await.unwrap() });

        let dir = tempfile::tempdir().unwrap();
        let s = state(dir.path(), None).await;
        *s.audius_base.write().unwrap() = format!("http://127.0.0.1:{port}");
        let app = router(s.clone());
        let (_, sources) = call(&app, "GET", "/v1/sources", None).await;
        assert!(sources.as_array().unwrap().iter().any(|x| x["id"] == "audius"));
        let (st, _) = call(&app, "GET", "/v1/sources/nope/search?q=x", None).await;
        assert_eq!(st, StatusCode::NOT_FOUND);
        let (st, r) = call(&app, "GET", "/v1/sources/audius/search?q=ambient", None).await;
        assert_eq!(st, StatusCode::OK, "{r}");
        assert_eq!(r["source"], "audius");
        assert_eq!(r["items"][0]["kind"], "track");
        assert_eq!(
            r["items"][0]["url"],
            "https://audius.co/techwipe/ambient-piano-interlude-353852"
        );
        let (st, it) = call(&app, "GET", "/v1/sources/audius/items/W0NJ0", None).await;
        assert_eq!(st, StatusCode::OK, "{it}");
        let url = it["tracks"][0]["url"].as_str().unwrap().to_string();
        assert!(
            url.ends_with("/v1/tracks/W0NJ0/stream?app_name=pubky-music-node"),
            "{url}"
        );
        let (_, zones) = call(&app, "GET", "/v1/zones", None).await;
        let zid = zones[0]["id"].as_str().unwrap().to_string();
        let (st, q) = call(
            &app,
            "POST",
            &format!("/v1/zones/{zid}/queue"),
            Some(serde_json::json!({"items": [{"uri": url, "title": "Ambient Piano Interlude"}], "replace": true, "play": true})),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{q}");
        let zone = s.zones.get(&ZoneId::new(&zid)).unwrap();
        assert_eq!(zone.wait_stopped().await.error, None);
    }

    /// Deezer against a local mock: artists, albums and tracks come back from
    /// one search with ISRC/UPC, an album item lists its tracks, versions of a
    /// library track/album are found by ISRC/UPC, the browser proxy redirects
    /// to the preview, and a `deezer:track:` URI plays through the zone.
    #[tokio::test]
    async fn deezer_source_searches_matches_and_plays_previews() {
        let flac = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../pm-audio/tests/fixtures/60_mono_audio.flac"
        ))
        .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let preview = format!("http://127.0.0.1:{port}/preview.mp3");
        let track = serde_json::json!({"id": 138547415, "title": "Creep", "isrc": "GBAYE9200070", "duration": 238, "rank": 9,
            "preview": preview, "artist": {"id": 399, "name": "Radiohead"},
            "album": {"id": 12047952, "title": "Pablo Honey", "cover_medium": "http://img/pablo.jpg", "release_date": "1993-02-22"}});
        let album = serde_json::json!({"id": 12047952, "title": "Pablo Honey", "upc": "724381230321", "release_date": "1993-02-22", "label": "Parlophone",
            "cover_big": "http://img/pablo-big.jpg", "artist": {"id": 399, "name": "Radiohead"}, "tracks": {"data": [track]}});
        let artist = serde_json::json!({"id": 399, "name": "Radiohead", "nb_fan": 1000, "picture_big": "http://img/rh.jpg"});
        let (t1, t2, t3, a1, a2, a3, a4, ar1, ar2, ar3) = (
            track.clone(),
            track.clone(),
            track.clone(),
            album.clone(),
            album.clone(),
            album.clone(),
            album.clone(),
            artist.clone(),
            artist.clone(),
            artist.clone(),
        );
        let mock = Router::new()
            .route(
                "/search/track",
                get(move || {
                    let t = t1.clone();
                    async move { Json(serde_json::json!({"data": [t]})) }
                }),
            )
            .route(
                "/search/album",
                get(move || {
                    let a = a1.clone();
                    async move { Json(serde_json::json!({"data": [a]})) }
                }),
            )
            .route(
                "/search/artist",
                get(move || {
                    let a = ar1.clone();
                    async move { Json(serde_json::json!({"data": [a]})) }
                }),
            )
            .route(
                "/album/{id}",
                get(move |Path(id): Path<String>| {
                    let a = a2.clone();
                    async move {
                        if id == "12047952" || id == "upc:724381230321" {
                            Json(a)
                        } else {
                            Json(serde_json::json!({"error": {"code": 800, "message": "no data"}}))
                        }
                    }
                }),
            )
            .route(
                "/track/{id}",
                get(move |Path(id): Path<String>| {
                    let t = t2.clone();
                    async move {
                        if id == "138547415" || id == "isrc:GBAYE9200070" {
                            Json(t)
                        } else {
                            Json(serde_json::json!({"error": {"code": 800, "message": "no data"}}))
                        }
                    }
                }),
            )
            .route(
                "/artist/{id}",
                get(move || {
                    let a = ar2.clone();
                    async move { Json(a) }
                }),
            )
            .route(
                "/artist/{id}/albums",
                get(move || {
                    let a = a3.clone();
                    async move { Json(serde_json::json!({"data": [a]})) }
                }),
            )
            .route(
                "/preview.mp3",
                get(move || {
                    let flac = flac.clone();
                    async move { ([(header::CONTENT_TYPE, "audio/flac")], flac) }
                }),
            );
        let _ = (t3, a4, ar3);
        tokio::spawn(async move { axum::serve(listener, mock).await.unwrap() });

        let dir = tempfile::tempdir().unwrap();
        let s = state(dir.path(), None).await;
        *s.deezer_base.write().unwrap() = format!("http://127.0.0.1:{port}");
        let app = router(s.clone());
        let (st, r) = call(&app, "GET", "/v1/sources/deezer/search?q=creep", None).await;
        assert_eq!(st, StatusCode::OK, "{r}");
        let kinds: Vec<&str> = r["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|h| h["kind"].as_str().unwrap())
            .collect();
        assert_eq!(kinds, ["artist", "album", "track"]);
        assert_eq!(r["items"][2]["uri"], "deezer:track:138547415");
        assert_eq!(r["items"][2]["isrc"], "GBAYE9200070");
        assert_eq!(r["items"][2]["duration_ms"], 238_000);
        assert_eq!(r["items"][1]["upc"], "724381230321");
        let (st, it) = call(&app, "GET", "/v1/sources/deezer/items/album:12047952", None).await;
        assert_eq!(st, StatusCode::OK, "{it}");
        assert_eq!(it["tracks"][0]["url"], "deezer:track:138547415");
        assert_eq!(it["image"], "http://img/pablo-big.jpg");
        let (st, _) = call(&app, "GET", "/v1/sources/deezer/items/album:1", None).await;
        assert_eq!(st, StatusCode::NOT_FOUND);
        let (st, ar) = call(&app, "GET", "/v1/sources/deezer/artists/artist:399", None).await;
        assert_eq!(st, StatusCode::OK, "{ar}");
        assert_eq!(ar["albums"][0]["id"], "album:12047952");
        // The unified search shows Deezer without any configuration.
        let (st, u) = call(&app, "GET", "/v1/library/search?q=creep", None).await;
        assert_eq!(st, StatusCode::OK, "{u}");
        assert!(
            u["sources"]
                .as_array()
                .unwrap()
                .iter()
                .any(|x| x["source"] == "deezer"),
            "{u}"
        );
        // Versions of a library track/album with the same ISRC/UPC.
        let tid = {
            let lib = s.library.lock().unwrap();
            let t = pm_model::Track {
                id: None,
                title: "Creep".into(),
                artists: vec![pm_model::Artist {
                    id: None,
                    name: "Radiohead".into(),
                    mbid: None,
                }],
                album: Some(pm_model::Album {
                    id: None,
                    title: "Pablo Honey".into(),
                    album_artist: Some("Radiohead".into()),
                    release_mbid: None,
                    release_group_mbid: None,
                    year: Some(1993),
                    upc: Some(pm_model::Upc::parse("724381230321").unwrap()),
                    total_discs: None,
                }),
                track_no: Some(2),
                disc_no: None,
                duration_ms: Some(238_000),
                file: pm_model::AudioFile {
                    path: dir.path().join("creep.flac"),
                    size_bytes: 1,
                    mtime_unix: 0,
                    container: "flac".into(),
                    format: pm_model::AudioFormat::pcm(44_100, 16, 2),
                    duration_ms: Some(238_000),
                },
                recording_mbid: None,
                isrc: Some(pm_model::Isrc::parse("GBAYE9200070").unwrap()),
                acoustid: None,
                canonical: None,
                replaygain_track_gain_db: None,
                replaygain_album_gain_db: None,
                genres: vec![],
            };
            lib.upsert_track(&t).unwrap().0
        };
        let (st, v) = call(&app, "GET", &format!("/v1/library/tracks/{tid}/versions"), None).await;
        assert_eq!(st, StatusCode::OK, "{v}");
        assert_eq!(v[0]["source"], "deezer");
        assert_eq!(v[0]["uri"], "deezer:track:138547415");
        let (_, t) = call(&app, "GET", &format!("/v1/library/tracks/{tid}"), None).await;
        let aid = t["album"]["id"].as_i64().unwrap();
        let (st, v) = call(&app, "GET", &format!("/v1/library/albums/{aid}/versions"), None).await;
        assert_eq!(st, StatusCode::OK, "{v}");
        assert_eq!(v[0]["id"], "album:12047952");
        // Browser proxy: a redirect to the preview.
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/v1/sources/deezer/stream/138547415")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::TEMPORARY_REDIRECT);
        assert_eq!(
            resp.headers()["location"],
            format!("http://127.0.0.1:{port}/preview.mp3")
        );
        // And the zone plays the URI itself.
        let (_, zones) = call(&app, "GET", "/v1/zones", None).await;
        let zid = zones[0]["id"].as_str().unwrap().to_string();
        let (st, q) = call(
            &app,
            "POST",
            &format!("/v1/zones/{zid}/queue"),
            Some(serde_json::json!({"items": [{"uri": "deezer:track:138547415", "title": "Creep"}], "replace": true, "play": true})),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{q}");
        let zone = s.zones.get(&ZoneId::new(&zid)).unwrap();
        assert_eq!(zone.wait_stopped().await.error, None);
    }

    /// Library radio: with the flag on, a zone that runs out of queue gets
    /// similar tracks appended and keeps playing.
    #[tokio::test]
    async fn backup_downloads_a_sqlite_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let s = state(dir.path(), None).await;
        let app = router(s.clone());
        let req = Request::builder()
            .method("GET")
            .uri("/v1/backup")
            .header("authorization", "Bearer t")
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let disposition = resp
            .headers()
            .get("content-disposition")
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        assert!(disposition.contains("pubky-music-"), "{disposition}");
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        assert!(body.starts_with(b"SQLite format 3\0"));
        let copy = dir.path().join("copy.db");
        std::fs::write(&copy, &body).unwrap();
        assert!(pm_library::Library::open(&copy).is_ok());
    }

    #[tokio::test]
    async fn lyrics_come_from_a_sidecar_lrc() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("music")).unwrap();
        write_wav(&dir.path().join("music/song.wav"), 4410);
        std::fs::write(
            dir.path().join("music/song.lrc"),
            "[ti:Song]\n[00:01.00]Hello\n[00:02.50]World\n",
        )
        .unwrap();
        write_wav(&dir.path().join("music/mute.wav"), 4410);
        let s = state(dir.path(), None).await;
        let app = router(s.clone());
        call(&app, "POST", "/v1/library/scan", Some(serde_json::json!({}))).await;
        for _ in 0..500 {
            if !s.is_scanning() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let (_, tracks) = call(&app, "GET", "/v1/library/tracks?limit=10", None).await;
        let id_of = |name: &str| {
            tracks["tracks"]
                .as_array()
                .unwrap()
                .iter()
                .find(|t| t["file"]["path"].as_str().unwrap().ends_with(name))
                .map(|t| t["id"].as_i64().unwrap())
                .unwrap()
        };
        let (st, l) = call(
            &app,
            "GET",
            &format!("/v1/library/tracks/{}/lyrics", id_of("song.wav")),
            None,
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{l}");
        assert_eq!(l["source"], "lrc");
        assert_eq!(l["synced"], true);
        assert_eq!(l["lines"].as_array().unwrap().len(), 2);
        assert_eq!(l["lines"][1]["ms"], 2500);
        assert_eq!(l["lines"][1]["text"], "World");
        let (st, _) = call(
            &app,
            "GET",
            &format!("/v1/library/tracks/{}/lyrics", id_of("mute.wav")),
            None,
        )
        .await;
        assert_eq!(st, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn a_queue_can_be_saved_as_a_playlist() {
        let dir = tempfile::tempdir().unwrap();
        let wav = dir.path().join("untagged.wav");
        write_wav(&wav, 441);
        let s = state(dir.path(), None).await;
        let app = router(s.clone());
        let (_, zones) = call(&app, "GET", "/v1/zones", None).await;
        let zid = zones[0]["id"].as_str().unwrap().to_string();
        let archive = "https://archive.org/download/gd1977-05-08/gd77-05-08d1t01.flac";
        let (st, _) = call(
            &app,
            "POST",
            &format!("/v1/zones/{zid}/queue"),
            Some(serde_json::json!({"items": [
                {"uri": archive, "title": "Scarlet Begonias"},
                {"uri": wav.to_string_lossy(), "title": "Untagged"},
            ], "replace": true})),
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        let (st, r) = call(
            &app,
            "POST",
            "/v1/playlists/from-queue",
            Some(serde_json::json!({"zone": zid})),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{r}");
        assert!(r["playlist"]["summary"]["name"]
            .as_str()
            .unwrap()
            .starts_with("Queue · "));
        assert_eq!(r["playlist"]["playlist"]["items"].as_array().unwrap().len(), 1);
        assert_eq!(r["playlist"]["playlist"]["items"][0]["uri"], archive);
        assert_eq!(r["skipped"], serde_json::json!(["Untagged"]));
        // Explicit URIs (the browser's queue) and a name of one's own.
        let (st, r) = call(
            &app,
            "POST",
            "/v1/playlists/from-queue",
            Some(serde_json::json!({"uris": [archive, archive], "name": "Twice"})),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{r}");
        assert_eq!(r["playlist"]["summary"]["name"], "Twice");
        assert_eq!(r["playlist"]["playlist"]["items"].as_array().unwrap().len(), 2);
        // Nothing canonical: 422, no playlist.
        let (st, _) = call(
            &app,
            "POST",
            "/v1/playlists/from-queue",
            Some(serde_json::json!({"uris": [wav.to_string_lossy()]})),
        )
        .await;
        assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);
        let (_, pls) = call(&app, "GET", "/v1/playlists", None).await;
        assert_eq!(pls.as_array().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn new_releases_come_from_musicbrainz_for_tagged_artists() {
        let artist_mbid = "056e4f3e-d505-4dad-8ec1-d04f521cbb56";
        let fresh_rg = "11111111-2222-3333-4444-555555555555";
        let owned_rg = "66666666-7777-8888-9999-000000000000";
        let today = time::OffsetDateTime::now_utc().date();
        let recent = (today - time::Duration::days(30)).to_string();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let hits2 = hits.clone();
        let mock = Router::new().route(
            "/release-group",
            get(move |Query(q): Query<std::collections::HashMap<String, String>>| {
                let hits = hits2.clone();
                let recent = recent.clone();
                async move {
                    hits.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    assert_eq!(q.get("artist").map(String::as_str), Some(artist_mbid));
                    Json(serde_json::json!({"release-group-count": 4, "release-group-offset": 0, "release-groups": [
                        {"id": fresh_rg, "title": "Brand New", "first-release-date": recent, "primary-type": "Album", "secondary-types": []},
                        {"id": owned_rg, "title": "Already Here", "first-release-date": recent, "primary-type": "EP", "secondary-types": []},
                        {"id": "aaaaaaaa-0000-0000-0000-000000000000", "title": "Old", "first-release-date": "2001-05-01", "primary-type": "Album", "secondary-types": []},
                        {"id": "bbbbbbbb-0000-0000-0000-000000000000", "title": "Live", "first-release-date": recent, "primary-type": "Album", "secondary-types": ["Live"]}
                    ]}))
                }
            }),
        );
        tokio::spawn(async move { axum::serve(listener, mock).await.unwrap() });

        let dir = tempfile::tempdir().unwrap();
        let s = state(dir.path(), None).await;
        s.info_bases.write().unwrap().musicbrainz = format!("http://127.0.0.1:{port}");
        s.set_contact("tests@example.org").ok();
        {
            let lib = s.library.lock().unwrap();
            let mut t = pm_model::Track {
                id: None,
                title: "Song".into(),
                artists: vec![pm_model::Artist {
                    id: None,
                    name: "Daft Punk".into(),
                    mbid: Some(pm_model::Mbid::parse(artist_mbid).unwrap()),
                }],
                album: Some(pm_model::Album {
                    id: None,
                    title: "Already Here".into(),
                    album_artist: Some("Daft Punk".into()),
                    release_mbid: None,
                    release_group_mbid: Some(pm_model::Mbid::parse(owned_rg).unwrap()),
                    year: Some(2026),
                    upc: None,
                    total_discs: None,
                }),
                track_no: Some(1),
                disc_no: None,
                duration_ms: Some(1000),
                file: pm_model::AudioFile {
                    path: dir.path().join("song.flac"),
                    size_bytes: 1,
                    mtime_unix: 0,
                    container: "flac".into(),
                    format: pm_model::AudioFormat::pcm(44_100, 16, 2),
                    duration_ms: Some(1000),
                },
                recording_mbid: None,
                isrc: None,
                acoustid: None,
                canonical: None,
                replaygain_track_gain_db: None,
                replaygain_album_gain_db: None,
                genres: vec![],
            };
            t.id = None;
            lib.upsert_track(&t).unwrap();
        }
        let app = router(s.clone());
        // First call: nothing cached yet, a refresh starts in the background.
        let (st, p) = call(&app, "GET", "/v1/library/new-releases", None).await;
        assert_eq!(st, StatusCode::OK, "{p}");
        assert_eq!(p["artists_total"], 1);
        let mut page = p;
        for _ in 0..300 {
            if page["artists_checked"] == 1 && page["refreshing"] == false {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            page = call(&app, "GET", "/v1/library/new-releases", None).await.1;
        }
        assert_eq!(page["artists_checked"], 1, "{page}");
        let items = page["items"].as_array().unwrap();
        let titles: Vec<&str> = items.iter().map(|i| i["title"].as_str().unwrap()).collect();
        assert_eq!(
            titles,
            ["Brand New", "Already Here"],
            "old and live records are left out"
        );
        assert!(items[0].get("in_library").is_none());
        assert_eq!(items[1]["in_library"]["title"], "Already Here");
        assert_eq!(
            items[0]["url"],
            format!("https://musicbrainz.org/release-group/{fresh_rg}")
        );
        // Cached for a week: another page read does not hit MusicBrainz again.
        let before = hits.load(std::sync::atomic::Ordering::SeqCst);
        call(&app, "GET", "/v1/library/new-releases", None).await;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), before);
    }

    #[tokio::test]
    async fn a_volume_limit_caps_the_dsp_volume_and_requests() {
        let dir = tempfile::tempdir().unwrap();
        let s = state(dir.path(), None).await;
        let app = router(s.clone());
        let (_, zones) = call(&app, "GET", "/v1/zones", None).await;
        let zid = zones[0]["id"].as_str().unwrap().to_string();
        let (st, _) = call(
            &app,
            "PUT",
            &format!("/v1/zones/{zid}/dsp"),
            Some(serde_json::json!({"max_volume_db": 3.0})),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        let (st, cfg) = call(
            &app,
            "PUT",
            &format!("/v1/zones/{zid}/dsp"),
            Some(serde_json::json!({"max_volume_db": -20.0, "volume_db": -6.0})),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{cfg}");
        assert_eq!(cfg["volume_db"], -20.0, "software volume is capped at the limit");
        let (_, cfg) = call(&app, "GET", &format!("/v1/zones/{zid}/dsp"), None).await;
        assert_eq!(cfg["max_volume_db"], -20.0);
        // Requests are capped before they reach the output.
        use pm_model::Volume;
        assert_eq!(cap_volume(Volume::Db(-3.0), Some(-20.0)), Volume::Db(-20.0));
        assert_eq!(cap_volume(Volume::Db(-30.0), Some(-20.0)), Volume::Db(-30.0));
        assert_eq!(cap_volume(Volume::Db(-3.0), None), Volume::Db(-3.0));
        assert_eq!(cap_volume(Volume::Up, Some(-20.0)), Volume::Up);
        // The null output has no volume control at all: the request is refused as before.
        let (st, body) = call(
            &app,
            "POST",
            &format!("/v1/zones/{zid}/volume"),
            Some(serde_json::json!({"volume": {"db": -3.0}})),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{body}");
    }

    /// A renderer on another subnet is added by its description URL, becomes
    /// an output and a zone, is remembered for the next start, and can be
    /// forgotten again.
    #[tokio::test]
    async fn a_renderer_can_be_added_by_address() {
        let desc = r#"<?xml version="1.0"?>
<root xmlns="urn:schemas-upnp-org:device-1-0">
  <device>
    <deviceType>urn:schemas-upnp-org:device:MediaRenderer:1</deviceType>
    <friendlyName>Living Room DSM</friendlyName>
    <manufacturer>Linn Products Ltd.</manufacturer>
    <UDN>uuid:4c494e4e-0026-0f21-cc3f-01234567890a</UDN>
    <serviceList>
      <service>
        <serviceType>urn:schemas-upnp-org:service:AVTransport:1</serviceType>
        <serviceId>urn:upnp-org:serviceId:AVTransport</serviceId>
        <controlURL>/AVTransport/control</controlURL>
      </service>
    </serviceList>
  </device>
</root>"#;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let mock = Router::new().route(
            "/desc.xml",
            get(move || async move { ([(header::CONTENT_TYPE, "text/xml")], desc) }),
        );
        tokio::spawn(async move { axum::serve(listener, mock).await.unwrap() });
        let dir = tempfile::tempdir().unwrap();
        let s = state(dir.path(), None).await;
        let app = router(s.clone());
        let address = format!("http://127.0.0.1:{port}/desc.xml");
        let (st, outs) = call(
            &app,
            "POST",
            "/v1/outputs/upnp",
            Some(serde_json::json!({"address": address})),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{outs}");
        assert!(
            outs.as_array()
                .unwrap()
                .iter()
                .any(|o| o["name"] == "Living Room DSM"),
            "{outs}"
        );
        let (_, zones) = call(&app, "GET", "/v1/zones", None).await;
        assert!(zones
            .as_array()
            .unwrap()
            .iter()
            .any(|z| z["name"] == "Living Room DSM"));
        let (_, rep) = call(&app, "GET", "/v1/outputs/discovery", None).await;
        assert_eq!(rep["saved_addresses"][0], address);
        // Adding again is idempotent; a dead address is a 400 with the reason.
        let (st, _) = call(
            &app,
            "POST",
            "/v1/outputs/upnp",
            Some(serde_json::json!({"address": address})),
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        let (st, e) = call(
            &app,
            "POST",
            "/v1/outputs/upnp",
            Some(serde_json::json!({"address": "127.0.0.1:1"})),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{e}");
        assert!(e["error"].as_str().unwrap().contains("did not answer"), "{e}");
        let enc = address.replace('/', "%2F").replace(':', "%3A");
        let (st, left) = call(&app, "DELETE", &format!("/v1/outputs/upnp/{enc}"), None).await;
        assert_eq!(st, StatusCode::OK, "{left}");
        assert_eq!(left.as_array().unwrap().len(), 0);
    }

    /// Behind a reverse proxy with no public_url, the TIDAL redirect URI is
    /// built from the request the browser made.
    #[tokio::test]
    async fn the_tidal_redirect_follows_the_request_host() {
        let dir = tempfile::tempdir().unwrap();
        let s = state(dir.path(), None).await;
        let app = router(s);
        let req = Request::builder()
            .uri("/v1/sources/tidal/status")
            .header("host", "music.example")
            .header("x-forwarded-proto", "https")
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["redirect_uri"], "https://music.example/tidal/callback");
    }

    #[tokio::test]
    async fn the_discovery_report_explains_an_empty_network() {
        let dir = tempfile::tempdir().unwrap();
        let s = state(dir.path(), None).await;
        let app = router(s);
        let (st, v) = call(&app, "GET", "/v1/outputs/discovery", None).await;
        assert_eq!(st, StatusCode::OK, "{v}");
        assert_eq!(v["upnp_enabled"], false);
        assert_eq!(v["upnp"].as_array().unwrap().len(), 0);
        assert_eq!(v["public_url"], "http://127.0.0.1:7788");
        let notes: Vec<String> = serde_json::from_value(v["notes"].clone()).unwrap();
        assert!(notes.iter().any(|n| n.contains("both off")), "{notes:?}");
        assert!(
            notes
                .iter()
                .any(|n| n.contains("public_url is http://127.0.0.1:7788")),
            "{notes:?}"
        );
    }

    #[tokio::test]
    async fn the_jukebox_zone_is_a_node_setting() {
        let dir = tempfile::tempdir().unwrap();
        let s = state(dir.path(), None).await;
        let app = router(s.clone());
        let (_, zones) = call(&app, "GET", "/v1/zones", None).await;
        let zid = zones[0]["id"].as_str().unwrap().to_string();
        let (_, v) = call(&app, "GET", "/v1/node/settings", None).await;
        assert!(v.get("jukebox_zone").is_none());
        let (st, _) = call(
            &app,
            "PUT",
            "/v1/node/settings",
            Some(serde_json::json!({"jukebox_zone": "nope"})),
        )
        .await;
        assert_eq!(st, StatusCode::NOT_FOUND);
        let (st, v) = call(
            &app,
            "PUT",
            "/v1/node/settings",
            Some(serde_json::json!({"jukebox_zone": zid})),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{v}");
        assert_eq!(v["jukebox_zone"], zid);
        let (_, v) = call(
            &app,
            "PUT",
            "/v1/node/settings",
            Some(serde_json::json!({"jukebox_zone": ""})),
        )
        .await;
        assert!(v.get("jukebox_zone").is_none());
    }

    #[tokio::test]
    async fn tidal_searches_signs_in_and_lists_albums() {
        use axum::extract::RawQuery;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let base = format!("http://127.0.0.1:{port}");
        let tokens = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let tokens2 = tokens.clone();
        let album = serde_json::json!({"id": "20115556", "type": "albums", "attributes": {"title": "Random Access Memories", "releaseDate": "2013-05-17", "mediaTags": ["LOSSLESS"], "barcodeId": "886443927087"},
            "relationships": {"artists": {"data": [{"id": "3996", "type": "artists"}]}, "coverArt": {"data": [{"id": "art1", "type": "artworks"}]},
                              "items": {"data": [{"id": "20115564", "type": "tracks", "meta": {"trackNumber": 8, "volumeNumber": 1}}, {"id": "20115557", "type": "tracks", "meta": {"trackNumber": 1, "volumeNumber": 1}}]}}});
        let artist =
            serde_json::json!({"id": "3996", "type": "artists", "attributes": {"name": "Daft Punk"}});
        let art = serde_json::json!({"id": "art1", "type": "artworks", "attributes": {"files": [{"href": "https://img/160.jpg", "meta": {"width": 160, "height": 160}}, {"href": "https://img/640.jpg", "meta": {"width": 640, "height": 640}}]}});
        let track = |id: &str, title: &str| {
            serde_json::json!({"id": id, "type": "tracks", "attributes": {"title": title, "isrc": "USQX91300108", "duration": "PT6M9S", "mediaTags": ["HIRES_LOSSLESS"]},
            "relationships": {"artists": {"data": [{"id": "3996", "type": "artists"}]}, "albums": {"data": [{"id": "20115556", "type": "albums"}]}}})
        };
        let (t1, t2) = (
            track("20115564", "Get Lucky"),
            track("20115557", "Give Life Back to Music"),
        );
        let mock = Router::new()
            .route(
                "/v1/oauth2/token",
                post(move |body: String| {
                    let tokens = tokens2.clone();
                    async move {
                        let grant = body.split('&').find(|p| p.starts_with("grant_type=")).unwrap_or("").to_string();
                        tokens.lock().unwrap().push(body.clone());
                        if grant.ends_with("client_credentials") {
                            Json(serde_json::json!({"access_token": "app-token", "expires_in": 14400, "token_type": "Bearer"}))
                        } else {
                            assert!(body.contains("code_verifier="), "{body}");
                            assert!(body.contains("code=the-code"), "{body}");
                            Json(serde_json::json!({"access_token": "user-token", "refresh_token": "refresh-1", "expires_in": 3600, "user_id": 4242, "scope": "playback collection.read"}))
                        }
                    }
                }),
            )
            .route("/users/{id}", get(|| async { Json(serde_json::json!({"data": {"id": "4242", "type": "users", "attributes": {"country": "DE"}}})) }))
            .route(
                "/searchResults",
                get({
                    let (album, t1) = (album.clone(), t1.clone());
                    move |RawQuery(q): RawQuery| {
                        let (album, t1) = (album.clone(), t1.clone());
                        async move {
                            let q = q.unwrap_or_default();
                            assert!(q.contains("filter%5Bquery%5D=daft+punk") || q.contains("filter%5Bquery%5D=daft%20punk"), "{q}");
                            if q.contains("include=albums") {
                                Json(serde_json::json!({"data": [{"id": "s", "type": "searchResults", "relationships": {"albums": {"data": [{"id": "20115556", "type": "albums"}]}}}], "included": [album]}))
                            } else if q.contains("include=artists") {
                                Json(serde_json::json!({"data": [{"id": "s", "type": "searchResults", "relationships": {"artists": {"data": [{"id": "3996", "type": "artists"}]}}}]}))
                            } else {
                                Json(serde_json::json!({"data": [{"id": "s", "type": "searchResults", "relationships": {"tracks": {"data": [{"id": "20115564", "type": "tracks"}]}}}], "included": [t1]}))
                            }
                        }
                    }
                }),
            )
            .route("/albums", get({ let (album, artist, art) = (album.clone(), artist.clone(), art.clone()); move || async move { Json(serde_json::json!({"data": [album], "included": [artist, art]})) } }))
            .route("/albums/{id}", get({ let (album, artist, art, t1, t2) = (album.clone(), artist.clone(), art.clone(), t1.clone(), t2.clone()); move || async move { Json(serde_json::json!({"data": album, "included": [artist, art, t1, t2]})) } }))
            .route("/tracks", get({ let (t1, artist, album) = (t1.clone(), artist.clone(), album.clone()); move || async move { Json(serde_json::json!({"data": [t1], "included": [artist, album]})) } }))
            .route("/tracks/{id}", get({ let (t1, artist, album, art) = (t1.clone(), artist.clone(), album.clone(), art.clone()); move || async move { Json(serde_json::json!({"data": t1, "included": [artist, album, art]})) } }))
            .route(
                "/userCollectionTracks/{uid}/relationships/items",
                get(|Path(uid): Path<String>| async move {
                    assert_eq!(uid, "me");
                    Json(serde_json::json!({"data": [{"id": "20115564", "type": "tracks"}]}))
                }),
            )
            .route(
                "/userCollectionAlbums/{uid}/relationships/items",
                get(|| async { Json(serde_json::json!({"data": [{"id": "20115556", "type": "albums"}]})) }),
            )
            .route(
                "/playlists",
                get(|RawQuery(q): RawQuery| async move {
                    assert!(q.unwrap_or_default().contains("owners.id%5D=4242"), "owner filter");
                    Json(serde_json::json!({"data": [{"id": "pl-1", "type": "playlists", "attributes": {"name": "Road trip", "description": "", "numberOfTrackItems": 2}}]}))
                }),
            )
            .route(
                "/playlists/{id}",
                get(|| async { Json(serde_json::json!({"data": {"id": "pl-1", "type": "playlists", "attributes": {"name": "Road trip"}}})) }),
            )
            .route(
                "/playlists/{id}/relationships/items",
                get({
                    let (t1, t2) = (t1.clone(), t2.clone());
                    move || async move {
                        let mut t2 = t2.clone();
                        t2["attributes"]["isrc"] = serde_json::Value::Null;
                        Json(serde_json::json!({"data": [{"id": "20115564", "type": "tracks"}, {"id": "20115557", "type": "tracks"}, {"id": "v1", "type": "videos"}], "included": [t1, t2]}))
                    }
                }),
            )
            .route(
                "/artists",
                get(|| async {
                    Json(serde_json::json!({"data": [{"id": "3996", "type": "artists", "attributes": {"name": "Daft Punk", "numberOfFollowers": 12}, "relationships": {"profileArt": {"data": [{"id": "pa1", "type": "artworks"}]}}}],
                        "included": [{"id": "pa1", "type": "artworks", "attributes": {"files": [{"href": "https://img/artist.jpg", "meta": {"width": 640, "height": 640}}]}}]}))
                }),
            )
            .route(
                "/artists/{id}/relationships/albums",
                get(|| async { Json(serde_json::json!({"data": [{"id": "20115556", "type": "albums"}]})) }),
            );
        tokio::spawn(async move { axum::serve(listener, mock).await.unwrap() });

        let dir = tempfile::tempdir().unwrap();
        let s = state(dir.path(), None).await;
        {
            let mut ep = s.tidal.write().unwrap();
            ep.api_base = base.clone();
            ep.auth_base = base.clone();
            ep.login_base = base.clone();
        }
        let app = router(s.clone());
        // Not configured: the source says so and login refuses.
        let (_, srcs) = call(&app, "GET", "/v1/sources", None).await;
        let tidal = srcs
            .as_array()
            .unwrap()
            .iter()
            .find(|x| x["id"] == "tidal")
            .unwrap();
        assert!(tidal["status"].as_str().unwrap().starts_with("not configured"));
        let (st, _) = call(&app, "GET", "/v1/sources/tidal/login", None).await;
        assert_eq!(st, StatusCode::PRECONDITION_FAILED);
        s.tidal.write().unwrap().creds = tidal::TidalCredentials {
            client_id: "cid".into(),
            client_secret: "sec".into(),
            scopes: String::new(),
        };
        // Search with the app token: albums carry artists and art, tracks carry artists.
        let (st, r) = call(
            &app,
            "GET",
            "/v1/sources/tidal/search?q=daft%20punk&limit=10",
            None,
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{r}");
        let items = r["items"].as_array().unwrap();
        assert_eq!(items[0]["kind"], "artist", "artists come first: {r}");
        assert_eq!(items[0]["id"], "artist:3996");
        assert_eq!(items[0]["image"], "https://img/artist.jpg");
        assert_eq!(items[1]["kind"], "album");
        assert_eq!(items[1]["creator"], "Daft Punk");
        assert_eq!(items[1]["image"], "https://img/640.jpg");
        assert_eq!(items[1]["url"], "https://tidal.com/browse/album/20115556");
        assert_eq!(items[2]["kind"], "track");
        assert_eq!(items[2]["title"], "Get Lucky");
        assert_eq!(items[2]["creator"], "Daft Punk");
        assert_eq!(items[2]["duration_ms"], 369_000);
        // An artist page lists the artist's albums with library matches.
        let (st, a) = call(&app, "GET", "/v1/sources/tidal/artists/artist:3996", None).await;
        assert_eq!(st, StatusCode::OK, "{a}");
        assert_eq!(a["name"], "Daft Punk");
        assert_eq!(a["albums"][0]["id"], "album:20115556");
        let (st, _) = call(&app, "GET", "/v1/sources/archive/artists/x", None).await;
        assert_eq!(st, StatusCode::NOT_FOUND);
        assert!(tokens
            .lock()
            .unwrap()
            .iter()
            .any(|b| b.contains("client_credentials")));
        // An album item lists its tracks in order with hi-res labels and tidal: URIs.
        let (st, it) = call(&app, "GET", "/v1/sources/tidal/items/album:20115556", None).await;
        assert_eq!(st, StatusCode::OK, "{it}");
        assert_eq!(it["title"], "Random Access Memories");
        assert_eq!(it["creator"], "Daft Punk");
        assert_eq!(it["year"], 2013);
        let tracks = it["tracks"].as_array().unwrap();
        assert_eq!(tracks.len(), 2);
        assert_eq!(tracks[0]["track_no"], 8);
        assert_eq!(tracks[0]["url"], "tidal:track:20115564");
        assert_eq!(tracks[0]["duration_ms"], 369_000);
        assert_eq!(tracks[0]["format"], "FLAC hi-res (TIDAL)");
        let (st, _) = call(&app, "GET", "/v1/sources/tidal/items/nope", None).await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        // Unified search: the library answer carries TIDAL hits, flagged when the
        // library already has the same ISRC or UPC.
        {
            let lib = s.library.lock().unwrap();
            let mut t = pm_model::Track {
                id: None,
                title: "Get Lucky".into(),
                artists: vec![pm_model::Artist {
                    id: None,
                    name: "Daft Punk".into(),
                    mbid: None,
                }],
                album: Some(pm_model::Album {
                    id: None,
                    title: "Random Access Memories".into(),
                    album_artist: Some("Daft Punk".into()),
                    release_mbid: None,
                    release_group_mbid: None,
                    year: Some(2013),
                    upc: pm_model::Upc::parse("886443927087").ok(),
                    total_discs: None,
                }),
                track_no: Some(8),
                disc_no: None,
                duration_ms: Some(369_000),
                file: pm_model::AudioFile {
                    path: dir.path().join("get-lucky.flac"),
                    size_bytes: 1,
                    mtime_unix: 0,
                    container: "flac".into(),
                    format: pm_model::AudioFormat::pcm(44_100, 16, 2),
                    duration_ms: Some(369_000),
                },
                recording_mbid: None,
                isrc: pm_model::Isrc::parse("USQX91300108").ok(),
                acoustid: None,
                canonical: Some((
                    pm_model::MusicUri::Isrc {
                        isrc: pm_model::Isrc::parse("USQX91300108").unwrap(),
                    },
                    pm_model::Confidence::TaggedIsrc,
                )),
                replaygain_track_gain_db: None,
                replaygain_album_gain_db: None,
                genres: vec![],
            };
            t.id = None;
            lib.upsert_track(&t).unwrap();
        }
        let (st, r) = call(&app, "GET", "/v1/library/search?q=daft%20punk", None).await;
        assert_eq!(st, StatusCode::OK, "{r}");
        assert_eq!(r["tracks"][0]["title"], "Get Lucky");
        let src = r["sources"].as_array().unwrap();
        assert_eq!(src.len(), 1, "{r}");
        assert_eq!(src[0]["source"], "tidal");
        let tidal_hits = src[0]["items"].as_array().unwrap();
        let track_hit = tidal_hits.iter().find(|h| h["kind"] == "track").unwrap();
        assert_eq!(track_hit["isrc"], "USQX91300108");
        assert!(track_hit["library_track_id"].is_i64(), "{track_hit}");
        let album_hit = tidal_hits.iter().find(|h| h["kind"] == "album").unwrap();
        assert_eq!(album_hit["upc"], "886443927087");
        assert!(album_hit["library_album_id"].is_i64(), "{album_hit}");
        let (_, r) = call(&app, "GET", "/v1/library/search?q=daft%20punk&sources=0", None).await;
        assert!(r["sources"].as_array().unwrap().is_empty());
        // Versions: the library album and track point back at TIDAL by UPC / ISRC.
        let lid = track_hit["library_track_id"].as_i64().unwrap();
        let aid = album_hit["library_album_id"].as_i64().unwrap();
        let (st, v) = call(&app, "GET", &format!("/v1/library/albums/{aid}/versions"), None).await;
        assert_eq!(st, StatusCode::OK, "{v}");
        assert_eq!(v[0]["id"], "album:20115556");
        assert!(v[0]["label"].as_str().unwrap().starts_with("TIDAL"));
        let (st, v) = call(&app, "GET", &format!("/v1/library/tracks/{lid}/versions"), None).await;
        assert_eq!(st, StatusCode::OK, "{v}");
        assert_eq!(v[0]["uri"], "tidal:track:20115564");
        // Sign-in: the login URL carries PKCE, the callback exchanges the code and stores the user.
        let (st, l) = call(&app, "GET", "/v1/sources/tidal/login", None).await;
        assert_eq!(st, StatusCode::OK);
        let url = url::Url::parse(l["url"].as_str().unwrap()).unwrap();
        let q: std::collections::HashMap<String, String> = url.query_pairs().into_owned().collect();
        assert_eq!(q["code_challenge_method"], "S256");
        assert!(q["scope"].contains("playback"));
        assert!(q["redirect_uri"].ends_with("/tidal/callback"));
        let state_param = q["state"].clone();
        let req = Request::builder()
            .uri(format!("/tidal/callback?code=the-code&state={state_param}"))
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::SEE_OTHER, "callback redirects");
        assert_eq!(resp.headers().get("location").unwrap(), "/?tidal=signed_in");
        let (_, stv) = call(&app, "GET", "/v1/sources/tidal/status", None).await;
        assert_eq!(stv["signed_in"], true, "{stv}");
        assert_eq!(stv["user_id"], "4242");
        assert_eq!(stv["country"], "DE");
        let (_, srcs) = call(&app, "GET", "/v1/sources", None).await;
        let tidal = srcs
            .as_array()
            .unwrap()
            .iter()
            .find(|x| x["id"] == "tidal")
            .unwrap();
        assert_eq!(tidal["status"], "ready");
        // Signed in: the collection lists, and imports land as favourites and a Pubky playlist.
        let (st, c) = call(&app, "GET", "/v1/sources/tidal/collection", None).await;
        assert_eq!(st, StatusCode::OK, "{c}");
        assert_eq!(c["tracks"][0]["title"], "Get Lucky");
        assert_eq!(c["albums"][0]["creator"], "Daft Punk");
        let (st, r) = call(&app, "POST", "/v1/sources/tidal/collection/import", None).await;
        assert_eq!(st, StatusCode::OK, "{r}");
        assert_eq!(r["imported"], 2);
        let (_, favs) = call(&app, "GET", "/v1/favourites", None).await;
        let uris: Vec<&str> = favs["uris"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|u| u.as_str())
            .collect();
        assert!(
            uris.contains(&"https://musicbrainz.org/isrc/USQX91300108"),
            "{uris:?}"
        );
        assert!(
            uris.iter().any(|u| u.contains("tidal.com/album/20115556")),
            "{uris:?}"
        );
        let (st, pls) = call(&app, "GET", "/v1/sources/tidal/playlists", None).await;
        assert_eq!(st, StatusCode::OK, "{pls}");
        assert_eq!(pls[0]["name"], "Road trip");
        assert_eq!(pls[0]["items"], 2);
        let (st, r) = call(&app, "POST", "/v1/sources/tidal/playlists/pl-1/import", None).await;
        assert_eq!(st, StatusCode::OK, "{r}");
        assert_eq!(
            r["imported"], 2,
            "ISRC for one, the TIDAL link for the other; the video is skipped"
        );
        assert_eq!(r["skipped"], 1);
        let items = r["playlist"]["playlist"]["items"].as_array().unwrap();
        assert_eq!(items[0]["uri"], "https://musicbrainz.org/isrc/USQX91300108");
        assert!(items[1]["uri"]
            .as_str()
            .unwrap()
            .contains("tidal.com/track/20115557"));
        // A stale state is refused; sign-out forgets the tokens.
        let req = Request::builder()
            .uri("/tidal/callback?code=x&state=unknown")
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert!(resp
            .headers()
            .get("location")
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("/?tidal=error"));
        let (_, stv) = call(&app, "DELETE", "/v1/sources/tidal/session", None).await;
        assert_eq!(stv["signed_in"], false);
        let (st, _) = call(&app, "GET", "/v1/sources/tidal/collection", None).await;
        assert_eq!(st, StatusCode::PRECONDITION_FAILED);
    }

    #[tokio::test]
    async fn tidal_tracks_play_through_the_manifest_and_hls_segments() {
        let flac = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../pm-audio/tests/fixtures/60_mono_audio.flac"
        ))
        .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let base = format!("http://127.0.0.1:{port}");
        let seg_len = flac.len().div_ceil(3);
        let segs: Vec<Vec<u8>> = flac.chunks(seg_len).map(|c| c.to_vec()).collect();
        let manifest_base = base.clone();
        let mock = Router::new()
            .route(
                "/trackManifests/{id}",
                get(move |Path(id): Path<String>, headers: HeaderMap| {
                    let manifest_base = manifest_base.clone();
                    async move {
                        assert_eq!(id, "20115564");
                        assert_eq!(headers.get("authorization").unwrap(), "Bearer user-token");
                        Json(serde_json::json!({"data": {"id": id, "type": "trackManifests", "attributes": {
                            "uri": format!("{manifest_base}/hls/master.m3u8"), "formats": ["FLAC"], "trackPresentation": "FULL", "hash": "h"}}}))
                    }
                }),
            )
            .route("/hls/master.m3u8", get(|| async { "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1000000\nmedia.m3u8\n" }))
            .route("/hls/media.m3u8", get(|| async { "#EXTM3U\n#EXT-X-PLAYLIST-TYPE:VOD\n#EXTINF:1.0,\nseg/0\n#EXTINF:1.0,\nseg/1\n#EXTINF:1.0,\n/hls/seg/2\n#EXT-X-ENDLIST\n" }))
            .route(
                "/hls/seg/{n}",
                get(move |Path(n): Path<usize>| {
                    let segs = segs.clone();
                    async move { ([(header::CONTENT_TYPE, "application/octet-stream")], segs[n].clone()) }
                }),
            );
        tokio::spawn(async move { axum::serve(listener, mock).await.unwrap() });

        let dir = tempfile::tempdir().unwrap();
        let s = state(dir.path(), None).await;
        {
            let mut ep = s.tidal.write().unwrap();
            ep.api_base = base.clone();
            ep.auth_base = base.clone();
            ep.creds = tidal::TidalCredentials {
                client_id: "cid".into(),
                client_secret: "sec".into(),
                scopes: String::new(),
            };
        }
        s.library
            .lock()
            .unwrap()
            .set_setting(
                "tidal.user",
                &serde_json::json!({"access_token": "user-token", "refresh_token": "r", "expires_at": pm_library::db::now_unix() + 3600, "user_id": "4242", "scope": "playback"}).to_string(),
            )
            .unwrap();
        let app = router(s.clone());
        let (_, zones) = call(&app, "GET", "/v1/zones", None).await;
        let zid = zones[0]["id"].as_str().unwrap().to_string();
        let zone = s.zones.get(&ZoneId::new(&zid)).unwrap();
        let mut events = zone.subscribe();
        let (st, _) = call(
            &app,
            "POST",
            &format!("/v1/zones/{zid}/queue"),
            Some(serde_json::json!({"items": [{"uri": "tidal:track:20115564", "title": "Get Lucky"}], "replace": true, "play": true})),
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        let mut started = false;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while std::time::Instant::now() < deadline {
            match tokio::time::timeout(std::time::Duration::from_secs(1), events.recv()).await {
                Ok(Ok(pm_zones::ZoneEvent::TrackStarted { source_format, .. })) => {
                    started = true;
                    assert_eq!(source_format.rate_hz, 44_100);
                }
                Ok(Ok(pm_zones::ZoneEvent::Error(e))) => panic!("playback error: {e}"),
                Ok(Ok(pm_zones::ZoneEvent::Stopped)) if started => break,
                Ok(Ok(_)) => {}
                Ok(Err(_)) => break,
                Err(_) => {}
            }
        }
        assert!(started, "the TIDAL track never started");
        let st = zone.wait_stopped().await;
        assert!(st.error.is_none(), "{:?}", st.error);
        // The browser proxy serves the same segments as one body.
        let req = Request::builder()
            .uri("/v1/sources/tidal/stream/20115564")
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(resp
            .headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("audio/mp4"));
        assert_eq!(resp.headers().get("accept-ranges").unwrap(), "bytes");
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        assert_eq!(body.len(), flac.len(), "proxy body is the whole file");
        assert_eq!(&body[..], &flac[..]);
        // Seeking: a byte range comes back as 206 with the right slice.
        let req = Request::builder()
            .uri("/v1/sources/tidal/stream/20115564")
            .header("range", "bytes=100-199")
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(
            resp.headers().get("content-range").unwrap(),
            &format!("bytes 100-199/{}", flac.len())
        );
        let part = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        assert_eq!(&part[..], &flac[100..200]);
        // Without a session the resolver refuses with a clear reason.
        s.library.lock().unwrap().delete_setting("tidal.user").unwrap();
        let (_, _) = call(
            &app,
            "POST",
            &format!("/v1/zones/{zid}/queue"),
            Some(serde_json::json!({"items": [{"uri": "tidal:track:20115564"}], "replace": true, "play": true})),
        )
        .await;
        let st = zone.wait_stopped().await;
        assert!(
            st.error.as_deref().is_some_and(|e| e.contains("sign in")),
            "{:?}",
            st.error
        );
    }

    #[tokio::test]
    async fn a_sleep_timer_pauses_the_zone() {
        let dir = tempfile::tempdir().unwrap();
        let wav = dir.path().join("long.wav");
        write_wav(&wav, 441);
        let s = state(dir.path(), None).await;
        let app = router(s.clone());
        let (_, zones) = call(&app, "GET", "/v1/zones", None).await;
        let zid = zones[0]["id"].as_str().unwrap().to_string();
        call(
            &app,
            "PUT",
            &format!("/v1/zones/{zid}/repeat"),
            Some(serde_json::json!({"repeat": "one"})),
        )
        .await;
        call(
            &app,
            "POST",
            &format!("/v1/zones/{zid}/queue"),
            Some(
                serde_json::json!({"items": [{"uri": wav.to_string_lossy()}], "replace": true, "play": true}),
            ),
        )
        .await;
        let (_, v) = call(&app, "GET", &format!("/v1/zones/{zid}/sleep"), None).await;
        assert!(v.get("ends_at_ms").is_none());
        let (st, _) = call(
            &app,
            "PUT",
            &format!("/v1/zones/{zid}/sleep"),
            Some(serde_json::json!({"seconds": 0})),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        let (st, v) = call(
            &app,
            "PUT",
            &format!("/v1/zones/{zid}/sleep"),
            Some(serde_json::json!({"seconds": 1})),
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        assert!(v["remaining_ms"].as_u64().unwrap() <= 1000);
        let zone = s.zones.get(&ZoneId::new(&zid)).unwrap();
        let mut paused = false;
        for _ in 0..400 {
            if zone.state().transport == pm_zones::Transport::Paused {
                paused = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(paused, "the zone did not pause: {:?}", zone.state().transport);
        let (_, v) = call(&app, "GET", &format!("/v1/zones/{zid}/sleep"), None).await;
        assert!(v.get("ends_at_ms").is_none(), "timer cleared after firing");
        // Cancel: set and clear, nothing pauses later.
        let (st, _) = call(
            &app,
            "PUT",
            &format!("/v1/zones/{zid}/sleep"),
            Some(serde_json::json!({"seconds": 3600})),
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        assert!(s.sleep_ends_at(zone.id()).is_some());
        let (_, v) = call(
            &app,
            "PUT",
            &format!("/v1/zones/{zid}/sleep"),
            Some(serde_json::json!({"seconds": null})),
        )
        .await;
        assert!(v.get("ends_at_ms").is_none());
        assert!(s.sleep_ends_at(zone.id()).is_none());
    }

    #[tokio::test]
    async fn playback_transfers_between_zones() {
        let dir = tempfile::tempdir().unwrap();
        let wav = dir.path().join("long.wav");
        write_wav(&wav, 44_100 * 30);
        let s = state(dir.path(), None).await;
        let app = router(s.clone());
        let out = Arc::new(pm_zones::outputs::WavFileOutput::new(dir.path().join("sink.wav")));
        s.outputs.register(out.clone());
        let target = s.create_zone("Target", out);
        let tid = target.id().to_string();
        let (_, zones) = call(&app, "GET", "/v1/zones", None).await;
        let zid = zones
            .as_array()
            .unwrap()
            .iter()
            .map(|z| z["id"].as_str().unwrap().to_string())
            .find(|z| *z != tid)
            .unwrap();
        let items: Vec<_> = (0..3)
            .map(|i| serde_json::json!({"uri": wav.to_string_lossy(), "title": format!("t{i}")}))
            .collect();
        call(
            &app,
            "POST",
            &format!("/v1/zones/{zid}/queue"),
            Some(serde_json::json!({"items": items, "replace": true})),
        )
        .await;
        call(
            &app,
            "PUT",
            &format!("/v1/zones/{zid}/repeat"),
            Some(serde_json::json!({"repeat": "all"})),
        )
        .await;
        call(
            &app,
            "POST",
            &format!("/v1/zones/{zid}/transport"),
            Some(serde_json::json!({"action": "play", "index": 1})),
        )
        .await;
        let source = s.zones.get(&ZoneId::new(&zid)).unwrap();
        for _ in 0..500 {
            if source.state().now_playing.as_ref().map(|n| n.index) == Some(1) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        // The null output does not pace, so pause right away to hold a position.
        source.pause();
        for _ in 0..500 {
            if source.state().transport == pm_zones::Transport::Paused {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let (st, _) = call(
            &app,
            "POST",
            &format!("/v1/zones/{zid}/transfer"),
            Some(serde_json::json!({"to": zid})),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        let (st, z) = call(
            &app,
            "POST",
            &format!("/v1/zones/{zid}/transfer"),
            Some(serde_json::json!({"to": tid})),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{z}");
        assert_eq!(z["id"], tid);
        assert_eq!(z["queue"].as_array().unwrap().len(), 3);
        assert_eq!(z["queue"][1]["title"], "t1");
        assert_eq!(z["repeat"], "all");
        assert_eq!(z["now_playing"]["index"], 1);
        assert_eq!(z["transport"], "paused");
        let src = source.state();
        assert!(src.queue.is_empty());
        assert_eq!(src.transport, pm_zones::Transport::Stopped);
        // Nothing left to move.
        let (st, _) = call(
            &app,
            "POST",
            &format!("/v1/zones/{zid}/transfer"),
            Some(serde_json::json!({"to": tid})),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn shuffle_reorders_only_the_upcoming_items() {
        let dir = tempfile::tempdir().unwrap();
        let wav = dir.path().join("short.wav");
        write_wav(&wav, 441);
        let s = state(dir.path(), None).await;
        let app = router(s.clone());
        let (_, zones) = call(&app, "GET", "/v1/zones", None).await;
        let zid = zones[0]["id"].as_str().unwrap().to_string();
        let items: Vec<_> = (0..8)
            .map(|_| serde_json::json!({"uri": wav.to_string_lossy()}))
            .collect();
        let (st, _) = call(
            &app,
            "POST",
            &format!("/v1/zones/{zid}/queue"),
            Some(serde_json::json!({"items": items, "replace": true})),
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        // Loop the third item so the "current" index holds still on the null output.
        call(
            &app,
            "PUT",
            &format!("/v1/zones/{zid}/repeat"),
            Some(serde_json::json!({"repeat": "one"})),
        )
        .await;
        call(
            &app,
            "POST",
            &format!("/v1/zones/{zid}/transport"),
            Some(serde_json::json!({"action": "play", "index": 2})),
        )
        .await;
        let zone = s.zones.get(&ZoneId::new(&zid)).unwrap();
        for _ in 0..500 {
            if zone.state().queue_index == Some(2) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(zone.state().queue_index, Some(2));
        // The playing item and the two before it must stay put.
        let before: Vec<u64> = zone.queue().iter().map(|i| i.id).collect();
        let mut changed = false;
        for _ in 0..5 {
            let (st, _) = call(&app, "POST", &format!("/v1/zones/{zid}/shuffle"), None).await;
            assert_eq!(st, StatusCode::NO_CONTENT);
            let after: Vec<u64> = zone.queue().iter().map(|i| i.id).collect();
            assert_eq!(&after[..3], &before[..3]);
            let mut rest = after[3..].to_vec();
            rest.sort_unstable();
            assert_eq!(rest, before[3..]);
            if after[3..] != before[3..] {
                changed = true;
                break;
            }
        }
        assert!(changed, "five shuffles of five items never moved anything");
        let (_, z) = call(&app, "GET", &format!("/v1/zones/{zid}"), None).await;
        assert_eq!(z["queue_index"], 2);
        let (st, _) = call(&app, "POST", "/v1/zones/nope/shuffle", None).await;
        assert_eq!(st, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn library_radio_continues_after_the_last_track() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("music")).unwrap();
        write_wav(&dir.path().join("music/one.wav"), 4410);
        write_wav(&dir.path().join("music/two.wav"), 4410);
        let s = state(dir.path(), None).await;
        let app = router(s.clone());
        call(&app, "POST", "/v1/library/scan", Some(serde_json::json!({}))).await;
        for _ in 0..500 {
            if !s.is_scanning() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let (_, tracks) = call(&app, "GET", "/v1/library/tracks?limit=10", None).await;
        let first = tracks["tracks"][0]["id"].as_i64().unwrap();
        let (_, zones) = call(&app, "GET", "/v1/zones", None).await;
        let zid = zones[0]["id"].as_str().unwrap().to_string();
        let (st, _) = call(
            &app,
            "PUT",
            &format!("/v1/zones/{zid}/radio"),
            Some(serde_json::json!({"on": true})),
        )
        .await;
        assert_eq!(st, StatusCode::NO_CONTENT);
        let (_, z) = call(&app, "GET", &format!("/v1/zones/{zid}"), None).await;
        assert_eq!(z["radio"], true);
        let (st, _) = call(
            &app,
            "POST",
            &format!("/v1/zones/{zid}/queue"),
            Some(serde_json::json!({"track_ids": [first], "replace": true, "play": true})),
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        let zone = s.zones.get(&ZoneId::new(&zid)).unwrap();
        let mut grew = false;
        for _ in 0..500 {
            if zone.queue().len() >= 2 {
                grew = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(grew, "radio did not add tracks");
        assert!(zone.queue().len() <= 2, "only the one other track exists");
        zone.set_radio(false);
        assert_eq!(zone.wait_stopped().await.error, None);
    }

    #[tokio::test]
    async fn web_ui_is_served_without_a_token() {
        let dir = tempfile::tempdir().unwrap();
        let s = state(dir.path(), Some("s3cret")).await;
        let app = router(s);
        for (path, needle) in [
            ("/", "<title>Pubky Music</title>"),
            ("/app", "<title>Pubky Music</title>"),
            ("/app/", "<title>Pubky Music</title>"),
            ("/app.css", ".player"),
            ("/app.js", "/v1/events"),
            ("/manifest.webmanifest", "\"display\": \"standalone\""),
            ("/icon.svg", "<svg"),
        ] {
            let resp = app
                .clone()
                .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::OK, "{path}");
            let body = resp.into_body().collect().await.unwrap().to_bytes();
            assert!(std::str::from_utf8(&body).unwrap().contains(needle), "{path}");
        }
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
