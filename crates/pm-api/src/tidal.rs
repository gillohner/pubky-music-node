//! TIDAL through the official TIDAL API (openapi.tidal.com/v2, JSON:API).
//! The developer app's client credentials search the catalogue; one user
//! signs in with OAuth 2 + PKCE (scopes incl. `playback`) so the node can
//! fetch stream manifests and the user's collection. One account per node,
//! never proxied for other people (handoff §2). Tokens live in the settings
//! table of the node's own database.

use crate::state::NodeState;
use crate::streaming::StreamingService;
use crate::types::*;
use crate::{ApiError, ApiResult};
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Redirect, Response};
use axum::Json;
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

pub const SOURCE_ID: &str = "tidal";
const USER_KEY: &str = "tidal.user";
const SCOPES: &str =
    "user.read collection.read collection.write playlists.read playlists.write playback entitlements.read search.read";
const ACCEPT: &str = "application/vnd.api+json";

/// Developer app credentials from the node config.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TidalCredentials {
    pub client_id: String,
    pub client_secret: String,
}

impl TidalCredentials {
    pub fn configured(&self) -> bool {
        !self.client_id.trim().is_empty() && !self.client_secret.trim().is_empty()
    }
}

/// Credentials plus the three origins TIDAL is reached at (tests point them
/// at a mock). Shared by the API handlers and the audio thread's resolver.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TidalEndpoints {
    pub creds: TidalCredentials,
    pub api_base: String,
    pub auth_base: String,
    pub login_base: String,
}

impl Default for TidalEndpoints {
    fn default() -> Self {
        Self {
            creds: TidalCredentials::default(),
            api_base: "https://openapi.tidal.com/v2".into(),
            auth_base: "https://auth.tidal.com".into(),
            login_base: "https://login.tidal.com".into(),
        }
    }
}

pub type SharedEndpoints = Arc<std::sync::RwLock<TidalEndpoints>>;

/// The signed-in user's tokens, stored as JSON under `tidal.user`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserSession {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    /// Unix seconds.
    pub expires_at: i64,
    #[serde(default)]
    pub user_id: Option<String>,
    #[serde(default)]
    pub country: Option<String>,
    #[serde(default)]
    pub scope: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TidalStatus {
    /// Client credentials are present in the node config.
    pub configured: bool,
    pub signed_in: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub country: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
    /// Where TIDAL must be allowed to redirect to (register it on the developer app).
    pub redirect_uri: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoginUrl {
    pub url: String,
}

fn upstream(e: impl std::fmt::Display) -> ApiError {
    ApiError::new(StatusCode::BAD_GATEWAY, format!("tidal: {e}"))
}

fn now_unix() -> i64 {
    pm_library::db::now_unix()
}

fn endpoints(s: &NodeState) -> TidalEndpoints {
    s.tidal.read().expect("tidal endpoints").clone()
}

fn creds(s: &NodeState) -> TidalCredentials {
    endpoints(s).creds
}

fn api_base(s: &NodeState) -> String {
    endpoints(s).api_base.trim_end_matches('/').to_string()
}

fn auth_base(s: &NodeState) -> String {
    endpoints(s).auth_base.trim_end_matches('/').to_string()
}

fn login_base(s: &NodeState) -> String {
    endpoints(s).login_base.trim_end_matches('/').to_string()
}

pub(crate) fn redirect_uri(s: &NodeState) -> String {
    format!("{}/tidal/callback", s.public_url.trim_end_matches('/'))
}

// ---- tokens ----

/// Pending sign-ins: OAuth `state` → PKCE verifier.
fn pending() -> &'static Mutex<HashMap<String, String>> {
    static P: std::sync::OnceLock<Mutex<HashMap<String, String>>> = std::sync::OnceLock::new();
    P.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The app's own token (client credentials), cached until shortly before it expires.
fn app_token_cache() -> &'static Mutex<Option<(String, i64)>> {
    static T: std::sync::OnceLock<Mutex<Option<(String, i64)>>> = std::sync::OnceLock::new();
    T.get_or_init(|| Mutex::new(None))
}

async fn token_request(s: &NodeState, form: &[(&str, &str)]) -> Result<Value, ApiError> {
    let c = creds(s);
    if !c.configured() {
        return Err(ApiError::new(
            StatusCode::PRECONDITION_FAILED,
            "TIDAL is not configured: add [tidal] client_id and client_secret to the node config",
        ));
    }
    // Built before any await: the serializer is not Send.
    let body: String = {
        let mut ser = url::form_urlencoded::Serializer::new(String::new());
        for (k, v) in form {
            ser.append_pair(k, v);
        }
        ser.finish()
    };
    let resp = s
        .http_client()
        .post(format!("{}/v1/oauth2/token", auth_base(s)))
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
            "token endpoint answered {status}: {}",
            v["error_description"]
                .as_str()
                .or(v["error"].as_str())
                .unwrap_or("")
        )));
    }
    Ok(v)
}

async fn app_token(s: &NodeState) -> Result<String, ApiError> {
    if let Some((t, exp)) = app_token_cache().lock().expect("app token").clone() {
        if exp - now_unix() > 60 {
            return Ok(t);
        }
    }
    let v = token_request(s, &[("grant_type", "client_credentials")]).await?;
    let token = v["access_token"]
        .as_str()
        .ok_or_else(|| upstream("no access_token in the token reply"))?
        .to_string();
    let expires_at = now_unix() + v["expires_in"].as_i64().unwrap_or(3600);
    *app_token_cache().lock().expect("app token") = Some((token.clone(), expires_at));
    Ok(token)
}

