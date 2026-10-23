//! Your Plex Media Server as a source (STEWARD-COMPAT-HA-MA.md §6.3), through Plex's documented
//! APIs (developer.plex.tv). Two ways in:
//! - **Sign in with Plex:** the node asks plex.tv for a PIN, you approve it on app.plex.tv, and
//!   the node picks your server and a connection that answers;
//! - **address and token:** for a server you reach directly.
//!
//! The node keeps only the server's access token and sends it in `X-Plex-Token`. Songs queue as
//! `plex:track:<ratingKey>` and play the original file; browser streams and covers are relayed,
//! so the token never reaches the page.

use crate::state::NodeState;
use crate::streaming::StreamingService;
use crate::types::*;
use crate::{ApiError, ApiResult};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::{Arc, RwLock};
use std::time::Duration;

pub const SOURCE_ID: &str = "plex";
pub const TRACK_URI_PREFIX: &str = "plex:track:";
const SETTING: &str = "plex.server";
const CLIENT_ID_SETTING: &str = "plex.client_id";
const PRODUCT: &str = "Pubky Music";
pub const PLEX_TV: &str = "https://plex.tv";
const AUTH_APP: &str = "https://app.plex.tv/auth";

/// The connected server. `Debug` never shows the token.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Server {
    pub url: String,
    pub token: String,
    #[serde(default)]
    pub server_name: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub machine_id: Option<String>,
    /// The plex.tv account, when signed in with Plex.
    #[serde(default)]
    pub user: Option<String>,
}

impl std::fmt::Debug for Server {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Server")
            .field("url", &self.url)
            .field("server_name", &self.server_name)
            .field("token", &"<set>")
            .finish()
    }
}

/// What the node knows about Plex: the server, its own client id, and plex.tv's origin
/// (tests point it at a mock).
#[derive(Clone, Debug, Default)]
pub struct Plex {
    pub server: Option<Server>,
    pub client_id: String,
    pub tv_base: String,
}

pub type Shared = Arc<RwLock<Plex>>;

/// Loads the server and this node's Plex client id (made once, then kept).
pub fn load(lib: &pm_library::Library) -> Plex {
    let client_id = match lib.get_setting(CLIENT_ID_SETTING).ok().flatten() {
        Some(id) => id,
        None => {
            let id = format!("pubky-music-{}", crate::tidal::random_urlsafe(12));
            let _ = lib.set_setting(CLIENT_ID_SETTING, &id);
            id
        }
    };
    Plex {
        server: lib
            .get_setting(SETTING)
            .ok()
            .flatten()
            .and_then(|j| serde_json::from_str(&j).ok()),
        client_id,
        tv_base: PLEX_TV.into(),
    }
}

pub fn track_uri(id: &str) -> String {
    format!("{TRACK_URI_PREFIX}{id}")
}

fn upstream(e: impl std::fmt::Display) -> ApiError {
    ApiError::new(StatusCode::BAD_GATEWAY, format!("Plex: {e}"))
}

/// Plex rating keys are numbers.
fn valid_id(id: &str) -> Result<&str, ApiError> {
    if id.is_empty() || id.len() > 20 || !id.bytes().all(|b| b.is_ascii_digit()) {
        return Err(ApiError::bad_request("bad Plex id"));
    }
    Ok(id)
}

fn server(s: &NodeState) -> Result<Server, ApiError> {
    s.plex.read().expect("plex").server.clone().ok_or_else(|| {
        ApiError::new(
            StatusCode::PRECONDITION_FAILED,
            "connect your Plex first (Sources › Plex)",
        )
    })
}

pub(crate) fn connected(s: &NodeState) -> bool {
    s.plex.read().expect("plex").server.is_some()
}

fn client_id(s: &NodeState) -> String {
    s.plex.read().expect("plex").client_id.clone()
}

/// The headers Plex asks of every client.
fn with_headers(
    rb: reqwest::RequestBuilder,
    client_id: &str,
    token: Option<&str>,
) -> reqwest::RequestBuilder {
    let rb = rb
        .header(reqwest::header::ACCEPT, "application/json")
        .header("X-Plex-Product", PRODUCT)
        .header("X-Plex-Version", env!("CARGO_PKG_VERSION"))
        .header("X-Plex-Client-Identifier", client_id);
    match token {
        Some(t) => rb.header("X-Plex-Token", t),
        None => rb,
    }
}

