//! Spotify through the official Web API: catalogue search (ISRC/UPC on every
//! hit, so the same recording merges with TIDAL and Deezer), the user's
//! saved tracks, albums and playlists imported by ISRC, Versions of library
//! items, and Spotify Connect devices as play targets. Spotify hands out no
//! raw audio (DRM), so nothing here streams into node zones: playback is
//! "play on a Spotify device", driven over the API.
//!
//! Sign-in is OAuth 2 Authorization Code with PKCE; the app must list
//! `<public_url>/spotify/callback` (the public URL plus `/spotify/callback`) as a redirect URI and, in development
//! mode, the owner's Spotify account under its user management.

use crate::state::NodeState;
use crate::streaming::StreamingService;
use crate::tidal::{random_urlsafe, ImportReport};
use crate::types::*;
use crate::{ApiError, ApiResult};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Redirect, Response};
use axum::Json;
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};

pub const SOURCE_ID: &str = "spotify";
const USER_KEY: &str = "spotify.user";
pub const DEFAULT_SCOPES: &str =
    "user-library-read playlist-read-private playlist-read-collaborative user-read-playback-state user-modify-playback-state";

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct SpotifyCredentials {
    pub client_id: String,
    pub client_secret: String,
}

impl SpotifyCredentials {
    pub fn configured(&self) -> bool {
        !self.client_id.trim().is_empty() && !self.client_secret.trim().is_empty()
    }
}

/// Origins, replaceable in tests.
#[derive(Debug, Clone)]
pub struct SpotifyEndpoints {
    pub creds: SpotifyCredentials,
    pub api_base: String,
    pub accounts_base: String,
}

impl Default for SpotifyEndpoints {
    fn default() -> Self {
        Self {
            creds: SpotifyCredentials::default(),
            api_base: "https://api.spotify.com/v1".into(),
            accounts_base: "https://accounts.spotify.com".into(),
        }
    }
}

pub type SharedEndpoints = Arc<RwLock<SpotifyEndpoints>>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserSession {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    pub expires_at: i64,
    #[serde(default)]
    pub user_id: Option<String>,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub country: Option<String>,
    #[serde(default)]
    pub product: Option<String>,
    #[serde(default)]
    pub scope: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpotifyStatus {
    pub configured: bool,
    pub signed_in: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub country: Option<String>,
    /// `premium`, `free`, … Connect playback needs Premium.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub product: Option<String>,
    pub redirect_uri: String,
    pub requested_scopes: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpotifyDevice {
    pub id: String,
    pub name: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub is_active: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub volume_percent: Option<u8>,
}

/// `POST /v1/sources/spotify/play {uris, device_id?}`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PlayOnSpotify {
    /// `spotify:track:<id>` URIs or `track:<id>` / `album:<id>` source ids.
    pub uris: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpotifyPlaylist {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub items: u64,
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
}

fn upstream(e: impl std::fmt::Display) -> ApiError {
    ApiError::new(StatusCode::BAD_GATEWAY, format!("spotify: {e}"))
}

fn now_unix() -> i64 {
    pm_library::db::now_unix()
}

fn endpoints(s: &NodeState) -> SpotifyEndpoints {
    s.spotify.read().expect("spotify endpoints").clone()
}

fn creds(s: &NodeState) -> SpotifyCredentials {
    endpoints(s).creds
}

fn api_base(s: &NodeState) -> String {
    endpoints(s).api_base.trim_end_matches('/').to_string()
}

fn accounts_base(s: &NodeState) -> String {
    endpoints(s).accounts_base.trim_end_matches('/').to_string()
}

pub(crate) fn redirect_uri(s: &NodeState, headers: &axum::http::HeaderMap) -> String {
    format!("{}/spotify/callback", crate::tidal::external_base(s, headers))
}

/// Pending sign-ins: state → (PKCE verifier, redirect URI used).
fn pending() -> &'static Mutex<HashMap<String, (String, String)>> {
    static P: std::sync::OnceLock<Mutex<HashMap<String, (String, String)>>> = std::sync::OnceLock::new();
    P.get_or_init(|| Mutex::new(HashMap::new()))
}

fn app_token_cache() -> &'static Mutex<Option<(String, i64)>> {
    static C: std::sync::OnceLock<Mutex<Option<(String, i64)>>> = std::sync::OnceLock::new();
    C.get_or_init(|| Mutex::new(None))
}