fn load_user_from(lib: &pm_library::Library) -> Option<UserSession> {
    lib.get_setting(USER_KEY)
        .ok()
        .flatten()
        .and_then(|j| serde_json::from_str(&j).ok())
}

pub(crate) fn load_user(s: &NodeState) -> Option<UserSession> {
    load_user_from(&s.library.lock().expect("library poisoned"))
}

fn store_user(s: &NodeState, u: Option<&UserSession>) -> Result<(), ApiError> {
    let lib = s.library.lock().expect("library poisoned");
    match u {
        Some(u) => lib.set_setting(USER_KEY, &serde_json::to_string(u).expect("session serializes"))?,
        None => {
            lib.delete_setting(USER_KEY)?;
        }
    }
    Ok(())
}

/// The user's id from the token reply, or from the JWT's `uid` claim.
fn user_id_from(v: &Value, access_token: &str) -> Option<String> {
    if let Some(id) = v["user_id"].as_i64() {
        return Some(id.to_string());
    }
    if let Some(id) = v["user_id"].as_str() {
        return Some(id.to_string());
    }
    let payload = access_token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    let claims: Value = serde_json::from_slice(&bytes).ok()?;
    claims["uid"]
        .as_i64()
        .map(|i| i.to_string())
        .or_else(|| claims["uid"].as_str().map(str::to_string))
}

fn session_from(v: &Value, previous: Option<&UserSession>) -> Result<UserSession, ApiError> {
    let access_token = v["access_token"]
        .as_str()
        .ok_or_else(|| upstream("no access_token in the token reply"))?
        .to_string();
    Ok(UserSession {
        user_id: user_id_from(v, &access_token).or_else(|| previous.and_then(|p| p.user_id.clone())),
        refresh_token: v["refresh_token"]
            .as_str()
            .map(str::to_string)
            .or_else(|| previous.and_then(|p| p.refresh_token.clone())),
        expires_at: now_unix() + v["expires_in"].as_i64().unwrap_or(3600),
        country: previous.and_then(|p| p.country.clone()),
        scope: v["scope"]
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| previous.map(|p| p.scope.clone()).unwrap_or_default()),
        access_token,
    })
}

/// The signed-in user's access token, refreshed when it is about to expire.
pub(crate) async fn user_token(s: &NodeState) -> Result<Option<String>, ApiError> {
    let Some(u) = load_user(s) else {
        return Ok(None);
    };
    if u.expires_at - now_unix() > 60 {
        return Ok(Some(u.access_token));
    }
    let Some(refresh) = u.refresh_token.clone() else {
        store_user(s, None)?;
        return Ok(None);
    };
    let c = creds(s);
    let v = token_request(
        s,
        &[
            ("grant_type", "refresh_token"),
            ("refresh_token", &refresh),
            ("client_id", &c.client_id),
        ],
    )
    .await?;
    let fresh = session_from(&v, Some(&u))?;
    store_user(s, Some(&fresh))?;
    Ok(Some(fresh.access_token))
}

/// User token when signed in, else the app token.
async fn bearer(s: &NodeState) -> Result<String, ApiError> {
    match user_token(s).await? {
        Some(t) => Ok(t),
        None => app_token(s).await,
    }
}

fn country(s: &NodeState) -> String {
    load_user(s)
        .and_then(|u| u.country)
        .unwrap_or_else(|| "US".to_string())
}

/// GET a v2 path (or an absolute `links.next` URL) as JSON:API.
async fn get_json(s: &NodeState, path: &str, query: &[(&str, String)]) -> Result<Value, ApiError> {
    let url = if path.starts_with("http") {
        path.to_string()
    } else {
        format!("{}{}", api_base(s), path)
    };
    let token = bearer(s).await?;
    let q: Vec<(&str, &str)> = query.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let resp = s
        .http_client()
        .get(&url)
        .bearer_auth(token)
        .header(reqwest::header::ACCEPT, ACCEPT)
        .query(&q)
        .send()
        .await
        .map_err(upstream)?;
    let status = resp.status();
    if status == StatusCode::NOT_FOUND {
        return Err(ApiError::not_found(format!("tidal: nothing at {path}")));
    }
    let v: Value = resp.json().await.map_err(upstream)?;
    if !status.is_success() {
        let detail = v["errors"][0]["detail"]
            .as_str()
            .or(v["errors"][0]["code"].as_str())
            .unwrap_or("");
        return Err(upstream(format!("{path} answered {status}: {detail}")));
    }
    Ok(v)
}

// ---- JSON:API helpers ----

type Included<'a> = HashMap<(String, String), &'a Value>;

fn included(v: &Value) -> Included<'_> {
    v["included"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|i| {
                    Some((
                        (i["type"].as_str()?.to_string(), i["id"].as_str()?.to_string()),
                        i,
                    ))
                })
                .collect()
        })
        .unwrap_or_default()
}

fn rel_ids(res: &Value, name: &str) -> Vec<(String, Value)> {
    res["relationships"][name]["data"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|d| Some((d["id"].as_str()?.to_string(), d["meta"].clone())))
                .collect()
        })
        .unwrap_or_default()
}

fn names(inc: &Included<'_>, res: &Value, rel: &str, kind: &str) -> Vec<String> {
    rel_ids(res, rel)
        .into_iter()
        .filter_map(|(id, _)| inc.get(&(kind.to_string(), id)))
        .filter_map(|r| r["attributes"]["name"].as_str().map(str::to_string))
        .collect()
}