async fn get(s: &NodeState, srv: &Server, path: &str, query: &[(&str, &str)]) -> Result<Value, ApiError> {
    let resp = with_headers(
        s.http_client()
            .get(format!("{}{path}", srv.url.trim_end_matches('/'))),
        &client_id(s),
        Some(&srv.token),
    )
    .query(query)
    .send()
    .await
    .map_err(|e| upstream(e.without_url()))?;
    match resp.status() {
        st if st.is_success() => resp.json().await.map_err(|e| upstream(e.without_url())),
        StatusCode::UNAUTHORIZED => Err(ApiError::new(
            StatusCode::PRECONDITION_FAILED,
            "Plex no longer accepts this token: connect again (Sources › Plex)",
        )),
        StatusCode::NOT_FOUND => Err(ApiError::not_found("Plex has no such item")),
        st => Err(upstream(format!("{path} answered {st}"))),
    }
}

/// Plex names pictures `/library/metadata/<id>/thumb/<version>`, and an album's may be its
/// artist's: the node relays that exact picture as `/v1/sources/plex/cover/<id>/<version>`.
fn cover_path(thumb: &str) -> Option<String> {
    let rest = thumb.strip_prefix("/library/metadata/")?;
    let (id, ver) = rest.split_once("/thumb/")?;
    (valid_id(id).is_ok() && valid_id(ver).is_ok()).then(|| format!("/v1/sources/plex/cover/{id}/{ver}"))
}

