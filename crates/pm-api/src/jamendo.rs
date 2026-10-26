//! Jamendo (STEWARD-PLAN step 5.4): Creative Commons music with an official API and full-length
//! streams, FLAC included. A free developer app (`client_id`, `client_secret`) is all it needs;
//! no account to search and play. Tracks queue as `jamendo:track:<id>`: the stream URL carries
//! the app's `client_id`, so it is looked up only when a track plays and never stored or shown.

use crate::state::NodeState;
use crate::streaming::StreamingService;
use crate::types::*;
use crate::ApiError;
use axum::http::StatusCode;
use serde_json::Value;
use std::sync::{Arc, RwLock};

pub const SOURCE_ID: &str = "jamendo";
pub const TRACK_URI_PREFIX: &str = "jamendo:track:";
const API_BASE: &str = "https://api.jamendo.com/v3.0";

/// The developer app. `Debug` never shows the values.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct JamendoCredentials {
    pub client_id: String,
    pub client_secret: String,
}

impl std::fmt::Debug for JamendoCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JamendoCredentials")
            .field("client_id", &if self.client_id.is_empty() { "" } else { "<set>" })
            .field(
                "client_secret",
                &if self.client_secret.is_empty() {
                    ""
                } else {
                    "<set>"
                },
            )
            .finish()
    }
}

impl JamendoCredentials {
    pub fn configured(&self) -> bool {
        !self.client_id.trim().is_empty()
    }
}

/// Credentials and the API origin (tests point it at a mock), shared with the audio thread.
#[derive(Debug, Clone)]
pub struct JamendoEndpoints {
    pub creds: JamendoCredentials,
    pub base: String,
}

impl Default for JamendoEndpoints {
    fn default() -> Self {
        Self {
            creds: JamendoCredentials::default(),
            base: API_BASE.into(),
        }
    }
}

pub type SharedEndpoints = Arc<RwLock<JamendoEndpoints>>;

pub fn track_uri(id: &str) -> String {
    format!("{TRACK_URI_PREFIX}{id}")
}

/// `https://www.jamendo.com/{kind}/{id}`: the canonical service link (ADR 0001, last resort).
pub fn page_url(kind: &str, id: &str) -> String {
    format!("https://www.jamendo.com/{kind}/{id}")
}

pub(crate) fn configured(s: &NodeState) -> bool {
    s.jamendo.read().expect("jamendo").creds.configured()
}

fn upstream(e: impl std::fmt::Display) -> ApiError {
    ApiError::new(StatusCode::BAD_GATEWAY, format!("jamendo: {e}"))
}

fn numeric(id: &str) -> Result<&str, ApiError> {
    if id.is_empty() || id.len() > 12 || !id.bytes().all(|b| b.is_ascii_digit()) {
        return Err(ApiError::bad_request("bad Jamendo id"));
    }
    Ok(id)
}

/// The client id the node's Jamendo sign-in is made with (`vault_cache`).
pub(crate) fn app_id(s: &NodeState) -> Option<String> {
    Some(s.jamendo.read().expect("jamendo").creds.client_id.clone()).filter(|c| !c.trim().is_empty())
}

fn endpoints(s: &NodeState) -> Result<JamendoEndpoints, ApiError> {
    let e = s.jamendo.read().expect("jamendo").clone();
    if !e.creds.configured() {
        return Err(ApiError::new(
            StatusCode::PRECONDITION_FAILED,
            "Jamendo needs a developer app: [jamendo] client_id in the node config or PM_JAMENDO_CLIENT_ID",
        ));
    }
    Ok(e)
}

/// One API call; Jamendo answers 200 with `headers.status` = `failed` on errors.
pub(crate) async fn get_json(s: &NodeState, path: &str, query: &[(&str, String)]) -> Result<Value, ApiError> {
    let e = endpoints(s)?;
    let mut q: Vec<(&str, String)> = vec![
        ("client_id", e.creds.client_id.clone()),
        ("format", "json".into()),
    ];
    q.extend(query.iter().cloned());
    let resp = s
        .http_client()
        .get(format!("{}{path}", e.base.trim_end_matches('/')))
        .query(&q)
        .send()
        .await
        .map_err(|err| upstream(err.without_url()))?;
    if !resp.status().is_success() {
        return Err(upstream(format!("{path} answered {}", resp.status())));
    }
    let v: Value = resp.json().await.map_err(|err| upstream(err.without_url()))?;
    if v["headers"]["status"].as_str() == Some("failed") {
        return Err(upstream(
            v["headers"]["error_message"].as_str().unwrap_or("request failed"),
        ));
    }
    Ok(v)
}