/// `PT4M32S` → milliseconds.
pub(crate) fn duration_ms(iso: &str) -> Option<u64> {
    let rest = iso.strip_prefix("PT")?;
    let mut ms = 0u64;
    let mut num = String::new();
    for ch in rest.chars() {
        if ch.is_ascii_digit() || ch == '.' {
            num.push(ch);
            continue;
        }
        let n: f64 = num.parse().ok()?;
        num.clear();
        ms += match ch {
            'H' => (n * 3_600_000.0) as u64,
            'M' => (n * 60_000.0) as u64,
            'S' => (n * 1000.0) as u64,
            _ => return None,
        };
    }
    Some(ms)
}

fn year_of(date: Option<&str>) -> Option<i32> {
    date.and_then(|d| d.get(..4)).and_then(|y| y.parse().ok())
}

/// The artwork file closest to 640 px wide.
fn artwork_url(inc: &Included<'_>, res: &Value) -> Option<String> {
    let (id, _) = rel_ids(res, "coverArt").into_iter().next()?;
    let art = inc.get(&("artworks".to_string(), id))?;
    let files = art["attributes"]["files"].as_array()?;
    files
        .iter()
        .filter_map(|f| {
            let w = f["meta"]["width"].as_i64().unwrap_or(0);
            Some((f["href"].as_str()?.to_string(), (w - 640).abs()))
        })
        .min_by_key(|(_, d)| *d)
        .map(|(href, _)| href)
}

fn format_label(res: &Value) -> String {
    let tags: Vec<&str> = res["attributes"]["mediaTags"]
        .as_array()
        .map(|a| a.iter().filter_map(|t| t.as_str()).collect())
        .unwrap_or_default();
    if tags.contains(&"HIRES_LOSSLESS") {
        "FLAC hi-res (TIDAL)".into()
    } else if tags.contains(&"LOSSLESS") {
        "FLAC 16/44.1 (TIDAL)".into()
    } else {
        "AAC (TIDAL)".into()
    }
}

pub(crate) fn track_uri(id: &str) -> String {
    format!("tidal:track:{id}")
}

fn tidal_url(kind: &str, id: &str) -> String {
    format!("https://tidal.com/browse/{kind}/{id}")
}

fn source_track(
    inc: &Included<'_>,
    t: &Value,
    track_no: Option<u32>,
    fallback_artist: Option<&str>,
) -> SourceTrack {
    let id = t["id"].as_str().unwrap_or("").to_string();
    let a = &t["attributes"];
    let artists = names(inc, t, "artists", "artists");
    SourceTrack {
        title: a["title"].as_str().unwrap_or("").to_string(),
        track_no,
        duration_ms: a["duration"].as_str().and_then(duration_ms),
        format: format_label(t),
        url: track_uri(&id),
        artist: if artists.is_empty() {
            fallback_artist.map(str::to_string)
        } else {
            Some(artists.join(", "))
        },
    }
}

/// Tracks by id (≤ 20 per call) with their artists and albums.
async fn tracks_by_ids(s: &NodeState, ids: &[String]) -> Result<Value, ApiError> {
    let mut q: Vec<(&str, String)> = vec![
        ("countryCode", country(s)),
        ("include", "artists".into()),
        ("include", "albums".into()),
    ];
    for id in ids.iter().take(20) {
        q.push(("filter[id]", id.clone()));
    }
    get_json(s, "/tracks", &q).await
}

pub struct Tidal;