fn str_of(v: &Value, k: &str) -> Option<String> {
    match &v[k] {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn metadata(v: &Value) -> Vec<Value> {
    v["MediaContainer"]["Metadata"]
        .as_array()
        .cloned()
        .unwrap_or_default()
}

/// A track's artist: its own (`originalTitle`, for compilations), else the album artist.
fn artist_of(v: &Value) -> Option<String> {
    str_of(v, "originalTitle").or_else(|| str_of(v, "grandparentTitle"))
}

/// The item's own picture, else its album's, else its artist's.
fn image_of(v: &Value) -> Option<String> {
    ["thumb", "parentThumb", "grandparentThumb"]
        .iter()
        .find_map(|k| v[*k].as_str().and_then(cover_path))
}

fn year_of(v: &Value) -> Option<i32> {
    v["year"]
        .as_i64()
        .or_else(|| v["parentYear"].as_i64())
        .map(|y| y as i32)
}

pub(crate) fn hit(v: &Value) -> Option<SourceHit> {
    let id = str_of(v, "ratingKey")?;
    let kind = match v["type"].as_str()? {
        "track" => "track",
        "album" => "album",
        _ => return None,
    };
    Some(SourceHit {
        id: format!("{kind}:{id}"),
        title: str_of(v, "title")?,
        creator: if kind == "album" {
            str_of(v, "parentTitle")
        } else {
            artist_of(v)
        },
        year: year_of(v),
        downloads: v["viewCount"].as_u64(),
        image: image_of(v),
        kind: kind.into(),
        duration_ms: v["duration"].as_u64(),
        library_artist_id: None,
        isrc: None,
        upc: None,
        library_track_id: None,
        library_album_id: None,
        uri: (kind == "track").then(|| track_uri(&id)),
        url: String::new(),
    })
}

fn format_of(v: &Value) -> String {
    let m = &v["Media"][0];
    let codec = m["audioCodec"].as_str().or(m["container"].as_str());
    match (codec, m["bitrate"].as_u64()) {
        (Some(c), Some(b)) => format!("{} {b} kbps (Plex)", c.to_uppercase()),
        (Some(c), None) => format!("{} (Plex)", c.to_uppercase()),
        _ => "Plex".into(),
    }
}

fn source_track(v: &Value, n: u32) -> Option<SourceTrack> {
    let id = str_of(v, "ratingKey")?;
    Some(SourceTrack {
        title: str_of(v, "title")?,
        track_no: v["index"].as_u64().map(|t| t as u32).or(Some(n)),
        duration_ms: v["duration"].as_u64(),
        format: format_of(v),
        url: track_uri(&id),
        artist: artist_of(v),
    })
}

pub struct PlexSource;

#[async_trait::async_trait]
impl StreamingService for PlexSource {
    fn id(&self) -> &'static str {
        SOURCE_ID
    }

    fn features(&self) -> &'static [&'static str] {
        &["search", "play"]
    }

    fn stage(&self) -> &'static str {
        "beta"
    }

    fn info(&self, s: &NodeState) -> SourceInfo {
        let srv = s.plex.read().expect("plex").server.clone();
        SourceInfo {
            id: SOURCE_ID.into(),
            name: "Plex".into(),
            description: match &srv {
                Some(v) => format!(
                    "Your Plex{} at {}",
                    v.server_name
                        .as_ref()
                        .map(|n| format!(" “{n}”"))
                        .unwrap_or_default(),
                    v.url
                ),
                None => "Your Plex Media Server's music".into(),
            },
            status: if srv.is_some() { "ready" } else { "needs_config" }.into(),
        }
    }

    fn unified(&self, s: &NodeState) -> bool {
        connected(s)
    }

    async fn search(&self, s: &NodeState, term: &str, limit: u32) -> Result<Vec<SourceHit>, ApiError> {
        let srv = server(s)?;
        let n = limit.to_string();
        let v = get(s, &srv, "/hubs/search", &[("query", term), ("limit", &n)]).await?;
        let mut out: Vec<SourceHit> = v["MediaContainer"]["Hub"]
            .as_array()
            .map(|hubs| {
                hubs.iter()
                    .filter(|h| matches!(h["type"].as_str(), Some("album" | "track")))
                    .flat_map(|h| h["Metadata"].as_array().cloned().unwrap_or_default())
                    .filter_map(|m| hit(&m))
                    .collect()
            })
            .unwrap_or_default();
        // Albums first, as the other sources list them
        out.sort_by_key(|h| h.kind != "album");
        out.truncate(limit as usize * 2);
        Ok(out)
    }

    async fn item(&self, s: &NodeState, id: &str) -> Result<SourceItem, ApiError> {
        let srv = server(s)?;
        let (kind, raw) = id.split_once(':').unwrap_or(("track", id));
        let raw = valid_id(raw)?;
        let it = metadata(&get(s, &srv, &format!("/library/metadata/{raw}"), &[]).await?)
            .into_iter()
            .next()
            .ok_or_else(|| ApiError::not_found("Plex has no such item"))?;
        match kind {
            "album" => {
                let children = get(s, &srv, &format!("/library/metadata/{raw}/children"), &[]).await?;
                let tracks = metadata(&children)
                    .iter()
                    .enumerate()
                    .filter_map(|(i, t)| source_track(t, i as u32 + 1))
                    .collect();
                Ok(SourceItem {
                    source: SOURCE_ID.into(),
                    id: format!("album:{raw}"),
                    title: str_of(&it, "title").unwrap_or_else(|| "Untitled".into()),
                    creator: str_of(&it, "parentTitle"),
                    year: year_of(&it),
                    description: str_of(&it, "summary"),
                    licence: None,
                    image: image_of(&it),
                    url: String::new(),
                    tracks,
                    library_album_id: None,
                })
            }
            "track" => Ok(SourceItem {
                source: SOURCE_ID.into(),
                id: format!("track:{raw}"),
                title: str_of(&it, "parentTitle")
                    .or_else(|| str_of(&it, "title"))
                    .unwrap_or_else(|| "Untitled".into()),
                creator: artist_of(&it),
                year: year_of(&it),
                description: None,
                licence: None,
                image: image_of(&it),
                url: String::new(),
                tracks: source_track(&it, 1).into_iter().collect(),
                library_album_id: None,
            }),
            _ => Err(ApiError::bad_request("Plex items are track:<id> or album:<id>")),
        }
    }
}

// ---- playback ----

/// The original file's path on the server (`/library/parts/…`).
fn part_key(v: &Value) -> Option<String> {
    v["Media"][0]["Part"][0]["key"]
        .as_str()
        .filter(|k| k.starts_with('/'))
        .map(str::to_string)
}