async fn token_request(s: &NodeState, form: &[(&str, &str)]) -> Result<Value, ApiError> {
    let c = creds(s);
    if !c.configured() {
        return Err(ApiError::new(
            StatusCode::PRECONDITION_FAILED,
            "Spotify is not configured: add [spotify] client_id and client_secret (PM_SPOTIFY_CLIENT_ID/SECRET)",
        ));
    }
    let body = {
        let mut ser = url::form_urlencoded::Serializer::new(String::new());
        for (k, v) in form {
            ser.append_pair(k, v);
        }
        ser.finish()
    };
    let resp = s
        .http_client()
        .post(format!("{}/api/token", accounts_base(s)))
        .basic_auth(&c.client_id, Some(&c.client_secret))
        .header(reqwest::header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await
        .map_err(upstream)?;
    let status = resp.status();
    let v: Value = resp.json().await.map_err(upstream)?;
    if !status.is_success() {
        return Err(upstream(format!(
            "token endpoint {status}: {}",
            v["error_description"]
                .as_str()
                .or(v["error"].as_str())
                .unwrap_or("")
        )));
    }
    Ok(v)
}

/// Client-credentials token for catalogue search without a user.
async fn app_token(s: &NodeState) -> Result<String, ApiError> {
    if let Some((t, exp)) = app_token_cache().lock().expect("app token").clone() {
        if exp - now_unix() > 60 {
            return Ok(t);
        }
    }
    let v = token_request(s, &[("grant_type", "client_credentials")]).await?;
    let token = v["access_token"]
        .as_str()
        .ok_or_else(|| upstream("no access_token"))?
        .to_string();
    let expires_at = now_unix() + v["expires_in"].as_i64().unwrap_or(3600);
    *app_token_cache().lock().expect("app token") = Some((token.clone(), expires_at));
    Ok(token)
}

pub(crate) fn load_user(s: &NodeState) -> Option<UserSession> {
    let lib = s.library.lock().expect("library poisoned");
    lib.get_setting(USER_KEY)
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

/// The user's access token, refreshed when within a minute of expiry.
pub(crate) async fn user_token(s: &NodeState) -> Result<Option<String>, ApiError> {
    let Some(mut u) = load_user(s) else {
        return Ok(None);
    };
    if u.expires_at - now_unix() > 60 {
        return Ok(Some(u.access_token));
    }
    let Some(refresh) = u.refresh_token.clone() else {
        return Ok(None);
    };
    let v = token_request(s, &[("grant_type", "refresh_token"), ("refresh_token", &refresh)]).await?;
    u.access_token = v["access_token"]
        .as_str()
        .ok_or_else(|| upstream("no access_token"))?
        .to_string();
    u.expires_at = now_unix() + v["expires_in"].as_i64().unwrap_or(3600);
    if let Some(r) = v["refresh_token"].as_str() {
        u.refresh_token = Some(r.to_string());
    }
    store_user(s, Some(&u))?;
    Ok(Some(u.access_token))
}

/// A user token when signed in, else the app token (catalogue only).
async fn bearer(s: &NodeState) -> Result<String, ApiError> {
    match user_token(s).await? {
        Some(t) => Ok(t),
        None => app_token(s).await,
    }
}

fn need_user(s: &NodeState) -> Result<UserSession, ApiError> {
    load_user(s).ok_or_else(|| {
        ApiError::new(
            StatusCode::PRECONDITION_FAILED,
            "not signed in to Spotify: Sources → Connect Spotify",
        )
    })
}

/// The album of a Spotify track: (title, link, cover, year).
pub(crate) async fn track_album(
    s: &NodeState,
    id: &str,
) -> Option<(String, String, Option<String>, Option<i32>)> {
    let t = valid_id(id).ok()?;
    let v = get_json(s, &format!("/tracks/{t}"), &[]).await.ok()?;
    let a = &v["album"];
    let aid = a["id"].as_str()?;
    Some((
        a["name"].as_str()?.to_string(),
        spotify_url("album", aid),
        image_of(a),
        year_of(a),
    ))
}

/// A Spotify track's ISRC (needs the node's Spotify app).
pub(crate) async fn track_isrc(s: &NodeState, id: &str) -> Option<String> {
    let t = valid_id(id).ok()?;
    let v = get_json(s, &format!("/tracks/{t}"), &[]).await.ok()?;
    v["external_ids"]["isrc"].as_str().map(str::to_string)
}

async fn get_json(s: &NodeState, path: &str, query: &[(&str, String)]) -> Result<Value, ApiError> {
    let token = bearer(s).await?;
    let url = if path.starts_with("http") {
        path.to_string()
    } else {
        format!("{}{path}", api_base(s))
    };
    for attempt in 0..3 {
        let resp = s
            .http_client()
            .get(&url)
            .query(query)
            .bearer_auth(&token)
            .send()
            .await
            .map_err(upstream)?;
        let status = resp.status();
        if status == StatusCode::TOO_MANY_REQUESTS && attempt < 2 {
            let wait = resp
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(1)
                .min(10);
            tokio::time::sleep(std::time::Duration::from_secs(wait)).await;
            continue;
        }
        if status == StatusCode::NOT_FOUND {
            return Err(ApiError::not_found(format!("Spotify has no {path}")));
        }
        let text = resp.text().await.map_err(upstream)?;
        if !status.is_success() {
            let msg = serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|v| v["error"]["message"].as_str().map(str::to_string))
                .unwrap_or_else(|| text.chars().take(200).collect());
            return Err(upstream(format!("{path} answered {status}: {msg}")));
        }
        if text.trim().is_empty() {
            return Ok(Value::Null);
        }
        return serde_json::from_str(&text).map_err(|e| upstream(format!("{path}: {e}")));
    }
    Err(upstream("rate limited"))
}

// ---- hits ----

fn spotify_url(kind: &str, id: &str) -> String {
    format!("https://open.spotify.com/{kind}/{id}")
}

fn image_of(v: &Value) -> Option<String> {
    v["images"]
        .as_array()
        .and_then(|a| a.first())
        .and_then(|i| i["url"].as_str())
        .map(str::to_string)
}

fn artists_of(v: &Value) -> Option<String> {
    let names: Vec<&str> = v["artists"]
        .as_array()
        .map(|a| a.iter().filter_map(|x| x["name"].as_str()).collect())
        .unwrap_or_default();
    (!names.is_empty()).then(|| names.join(", "))
}

fn year_of(v: &Value) -> Option<i32> {
    v["release_date"]
        .as_str()
        .and_then(|d| d.get(..4))
        .and_then(|y| y.parse().ok())
}

fn track_hit(t: &Value) -> Option<SourceHit> {
    let id = t["id"].as_str()?.to_string();
    Some(SourceHit {
        id: format!("track:{id}"),
        title: t["name"].as_str()?.to_string(),
        creator: artists_of(t),
        year: year_of(&t["album"]),
        downloads: t["popularity"].as_u64(),
        image: image_of(&t["album"]),
        kind: "track".into(),
        duration_ms: t["duration_ms"].as_u64(),
        library_artist_id: None,
        isrc: t["external_ids"]["isrc"].as_str().map(str::to_string),
        upc: None,
        library_track_id: None,
        library_album_id: None,
        // No queue URI: Spotify audio never enters a node zone; "Play on Spotify" uses the id.
        uri: None,
        url: spotify_url("track", &id),
    })
}

fn album_hit(a: &Value) -> Option<SourceHit> {
    let id = a["id"].as_str()?.to_string();
    Some(SourceHit {
        id: format!("album:{id}"),
        title: a["name"].as_str()?.to_string(),
        creator: artists_of(a),
        year: year_of(a),
        downloads: None,
        image: image_of(a),
        kind: "album".into(),
        duration_ms: None,
        library_artist_id: None,
        isrc: None,
        upc: a["external_ids"]["upc"].as_str().map(str::to_string),
        library_track_id: None,
        library_album_id: None,
        uri: None,
        url: spotify_url("album", &id),
    })
}

fn artist_hit(a: &Value) -> Option<SourceHit> {
    let id = a["id"].as_str()?.to_string();
    Some(SourceHit {
        id: format!("artist:{id}"),
        title: a["name"].as_str()?.to_string(),
        creator: None,
        year: None,
        downloads: a["followers"]["total"].as_u64(),
        image: image_of(a),
        kind: "artist".into(),
        duration_ms: None,
        library_artist_id: None,
        isrc: None,
        upc: None,
        library_track_id: None,
        library_album_id: None,
        uri: None,
        url: spotify_url("artist", &id),
    })
}

fn source_track(t: &Value, n: u32) -> Option<SourceTrack> {
    Some(SourceTrack {
        title: t["name"].as_str()?.to_string(),
        track_no: t["track_number"].as_u64().map(|x| x as u32).or(Some(n)),
        duration_ms: t["duration_ms"].as_u64(),
        format: "Spotify (plays on a Spotify device)".into(),
        url: format!("spotify:track:{}", t["id"].as_str().unwrap_or("")),
        artist: artists_of(t),
    })
}

/// The canonical URI of a track: ISRC first, else its Spotify link.
pub(crate) fn canonical_track(t: &Value) -> Option<pm_model::MusicUri> {
    if let Some(isrc) = t["external_ids"]["isrc"].as_str() {
        if let Ok(i) = pm_model::Isrc::parse(isrc) {
            return Some(pm_model::MusicUri::Isrc { isrc: i });
        }
    }
    let id = t["id"].as_str()?;
    pm_model::MusicUri::parse(&spotify_url("track", id)).ok()
}

fn valid_id(id: &str) -> Result<&str, ApiError> {
    if id.is_empty() || id.len() > 64 || !id.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return Err(ApiError::bad_request("bad Spotify id"));
    }
    Ok(id)
}