/// One track or album by id. Jamendo sometimes answers an id lookup with no results, then
/// with the item a moment later: ask twice.
async fn by_id(s: &NodeState, path: &str, id: &str, extra: &[(&str, String)]) -> Result<Value, ApiError> {
    let mut q: Vec<(&str, String)> = vec![("id", id.to_string())];
    q.extend(extra.iter().cloned());
    for attempt in 0..2 {
        let v = get_json(s, path, &q).await?;
        if let Some(t) = v["results"].as_array().and_then(|a| a.first()) {
            return Ok(t.clone());
        }
        if attempt == 0 {
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        }
    }
    Err(ApiError::not_found(format!("no Jamendo item {id}")))
}

fn year_of(v: &Value) -> Option<i32> {
    v["releasedate"].as_str()?.get(..4)?.parse().ok()
}

fn id_of(v: &Value) -> Option<String> {
    match &v["id"] {
        Value::String(s) if !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()) => Some(s.clone()),
        Value::Number(n) => n.as_u64().map(|n| n.to_string()),
        _ => None,
    }
}

fn image_of(v: &Value) -> Option<String> {
    v["image"]
        .as_str()
        .or(v["album_image"].as_str())
        .filter(|i| i.starts_with("https://"))
        .map(str::to_string)
}

/// A track or album document → hit.
pub(crate) fn hit(kind: &str, v: &Value) -> Option<SourceHit> {
    let id = id_of(v)?;
    Some(SourceHit {
        id: format!("{kind}:{id}"),
        title: v["name"].as_str().filter(|n| !n.trim().is_empty())?.to_string(),
        creator: v["artist_name"].as_str().map(str::to_string),
        year: year_of(v),
        downloads: None,
        image: image_of(v),
        kind: kind.into(),
        duration_ms: v["duration"].as_u64().map(|d| d * 1000),
        library_artist_id: None,
        isrc: None,
        upc: None,
        library_track_id: None,
        library_album_id: None,
        uri: (kind == "track").then(|| track_uri(&id)),
        url: page_url(kind, &id),
    })
}

fn source_track(v: &Value, n: u32) -> Option<SourceTrack> {
    let id = id_of(v)?;
    Some(SourceTrack {
        title: v["name"].as_str()?.to_string(),
        track_no: v["position"]
            .as_u64()
            .or_else(|| v["position"].as_str().and_then(|p| p.parse().ok()))
            .map(|p| p as u32)
            .or(Some(n)),
        duration_ms: v["duration"].as_u64().map(|d| d * 1000),
        format: "FLAC (Jamendo)".into(),
        url: track_uri(&id),
        artist: v["artist_name"].as_str().map(str::to_string),
    })
}

fn licence_of(v: &Value) -> Option<String> {
    v["license_ccurl"]
        .as_str()
        .filter(|l| !l.is_empty())
        .map(str::to_string)
}

/// A track document → single-track item.
pub(crate) fn track_item(v: &Value) -> Option<SourceItem> {
    let h = hit("track", v)?;
    Some(SourceItem {
        source: SOURCE_ID.into(),
        id: h.id.clone(),
        title: v["album_name"]
            .as_str()
            .filter(|a| !a.trim().is_empty())
            .unwrap_or(&h.title)
            .to_string(),
        creator: h.creator.clone(),
        year: h.year,
        description: None,
        licence: licence_of(v),
        image: h.image.clone(),
        url: h.url.clone(),
        tracks: vec![source_track(v, 1)?],
        library_album_id: None,
    })
}

pub struct Jamendo;