/// Open `plex:track:<ratingKey>` for a zone: the original file, the token kept in the node.
pub(crate) fn open_track_blocking(
    shared: &Shared,
    id: &str,
) -> Result<Box<dyn pm_zones::FrameSource>, pm_zones::SourceError> {
    let fail = |reason: String| pm_zones::SourceError::Open {
        uri: track_uri(id),
        reason,
    };
    valid_id(id).map_err(|e| fail(e.message))?;
    let (srv, cid) = {
        let p = shared.read().map_err(|_| fail("server settings".into()))?;
        (
            p.server
                .clone()
                .ok_or_else(|| fail("no Plex is connected".into()))?,
            p.client_id.clone(),
        )
    };
    let base = srv.url.trim_end_matches('/');
    let redact = |e: String| e.replace(&srv.token, "<token>");
    let meta: Value = crate::sources::http_client()
        .get(format!("{base}/library/metadata/{id}"))
        .header(reqwest::header::ACCEPT, "application/json")
        .header("X-Plex-Product", PRODUCT)
        .header("X-Plex-Client-Identifier", &cid)
        .header("X-Plex-Token", &srv.token)
        .send()
        .and_then(|r| r.error_for_status())
        .and_then(|r| r.json())
        .map_err(|e| fail(redact(e.without_url().to_string())))?;
    let key = metadata(&meta)
        .first()
        .and_then(part_key)
        .ok_or_else(|| fail("Plex has no file for this track".into()))?;
    // The HTTP reader takes a URL only: the token goes in Plex's `X-Plex-Token` query here,
    // kept inside the node.
    let sep = if key.contains('?') { '&' } else { '?' };
    let url = format!("{base}{key}{sep}X-Plex-Token={}", srv.token);
    crate::sources::open_http_stream(&url).map_err(|e| match e {
        pm_zones::SourceError::Open { reason, .. } => fail(redact(reason)),
        other => other,
    })
}

async fn relay(
    s: &NodeState,
    url: String,
    fallback_type: &str,
) -> Result<axum::response::Response, ApiError> {
    let srv = server(s)?;
    let resp = with_headers(s.http_client().get(url), &client_id(s), Some(&srv.token))
        .header(reqwest::header::ACCEPT, "*/*")
        .send()
        .await
        .map_err(|e| upstream(e.without_url()))?;
    if !resp.status().is_success() {
        return Err(if resp.status() == StatusCode::NOT_FOUND {
            ApiError::not_found("Plex has no such item")
        } else {
            upstream(format!("answered {}", resp.status()))
        });
    }
    let ctype = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or(fallback_type)
        .to_string();
    let mut out = axum::response::Response::builder()
        .header(axum::http::header::CONTENT_TYPE, ctype)
        .header(axum::http::header::CACHE_CONTROL, "private, max-age=3600");
    if let Some(n) = resp.content_length() {
        out = out.header(axum::http::header::CONTENT_LENGTH, n);
    }
    out.body(axum::body::Body::from_stream(resp.bytes_stream()))
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

/// `GET /v1/sources/plex/stream/{id}`: the original file for browsers.
pub(crate) async fn stream(
    State(s): State<Arc<NodeState>>,
    Path(id): Path<String>,
) -> Result<axum::response::Response, ApiError> {
    crate::streaming::ensure_on(&s, SOURCE_ID)?;
    let id = valid_id(&id)?.to_string();
    let srv = server(&s)?;
    let meta = get(&s, &srv, &format!("/library/metadata/{id}"), &[]).await?;
    let key = metadata(&meta)
        .first()
        .and_then(part_key)
        .ok_or_else(|| ApiError::not_found("Plex has no file for this track"))?;
    relay(
        &s,
        format!("{}{key}", srv.url.trim_end_matches('/')),
        "audio/flac",
    )
    .await
}

/// `GET /v1/sources/plex/cover/{id}/{version}`.
pub(crate) async fn cover(
    State(s): State<Arc<NodeState>>,
    Path((id, ver)): Path<(String, String)>,
) -> Result<axum::response::Response, ApiError> {
    crate::streaming::ensure_on(&s, SOURCE_ID)?;
    let (id, ver) = (valid_id(&id)?.to_string(), valid_id(&ver)?.to_string());
    let srv = server(&s)?;
    let url = format!(
        "{}/library/metadata/{id}/thumb/{ver}",
        srv.url.trim_end_matches('/')
    );
    relay(&s, url, "image/jpeg").await
}

// ---- connecting ----

/// The connected Plex as the UI sees it: never the token.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PlexView {
    pub connected: bool,
    pub url: Option<String>,
    pub server_name: Option<String>,
    pub version: Option<String>,
    pub user: Option<String>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct PlexConnect {
    pub url: String,
    pub token: String,
}

fn view(srv: Option<&Server>) -> PlexView {
    PlexView {
        connected: srv.is_some(),
        url: srv.map(|v| v.url.clone()),
        server_name: srv.and_then(|v| v.server_name.clone()),
        version: srv.and_then(|v| v.version.clone()),
        user: srv.and_then(|v| v.user.clone()),
    }
}

fn save(s: &NodeState, srv: Server) -> Result<PlexView, ApiError> {
    s.library
        .lock()
        .expect("library poisoned")
        .set_setting(SETTING, &serde_json::to_string(&srv).expect("serializes"))?;
    tracing::info!(url = %srv.url, server = ?srv.server_name, "Plex connected");
    let v = view(Some(&srv));
    s.plex.write().expect("plex").server = Some(srv);
    Ok(v)
}

fn http_url(url: &str) -> Result<String, ApiError> {
    let url = url.trim().trim_end_matches('/').to_string();
    let parsed = url::Url::parse(&url)
        .map_err(|_| ApiError::bad_request("the server address must be an http(s) URL"))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(ApiError::bad_request("the server address must be an http(s) URL"));
    }
    Ok(url)
}