pub struct Spotify;

#[async_trait::async_trait]
impl StreamingService for Spotify {
    fn id(&self) -> &'static str {
        SOURCE_ID
    }

    fn unified(&self, s: &NodeState) -> bool {
        creds(s).configured()
    }

    fn info(&self, s: &NodeState) -> SourceInfo {
        let c = creds(s).configured();
        let u = load_user(s);
        SourceInfo {
            id: SOURCE_ID.into(),
            name: "Spotify".into(),
            description: "Catalogue with ISRC/UPC, your library and playlists imported by ISRC, playback on your Spotify Connect devices (no audio enters the node: DRM)".into(),
            status: if !c {
                "not configured: add [spotify] client_id and client_secret".into()
            } else if u.is_some() {
                "signed in".into()
            } else {
                "ready (sign in for your library and devices)".into()
            },
        }
    }

    async fn search(&self, s: &NodeState, term: &str, limit: u32) -> Result<Vec<SourceHit>, ApiError> {
        let v = get_json(
            s,
            "/search",
            &[
                ("q", term.to_string()),
                ("type", "track,album,artist".into()),
                ("limit", limit.clamp(1, 50).to_string()),
            ],
        )
        .await?;
        let mut out = Vec::new();
        let needle = term.trim().to_lowercase();
        let mut artists: Vec<SourceHit> = v["artists"]["items"]
            .as_array()
            .map(|a| a.iter().filter_map(artist_hit).collect())
            .unwrap_or_default();
        artists.sort_by_key(|h| {
            let n = h.title.to_lowercase();
            if n == needle {
                0
            } else if n.starts_with(&needle) {
                1
            } else {
                2
            }
        });
        artists.truncate(3);
        out.extend(artists);
        out.extend(
            v["albums"]["items"]
                .as_array()
                .map(|a| a.iter().filter_map(album_hit).take(20).collect::<Vec<_>>())
                .unwrap_or_default(),
        );
        out.extend(
            v["tracks"]["items"]
                .as_array()
                .map(|a| a.iter().filter_map(track_hit).collect::<Vec<_>>())
                .unwrap_or_default(),
        );
        Ok(out)
    }

    async fn item(&self, s: &NodeState, id: &str) -> Result<SourceItem, ApiError> {
        if let Some(a) = id.strip_prefix("album:") {
            let a = valid_id(a)?;
            let v = get_json(s, &format!("/albums/{a}"), &[]).await?;
            let mut tracks = Vec::new();
            let mut page = v["tracks"].clone();
            for _ in 0..20 {
                for (i, t) in page["items"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
                    .iter()
                    .enumerate()
                {
                    if let Some(st) = source_track(t, i as u32 + 1) {
                        tracks.push(st);
                    }
                }
                let Some(next) = page["next"].as_str().map(str::to_string) else {
                    break;
                };
                page = get_json(s, &next, &[]).await?;
            }
            return Ok(SourceItem {
                source: SOURCE_ID.into(),
                id: format!("album:{a}"),
                title: v["name"].as_str().unwrap_or("").to_string(),
                creator: artists_of(&v),
                year: year_of(&v),
                description: v["label"].as_str().map(|l| format!("Label: {l}")),
                licence: None,
                image: image_of(&v),
                url: spotify_url("album", a),
                tracks,
                library_album_id: None,
            });
        }
        let t = valid_id(id.strip_prefix("track:").unwrap_or(id))?;
        let v = get_json(s, &format!("/tracks/{t}"), &[]).await?;
        Ok(SourceItem {
            source: SOURCE_ID.into(),
            id: format!("track:{t}"),
            title: v["name"].as_str().unwrap_or("").to_string(),
            creator: artists_of(&v),
            year: year_of(&v["album"]),
            description: v["album"]["name"].as_str().map(|a| format!("From {a}")),
            licence: None,
            image: image_of(&v["album"]),
            url: spotify_url("track", t),
            tracks: source_track(&v, 1).into_iter().collect(),
            library_album_id: None,
        })
    }

    async fn artist(&self, s: &NodeState, id: &str) -> Result<SourceArtist, ApiError> {
        let a = valid_id(id.strip_prefix("artist:").unwrap_or(id))?;
        let (p1, p2, q2) = (
            format!("/artists/{a}"),
            format!("/artists/{a}/albums"),
            [
                ("include_groups", "album,single".to_string()),
                ("limit", "50".to_string()),
            ],
        );
        let (ar, albums) = tokio::join!(get_json(s, &p1, &[]), get_json(s, &p2, &q2));
        let ar = ar?;
        let name = ar["name"].as_str().unwrap_or("").to_string();
        let mut albums: Vec<SourceHit> = albums?["items"]
            .as_array()
            .map(|l| l.iter().filter_map(album_hit).collect())
            .unwrap_or_default();
        albums.sort_by_key(|x| std::cmp::Reverse(x.year));
        Ok(SourceArtist {
            source: SOURCE_ID.into(),
            id: format!("artist:{a}"),
            name,
            image: image_of(&ar),
            url: spotify_url("artist", a),
            albums,
            library_artist_id: None,
        })
    }
}