#[async_trait::async_trait]
impl StreamingService for Jamendo {
    fn id(&self) -> &'static str {
        SOURCE_ID
    }

    fn features(&self) -> &'static [&'static str] {
        &["search", "play", "library"]
    }

    fn info(&self, s: &NodeState) -> SourceInfo {
        SourceInfo {
            id: SOURCE_ID.into(),
            name: "Jamendo".into(),
            description: if configured(s) {
                "Independent Creative Commons music, full length and lossless; no account needed"
            } else {
                "Independent Creative Commons music, full length and lossless. No account needed, but whoever runs this node has to add a free Jamendo app key first"
            }
            .into(),
            status: if configured(s) { "ready" } else { "needs_config" }.into(),
        }
    }

    fn unified(&self, s: &NodeState) -> bool {
        configured(s)
    }

    async fn search(&self, s: &NodeState, term: &str, limit: u32) -> Result<Vec<SourceHit>, ApiError> {
        let albums = get_json(
            s,
            "/albums/",
            &[
                ("namesearch", term.to_string()),
                ("limit", (limit / 4).max(1).to_string()),
            ],
        )
        .await?;
        let tracks = get_json(
            s,
            "/tracks/",
            &[("search", term.to_string()), ("limit", limit.to_string())],
        )
        .await?;
        let mut out: Vec<SourceHit> = Vec::new();
        for (kind, v) in [("album", &albums), ("track", &tracks)] {
            if let Some(a) = v["results"].as_array() {
                out.extend(a.iter().filter_map(|x| hit(kind, x)));
            }
        }
        Ok(out.into_iter().take(limit as usize).collect())
    }

    async fn item(&self, s: &NodeState, id: &str) -> Result<SourceItem, ApiError> {
        let (kind, raw) = id.split_once(':').unwrap_or(("track", id));
        let raw = numeric(raw)?;
        match kind {
            "track" => {
                let t = by_id(s, "/tracks/", raw, &[]).await?;
                track_item(&t).ok_or_else(|| ApiError::not_found(format!("no Jamendo track {raw}")))
            }
            "album" => {
                let a = by_id(s, "/albums/", raw, &[]).await?;
                let v = get_json(
                    s,
                    "/tracks/",
                    &[
                        ("album_id", raw.to_string()),
                        ("order", "position".into()),
                        ("limit", "200".into()),
                    ],
                )
                .await?;
                let tracks: Vec<SourceTrack> = v["results"]
                    .as_array()
                    .map(|l| {
                        l.iter()
                            .enumerate()
                            .filter_map(|(i, t)| source_track(t, i as u32 + 1))
                            .collect()
                    })
                    .unwrap_or_default();
                Ok(SourceItem {
                    source: SOURCE_ID.into(),
                    id: format!("album:{raw}"),
                    title: a["name"].as_str().unwrap_or("Untitled").to_string(),
                    creator: a["artist_name"].as_str().map(str::to_string),
                    year: year_of(&a),
                    description: None,
                    licence: v["results"][0]["license_ccurl"].as_str().map(str::to_string),
                    image: image_of(&a),
                    url: page_url("album", raw),
                    tracks,
                    library_album_id: None,
                })
            }
            _ => Err(ApiError::bad_request(
                "Jamendo items are track:<id> or album:<id>",
            )),
        }
    }
}

/// The stream URL of a track (blocking; the audio thread). FLAC when Jamendo serves it.
fn audio_url_blocking(e: &JamendoEndpoints, id: &str, format: &str) -> Result<String, String> {
    if !e.creds.configured() {
        return Err("Jamendo is not configured on this node".into());
    }
    let q = [
        ("client_id", e.creds.client_id.as_str()),
        ("format", "json"),
        ("id", id),
        ("audioformat", format),
    ];
    for _ in 0..2 {
        let v: Value = crate::sources::http_client()
            .get(format!("{}/tracks/", e.base.trim_end_matches('/')))
            .query(&q)
            .send()
            .map_err(|err| err.without_url().to_string())?
            .json()
            .map_err(|err| err.without_url().to_string())?;
        if let Some(url) = v["results"][0]["audio"]
            .as_str()
            .filter(|u| u.starts_with("https://") || u.starts_with("http://"))
        {
            return Ok(url.to_string());
        }
    }
    Err(format!("Jamendo has no stream for track {id}"))
}

/// Open `jamendo:track:<id>` for a zone: FLAC first, MP3 when FLAC does not open.
pub(crate) fn open_track_blocking(
    shared: &SharedEndpoints,
    id: &str,
) -> Result<Box<dyn pm_zones::FrameSource>, pm_zones::SourceError> {
    let fail = |reason: String| pm_zones::SourceError::Open {
        uri: track_uri(id),
        reason,
    };
    if id.is_empty() || !id.bytes().all(|b| b.is_ascii_digit()) {
        return Err(fail("bad Jamendo track id".into()));
    }
    let e = shared
        .read()
        .map_err(|_| fail("jamendo endpoints".into()))?
        .clone();
    let flac = audio_url_blocking(&e, id, "flac").map_err(fail)?;
    match crate::sources::open_http_stream(&flac) {
        Ok(src) => Ok(src),
        Err(_) => {
            let mp3 = audio_url_blocking(&e, id, "mp32").map_err(fail)?;
            crate::sources::open_http_stream(&mp3).map_err(|_| fail("the stream did not open".into()))
        }
    }
}