/// Checks a server with a token: `/identity` (open to all) says it is a Plex Media Server, the
/// root (`/`) proves the token and names the server.
async fn probe(s: &NodeState, url: &str, token: &str, timeout: Duration) -> Result<Server, ApiError> {
    let cid = client_id(s);
    with_headers(s.http_client().get(format!("{url}/identity")), &cid, None)
        .timeout(timeout)
        .send()
        .await
        .map_err(|e| upstream(e.without_url()))?
        .json::<Value>()
        .await
        .ok()
        .filter(|v| v["MediaContainer"]["machineIdentifier"].is_string())
        .ok_or_else(|| upstream("this address does not answer like a Plex Media Server"))?;
    let resp = with_headers(s.http_client().get(format!("{url}/")), &cid, Some(token))
        .timeout(timeout)
        .send()
        .await
        .map_err(|e| upstream(e.without_url()))?;
    if resp.status() == StatusCode::UNAUTHORIZED {
        return Err(ApiError::new(StatusCode::UNAUTHORIZED, "Plex refused this token"));
    }
    if !resp.status().is_success() {
        return Err(upstream(format!("the server answered {}", resp.status())));
    }
    let root: Value = resp.json().await.map_err(|e| upstream(e.without_url()))?;
    let mc = &root["MediaContainer"];
    Ok(Server {
        url: url.to_string(),
        token: token.to_string(),
        server_name: str_of(mc, "friendlyName"),
        version: str_of(mc, "version"),
        machine_id: str_of(mc, "machineIdentifier"),
        user: None,
    })
}

/// `GET /v1/sources/plex/server`: never the token.
pub(crate) async fn get_server(State(s): State<Arc<NodeState>>) -> ApiResult<PlexView> {
    Ok(Json(view(s.plex.read().expect("plex").server.as_ref())))
}

/// `PUT /v1/sources/plex/server {url, token}`: a server you reach directly.
pub(crate) async fn connect(
    State(s): State<Arc<NodeState>>,
    Json(body): Json<PlexConnect>,
) -> ApiResult<PlexView> {
    crate::streaming::ensure_on(&s, SOURCE_ID)?;
    let url = http_url(&body.url)?;
    let token = body.token.trim();
    if token.is_empty() || token.len() > 200 || !token.bytes().all(|b| b.is_ascii_graphic()) {
        return Err(ApiError::bad_request("a Plex token is required"));
    }
    let srv = probe(&s, &url, token, Duration::from_secs(15)).await?;
    Ok(Json(save(&s, srv)?))
}