// ---- sign-in ----

pub(crate) async fn login(
    State(s): State<Arc<NodeState>>,
    headers: axum::http::HeaderMap,
) -> ApiResult<crate::tidal::LoginUrl> {
    let c = creds(&s);
    if !c.configured() {
        return Err(ApiError::new(
            StatusCode::PRECONDITION_FAILED,
            "Spotify is not configured: add [spotify] client_id and client_secret",
        ));
    }
    use sha2::Digest;
    let verifier = random_urlsafe(48);
    let challenge =
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sha2::Sha256::digest(verifier.as_bytes()));
    let state = random_urlsafe(24);
    let redirect = redirect_uri(&s, &headers);
    pending()
        .lock()
        .expect("pending")
        .insert(state.clone(), (verifier, redirect.clone()));
    let mut url = url::Url::parse(&format!("{}/authorize", accounts_base(&s))).map_err(upstream)?;
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", &c.client_id)
        .append_pair("redirect_uri", &redirect)
        .append_pair("scope", DEFAULT_SCOPES)
        .append_pair("code_challenge_method", "S256")
        .append_pair("code_challenge", &challenge)
        .append_pair("state", &state);
    Ok(Json(crate::tidal::LoginUrl { url: url.to_string() }))
}

#[derive(Debug, Deserialize, Default)]
pub(crate) struct CallbackQuery {
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    error: Option<String>,
}