/// `GET /v1/sources/jamendo/stream/{id}`: the track for browsers, relayed by the node so the
/// app's `client_id` never reaches the page (MP3: every browser plays it).
pub(crate) async fn stream(
    axum::extract::State(s): axum::extract::State<Arc<NodeState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Result<axum::response::Response, ApiError> {
    let id = numeric(&id)?.to_string();
    let t = by_id(&s, "/tracks/", &id, &[("audioformat", "mp32".into())]).await?;
    let url = t["audio"]
        .as_str()
        .filter(|u| u.starts_with("https://") || u.starts_with("http://"))
        .ok_or_else(|| ApiError::not_found("Jamendo has no stream for this track"))?;
    let resp = s
        .http_client()
        .get(url)
        .send()
        .await
        .map_err(|e| upstream(e.without_url()))?;
    if !resp.status().is_success() {
        return Err(upstream(format!("stream answered {}", resp.status())));
    }
    let len = resp.content_length();
    let mut out = axum::response::Response::builder()
        .header(axum::http::header::CONTENT_TYPE, "audio/mpeg")
        .header(axum::http::header::CACHE_CONTROL, "private, max-age=3600");
    if let Some(n) = len {
        out = out.header(axum::http::header::CONTENT_LENGTH, n);
    }
    out.body(axum::body::Body::from_stream(resp.bytes_stream()))
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

// ---- sign-in (OAuth 2, scope `music`): the user's likes and favourites ----

const USER_KEY: &str = "jamendo.user";

/// The signed-in Jamendo account. Access tokens last two hours; the refresh token renews them.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct UserSession {
    access_token: String,
    refresh_token: String,
    expires_at: i64,
    #[serde(default)]
    name: Option<String>,
}

fn now_unix() -> i64 {
    pm_library::db::now_unix()
}

fn pending() -> &'static std::sync::Mutex<std::collections::HashMap<String, (String, i64)>> {
    static P: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, (String, i64)>>> =
        std::sync::OnceLock::new();
    P.get_or_init(Default::default)
}

pub(crate) fn redirect_uri(s: &NodeState, headers: &axum::http::HeaderMap) -> String {
    format!("{}/jamendo/callback", crate::tidal::external_base(s, headers))
}

fn load_user(s: &NodeState) -> Option<UserSession> {
    s.library
        .lock()
        .expect("library poisoned")
        .get_setting(USER_KEY)
        .ok()
        .flatten()
        .and_then(|j| serde_json::from_str(&j).ok())
}

fn store_user(s: &NodeState, u: Option<&UserSession>) -> Result<(), ApiError> {
    let lib = s.library.lock().expect("library poisoned");
    match u {
        Some(u) => lib.set_setting(USER_KEY, &serde_json::to_string(u).expect("serializes"))?,
        None => {
            lib.delete_setting(USER_KEY)?;
        }
    }
    Ok(())
}