/// `DELETE /v1/sources/plex/server`: forgets the server and its token.
pub(crate) async fn disconnect(State(s): State<Arc<NodeState>>) -> ApiResult<PlexView> {
    s.library
        .lock()
        .expect("library poisoned")
        .delete_setting(SETTING)?;
    s.plex.write().expect("plex").server = None;
    Ok(Json(view(None)))
}

/// A sign-in in progress: open `auth_url`, approve, then poll `GET …/pin/{id}`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PlexPin {
    pub id: u64,
    pub code: String,
    pub auth_url: String,
}

/// `POST /v1/sources/plex/pin`: starts "Sign in with Plex".
pub(crate) async fn start_pin(State(s): State<Arc<NodeState>>) -> ApiResult<PlexPin> {
    crate::streaming::ensure_on(&s, SOURCE_ID)?;
    let (cid, tv) = {
        let p = s.plex.read().expect("plex");
        (p.client_id.clone(), p.tv_base.clone())
    };
    let resp = with_headers(s.http_client().post(format!("{tv}/api/v2/pins")), &cid, None)
        .query(&[("strong", "true")])
        .send()
        .await
        .map_err(|e| upstream(e.without_url()))?;
    if !resp.status().is_success() {
        return Err(upstream(format!("plex.tv answered {}", resp.status())));
    }
    let v: Value = resp.json().await.map_err(|e| upstream(e.without_url()))?;
    let id = v["id"].as_u64().ok_or_else(|| upstream("no PIN id"))?;
    let code = str_of(&v, "code").ok_or_else(|| upstream("no PIN code"))?;
    let mut auth = url::Url::parse(AUTH_APP).expect("valid");
    // app.plex.tv reads its parameters from the fragment
    let frag = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("clientID", &cid)
        .append_pair("code", &code)
        .append_pair("context[device][product]", PRODUCT)
        .finish();
    auth.set_fragment(Some(&format!("?{frag}")));
    Ok(Json(PlexPin {
        id,
        code,
        auth_url: auth.to_string(),
    }))
}

/// Where a sign-in stands.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PlexPinStatus {
    /// `waiting`, `connected` or `expired`.
    pub status: String,
    pub server: PlexView,
}

/// Connections to try, in order: local, then direct remote, then Plex's relay.
fn connection_order(res: &Value) -> Vec<String> {
    let mut cs: Vec<&Value> = res["connections"]
        .as_array()
        .map(|a| a.iter().collect())
        .unwrap_or_default();
    cs.sort_by_key(|c| {
        (
            c["relay"].as_bool().unwrap_or(false),
            !c["local"].as_bool().unwrap_or(false),
        )
    });
    cs.iter()
        .filter_map(|c| c["uri"].as_str().map(str::to_string))
        .collect()
}