/// `GET /spotify/callback`: finishes the sign-in and sends the browser back to the UI.
pub(crate) async fn callback(State(s): State<Arc<NodeState>>, Query(q): Query<CallbackQuery>) -> Response {
    match finish_login(&s, q).await {
        Ok(()) => Redirect::to("/?spotify=ok").into_response(),
        Err(e) => {
            Redirect::to(&format!("/?spotify=error&message={}", urlencoding(&e.message))).into_response()
        }
    }
}

fn urlencoding(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

async fn finish_login(s: &NodeState, q: CallbackQuery) -> Result<(), ApiError> {
    if let Some(err) = q.error {
        return Err(ApiError::bad_request(format!(
            "Spotify refused the sign-in: {err}"
        )));
    }
    let (Some(code), Some(state)) = (q.code, q.state) else {
        return Err(ApiError::bad_request("missing code or state"));
    };
    let (verifier, redirect) = pending()
        .lock()
        .expect("pending")
        .remove(&state)
        .ok_or_else(|| ApiError::bad_request("unknown or expired sign-in state; start again"))?;
    let c = creds(s);
    let v = token_request(
        s,
        &[
            ("grant_type", "authorization_code"),
            ("code", &code),
            ("redirect_uri", &redirect),
            ("client_id", &c.client_id),
            ("code_verifier", &verifier),
        ],
    )
    .await?;
    let mut session = UserSession {
        access_token: v["access_token"]
            .as_str()
            .ok_or_else(|| upstream("no access_token"))?
            .to_string(),
        refresh_token: v["refresh_token"].as_str().map(str::to_string),
        expires_at: now_unix() + v["expires_in"].as_i64().unwrap_or(3600),
        user_id: None,
        display_name: None,
        country: None,
        product: None,
        scope: v["scope"].as_str().unwrap_or("").to_string(),
    };
    store_user(s, Some(&session))?;
    if let Ok(me) = get_json(s, "/me", &[]).await {
        session.user_id = me["id"].as_str().map(str::to_string);
        session.display_name = me["display_name"].as_str().map(str::to_string);
        session.country = me["country"].as_str().map(str::to_string);
        session.product = me["product"].as_str().map(str::to_string);
        store_user(s, Some(&session))?;
    }
    tracing::info!(user = ?session.user_id, "Spotify sign-in stored");
    Ok(())
}

pub(crate) fn status_view(s: &NodeState, headers: &axum::http::HeaderMap) -> SpotifyStatus {
    let u = load_user(s);
    SpotifyStatus {
        configured: creds(s).configured(),
        signed_in: u.is_some(),
        user_id: u.as_ref().and_then(|u| u.user_id.clone()),
        display_name: u.as_ref().and_then(|u| u.display_name.clone()),
        country: u.as_ref().and_then(|u| u.country.clone()),
        product: u.as_ref().and_then(|u| u.product.clone()),
        redirect_uri: redirect_uri(s, headers),
        requested_scopes: DEFAULT_SCOPES.into(),
    }
}

pub(crate) async fn status(
    State(s): State<Arc<NodeState>>,
    headers: axum::http::HeaderMap,
) -> ApiResult<SpotifyStatus> {
    Ok(Json(status_view(&s, &headers)))
}

pub(crate) async fn logout(
    State(s): State<Arc<NodeState>>,
    headers: axum::http::HeaderMap,
) -> ApiResult<SpotifyStatus> {
    store_user(&s, None)?;
    Ok(Json(status_view(&s, &headers)))
}

// ---- library ----

async fn paged_items(s: &NodeState, first: &str, query: &[(&str, String)]) -> Result<Vec<Value>, ApiError> {
    let mut out = Vec::new();
    let mut page = get_json(s, first, query).await?;
    for _ in 0..200 {
        out.extend(page["items"].as_array().cloned().unwrap_or_default());
        let Some(next) = page["next"].as_str().map(str::to_string) else {
            break;
        };
        page = get_json(s, &next, &[]).await?;
    }
    Ok(out)
}

/// `GET /v1/sources/spotify/collection`: saved tracks and albums as hits.
pub(crate) async fn collection(State(s): State<Arc<NodeState>>) -> ApiResult<SourceResults> {
    need_user(&s)?;
    let limit = [("limit", "50".to_string())];
    let (tracks, albums) = tokio::join!(
        paged_items(&s, "/me/tracks", &limit),
        paged_items(&s, "/me/albums", &limit)
    );
    let mut items: Vec<SourceHit> = albums?.iter().filter_map(|a| album_hit(&a["album"])).collect();
    items.extend(tracks?.iter().filter_map(|t| track_hit(&t["track"])));
    let lib = s.library.lock().expect("library poisoned");
    for h in &mut items {
        crate::streaming::mark_library(&lib, h);
    }
    Ok(Json(SourceResults {
        source: SOURCE_ID.into(),
        items,
    }))
}

/// `POST /v1/sources/spotify/collection/import`: saved tracks (by ISRC) and albums as favourites.
pub(crate) async fn import_collection(State(s): State<Arc<NodeState>>) -> ApiResult<ImportReport> {
    need_user(&s)?;
    let tracks = paged_items(&s, "/me/tracks", &[("limit", "50".into())]).await?;
    let albums = paged_items(&s, "/me/albums", &[("limit", "50".into())]).await?;
    let mut report = ImportReport::default();
    let lib = s.library.lock().expect("library poisoned");
    for t in &tracks {
        match canonical_track(&t["track"]) {
            Some(uri) => {
                crate::favourites::add(&lib, uri)?;
                report.imported += 1;
            }
            None => report.skipped += 1,
        }
    }
    for a in &albums {
        match a["album"]["id"]
            .as_str()
            .and_then(|id| pm_model::MusicUri::parse(&spotify_url("album", id)).ok())
        {
            Some(uri) => {
                crate::favourites::add(&lib, uri)?;
                report.imported += 1;
            }
            None => report.skipped += 1,
        }
    }
    drop(lib);
    s.notify_library();
    Ok(Json(report))
}

/// `GET /v1/sources/spotify/playlists`: the user's playlists.
pub(crate) async fn playlists(State(s): State<Arc<NodeState>>) -> ApiResult<Vec<SpotifyPlaylist>> {
    need_user(&s)?;
    let items = paged_items(&s, "/me/playlists", &[("limit", "50".into())]).await?;
    Ok(Json(
        items
            .iter()
            .filter_map(|p| {
                Some(SpotifyPlaylist {
                    id: p["id"].as_str()?.to_string(),
                    name: p["name"].as_str().unwrap_or("Playlist").to_string(),
                    description: p["description"]
                        .as_str()
                        .filter(|d| !d.is_empty())
                        .map(str::to_string),
                    items: p["tracks"]["total"].as_u64().unwrap_or(0),
                    url: spotify_url("playlist", p["id"].as_str()?),
                    image: image_of(p),
                })
            })
            .collect(),
    ))
}

/// `POST /v1/sources/spotify/playlists/{id}/import`: a Pubky playlist by ISRC.
pub(crate) async fn import_playlist(
    State(s): State<Arc<NodeState>>,
    Path(id): Path<String>,
    axum::extract::Query(opt): axum::extract::Query<ImportOptions>,
) -> ApiResult<ImportReport> {
    let keep = opt.keep_source.unwrap_or(false);
    need_user(&s)?;
    let id = valid_id(&id)?.to_string();
    let head = get_json(
        &s,
        &format!("/playlists/{id}"),
        &[("fields", "name,description".into())],
    )
    .await?;
    let name = head["name"].as_str().unwrap_or("Spotify playlist").to_string();
    let description = head["description"]
        .as_str()
        .filter(|d| !d.is_empty())
        .map(str::to_string);
    let items = paged_items(&s, &format!("/playlists/{id}/tracks"), &[("limit", "100".into())]).await?;
    let mut uris = Vec::new();
    let mut sources = Vec::new();
    let mut skipped = 0usize;
    for it in &items {
        match canonical_track(&it["track"]) {
            Some(u) => {
                uris.push(u.to_url());
                sources.push(keep.then(|| it["track"]["id"].as_str()).flatten().map(|tid| {
                    pm_pubky::ItemSource {
                        service: SOURCE_ID.into(),
                        uri: Some(format!("https://open.spotify.com/track/{tid}")),
                    }
                }));
            }
            None => skipped += 1,
        }
    }
    if uris.is_empty() {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "none of the playlist's items could be identified",
        ));
    }
    let imported = uris.len();
    let Json(detail) = crate::playlists::create(
        State(s.clone()),
        Json(CreatePlaylist {
            name,
            description,
            image: None,
            uris,
            track_ids: Vec::new(),
            sources,
        }),
    )
    .await?;
    Ok(Json(ImportReport {
        imported,
        skipped,
        playlist: Some(detail),
    }))
}