#[async_trait::async_trait]
impl StreamingService for Tidal {
    fn id(&self) -> &'static str {
        SOURCE_ID
    }

    fn info(&self, s: &NodeState) -> SourceInfo {
        let status = if !creds(s).configured() {
            "not configured: add [tidal] client_id and client_secret to the node config".to_string()
        } else if load_user(s).is_some() {
            "ready".to_string()
        } else {
            "needs sign-in".to_string()
        };
        SourceInfo {
            id: SOURCE_ID.into(),
            name: "TIDAL".into(),
            description: "Your TIDAL subscription through the official API: search, albums and your collection; sign in once on this node".into(),
            status,
        }
    }

    async fn search(&self, s: &NodeState, term: &str, limit: u32) -> Result<Vec<SourceHit>, ApiError> {
        let cc = country(s);
        let albums = get_json(
            s,
            "/searchResults",
            &[
                ("filter[query]", term.to_string()),
                ("countryCode", cc.clone()),
                ("include", "albums".into()),
            ],
        )
        .await?;
        let tracks = get_json(
            s,
            "/searchResults",
            &[
                ("filter[query]", term.to_string()),
                ("countryCode", cc),
                ("include", "tracks".into()),
            ],
        )
        .await?;
        let mut hits = Vec::new();
        let per_kind = (limit as usize).div_ceil(2).max(1);
        let album_ids: Vec<String> = rel_ids(&albums["data"][0], "albums")
            .into_iter()
            .map(|(id, _)| id)
            .take(per_kind.min(20))
            .collect();
        if !album_ids.is_empty() {
            // The search include carries bare albums; artists and art need one more call.
            let mut q: Vec<(&str, String)> = vec![
                ("countryCode", country(s)),
                ("include", "artists".into()),
                ("include", "coverArt".into()),
            ];
            for id in &album_ids {
                q.push(("filter[id]", id.clone()));
            }
            let detail = get_json(s, "/albums", &q).await?;
            let dinc = included(&detail);
            let by_id: HashMap<String, &Value> = detail["data"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|t| Some((t["id"].as_str()?.to_string(), t)))
                        .collect()
                })
                .unwrap_or_default();
            for id in album_ids {
                let Some(a) = by_id.get(&id) else {
                    continue;
                };
                let at = &a["attributes"];
                let artists = names(&dinc, a, "artists", "artists");
                hits.push(SourceHit {
                    id: format!("album:{id}"),
                    title: at["title"].as_str().unwrap_or("").to_string(),
                    creator: (!artists.is_empty()).then(|| artists.join(", ")),
                    year: year_of(at["releaseDate"].as_str()),
                    downloads: None,
                    image: artwork_url(&dinc, a),
                    kind: "album".into(),
                    url: tidal_url("album", &id),
                });
            }
        }
        let track_ids: Vec<String> = rel_ids(&tracks["data"][0], "tracks")
            .into_iter()
            .map(|(id, _)| id)
            .take(per_kind.min(20))
            .collect();
        if !track_ids.is_empty() {
            let detail = tracks_by_ids(s, &track_ids).await?;
            let dinc = included(&detail);
            let by_id: HashMap<String, &Value> = detail["data"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|t| Some((t["id"].as_str()?.to_string(), t)))
                        .collect()
                })
                .unwrap_or_default();
            for id in track_ids {
                let Some(t) = by_id.get(&id) else { continue };
                let artists = names(&dinc, t, "artists", "artists");
                let album = rel_ids(t, "albums")
                    .into_iter()
                    .next()
                    .and_then(|(aid, _)| dinc.get(&("albums".to_string(), aid)).copied());
                hits.push(SourceHit {
                    id: format!("track:{id}"),
                    title: t["attributes"]["title"].as_str().unwrap_or("").to_string(),
                    creator: (!artists.is_empty()).then(|| artists.join(", ")),
                    year: album.and_then(|a| year_of(a["attributes"]["releaseDate"].as_str())),
                    downloads: None,
                    image: album.and_then(|a| artwork_url(&dinc, a)),
                    kind: "track".into(),
                    url: tidal_url("track", &id),
                });
            }
        }
        Ok(hits)
    }

    async fn item(&self, s: &NodeState, id: &str) -> Result<SourceItem, ApiError> {
        let cc = country(s);
        if let Some(tid) = id.strip_prefix("track:") {
            if tid.is_empty() || !tid.bytes().all(|b| b.is_ascii_alphanumeric()) {
                return Err(ApiError::bad_request("bad TIDAL track id"));
            }
            let v = get_json(
                s,
                &format!("/tracks/{tid}"),
                &[
                    ("countryCode", cc),
                    ("include", "artists".into()),
                    ("include", "albums".into()),
                    ("include", "albums.coverArt".into()),
                ],
            )
            .await?;
            let inc = included(&v);
            let t = &v["data"];
            let album = rel_ids(t, "albums")
                .into_iter()
                .next()
                .and_then(|(aid, _)| inc.get(&("albums".to_string(), aid)).copied());
            let track = source_track(&inc, t, Some(1), None);
            return Ok(SourceItem {
                source: SOURCE_ID.into(),
                id: id.to_string(),
                title: album
                    .and_then(|a| a["attributes"]["title"].as_str())
                    .unwrap_or(&track.title)
                    .to_string(),
                creator: track.artist.clone(),
                year: album.and_then(|a| year_of(a["attributes"]["releaseDate"].as_str())),
                description: None,
                licence: None,
                image: album.and_then(|a| artwork_url(&inc, a)),
                url: tidal_url("track", tid),
                tracks: vec![track],
            });
        }
        let Some(aid) = id.strip_prefix("album:") else {
            return Err(ApiError::bad_request(
                "TIDAL ids look like album:<id> or track:<id>",
            ));
        };
        if aid.is_empty() || !aid.bytes().all(|b| b.is_ascii_alphanumeric()) {
            return Err(ApiError::bad_request("bad TIDAL album id"));
        }
        let v = get_json(
            s,
            &format!("/albums/{aid}"),
            &[
                ("countryCode", cc.clone()),
                ("include", "artists".into()),
                ("include", "coverArt".into()),
                ("include", "items".into()),
            ],
        )
        .await?;
        let inc = included(&v);
        let album = &v["data"];
        let artists = names(&inc, album, "artists", "artists");
        let creator = (!artists.is_empty()).then(|| artists.join(", "));
        let mut tracks = Vec::new();
        let mut items = rel_ids(album, "items");
        // Albums with more items than one page: follow the relationship's next link.
        let mut next = v["data"]["relationships"]["items"]["links"]["next"]
            .as_str()
            .map(str::to_string);
        let mut extra_inc = Vec::new();
        while let Some(n) = next.take() {
            let page = get_json(s, &n, &[("include", "items".into())]).await?;
            items.extend(
                page["data"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|d| Some((d["id"].as_str()?.to_string(), d["meta"].clone())))
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default(),
            );
            next = page["links"]["next"].as_str().map(str::to_string);
            extra_inc.push(page);
        }
        let mut all_inc: Included<'_> = inc.clone();
        for page in &extra_inc {
            all_inc.extend(included(page));
        }
        for (tid, meta) in items {
            let Some(t) = all_inc.get(&("tracks".to_string(), tid.clone())) else {
                continue;
            };
            let no = meta["trackNumber"].as_u64().map(|n| n as u32);
            tracks.push(source_track(&all_inc, t, no, creator.as_deref()));
        }
        Ok(SourceItem {
            source: SOURCE_ID.into(),
            id: id.to_string(),
            title: album["attributes"]["title"].as_str().unwrap_or("").to_string(),
            creator,
            year: year_of(album["attributes"]["releaseDate"].as_str()),
            description: None,
            licence: album["attributes"]["copyright"]["text"]
                .as_str()
                .map(str::to_string),
            image: artwork_url(&inc, album),
            url: tidal_url("album", aid),
            tracks,
        })
    }
}