/// `GET /v1/sources/plex/pin/{id}`: once approved, the node picks your server (your own
/// first) and the first connection that answers, and keeps that server's token.
pub(crate) async fn poll_pin(
    State(s): State<Arc<NodeState>>,
    Path(id): Path<u64>,
) -> ApiResult<PlexPinStatus> {
    crate::streaming::ensure_on(&s, SOURCE_ID)?;
    let (cid, tv) = {
        let p = s.plex.read().expect("plex");
        (p.client_id.clone(), p.tv_base.clone())
    };
    let resp = with_headers(s.http_client().get(format!("{tv}/api/v2/pins/{id}")), &cid, None)
        .send()
        .await
        .map_err(|e| upstream(e.without_url()))?;
    if resp.status() == StatusCode::NOT_FOUND {
        return Ok(Json(PlexPinStatus {
            status: "expired".into(),
            server: view(None),
        }));
    }
    if !resp.status().is_success() {
        return Err(upstream(format!("plex.tv answered {}", resp.status())));
    }
    let pin: Value = resp.json().await.map_err(|e| upstream(e.without_url()))?;
    let Some(account_token) = str_of(&pin, "authToken") else {
        return Ok(Json(PlexPinStatus {
            status: "waiting".into(),
            server: view(None),
        }));
    };
    let user = with_headers(
        s.http_client().get(format!("{tv}/api/v2/user")),
        &cid,
        Some(&account_token),
    )
    .send()
    .await
    .ok()
    .and_then(|r| r.error_for_status().ok());
    let user = match user {
        Some(r) => r.json::<Value>().await.ok().and_then(|u| str_of(&u, "username")),
        None => None,
    };
    let res: Value = with_headers(
        s.http_client().get(format!("{tv}/api/v2/resources")),
        &cid,
        Some(&account_token),
    )
    .query(&[("includeHttps", "1"), ("includeRelay", "1")])
    .send()
    .await
    .map_err(|e| upstream(e.without_url()))?
    .json()
    .await
    .map_err(|e| upstream(e.without_url()))?;
    let mut servers: Vec<&Value> = res
        .as_array()
        .map(|a| {
            a.iter()
                .filter(|r| {
                    r["provides"]
                        .as_str()
                        .is_some_and(|p| p.split(',').any(|x| x == "server"))
                })
                .collect()
        })
        .unwrap_or_default();
    servers.sort_by_key(|r| !r["owned"].as_bool().unwrap_or(false));
    for r in servers {
        let token = str_of(r, "accessToken").unwrap_or_else(|| account_token.clone());
        for uri in connection_order(r) {
            let Ok(url) = http_url(&uri) else { continue };
            if let Ok(mut srv) = probe(&s, &url, &token, Duration::from_secs(4)).await {
                srv.user = user.clone();
                srv.server_name = str_of(r, "name").or(srv.server_name);
                return Ok(Json(PlexPinStatus {
                    status: "connected".into(),
                    server: save(&s, srv)?,
                }));
            }
        }
    }
    Err(ApiError::new(
        StatusCode::BAD_GATEWAY,
        "signed in to Plex, but none of your servers answered from the node: connect by address and token instead",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn items_map_and_never_carry_the_token() {
        let song = serde_json::json!({"ratingKey": "1201", "type": "track", "title": "Bohemian Rhapsody",
            "grandparentTitle": "Queen", "parentTitle": "A Night at the Opera", "parentRatingKey": "1200",
            "parentThumb": "/library/metadata/1200/thumb/1", "duration": 354_000, "index": 11, "parentYear": 1975,
            "Media": [{"audioCodec": "flac", "bitrate": 1000, "Part": [{"key": "/library/parts/9/1/file.flac"}]}]});
        let h = hit(&song).unwrap();
        assert_eq!(h.uri.as_deref(), Some("plex:track:1201"));
        assert_eq!(h.creator.as_deref(), Some("Queen"));
        assert_eq!((h.duration_ms, h.year), (Some(354_000), Some(1975)));
        assert_eq!(h.image.as_deref(), Some("/v1/sources/plex/cover/1200/1"));
        assert!(cover_path("/library/metadata/1/../../x/thumb/2").is_none());
        let t = source_track(&song, 1).unwrap();
        assert_eq!(
            (t.track_no, t.format.as_str()),
            (Some(11), "FLAC 1000 kbps (Plex)")
        );
        assert_eq!(part_key(&song).as_deref(), Some("/library/parts/9/1/file.flac"));
        assert!(hit(&serde_json::json!({"ratingKey": "1", "title": "Film", "type": "movie"})).is_none());
        let srv = Server {
            token: "T0KEN".into(),
            ..Default::default()
        };
        assert!(!format!("{srv:?}").contains("T0KEN"));
        assert!(valid_id("../etc").is_err());
        assert!(valid_id("1201").is_ok());
    }

    #[test]
    fn local_connections_first_relay_last() {
        let r = serde_json::json!({"connections": [
            {"uri": "https://relay.plex.direct:8443", "local": false, "relay": true},
            {"uri": "https://remote.plex.direct:32400", "local": false, "relay": false},
            {"uri": "http://192.168.1.5:32400", "local": true, "relay": false}]});
        assert_eq!(
            connection_order(&r),
            [
                "http://192.168.1.5:32400",
                "https://remote.plex.direct:32400",
                "https://relay.plex.direct:8443"
            ]
        );
    }
}