// ---- versions ----

fn version(kind: &str, v: &Value) -> Option<Version> {
    let id = v["id"].as_str()?;
    Some(Version {
        page: None,
        source: SOURCE_ID.into(),
        id: format!("{kind}:{id}"),
        label: format!("Spotify · {}", v["name"].as_str().unwrap_or("")),
        url: spotify_url(kind, id),
        uri: None,
    })
}

pub(crate) async fn album_versions(s: &NodeState, album: &pm_model::Album) -> Result<Vec<Version>, ApiError> {
    if !creds(s).configured() {
        return Ok(Vec::new());
    }
    let Some(upc) = album.upc.as_ref() else {
        return Ok(Vec::new());
    };
    let key = format!("spotify.version.album.{}", album.id.unwrap_or(0));
    if let Some(v) = crate::tidal::cache_get(s, &key) {
        return Ok(v);
    }
    let v = get_json(
        s,
        "/search",
        &[
            ("q", format!("upc:{upc}")),
            ("type", "album".into()),
            ("limit", "5".into()),
        ],
    )
    .await?;
    let out: Vec<Version> = v["albums"]["items"]
        .as_array()
        .map(|a| a.iter().filter_map(|x| version("album", x)).collect())
        .unwrap_or_default();
    crate::tidal::cache_put(s, &key, &out);
    Ok(out)
}