async fn grant(s: &NodeState, form: &[(&str, &str)]) -> Result<UserSession, ApiError> {
    let e = endpoints(s)?;
    let mut body: Vec<(&str, &str)> = vec![
        ("client_id", e.creds.client_id.as_str()),
        ("client_secret", e.creds.client_secret.as_str()),
    ];
    body.extend_from_slice(form);
    let body: String = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(body.iter())
        .finish();
    let resp = s
        .http_client()
        .post(format!("{}/oauth/grant", e.base.trim_end_matches('/')))
        .header(reqwest::header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await
        .map_err(|err| upstream(err.without_url()))?;
    let status = resp.status();
    let v: Value = resp.json().await.map_err(|err| upstream(err.without_url()))?;
    let token = v["access_token"].as_str().filter(|t| !t.is_empty());
    match (status.is_success(), token) {
        (true, Some(t)) => Ok(UserSession {
            access_token: t.to_string(),
            refresh_token: v["refresh_token"].as_str().unwrap_or("").to_string(),
            expires_at: now_unix() + v["expires_in"].as_i64().unwrap_or(7200),
            name: None,
        }),
        _ => Err(upstream(format!(
            "sign-in refused: {}",
            v["error_description"]
                .as_str()
                .or(v["error"].as_str())
                .or(v["headers"]["error_message"].as_str())
                .unwrap_or("no access token")
        ))),
    }
}

/// The user's access token, renewed when it is about to expire.
async fn user_token(s: &NodeState) -> Result<String, ApiError> {
    let u = load_user(s).ok_or_else(|| {
        ApiError::new(
            StatusCode::PRECONDITION_FAILED,
            "sign in to Jamendo first (Sources › Jamendo)",
        )
    })?;
    if u.expires_at - now_unix() > 60 {
        return Ok(u.access_token);
    }
    let mut fresh = grant(
        s,
        &[
            ("grant_type", "refresh_token"),
            ("refresh_token", &u.refresh_token),
        ],
    )
    .await?;
    fresh.name = u.name;
    store_user(s, Some(&fresh))?;
    Ok(fresh.access_token)
}

/// `POST /v1/sources/jamendo/login`: the Jamendo page where the user approves the node.
pub(crate) async fn login(
    axum::extract::State(s): axum::extract::State<Arc<NodeState>>,
    headers: axum::http::HeaderMap,
) -> crate::ApiResult<crate::tidal::LoginUrl> {
    let e = endpoints(&s)?;
    if e.creds.client_secret.trim().is_empty() {
        return Err(ApiError::new(
            StatusCode::PRECONDITION_FAILED,
            "Jamendo sign-in needs the app's client_secret too ([jamendo] or PM_JAMENDO_CLIENT_SECRET)",
        ));
    }
    let state = crate::tidal::random_urlsafe(24);
    let redirect = redirect_uri(&s, &headers);
    {
        let mut p = pending().lock().expect("pending");
        p.retain(|_, (_, at)| now_unix() - *at < 600);
        p.insert(state.clone(), (redirect.clone(), now_unix()));
    }
    let mut url =
        url::Url::parse(&format!("{}/oauth/authorize", e.base.trim_end_matches('/'))).map_err(upstream)?;
    url.query_pairs_mut()
        .append_pair("client_id", &e.creds.client_id)
        .append_pair("redirect_uri", &redirect)
        .append_pair("scope", "music")
        .append_pair("response_type", "code")
        .append_pair("state", &state);
    Ok(axum::Json(crate::tidal::LoginUrl { url: url.to_string() }))
}

#[derive(Debug, serde::Deserialize, Default)]
pub(crate) struct CallbackQuery {
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    error: Option<String>,
}

/// `GET /jamendo/callback`: finishes the sign-in and sends the browser back to Sources › Jamendo.
pub(crate) async fn callback(
    axum::extract::State(s): axum::extract::State<Arc<NodeState>>,
    axum::extract::Query(q): axum::extract::Query<CallbackQuery>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    match finish_login(&s, q).await {
        Ok(()) => axum::response::Redirect::to("/source/jamendo?jamendo=signed_in").into_response(),
        Err(e) => {
            let msg: String = url::form_urlencoded::byte_serialize(e.message.as_bytes()).collect();
            axum::response::Redirect::to(&format!("/source/jamendo?jamendo=error&message={msg}"))
                .into_response()
        }
    }
}

async fn finish_login(s: &NodeState, q: CallbackQuery) -> Result<(), ApiError> {
    if let Some(err) = q.error {
        return Err(ApiError::bad_request(format!(
            "Jamendo refused the sign-in: {err}"
        )));
    }
    let (Some(code), Some(state)) = (q.code, q.state) else {
        return Err(ApiError::bad_request("missing code or state"));
    };
    let (redirect, _) = pending()
        .lock()
        .expect("pending")
        .remove(&state)
        .ok_or_else(|| ApiError::bad_request("unknown or expired sign-in; start again"))?;
    let mut session = grant(
        s,
        &[
            ("grant_type", "authorization_code"),
            ("code", &code),
            ("redirect_uri", &redirect),
        ],
    )
    .await?;
    if let Ok(v) = get_json(s, "/users/", &[("access_token", session.access_token.clone())]).await {
        session.name = v["results"][0]["dispname"]
            .as_str()
            .or(v["results"][0]["name"].as_str())
            .map(str::to_string);
    }
    store_user(s, Some(&session))?;
    crate::vault_cache::new_sign_in_of(s, "jamendo");
    tracing::info!("Jamendo sign-in stored");
    Ok(())
}

fn status_view(s: &NodeState, headers: &axum::http::HeaderMap) -> JamendoStatus {
    let u = load_user(s);
    JamendoStatus {
        configured: configured(s),
        signed_in: u.is_some(),
        name: u.and_then(|u| u.name),
        redirect_uri: redirect_uri(s, headers),
        can_sign_in: !s
            .jamendo
            .read()
            .expect("jamendo")
            .creds
            .client_secret
            .trim()
            .is_empty(),
    }
}

/// `GET /v1/sources/jamendo/status`.
pub(crate) async fn status(
    axum::extract::State(s): axum::extract::State<Arc<NodeState>>,
    headers: axum::http::HeaderMap,
) -> crate::ApiResult<JamendoStatus> {
    Ok(axum::Json(status_view(&s, &headers)))
}

/// `POST /v1/sources/jamendo/logout`: forgets the tokens.
pub(crate) async fn logout(
    axum::extract::State(s): axum::extract::State<Arc<NodeState>>,
    headers: axum::http::HeaderMap,
) -> crate::ApiResult<JamendoStatus> {
    store_user(&s, None)?;
    Ok(axum::Json(status_view(&s, &headers)))
}

/// `POST /v1/sources/jamendo/collection/import`: the tracks the user liked or added to favourites
/// on Jamendo, as favourites on this node (by their Jamendo link).
pub(crate) async fn import_collection(
    axum::extract::State(s): axum::extract::State<Arc<NodeState>>,
) -> crate::ApiResult<crate::tidal::ImportReport> {
    let token = user_token(&s).await?;
    let mut report = crate::tidal::ImportReport::default();
    let mut ids: Vec<String> = Vec::new();
    for offset in (0..2000).step_by(200) {
        let v = get_json(
            &s,
            "/users/tracks/",
            &[
                ("access_token", token.clone()),
                ("relation", "like favorite".into()),
                ("track_type", "single albumtrack".into()),
                ("limit", "200".into()),
                ("offset", offset.to_string()),
            ],
        )
        .await?;
        let page: Vec<String> = v["results"][0]["tracks"]
            .as_array()
            .map(|a| a.iter().filter_map(id_of).collect())
            .unwrap_or_default();
        let n = page.len();
        for id in page {
            if !ids.contains(&id) {
                ids.push(id);
            }
        }
        if n < 200 {
            break;
        }
    }
    let lib = s.library.lock().expect("library poisoned");
    for id in ids {
        match pm_model::MusicUri::parse(&page_url("track", &id)) {
            Ok(uri) => {
                crate::favourites::add(&lib, uri)?;
                report.imported += 1;
            }
            Err(_) => report.skipped += 1,
        }
    }
    drop(lib);
    s.notify_library();
    Ok(axum::Json(report))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn documents_parse_and_never_carry_the_stream_url() {
        let t = serde_json::json!({"id": "1369762", "name": "Trifle № 9", "duration": 56, "artist_name": "Vladimir",
            "album_name": "Trifle № 9 (single)", "album_id": "164070", "releasedate": "2016-08-20", "position": 1,
            "image": "https://usercontent.jamendo.com?type=album&id=164070&width=300",
            "audio": "https://prod-1.storage.jamendo.com/?trackid=1369762&format=flac&from=app-SECRET",
            "license_ccurl": "http://creativecommons.org/licenses/by-nc-nd/3.0/", "shareurl": "https://www.jamendo.com/track/1369762"});
        let h = hit("track", &t).unwrap();
        assert_eq!(h.id, "track:1369762");
        assert_eq!(h.uri.as_deref(), Some("jamendo:track:1369762"));
        assert_eq!(h.url, "https://www.jamendo.com/track/1369762");
        assert_eq!((h.year, h.duration_ms), (Some(2016), Some(56_000)));
        let it = track_item(&t).unwrap();
        assert_eq!(it.title, "Trifle № 9 (single)");
        assert_eq!(it.tracks[0].title, "Trifle № 9");
        assert_eq!(it.tracks[0].url, "jamendo:track:1369762");
        assert_eq!(
            it.licence.as_deref(),
            Some("http://creativecommons.org/licenses/by-nc-nd/3.0/")
        );
        let all = serde_json::to_string(&(h, it)).unwrap();
        assert!(
            !all.contains("SECRET") && !all.contains("storage.jamendo"),
            "{all}"
        );
        assert!(hit("track", &serde_json::json!({"id": "x1", "name": "bad id"})).is_none());
        let creds = JamendoCredentials {
            client_id: "abc".into(),
            client_secret: "def".into(),
        };
        assert!(!format!("{creds:?}").contains("abc"));
    }
}