// ---- sign-in ----

fn random_urlsafe(bytes: usize) -> String {
    use rand::RngCore;
    let mut buf = vec![0u8; bytes];
    rand::thread_rng().fill_bytes(&mut buf);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(buf)
}

/// `GET /v1/sources/tidal/login`: the URL to open in a browser.
pub(crate) async fn login(State(s): State<Arc<NodeState>>) -> ApiResult<LoginUrl> {
    let c = creds(&s);
    if !c.configured() {
        return Err(ApiError::new(
            StatusCode::PRECONDITION_FAILED,
            "TIDAL is not configured: add [tidal] client_id and client_secret to the node config",
        ));
    }
    use sha2::Digest;
    let verifier = random_urlsafe(48);
    let challenge =
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sha2::Sha256::digest(verifier.as_bytes()));
    let state = random_urlsafe(24);
    pending().lock().expect("pending").insert(state.clone(), verifier);
    let mut url = url::Url::parse(&format!("{}/authorize", login_base(&s))).map_err(upstream)?;
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", &c.client_id)
        .append_pair("redirect_uri", &redirect_uri(&s))
        .append_pair("scope", SCOPES)
        .append_pair("code_challenge_method", "S256")
        .append_pair("code_challenge", &challenge)
        .append_pair("state", &state);
    Ok(Json(LoginUrl { url: url.to_string() }))
}

#[derive(Debug, Deserialize, Default)]
pub(crate) struct CallbackQuery {
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    error_description: Option<String>,
}

/// `GET /tidal/callback?code&state`: finish the sign-in, then back to the web UI.
pub(crate) async fn callback(State(s): State<Arc<NodeState>>, Query(q): Query<CallbackQuery>) -> Response {
    match finish_login(&s, q).await {
        Ok(()) => Redirect::to("/?tidal=signed_in").into_response(),
        Err(e) => {
            tracing::warn!("tidal sign-in failed: {}", e.message);
            let msg: String = url::form_urlencoded::byte_serialize(e.message.as_bytes()).collect();
            Redirect::to(&format!("/?tidal=error&detail={msg}")).into_response()
        }
    }
}

async fn finish_login(s: &NodeState, q: CallbackQuery) -> Result<(), ApiError> {
    if let Some(err) = q.error {
        return Err(ApiError::bad_request(format!(
            "TIDAL refused the sign-in: {err} {}",
            q.error_description.unwrap_or_default()
        )));
    }
    let (Some(code), Some(state)) = (q.code, q.state) else {
        return Err(ApiError::bad_request("missing code or state"));
    };
    let verifier = pending()
        .lock()
        .expect("pending")
        .remove(&state)
        .ok_or_else(|| ApiError::bad_request("unknown or expired sign-in state; start again"))?;
    let c = creds(s);
    let redirect = redirect_uri(s);
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
    let mut session = session_from(&v, None)?;
    if let Some(uid) = session.user_id.clone() {
        // Country decides catalogue availability; best effort.
        let me = s
            .http_client()
            .get(format!("{}/users/{uid}", api_base(s)))
            .bearer_auth(&session.access_token)
            .header(reqwest::header::ACCEPT, ACCEPT)
            .send()
            .await;
        if let Ok(resp) = me {
            if let Ok(v) = resp.json::<Value>().await {
                session.country = v["data"]["attributes"]["country"].as_str().map(str::to_string);
            }
        }
    }
    store_user(s, Some(&session))?;
    tracing::info!(user = ?session.user_id, "TIDAL sign-in stored");
    Ok(())
}

pub(crate) fn status_view(s: &NodeState) -> TidalStatus {
    let u = load_user(s);
    TidalStatus {
        configured: creds(s).configured(),
        signed_in: u.is_some(),
        user_id: u.as_ref().and_then(|u| u.user_id.clone()),
        country: u.as_ref().and_then(|u| u.country.clone()),
        scope: u.as_ref().map(|u| u.scope.clone()),
        expires_at: u.as_ref().map(|u| u.expires_at),
        redirect_uri: redirect_uri(s),
    }
}

/// `GET /v1/sources/tidal/status`.
pub(crate) async fn status(State(s): State<Arc<NodeState>>) -> ApiResult<TidalStatus> {
    Ok(Json(status_view(&s)))
}

/// `DELETE /v1/sources/tidal/session`: forget the user's tokens.
pub(crate) async fn logout(State(s): State<Arc<NodeState>>) -> ApiResult<TidalStatus> {
    store_user(&s, None)?;
    Ok(Json(status_view(&s)))
}