pub(crate) async fn track_versions(s: &NodeState, track: &pm_model::Track) -> Result<Vec<Version>, ApiError> {
    if !creds(s).configured() {
        return Ok(Vec::new());
    }
    let Some(isrc) = track.isrc.as_ref() else {
        return Ok(Vec::new());
    };
    isrc_versions(s, &isrc.to_string()).await
}

/// Spotify tracks with this ISRC (cached per ISRC).
pub(crate) async fn isrc_versions(s: &NodeState, isrc: &str) -> Result<Vec<Version>, ApiError> {
    if !creds(s).configured() {
        return Ok(Vec::new());
    }
    let key = format!("spotify.version.isrc.{isrc}");
    if let Some(v) = crate::tidal::cache_get(s, &key) {
        return Ok(v);
    }
    let v = get_json(
        s,
        "/search",
        &[
            ("q", format!("isrc:{isrc}")),
            ("type", "track".into()),
            ("limit", "5".into()),
        ],
    )
    .await?;
    let out: Vec<Version> = v["tracks"]["items"]
        .as_array()
        .map(|a| a.iter().filter_map(|x| version("track", x)).collect())
        .unwrap_or_default();
    crate::tidal::cache_put(s, &key, &out);
    Ok(out)
}

// ---- Spotify Connect ----

/// `GET /v1/sources/spotify/devices`: the user's Spotify Connect devices.
pub(crate) async fn devices(State(s): State<Arc<NodeState>>) -> ApiResult<Vec<SpotifyDevice>> {
    need_user(&s)?;
    let v = get_json(&s, "/me/player/devices", &[]).await?;
    let d: Vec<SpotifyDevice> = serde_json::from_value(v["devices"].clone()).unwrap_or_default();
    Ok(Json(d))
}

/// `POST /v1/sources/spotify/play {uris, device_id?}`: start playback on a
/// device (the active one when none is given). Premium only, per Spotify.
pub(crate) async fn play(
    State(s): State<Arc<NodeState>>,
    Json(req): Json<PlayOnSpotify>,
) -> ApiResult<serde_json::Value> {
    need_user(&s)?;
    let token = user_token(&s).await?.ok_or_else(|| {
        ApiError::new(
            StatusCode::PRECONDITION_FAILED,
            "Spotify session expired; sign in again",
        )
    })?;
    let mut uris = Vec::new();
    let mut context: Option<String> = None;
    for u in &req.uris {
        if let Some(id) = u.strip_prefix("spotify:track:") {
            uris.push(format!("spotify:track:{}", valid_id(id)?));
        } else if let Some(id) = u.strip_prefix("track:") {
            uris.push(format!("spotify:track:{}", valid_id(id)?));
        } else if let Some(id) = u
            .strip_prefix("album:")
            .or_else(|| u.strip_prefix("spotify:album:"))
        {
            context = Some(format!("spotify:album:{}", valid_id(id)?));
        } else {
            return Err(ApiError::bad_request(format!(
                "{u} is not a Spotify track or album"
            )));
        }
    }
    if uris.is_empty() && context.is_none() {
        return Err(ApiError::bad_request("uris is required"));
    }
    let body = match context {
        Some(c) => serde_json::json!({ "context_uri": c }),
        None => serde_json::json!({ "uris": uris }),
    };
    let mut url = format!("{}/me/player/play", api_base(&s));
    if let Some(d) = req.device_id.as_deref().filter(|d| !d.is_empty()) {
        url = format!("{url}?device_id={}", urlencoding(d));
    }
    let resp = s
        .http_client()
        .put(&url)
        .bearer_auth(&token)
        .json(&body)
        .send()
        .await
        .map_err(upstream)?;
    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        let msg = serde_json::from_str::<Value>(&text)
            .ok()
            .and_then(|v| v["error"]["message"].as_str().map(str::to_string))
            .unwrap_or(text);
        let hint = if status == StatusCode::NOT_FOUND {
            " (no active Spotify device: open Spotify on a device first, or pass device_id)"
        } else if status == StatusCode::FORBIDDEN {
            " (Spotify Connect playback needs Premium)"
        } else {
            ""
        };
        return Err(upstream(format!("play answered {status}: {msg}{hint}")));
    }
    Ok(Json(serde_json::json!({ "ok": true })))
}