// ---- the user's collection and playlists ----

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TidalCollection {
    pub tracks: Vec<SourceHit>,
    pub albums: Vec<SourceHit>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TidalPlaylist {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub items: u64,
    pub url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ImportReport {
    /// Entries that became canonical URIs (ISRC, else the TIDAL link).
    pub imported: usize,
    pub skipped: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub playlist: Option<PlaylistDetail>,
}

fn signed_in_user(s: &NodeState) -> Result<UserSession, ApiError> {
    load_user(s).ok_or_else(|| {
        ApiError::new(
            StatusCode::PRECONDITION_FAILED,
            "sign in to TIDAL on the Sources page first",
        )
    })
}

fn user_id_of(u: &UserSession) -> Result<String, ApiError> {
    u.user_id.clone().ok_or_else(|| {
        ApiError::new(
            StatusCode::PRECONDITION_FAILED,
            "the TIDAL session has no user id; sign in again",
        )
    })
}

/// Resource ids of a paged to-many relationship, following `links.next`.
async fn relationship_ids(
    s: &NodeState,
    path: &str,
    query: &[(&str, String)],
) -> Result<Vec<String>, ApiError> {
    let mut ids = Vec::new();
    let mut page = get_json(s, path, query).await?;
    for _ in 0..50 {
        ids.extend(
            page["data"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|d| d["id"].as_str().map(str::to_string))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default(),
        );
        let Some(next) = page["links"]["next"].as_str().map(str::to_string) else {
            break;
        };
        page = get_json(s, &next, &[]).await?;
    }
    Ok(ids)
}

/// The canonical identity of a TIDAL track: its ISRC, else the TIDAL link.
pub(crate) fn canonical_track(track: &Value) -> Option<pm_model::MusicUri> {
    let id = track["id"].as_str()?;
    if let Some(isrc) = track["attributes"]["isrc"].as_str() {
        if let Ok(isrc) = pm_model::Isrc::parse(isrc) {
            return Some(pm_model::MusicUri::Isrc { isrc });
        }
    }
    pm_model::MusicUri::parse(&tidal_url("track", id)).ok()
}

async fn track_hits(s: &NodeState, ids: &[String]) -> Result<(Vec<SourceHit>, Vec<Value>), ApiError> {
    let mut hits = Vec::new();
    let mut docs = Vec::new();
    for chunk in ids.chunks(20) {
        let detail = tracks_by_ids(s, chunk).await?;
        let dinc = included(&detail);
        for t in detail["data"].as_array().cloned().unwrap_or_default() {
            let id = t["id"].as_str().unwrap_or("").to_string();
            let artists = names(&dinc, &t, "artists", "artists");
            let album = rel_ids(&t, "albums")
                .into_iter()
                .next()
                .and_then(|(aid, _)| dinc.get(&("albums".to_string(), aid)).copied());
            hits.push(SourceHit {
                id: format!("track:{id}"),
                title: t["attributes"]["title"].as_str().unwrap_or("").to_string(),
                creator: (!artists.is_empty()).then(|| artists.join(", ")),
                year: album.and_then(|a| year_of(a["attributes"]["releaseDate"].as_str())),
                downloads: None,
                image: album.and_then(|a| artwork_url(&dinc, a)),
                kind: "track".into(),
                url: tidal_url("track", &id),
            });
            docs.push(t);
        }
    }
    Ok((hits, docs))
}

async fn album_hits(s: &NodeState, ids: &[String]) -> Result<Vec<SourceHit>, ApiError> {
    let mut hits = Vec::new();
    for chunk in ids.chunks(20) {
        let mut q: Vec<(&str, String)> = vec![
            ("countryCode", country(s)),
            ("include", "artists".into()),
            ("include", "coverArt".into()),
        ];
        for id in chunk {
            q.push(("filter[id]", id.clone()));
        }
        let detail = get_json(s, "/albums", &q).await?;
        let dinc = included(&detail);
        for a in detail["data"].as_array().cloned().unwrap_or_default() {
            let id = a["id"].as_str().unwrap_or("").to_string();
            let artists = names(&dinc, &a, "artists", "artists");
            hits.push(SourceHit {
                id: format!("album:{id}"),
                title: a["attributes"]["title"].as_str().unwrap_or("").to_string(),
                creator: (!artists.is_empty()).then(|| artists.join(", ")),
                year: year_of(a["attributes"]["releaseDate"].as_str()),
                downloads: None,
                image: artwork_url(&dinc, &a),
                kind: "album".into(),
                url: tidal_url("album", &id),
            });
        }
    }
    Ok(hits)
}

/// `GET /v1/sources/tidal/collection`: the user's favourite tracks and albums.
pub(crate) async fn collection(State(s): State<Arc<NodeState>>) -> ApiResult<TidalCollection> {
    let uid = user_id_of(&signed_in_user(&s)?)?;
    let track_ids = relationship_ids(
        &s,
        &format!("/userCollectionTracks/{uid}/relationships/items"),
        &[("sort", "-addedAt".into())],
    )
    .await?;
    let album_ids = relationship_ids(
        &s,
        &format!("/userCollectionAlbums/{uid}/relationships/items"),
        &[("sort", "-addedAt".into())],
    )
    .await?;
    let (tracks, _) = track_hits(&s, &track_ids).await?;
    let albums = album_hits(&s, &album_ids).await?;
    Ok(Json(TidalCollection { tracks, albums }))
}

/// `POST /v1/sources/tidal/collection/import`: the collection as node
/// favourites (ISRC where TIDAL has one, else the TIDAL link; ADR 0010).
pub(crate) async fn import_collection(State(s): State<Arc<NodeState>>) -> ApiResult<ImportReport> {
    let uid = user_id_of(&signed_in_user(&s)?)?;
    let track_ids = relationship_ids(
        &s,
        &format!("/userCollectionTracks/{uid}/relationships/items"),
        &[],
    )
    .await?;
    let album_ids = relationship_ids(
        &s,
        &format!("/userCollectionAlbums/{uid}/relationships/items"),
        &[],
    )
    .await?;
    let (_, docs) = track_hits(&s, &track_ids).await?;
    let mut report = ImportReport::default();
    let lib = s.library.lock().expect("library poisoned");
    for t in &docs {
        match canonical_track(t) {
            Some(uri) => {
                crate::favourites::add(&lib, uri)?;
                report.imported += 1;
            }
            None => report.skipped += 1,
        }
    }
    for id in &album_ids {
        match pm_model::MusicUri::parse(&tidal_url("album", id)) {
            Ok(uri) => {
                crate::favourites::add(&lib, uri)?;
                report.imported += 1;
            }
            Err(_) => report.skipped += 1,
        }
    }
    drop(lib);
    s.notify_library();
    Ok(Json(report))
}

/// `GET /v1/sources/tidal/playlists`: playlists the user owns.
pub(crate) async fn playlists(State(s): State<Arc<NodeState>>) -> ApiResult<Vec<TidalPlaylist>> {
    let uid = user_id_of(&signed_in_user(&s)?)?;
    let mut out = Vec::new();
    let mut page = get_json(
        &s,
        "/playlists",
        &[("countryCode", country(&s)), ("filter[owners.id]", uid)],
    )
    .await?;
    for _ in 0..50 {
        for p in page["data"].as_array().cloned().unwrap_or_default() {
            let id = p["id"].as_str().unwrap_or("").to_string();
            let a = &p["attributes"];
            out.push(TidalPlaylist {
                url: tidal_url("playlist", &id),
                name: a["name"].as_str().unwrap_or("").to_string(),
                description: a["description"]
                    .as_str()
                    .filter(|d| !d.is_empty())
                    .map(str::to_string),
                items: a["numberOfTrackItems"]
                    .as_u64()
                    .or(a["numberOfItems"].as_u64())
                    .unwrap_or(0),
                id,
            });
        }
        let Some(next) = page["links"]["next"].as_str().map(str::to_string) else {
            break;
        };
        page = get_json(&s, &next, &[]).await?;
    }
    Ok(Json(out))
}

/// `POST /v1/sources/tidal/playlists/{id}/import`: a TIDAL playlist as a
/// private Pubky playlist of canonical URIs (ADR 0006).
pub(crate) async fn import_playlist(
    State(s): State<Arc<NodeState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> ApiResult<ImportReport> {
    signed_in_user(&s)?;
    if id.is_empty() || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
        return Err(ApiError::bad_request("bad TIDAL playlist id"));
    }
    let head = get_json(&s, &format!("/playlists/{id}"), &[("countryCode", country(&s))]).await?;
    let name = head["data"]["attributes"]["name"]
        .as_str()
        .unwrap_or("TIDAL playlist")
        .to_string();
    let description = head["data"]["attributes"]["description"]
        .as_str()
        .filter(|d| !d.is_empty())
        .map(str::to_string);
    let mut uris = Vec::new();
    let mut skipped = 0usize;
    let mut page = get_json(
        &s,
        &format!("/playlists/{id}/relationships/items"),
        &[("countryCode", country(&s)), ("include", "items".into())],
    )
    .await?;
    for _ in 0..50 {
        let inc = included(&page);
        for d in page["data"].as_array().cloned().unwrap_or_default() {
            let key = (
                d["type"].as_str().unwrap_or("").to_string(),
                d["id"].as_str().unwrap_or("").to_string(),
            );
            match inc.get(&key).and_then(|t| canonical_track(t)) {
                Some(u) => uris.push(u.to_url()),
                None => skipped += 1,
            }
        }
        let Some(next) = page["links"]["next"].as_str().map(str::to_string) else {
            break;
        };
        page = get_json(&s, &next, &[("include", "items".into())]).await?;
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
        }),
    )
    .await?;
    Ok(Json(ImportReport {
        imported,
        skipped,
        playlist: Some(detail),
    }))
}

// ---- playback (audio thread, blocking) ----

pub const TRACK_URI_PREFIX: &str = "tidal:track:";

/// What a stream manifest told us, for the signal path and the logs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestInfo {
    pub uri: String,
    pub formats: Vec<String>,
    pub presentation: String,
    pub preview_reason: Option<String>,
    pub drm: bool,
}

pub(crate) fn parse_manifest(v: &Value) -> Result<ManifestInfo, String> {
    let a = &v["data"]["attributes"];
    let uri = a["uri"]
        .as_str()
        .ok_or_else(|| {
            v["errors"][0]["detail"]
                .as_str()
                .map(|d| format!("manifest refused: {d}"))
                .unwrap_or_else(|| "manifest has no uri".to_string())
        })?
        .to_string();
    Ok(ManifestInfo {
        uri,
        formats: a["formats"]
            .as_array()
            .map(|f| f.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
            .unwrap_or_default(),
        presentation: a["trackPresentation"].as_str().unwrap_or("FULL").to_string(),
        preview_reason: a["previewReason"].as_str().map(str::to_string),
        drm: a["drmData"].as_object().is_some_and(|o| !o.is_empty()),
    })
}

/// A valid user access token for the audio thread: the stored one, or a
/// blocking refresh when it is about to expire.
fn user_token_blocking(
    library: &std::sync::Mutex<pm_library::Library>,
    ep: &TidalEndpoints,
    client: &reqwest::blocking::Client,
) -> Result<String, String> {
    let u = {
        let lib = library.lock().map_err(|_| "library poisoned")?;
        load_user_from(&lib).ok_or("TIDAL: sign in on the Sources page first")?
    };
    if u.expires_at - now_unix() > 60 {
        return Ok(u.access_token);
    }
    let refresh = u
        .refresh_token
        .clone()
        .ok_or("TIDAL session expired; sign in again")?;
    let body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("grant_type", "refresh_token")
        .append_pair("refresh_token", &refresh)
        .append_pair("client_id", &ep.creds.client_id)
        .finish();
    let v: Value = client
        .post(format!("{}/v1/oauth2/token", ep.auth_base.trim_end_matches('/')))
        .basic_auth(&ep.creds.client_id, Some(&ep.creds.client_secret))
        .header(reqwest::header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .and_then(|r| r.error_for_status())
        .map_err(|e| format!("TIDAL token refresh: {e}"))?
        .json()
        .map_err(|e| e.to_string())?;
    let fresh = session_from(&v, Some(&u)).map_err(|e| e.message)?;
    let lib = library.lock().map_err(|_| "library poisoned")?;
    lib.set_setting(
        USER_KEY,
        &serde_json::to_string(&fresh).expect("session serializes"),
    )
    .map_err(|e| e.to_string())?;
    Ok(fresh.access_token)
}

/// Open `tidal:track:<id>` for playback: fetch the stream manifest with the
/// user's token, then read the HLS segments as one FLAC/AAC stream.
pub(crate) fn open_track_blocking(
    library: &std::sync::Mutex<pm_library::Library>,
    endpoints: &SharedEndpoints,
    id: &str,
) -> Result<Box<dyn pm_zones::FrameSource>, pm_zones::SourceError> {
    let uri = format!("{TRACK_URI_PREFIX}{id}");
    let err = |reason: String| pm_zones::SourceError::Open {
        uri: uri.clone(),
        reason,
    };
    if id.is_empty() || !id.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return Err(err("malformed TIDAL track id".into()));
    }
    let ep = endpoints
        .read()
        .map_err(|_| err("endpoints poisoned".into()))?
        .clone();
    if !ep.creds.configured() {
        return Err(err("TIDAL is not configured on this node".into()));
    }
    let client = crate::sources::http_client();
    let token = user_token_blocking(library, &ep, client).map_err(err)?;
    let v: Value = client
        .get(format!(
            "{}/trackManifests/{id}",
            ep.api_base.trim_end_matches('/')
        ))
        .bearer_auth(&token)
        .header(reqwest::header::ACCEPT, ACCEPT)
        .query(&[
            ("adaptive", "false"),
            ("formats", "FLAC_HIRES,FLAC,AACLC"),
            ("manifestType", "HLS"),
            ("uriScheme", "HTTPS"),
            ("usage", "PLAYBACK"),
        ])
        .send()
        .map_err(|e| err(format!("manifest request: {e}")))?
        .json()
        .map_err(|e| err(format!("manifest body: {e}")))?;
    let m = parse_manifest(&v).map_err(err)?;
    if m.drm {
        return Err(err(
            "TIDAL serves this track DRM-protected; bit-perfect playback is not possible".into(),
        ));
    }
    if m.presentation != "FULL" {
        tracing::warn!(
            track = id,
            reason = ?m.preview_reason,
            "TIDAL serves a preview, not the full track"
        );
    }
    tracing::info!(track = id, formats = ?m.formats, presentation = %m.presentation, "TIDAL manifest");
    let reader = crate::hls::HlsReader::open(client, &m.uri).map_err(err)?;
    let ext = if m.formats.iter().any(|f| f.starts_with("FLAC")) {
        Some("m4a")
    } else {
        Some("aac")
    };
    crate::sources::frame_source_from_reader(Box::new(reader), ext, &uri)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifests_parse() {
        let v = serde_json::json!({"data": {"attributes": {"uri": "https://cdn/m.m3u8", "formats": ["FLAC"], "trackPresentation": "FULL"}}});
        let m = parse_manifest(&v).unwrap();
        assert_eq!(m.uri, "https://cdn/m.m3u8");
        assert!(!m.drm);
        let v = serde_json::json!({"data": {"attributes": {"uri": "u", "trackPresentation": "PREVIEW", "previewReason": "FULL_REQUIRES_SUBSCRIPTION", "drmData": {"system": "WIDEVINE"}}}});
        let m = parse_manifest(&v).unwrap();
        assert!(m.drm);
        assert_eq!(m.preview_reason.as_deref(), Some("FULL_REQUIRES_SUBSCRIPTION"));
        assert!(parse_manifest(&serde_json::json!({"errors": [{"detail": "no"}]}))
            .unwrap_err()
            .contains("no"));
    }

    #[test]
    fn iso_durations() {
        assert_eq!(duration_ms("PT4M32S"), Some(272_000));
        assert_eq!(duration_ms("PT1H21S"), Some(3_621_000));
        assert_eq!(duration_ms("PT2.5S"), Some(2_500));
        assert_eq!(duration_ms("4M"), None);
    }

    #[test]
    fn user_id_from_jwt_claim() {
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(br#"{"uid":12345,"sub":"x"}"#);
        let token = format!("h.{payload}.s");
        assert_eq!(
            user_id_from(&serde_json::json!({}), &token).as_deref(),
            Some("12345")
        );
        assert_eq!(
            user_id_from(&serde_json::json!({"user_id": 7}), "nojwt").as_deref(),
            Some("7")
        );
    }
}
