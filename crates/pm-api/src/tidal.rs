//! TIDAL through the official TIDAL API (openapi.tidal.com/v2, JSON:API).
//! The developer app's client credentials search the catalogue; one user
//! signs in with OAuth 2 + PKCE (scopes incl. `playback`) so the node can
//! fetch stream manifests and the user's collection. One account per node,
//! never proxied for other people (handoff §2). Tokens live in the settings
//! table of the node's own database.

use crate::accounts::Who;
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
const PLAYBACK_LIMIT_KEY: &str = "tidal.playback_limit";
/// Scopes requested at sign-in when the config names none. The app must have
/// every requested scope enabled in the TIDAL developer dashboard, or the
/// login page fails before showing anything.
pub const DEFAULT_SCOPES: &str =
    "user.read collection.read collection.write playlists.read playlists.write playback entitlements.read search.read";
const ACCEPT: &str = "application/vnd.api+json";

/// Developer app credentials from the node config.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TidalCredentials {
    pub client_id: String,
    pub client_secret: String,
    /// Space-separated OAuth scopes to request; empty = [`DEFAULT_SCOPES`].
    pub scopes: String,
}

impl TidalCredentials {
    /// An app to sign in with: the node's own, or the project's built-in one (no secret needed:
    /// PKCE).
    pub fn configured(&self) -> bool {
        !self.client_id.trim().is_empty()
    }

    /// The app's secret is on this node (the owner's own app): catalogue reads without a signed-in
    /// user (client credentials) need it.
    pub fn has_secret(&self) -> bool {
        !self.client_secret.trim().is_empty()
    }

    pub fn scopes(&self) -> String {
        let s = self.scopes.split_whitespace().collect::<Vec<_>>().join(" ");
        if s.is_empty() {
            DEFAULT_SCOPES.to_string()
        } else {
            s
        }
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
    /// TIDAL's private v1 API (full tracks, unofficial).
    pub v1_base: String,
}

impl Default for TidalEndpoints {
    fn default() -> Self {
        Self {
            creds: TidalCredentials::default(),
            api_base: "https://openapi.tidal.com/v2".into(),
            auth_base: "https://auth.tidal.com".into(),
            login_base: "https://login.tidal.com".into(),
            v1_base: "https://api.tidal.com/v1".into(),
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
    /// Developer access tier TIDAL stamps into the token (`THIRD_PARTY`, `PARTNER`, …);
    /// the THIRD_PARTY tier gets 30 s previews instead of full tracks.
    #[serde(default)]
    pub access_tier: Option<String>,
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
    /// Scopes the sign-in asks for; every one must be enabled on the developer app.
    #[serde(default)]
    pub requested_scopes: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access_tier: Option<String>,
    /// What the last stream manifest said when it was not a full track
    /// (e.g. `FULL_REQUIRES_HIGHER_ACCESS_TIER`): previews only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub playback_limit: Option<String>,
    /// The last sign-in's test of a code exchange without the client secret:
    /// `{outcome: accepted|refused, detail, at}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_client: Option<Value>,
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

/// Whether the node can stream TIDAL now: credentials and a user session.
pub(crate) fn connected(s: &NodeState) -> bool {
    creds(s).configured() && load_user(s, &crate::accounts::asking()).is_some()
}

/// The public client id the node's TIDAL sign-in is made with (`vault_cache`).
pub(crate) fn app_id(s: &NodeState) -> Option<String> {
    Some(creds(s).client_id).filter(|c| !c.trim().is_empty())
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

/// The node's address as the browser sees it: `public_url` when the owner set a real
/// address, else the origin of this request (reverse proxies set
/// `X-Forwarded-Proto` / `X-Forwarded-Host`; plain access has `Host`). A `public_url` the node
/// guessed for speakers (its LAN address) is not what a browser on `localhost`, behind a proxy
/// or a `.local` name used, and sign-in providers check the redirect exactly.
pub(crate) fn external_base(s: &NodeState, headers: &axum::http::HeaderMap) -> String {
    let configured = s.public_url.trim_end_matches('/');
    let unusable = s.public_url_guessed()
        || configured.is_empty()
        || ["0.0.0.0", "127.0.0.1", "localhost", "[::]"]
            .iter()
            .any(|h| configured.contains(h));
    if !unusable {
        return configured.to_string();
    }
    let h = |n: &str| {
        headers
            .get(n)
            .and_then(|v| v.to_str().ok())
            .map(|v| v.split(',').next().unwrap_or("").trim().to_string())
            .filter(|v| !v.is_empty())
    };
    match h("x-forwarded-host").or_else(|| h("host")) {
        Some(host) => {
            let proto = h("x-forwarded-proto").unwrap_or_else(|| "http".into());
            format!("{proto}://{host}")
        }
        None => configured.to_string(),
    }
}

/// The node's own callback.
fn callback_uri(s: &NodeState, headers: &axum::http::HeaderMap) -> String {
    format!("{}/tidal/callback", external_base(s, headers))
}

/// The redirect URI the app lists: the node's callback for an app with its secret here, else
/// (the project's built-in app, or any app without its secret) the return page.
pub(crate) fn redirect_uri(s: &NodeState, headers: &axum::http::HeaderMap) -> String {
    if creds(s).has_secret() {
        callback_uri(s, headers)
    } else {
        s.return_page()
    }
}

// ---- tokens ----

/// Pending sign-ins: OAuth `state` → (PKCE verifier, redirect URI used).
/// A sign-in in progress, under its PKCE state's nonce.
pub(crate) struct Pending {
    verifier: String,
    redirect: String,
    /// Who started it: the sign-in is kept for them.
    pub(crate) who: Who,
    /// A listener's: the browser cookie it must come back with (`accounts::login_binding`).
    binding: Option<String>,
}

pub(crate) fn pending() -> &'static Mutex<HashMap<String, Pending>> {
    static P: std::sync::OnceLock<Mutex<HashMap<String, Pending>>> = std::sync::OnceLock::new();
    P.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The app's own token (client credentials), cached until shortly before it expires.
/// The app token, with the app (`auth_base client_id`) it belongs to: another app (or another
/// node in the same process, as in tests) never gets this one.
#[allow(clippy::type_complexity)]
fn app_token_cache() -> &'static Mutex<Option<(String, String, i64)>> {
    static T: std::sync::OnceLock<Mutex<Option<(String, String, i64)>>> = std::sync::OnceLock::new();
    T.get_or_init(|| Mutex::new(None))
}

fn app_of_token(s: &NodeState) -> String {
    format!("{} {}", auth_base(s), creds(s).client_id)
}

async fn token_request(s: &NodeState, form: &[(&str, &str)]) -> Result<Value, ApiError> {
    let c = creds(s);
    if !c.configured() {
        return Err(ApiError::new(
            StatusCode::PRECONDITION_FAILED,
            "TIDAL is not set up on this node",
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
    let mut req = s
        .http_client()
        .post(format!("{}/v1/oauth2/token", auth_base(s)))
        .header(reqwest::header::CONTENT_TYPE, "application/x-www-form-urlencoded");
    if c.has_secret() {
        req = req.basic_auth(&c.client_id, Some(&c.client_secret));
    }
    let resp = req.body(body).send().await.map_err(upstream)?;
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

/// What TIDAL answered to a code exchange without the client secret (a public client with PKCE),
/// kept in the settings: the test that decides whether the project's app can be shared without
/// shipping its secret (STEWARD-SHARED-APPS.md).
pub const PUBLIC_CLIENT_KEY: &str = "tidal.public_client_probe";

/// The same token call as a public client: the client id in the form, no secret anywhere.
/// `Err((status, TIDAL's answer))` when refused.
async fn token_request_public(s: &NodeState, form: &[(&str, &str)]) -> Result<Value, (u16, String)> {
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
        .header(reqwest::header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await
        .map_err(|e| (0, e.to_string()))?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if status.is_success() {
        serde_json::from_str(&text).map_err(|e| (status.as_u16(), e.to_string()))
    } else {
        Err((status.as_u16(), text.chars().take(400).collect()))
    }
}

async fn app_token(s: &NodeState) -> Result<String, ApiError> {
    let app = app_of_token(s);
    if let Some((for_app, t, exp)) = app_token_cache().lock().expect("app token").clone() {
        if for_app == app && exp - now_unix() > 60 {
            return Ok(t);
        }
    }
    if !creds(s).has_secret() {
        // TIDAL refuses client credentials without the secret ("Invalid client credentials"),
        // and the project's secret never ships: the catalogue opens once you sign in
        return Err(ApiError::new(
            StatusCode::PRECONDITION_FAILED,
            "Sign in to TIDAL on this node to search and play it",
        ));
    }
    let v = token_request(s, &[("grant_type", "client_credentials")]).await?;
    let token = v["access_token"]
        .as_str()
        .ok_or_else(|| upstream("no access_token in the token reply"))?
        .to_string();
    let expires_at = now_unix() + v["expires_in"].as_i64().unwrap_or(3600);
    *app_token_cache().lock().expect("app token") = Some((app, token.clone(), expires_at));
    Ok(token)
}

/// This person's official sign-in.
fn load_user_from(lib: &pm_library::Library, who: &Who) -> Option<UserSession> {
    let mut u: UserSession = who
        .get(lib, USER_KEY)
        .and_then(|j| serde_json::from_str(&j).ok())?;
    // Sessions stored before these fields existed: fill them from the token's claims.
    if u.country.is_none() || u.access_tier.is_none() {
        let claims = jwt_claims(&u.access_token);
        if u.country.is_none() {
            u.country = claims["cc"].as_str().map(str::to_string);
        }
        if u.access_tier.is_none() {
            u.access_tier = claims["at"].as_str().map(str::to_string);
        }
    }
    Some(u)
}

pub(crate) fn load_user(s: &NodeState, who: &Who) -> Option<UserSession> {
    load_user_from(&s.library.lock().expect("library poisoned"), who)
}

fn store_user(s: &NodeState, who: &Who, u: Option<&UserSession>) -> Result<(), ApiError> {
    let raw = u.map(|u| serde_json::to_string(u).expect("session serializes"));
    who.put(
        &s.library.lock().expect("library poisoned"),
        USER_KEY,
        raw.as_deref(),
    )
}

/// The claims of the access token (a JWT): `uid`, `cc` (country), `at` (access tier).
fn jwt_claims(access_token: &str) -> Value {
    access_token
        .split('.')
        .nth(1)
        .and_then(|p| base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(p).ok())
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or(Value::Null)
}

/// The user's id from the token reply, or from the JWT's `uid` claim.
fn user_id_from(v: &Value, access_token: &str) -> Option<String> {
    if let Some(id) = v["user_id"].as_i64() {
        return Some(id.to_string());
    }
    if let Some(id) = v["user_id"].as_str() {
        return Some(id.to_string());
    }
    let claims = jwt_claims(access_token);
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
    let claims = jwt_claims(&access_token);
    Ok(UserSession {
        access_tier: claims["at"]
            .as_str()
            .map(str::to_string)
            .or_else(|| previous.and_then(|p| p.access_tier.clone())),
        user_id: user_id_from(v, &access_token).or_else(|| previous.and_then(|p| p.user_id.clone())),
        refresh_token: v["refresh_token"]
            .as_str()
            .map(str::to_string)
            .or_else(|| previous.and_then(|p| p.refresh_token.clone())),
        expires_at: now_unix() + v["expires_in"].as_i64().unwrap_or(3600),
        country: claims["cc"]
            .as_str()
            .map(str::to_string)
            .or_else(|| previous.and_then(|p| p.country.clone())),
        scope: v["scope"]
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| previous.map(|p| p.scope.clone()).unwrap_or_default()),
        access_token,
    })
}

/// This person's access token, refreshed when it is about to expire.
pub(crate) async fn user_token(s: &NodeState, who: &Who) -> Result<Option<String>, ApiError> {
    let Some(u) = load_user(s, who) else {
        return Ok(None);
    };
    if u.expires_at - now_unix() > 60 {
        return Ok(Some(u.access_token));
    }
    let Some(refresh) = u.refresh_token.clone() else {
        store_user(s, who, None)?;
        return Ok(None);
    };
    let key = who.setting(USER_KEY).expect("a person with a sign-in");
    let c = creds(s);
    let form = [
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh.as_str()),
        ("client_id", c.client_id.as_str()),
    ];
    // A public client first (what TIDAL answers is recorded), the secret only when refused
    let v = match token_request_public(s, &form).await {
        Ok(v) => {
            record_refresh_probe(s, "accepted", "a refresh worked without the client secret");
            v
        }
        Err((status, answer)) if c.has_secret() => {
            record_refresh_probe(s, "refused", &format!("{status}: {answer}"));
            token_request(s, &form).await?
        }
        Err((status, answer)) => {
            record_refresh_probe(s, "refused", &format!("{status}: {answer}"));
            return Err(upstream(format!(
                "TIDAL refused to renew the sign-in ({status}): {answer}"
            )));
        }
    };
    let fresh = session_from(&v, Some(&u))?;
    // Only over the session this refresh started from (the vault may have changed it meanwhile)
    let lib = s.library.lock().expect("library poisoned");
    let still =
        |cur: &str| serde_json::from_str::<UserSession>(cur).is_ok_and(|c| c.access_token == u.access_token);
    let raw = serde_json::to_string(&fresh).expect("session serializes");
    if !crate::vault_cache::store_refreshed(&lib, &key, still, &raw) {
        // Use what is there now, if anything
        return Ok(lib
            .get_setting(&key)?
            .and_then(|v| serde_json::from_str::<UserSession>(&v).ok())
            .map(|c| c.access_token));
    }
    Ok(Some(fresh.access_token))
}

/// The asking person's token when they are signed in, else the app's (never someone else's).
async fn bearer(s: &NodeState) -> Result<String, ApiError> {
    match user_token(s, &crate::accounts::asking()).await? {
        Some(t) => Ok(t),
        None => app_token(s).await,
    }
}

fn country(s: &NodeState) -> String {
    load_user(s, &crate::accounts::asking())
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
    for attempt in 0..3 {
        let mut req = s
            .http_client()
            .get(&url)
            .bearer_auth(&token)
            .header(reqwest::header::ACCEPT, ACCEPT);
        if !q.is_empty() {
            req = req.query(&q);
        }
        let resp = req.send().await.map_err(upstream)?;
        let status = resp.status();
        if status == StatusCode::NOT_FOUND {
            return Err(ApiError::not_found(format!("tidal: nothing at {path}")));
        }
        if status == StatusCode::TOO_MANY_REQUESTS && attempt < 2 {
            let wait = resp
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(1 + attempt);
            tracing::info!(%url, wait, "TIDAL rate limit: waiting before the retry");
            tokio::time::sleep(std::time::Duration::from_secs(wait)).await;
            continue;
        }
        let text = resp.text().await.map_err(upstream)?;
        let v: Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(e) => {
                return Err(upstream(format!(
                    "{path} answered {status} with a body that is not JSON ({e}): {}",
                    text.chars().take(160).collect::<String>()
                )))
            }
        };
        if !status.is_success() {
            let detail = v["errors"][0]["detail"]
                .as_str()
                .or(v["errors"][0]["code"].as_str())
                .unwrap_or("");
            return Err(upstream(format!("{path} answered {status}: {detail}")));
        }
        return Ok(v);
    }
    Err(upstream(format!("{path}: rate limited")))
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

    fn unified(&self, s: &NodeState) -> bool {
        // Without the app's secret, TIDAL's catalogue answers a signed-in user only
        let c = creds(s);
        c.configured() && (c.has_secret() || load_user(s, &crate::accounts::asking()).is_some())
    }

    fn features(&self) -> &'static [&'static str] {
        &["search", "play", "artists", "library", "playlists"]
    }

    fn info(&self, s: &NodeState) -> SourceInfo {
        // Either sign-in counts: the device way (or a sign-in imported from another app) plays
        // full tracks on its own, even on a build without the project's app
        let who = crate::accounts::asking();
        let status = if load_user(s, &who).is_some() || crate::tidal_unofficial::streams(s, &who) {
            "ready".to_string()
        } else if !creds(s).configured() {
            "not configured: this build has no built-in TIDAL app; add one in Settings › This node › Service apps".to_string()
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
        let artists_page = get_json(
            s,
            "/searchResults",
            &[
                ("filter[query]", term.to_string()),
                ("countryCode", country(s)),
                ("include", "artists".into()),
            ],
        )
        .await?;
        let mut hits = Vec::new();
        let per_kind = (limit as usize).div_ceil(2).max(1);
        let artist_ids: Vec<String> = rel_ids(&artists_page["data"][0], "artists")
            .into_iter()
            .map(|(id, _)| id)
            .take(6)
            .collect();
        if !artist_ids.is_empty() {
            // TIDAL ranks related artists high; the one whose name matches the query goes first.
            let needle = term.trim().to_lowercase();
            let mut artists = artist_hits(s, &artist_ids).await?;
            artists.sort_by_key(|h| {
                let name = h.title.to_lowercase();
                if name == needle {
                    0
                } else if name.starts_with(&needle) || needle.starts_with(&name) {
                    1
                } else if name.contains(&needle) {
                    2
                } else {
                    3
                }
            });
            hits.extend(artists);
        }
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
                    duration_ms: None,
                    library_artist_id: None,
                    isrc: None,
                    upc: at["barcodeId"].as_str().map(str::to_string),
                    library_track_id: None,
                    library_album_id: None,
                    uri: None,
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
                    duration_ms: t["attributes"]["duration"].as_str().and_then(duration_ms),
                    library_artist_id: None,
                    isrc: t["attributes"]["isrc"].as_str().map(str::to_string),
                    upc: None,
                    library_track_id: None,
                    library_album_id: None,
                    uri: Some(track_uri(&id)),
                    url: tidal_url("track", &id),
                });
            }
        }
        Ok(hits)
    }

    async fn artist(&self, s: &NodeState, id: &str) -> Result<SourceArtist, ApiError> {
        let aid = id.strip_prefix("artist:").unwrap_or(id);
        if aid.is_empty() || !aid.bytes().all(|b| b.is_ascii_alphanumeric()) {
            return Err(ApiError::bad_request("bad TIDAL artist id"));
        }
        let head = artist_hits(s, &[aid.to_string()]).await?;
        let Some(h) = head.into_iter().next() else {
            return Err(ApiError::not_found(format!("no TIDAL artist {aid}")));
        };
        let album_ids = relationship_ids(
            s,
            &format!("/artists/{aid}/relationships/albums"),
            &[("countryCode", country(s))],
            60,
        )
        .await?;
        let mut albums = album_hits(s, &album_ids.into_iter().take(60).collect::<Vec<_>>()).await?;
        albums.sort_by_key(|a| std::cmp::Reverse(a.year));
        Ok(SourceArtist {
            source: SOURCE_ID.into(),
            id: format!("artist:{aid}"),
            name: h.title,
            image: h.image,
            url: tidal_url("artist", aid),
            albums,
            library_artist_id: None,
        })
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
                library_album_id: None,
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
        let library_album_id = album["attributes"]["barcodeId"]
            .as_str()
            .and_then(|u| pm_model::Upc::parse(u).ok())
            .and_then(|u| {
                s.library
                    .lock()
                    .expect("library poisoned")
                    .album_by_upc(&u)
                    .ok()
                    .flatten()
            })
            .and_then(|a| a.id);
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
            library_album_id,
        })
    }
}

// ---- sign-in ----

pub(crate) fn random_urlsafe(bytes: usize) -> String {
    use rand::RngCore;
    let mut buf = vec![0u8; bytes];
    rand::thread_rng().fill_bytes(&mut buf);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(buf)
}

/// `GET /v1/sources/tidal/login`: the URL to open in a browser.
pub(crate) async fn login(
    State(s): State<Arc<NodeState>>,
    axum::Extension(caller): axum::Extension<crate::accounts::Caller>,
    headers: axum::http::HeaderMap,
) -> Result<Response, ApiError> {
    let who = Who::of(&caller);
    if who == Who::Nobody {
        return Err(ApiError::new(StatusCode::FORBIDDEN, "sign in to the node first"));
    }
    let c = creds(&s);
    if !c.configured() {
        return Err(ApiError::new(
            StatusCode::PRECONDITION_FAILED,
            "TIDAL is not set up on this node",
        ));
    }
    use sha2::Digest;
    let verifier = random_urlsafe(48);
    let challenge =
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sha2::Sha256::digest(verifier.as_bytes()));
    let nonce = random_urlsafe(24);
    // Without the secret (the project's app) the sign-in returns through the static page, which
    // forwards to the callback named in `state`; an app with its secret here comes straight back.
    // The verifier never leaves the node.
    let redirect = redirect_uri(&s, &headers);
    let state = if c.has_secret() {
        nonce.clone()
    } else {
        crate::project_apps::return_state(&nonce, &callback_uri(&s, &headers))
    };
    let binding = crate::accounts::login_binding("tidal", &who);
    pending().lock().expect("pending").insert(
        nonce,
        Pending {
            verifier,
            redirect: redirect.clone(),
            who,
            binding: binding.as_ref().map(|(v, _)| v.clone()),
        },
    );
    let mut url = url::Url::parse(&format!("{}/authorize", login_base(&s))).map_err(upstream)?;
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", &c.client_id)
        .append_pair("redirect_uri", &redirect)
        .append_pair("scope", &c.scopes())
        .append_pair("code_challenge_method", "S256")
        .append_pair("code_challenge", &challenge)
        .append_pair("state", &state);
    let mut resp = Json(LoginUrl { url: url.to_string() }).into_response();
    if let Some((_, cookie)) = binding {
        if let Ok(v) = axum::http::HeaderValue::from_str(&cookie) {
            resp.headers_mut().append(axum::http::header::SET_COOKIE, v);
        }
    }
    Ok(resp)
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

#[derive(Debug, Deserialize)]
pub struct PastedCallback {
    /// The address the browser landed on after the service's sign-in page.
    pub url: String,
}

/// The callback's parameters out of a pasted address (`?code=…&state=…`, or the service's error).
pub(crate) fn callback_from_url(url: &str) -> Result<CallbackQuery, ApiError> {
    let parsed = url::Url::parse(url.trim())
        .map_err(|_| ApiError::bad_request("Paste the whole address, starting with http:// or https://"))?;
    let q: std::collections::HashMap<String, String> = parsed.query_pairs().into_owned().collect();
    if !q.contains_key("code") && !q.contains_key("error") {
        return Err(ApiError::bad_request(
            "That address carries no sign-in code; paste the one the sign-in page sent you to",
        ));
    }
    Ok(serde_json::from_value(serde_json::to_value(q).expect("map")).expect("string fields"))
}

/// `POST /v1/sources/tidal/callback {url}`: the fallback when the browser could not reach the
/// node's callback (a phone on another network, an app allowing only loopback redirects): the
/// user pastes the address the sign-in page sent them to, and the node finishes the sign-in from
/// its parameters as if the browser had arrived there.
pub(crate) async fn pasted_callback(
    State(s): State<Arc<NodeState>>,
    axum::Extension(caller): axum::Extension<crate::accounts::Caller>,
    headers: axum::http::HeaderMap,
    Json(body): Json<PastedCallback>,
) -> ApiResult<TidalStatus> {
    let q = callback_from_url(&body.url)?;
    let finish = crate::accounts::LoginFinish::Pasted(Who::of(&caller));
    finish_login(&s, q, finish).await?;
    Ok(Json(status_view(&s, &Who::of(&caller), &headers)))
}

/// `GET /tidal/callback?code&state`: finish the sign-in, then back to the web UI.
pub(crate) async fn callback(
    State(s): State<Arc<NodeState>>,
    Query(q): Query<CallbackQuery>,
    headers: axum::http::HeaderMap,
) -> Response {
    match finish_login(&s, q, crate::accounts::LoginFinish::Browser(headers)).await {
        Ok(()) => Redirect::to("/?tidal=signed_in").into_response(),
        Err(e) => {
            tracing::warn!("tidal sign-in failed: {}", e.message);
            let msg: String = url::form_urlencoded::byte_serialize(e.message.as_bytes()).collect();
            Redirect::to(&format!("/?tidal=error&detail={msg}")).into_response()
        }
    }
}

/// The same test for a refresh (a token renewal) without the secret.
pub const REFRESH_PROBE_KEY: &str = "tidal.public_refresh_probe";

fn record_refresh_probe(s: &NodeState, outcome: &str, detail: &str) {
    let v = serde_json::json!({ "outcome": outcome, "detail": detail, "at": now_unix() });
    let _ = s
        .library
        .lock()
        .expect("library poisoned")
        .set_setting(REFRESH_PROBE_KEY, &v.to_string());
    tracing::info!(outcome, detail, "TIDAL public-client refresh test");
}

fn record_public_probe(s: &NodeState, outcome: &str, detail: &str) {
    let v = serde_json::json!({ "outcome": outcome, "detail": detail, "at": now_unix() });
    let _ = s
        .library
        .lock()
        .expect("library poisoned")
        .set_setting(PUBLIC_CLIENT_KEY, &v.to_string());
    tracing::info!(outcome, detail, "TIDAL public-client test");
}

async fn finish_login(
    s: &NodeState,
    q: CallbackQuery,
    finish: crate::accounts::LoginFinish,
) -> Result<(), ApiError> {
    if let Some(err) = q.error {
        return Err(ApiError::bad_request(format!(
            "TIDAL refused the sign-in: {err} {}",
            q.error_description.unwrap_or_default()
        )));
    }
    let (Some(code), Some(state)) = (q.code, q.state) else {
        return Err(ApiError::bad_request("missing code or state"));
    };
    // Kept for whoever started this sign-in (the callback itself comes without a node token),
    // and only where they started it
    let Pending {
        verifier,
        redirect,
        who,
        binding,
    } = pending()
        .lock()
        .expect("pending")
        .remove(crate::project_apps::state_nonce(&state))
        .ok_or_else(|| ApiError::bad_request("unknown or expired sign-in state; start again"))?;
    crate::accounts::check_login("tidal", &who, binding.as_deref(), &finish)?;
    let c = creds(s);
    let form = [
        ("grant_type", "authorization_code"),
        ("code", code.as_str()),
        ("redirect_uri", redirect.as_str()),
        ("client_id", c.client_id.as_str()),
        ("code_verifier", verifier.as_str()),
    ];
    // First as a public client (PKCE, no secret); what TIDAL answers is recorded. Refused, the
    // same code goes again with the secret when this node has one.
    let v = match token_request_public(s, &form).await {
        Ok(v) => {
            record_public_probe(
                s,
                "accepted",
                "the code exchange worked without the client secret",
            );
            v
        }
        Err((status, answer)) => {
            tracing::warn!(status, %answer, "TIDAL refused the code exchange without the client secret");
            record_public_probe(s, "refused", &format!("{status}: {answer}"));
            if !c.has_secret() {
                return Err(upstream(format!(
                    "TIDAL refused the sign-in ({status}): {answer}"
                )));
            }
            token_request(s, &form).await?
        }
    };
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
    {
        // Stored and marked new under one guard: no vault pass sees one without the other (the
        // vault is the owner's)
        let lib = s.library.lock().expect("library poisoned");
        who.put(
            &lib,
            USER_KEY,
            Some(&serde_json::to_string(&session).expect("session serializes")),
        )?;
        if who.is_owner() {
            crate::vault_cache::new_sign_in(&lib, "tidal");
        }
    }
    tracing::info!(user = ?session.user_id, "TIDAL sign-in stored");
    Ok(())
}

pub(crate) fn status_view(s: &NodeState, who: &Who, headers: &axum::http::HeaderMap) -> TidalStatus {
    let u = load_user(s, who);
    // Read the settings first, one lock at a time (two locks in one expression deadlock)
    let (playback_limit, public_client) = {
        let lib = s.library.lock().expect("library poisoned");
        (
            lib.get_setting(PLAYBACK_LIMIT_KEY).ok().flatten(),
            lib.get_setting(PUBLIC_CLIENT_KEY)
                .ok()
                .flatten()
                .and_then(|v| serde_json::from_str(&v).ok()),
        )
    };
    TidalStatus {
        configured: creds(s).configured(),
        signed_in: u.is_some(),
        user_id: u.as_ref().and_then(|u| u.user_id.clone()),
        country: u.as_ref().and_then(|u| u.country.clone()),
        scope: u.as_ref().map(|u| u.scope.clone()),
        expires_at: u.as_ref().map(|u| u.expires_at),
        redirect_uri: redirect_uri(s, headers),
        requested_scopes: creds(s).scopes(),
        access_tier: u.as_ref().and_then(|u| u.access_tier.clone()),
        playback_limit,
        public_client,
    }
}

/// `GET /v1/sources/tidal/status`.
pub(crate) async fn status(
    State(s): State<Arc<NodeState>>,
    axum::Extension(caller): axum::Extension<crate::accounts::Caller>,
    headers: axum::http::HeaderMap,
) -> ApiResult<TidalStatus> {
    Ok(Json(status_view(&s, &Who::of(&caller), &headers)))
}

/// `DELETE /v1/sources/tidal/session`: forget the caller's tokens.
pub(crate) async fn logout(
    State(s): State<Arc<NodeState>>,
    axum::Extension(caller): axum::Extension<crate::accounts::Caller>,
    headers: axum::http::HeaderMap,
) -> ApiResult<TidalStatus> {
    let who = Who::of(&caller);
    store_user(&s, &who, None)?;
    Ok(Json(status_view(&s, &who, &headers)))
}

/// The node's TIDAL app changed (Settings › Service apps): everyone's sign-in with it and the
/// app's own token belonged to the old one.
pub(crate) fn app_changed(s: &NodeState) -> Result<(), ApiError> {
    crate::accounts::forget_everyones(&s.library.lock().expect("library poisoned"), USER_KEY)?;
    *app_token_cache().lock().expect("app token") = None;
    Ok(())
}

// ---- one Sign in, the ways side by side (STEWARD-CREDENTIALS-REUSE.md) ----

pub(crate) const WAY_APP: &str = "app";
pub(crate) const WAY_IMPORT: &str = "import";
pub(crate) const WAY_DEVICE: &str = "device";
pub(crate) const WAY_PASTE: &str = "paste";

/// The official app's name by where it comes from: the project's built-in one or the node's own.
fn app_label(c: &TidalCredentials) -> &'static str {
    let project = crate::project_apps::tidal_client_id().is_some_and(|id| id == c.client_id);
    if project {
        "Pubky Music's app"
    } else {
        "Your own TIDAL app"
    }
}

/// The ways to sign in to TIDAL, which one is in use, and the overview line. The two sessions
/// (the official app's and the device client's) can both exist: the device one plays full
/// tracks, the official one serves the collection and playlist imports.
pub(crate) fn sign_in(
    s: &NodeState,
    who: &Who,
    headers: Option<&axum::http::HeaderMap>,
) -> crate::signin::SignIn {
    use crate::signin::{InUse, SignIn, Way, NOT_SIGNED_IN};
    let c = creds(s);
    // One lock, every read at once (two locks in one expression deadlock)
    let (user, mode, device_client, chosen, playback_limit) = {
        let lib = s.library.lock().expect("library poisoned");
        (
            load_user_from(&lib, who),
            crate::tidal_unofficial::load(&lib, who),
            crate::tidal_unofficial::client_of(&lib),
            crate::signin::chosen(&lib, who, SOURCE_ID),
            lib.get_setting(PLAYBACK_LIMIT_KEY).ok().flatten(),
        )
    };
    let app = app_label(&c);
    let device_label = "Your own device client";
    // Other apps' files are looked at for the tab, not for the overview line, and only for the
    // owner: they are on the owner's machine
    let found = if headers.is_some() && who.is_owner() {
        crate::imports::scan_tidal()
    } else {
        Vec::new()
    };
    let usable: Vec<&crate::imports::Import> = found.iter().filter(|i| i.found.usable).collect();
    let import_note = if found.is_empty() {
        format!(
            "Nothing in the known places on this machine ({}). From another machine, pick the file or paste it below.",
            crate::imports::tidal_places_text()
                .iter()
                .map(|p| p.split(':').next().unwrap_or(p).to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )
    } else {
        format!(
            "Found: {}. The other app's file is only read; the sign-in is kept here like the node's own.",
            found
                .iter()
                .map(|i| {
                    let f = &i.found;
                    match (&f.user, &f.reason) {
                        (_, Some(_)) => format!("{} (not usable)", f.app),
                        (Some(u), None) => format!("{} (user {u})", f.app),
                        (None, None) => f.app.clone(),
                    }
                })
                .collect::<Vec<_>>()
                .join(" · ")
        )
    };
    let ways = vec![
        Way {
            id: WAY_APP.into(),
            label: app.into(),
            note: "Official API, sign in at TIDAL in this browser: search, albums, your collection and playlists. TIDAL plays 30-second previews to apps on its third-party developer tier.".into(),
            action: "browser".into(),
            recommended: true,
            available: c.configured(),
            reason: (!c.configured()).then(|| {
                "This build has no built-in TIDAL app; add one in Settings › This node › Service apps."
                    .to_string()
            }),
        },
        Way {
            id: WAY_IMPORT.into(),
            label: "A sign-in from another app on this machine".into(),
            note: import_note,
            action: "import".into(),
            recommended: false,
            available: !usable.is_empty(),
            reason: usable.is_empty().then(|| {
                if !who.is_owner() {
                    "The sign-ins on the node's machine are the owner's; paste your own instead.".to_string()
                } else if found.is_empty() {
                    "Nothing found in the known places; pick the file or paste it instead.".to_string()
                } else {
                    "What was found is not usable; pick a newer file or paste one instead.".to_string()
                }
            }),
        },
        // Without a device client the way is still the place to go: its page asks for one first
        // (a step card with Check), so there is no separate "Your own app" way
        Way {
            id: WAY_DEVICE.into(),
            label: device_label.into(),
            note: if device_client.is_some() {
                "Full tracks (unofficial): the device sign-in tidal-dl-ng and python-tidal use, with a code you enter at link.tidal.com. TIDAL has not approved this for other apps and can stop it at any time.".into()
            } else {
                "Full tracks (unofficial): a code you enter at link.tidal.com, as tidal-dl-ng and python-tidal do. It needs a TIDAL device client: enter one you are allowed to use first. TIDAL has not approved this for other apps and can stop it at any time.".into()
            },
            action: "code".into(),
            recommended: false,
            // The device client is the node's: a listener signs in once the owner has added one
            available: device_client.is_some() || who.is_owner(),
            reason: (device_client.is_none() && !who.is_owner())
                .then(|| "The node's owner hasn't added a TIDAL device client yet.".to_string()),
        },
        Way {
            id: WAY_PASTE.into(),
            label: "Paste a token".into(),
            note: "The contents of one of those apps' token files, or a bare TIDAL access token (a refresh token on the next line keeps it alive). Nothing is shown again once saved.".into(),
            action: "paste".into(),
            recommended: false,
            available: true,
            reason: None,
        },
    ];
    let previews = playback_limit.is_some()
        || user.as_ref().and_then(|u| u.access_tier.as_deref()) == Some("THIRD_PARTY");
    let device_session = mode.session.as_ref().filter(|_| mode.enabled);
    // An imported session is its own way, named after the app it came from
    let device_in_use = || {
        device_session.map(|d| {
            let (way, label) = match d.from.as_deref() {
                Some("pasted") => (WAY_PASTE, "Pasted token".to_string()),
                Some(app) => (WAY_IMPORT, format!("{}'s sign-in", crate::imports::app_name(app))),
                None => (WAY_DEVICE, device_label.to_string()),
            };
            InUse {
                way: way.into(),
                label,
                user: d.user_id.clone(),
                expires_at: Some(d.expires_at),
                gives: Some("full tracks".into()),
            }
        })
    };
    let app_in_use = || {
        user.as_ref().map(|u| InUse {
            way: WAY_APP.into(),
            label: app.into(),
            user: u.user_id.clone(),
            expires_at: Some(u.expires_at),
            gives: Some(
                if previews {
                    "30-second previews, your collection"
                } else {
                    "the official API, your collection"
                }
                .into(),
            ),
        })
    };
    // The picked way's sign-in when it has one, else whichever exists (the device one streams)
    let in_use = match chosen.as_deref() {
        Some(WAY_APP) => app_in_use().or_else(device_in_use),
        _ => device_in_use().or_else(app_in_use),
    };
    let line = match &in_use {
        None => NOT_SIGNED_IN.to_string(),
        Some(u) if u.way != WAY_APP => format!(
            "{} · full tracks{}",
            u.label,
            if user.is_some() {
                format!("; collection through {app}")
            } else {
                String::new()
            }
        ),
        Some(u) => format!(
            "{} · {}",
            u.label,
            if previews { "previews" } else { "signed in" }
        ),
    };
    let details = match headers {
        Some(h) => serde_json::json!({
            "status": status_view(s, who, h),
            "unofficial": crate::tidal_unofficial::view(s, who),
        }),
        None => serde_json::Value::Null,
    };
    SignIn {
        source: SOURCE_ID.into(),
        ways,
        chosen,
        in_use,
        line,
        imports: found.into_iter().map(|i| i.found).collect(),
        places: if headers.is_some() {
            crate::imports::tidal_places_text()
        } else {
            Vec::new()
        },
        details,
    }
}

/// A sign-in from another app's file or a paste becomes the device-way session: full tracks,
/// kept like the node's own (`tidal.unofficial`, sealed in backups), renewed by the node where
/// the refresh token came from the client this build carries.
pub(crate) fn import_sign_in(s: &NodeState, who: &Who, session: crate::tidal_unofficial::Session) {
    crate::tidal_unofficial::update_signed_in(s, who, "tidal-full", |st| {
        st.session = Some(session);
        st.login = None;
        st.enabled = true;
        st.health = None;
    });
}

/// A way was picked. The device way is what the Full tracks switch used to turn on; the app way
/// turns it off again (its session is kept until Sign out). Setting up your own client changes
/// nothing yet.
pub(crate) fn way_chosen(s: &NodeState, who: &Who, way: &str) {
    match way {
        WAY_DEVICE | WAY_IMPORT | WAY_PASTE => {
            crate::tidal_unofficial::update(s, who, |st| {
                st.enabled = true;
                st.health = None;
            });
        }
        WAY_APP => {
            crate::tidal_unofficial::update(s, who, |st| st.enabled = false);
        }
        _ => {}
    }
}

/// TIDAL's ways, for `crate::signin`.
pub(crate) struct TidalSignIn;

#[async_trait::async_trait]
impl crate::signin::SignInSource for TidalSignIn {
    fn describe(
        &self,
        s: &NodeState,
        who: &Who,
        headers: Option<&axum::http::HeaderMap>,
    ) -> crate::signin::SignIn {
        sign_in(s, who, headers)
    }

    fn way_chosen(&self, s: &Arc<NodeState>, who: &Who, way: &str) {
        way_chosen(s, who, way);
    }

    fn sign_out_of(&self, s: &NodeState, who: &Who, way: &str) -> Result<(), ApiError> {
        sign_out_of(s, who, way)
    }

    async fn import(
        &self,
        s: &Arc<NodeState>,
        who: &Who,
        body: crate::signin::ImportBody,
    ) -> Result<&'static str, ApiError> {
        let (imp, way) = match (body.text.as_deref().map(str::trim), body.id.as_deref()) {
            (Some(text), _) if !text.is_empty() => (
                crate::imports::pasted_tidal(text)
                    .map_err(|e| ApiError::bad_request(format!("Not usable: {e}")))?,
                WAY_PASTE,
            ),
            (_, Some(id)) => {
                let imp = crate::imports::scan_tidal()
                    .into_iter()
                    .find(|i| i.found.id == id)
                    .ok_or_else(|| ApiError::not_found(format!("nothing found for {id} on this machine")))?;
                if !imp.found.usable {
                    return Err(ApiError::bad_request(format!(
                        "{}'s sign-in is not usable: {}",
                        imp.found.app,
                        imp.found.reason.clone().unwrap_or_default()
                    )));
                }
                (imp, WAY_IMPORT)
            }
            _ => {
                return Err(ApiError::bad_request(
                    "Say which sign-in (id) or paste one (text)",
                ))
            }
        };
        import_sign_in(s, who, imp.session);
        Ok(way)
    }
}

/// Sign out of one way: the device way also goes off, so TIDAL falls back to the official app.
pub(crate) fn sign_out_of(s: &NodeState, who: &Who, way: &str) -> Result<(), ApiError> {
    if way != WAY_APP {
        crate::tidal_unofficial::update(s, who, |st| {
            st.session = None;
            st.login = None;
            st.enabled = false;
        });
        Ok(())
    } else {
        store_user(s, who, None)
    }
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

fn is_zero(n: &usize) -> bool {
    *n == 0
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ImportReport {
    /// Entries that became canonical URIs (ISRC, else the service link).
    pub imported: usize,
    /// Entries left out (nothing to name them by).
    pub skipped: usize,
    /// Unidentified entries kept in the playlist as a MusicBrainz search for their words
    /// (TASK-pinakes-social-ux.md B4).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub kept: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub playlist: Option<PlaylistDetail>,
}

fn signed_in_user(s: &NodeState) -> Result<UserSession, ApiError> {
    load_user(s, &crate::accounts::asking()).ok_or_else(|| {
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

/// Resource ids of a paged to-many relationship, following `links.next` until `max` ids (a
/// big artist has dozens of pages, and TIDAL's rate limit made each one past the first few
/// wait seconds: an artist page took up to 80 s).
async fn relationship_ids(
    s: &NodeState,
    path: &str,
    query: &[(&str, String)],
    max: usize,
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
        if ids.len() >= max {
            break;
        }
        let Some(next) = page["links"]["next"].as_str().map(str::to_string) else {
            break;
        };
        page = get_json(s, &next, &[]).await?;
    }
    ids.truncate(max);
    Ok(ids)
}

/// The album of a TIDAL track: (title, link, cover, year).
pub(crate) async fn track_album(
    s: &NodeState,
    id: &str,
) -> Option<(String, String, Option<String>, Option<i32>)> {
    if id.is_empty() || !id.bytes().all(|b| b.is_ascii_digit()) || !endpoints(s).creds.configured() {
        return None;
    }
    let v = get_json(
        s,
        &format!("/tracks/{id}"),
        &[("countryCode", country(s)), ("include", "albums".into())],
    )
    .await
    .ok()?;
    let inc = included(&v);
    let (aid, _) = rel_ids(&v["data"], "albums").into_iter().next()?;
    let a = inc.get(&("albums".to_string(), aid.clone()))?;
    let attrs = &a["attributes"];
    Some((
        attrs["title"].as_str()?.to_string(),
        tidal_url("album", &aid),
        None,
        year_of(attrs["releaseDate"].as_str()),
    ))
}

/// A TIDAL track's ISRC (needs the node's TIDAL app).
pub(crate) async fn track_isrc(s: &NodeState, id: &str) -> Option<String> {
    if id.is_empty() || !id.bytes().all(|b| b.is_ascii_digit()) || !endpoints(s).creds.configured() {
        return None;
    }
    let v = get_json(s, &format!("/tracks/{id}"), &[("countryCode", country(s))])
        .await
        .ok()?;
    v["data"]["attributes"]["isrc"].as_str().map(str::to_string)
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

/// An artist's popular tracks on TIDAL (the artist's track relationship,
/// duplicates by fingerprint collapsed), as hits with queue URIs.
pub(crate) async fn artist_top_tracks(
    s: &NodeState,
    artist_id: &str,
    limit: usize,
) -> Result<Vec<SourceHit>, ApiError> {
    if artist_id.is_empty() || !artist_id.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return Err(ApiError::bad_request("bad TIDAL artist id"));
    }
    let ids = relationship_ids(
        s,
        &format!("/artists/{artist_id}/relationships/tracks"),
        &[("countryCode", country(s)), ("collapseBy", "FINGERPRINT".into())],
        limit,
    )
    .await?;
    Ok(track_hits(s, &ids).await?.0)
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
                duration_ms: t["attributes"]["duration"].as_str().and_then(duration_ms),
                library_artist_id: None,
                isrc: t["attributes"]["isrc"].as_str().map(str::to_string),
                upc: None,
                library_track_id: None,
                library_album_id: None,
                uri: Some(track_uri(&id)),
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
                duration_ms: None,
                library_artist_id: None,
                isrc: None,
                upc: a["attributes"]["barcodeId"].as_str().map(str::to_string),
                library_track_id: None,
                library_album_id: None,
                uri: None,
                url: tidal_url("album", &id),
            });
        }
    }
    Ok(hits)
}

const COLLECTION_KEY: &str = "tidal.collection";
const COLLECTION_TTL_SECS: i64 = 15 * 60;

#[derive(Debug, Deserialize, Default)]
pub(crate) struct CollectionQuery {
    #[serde(default)]
    refresh: Option<u8>,
}

/// Artist hits with their portrait, by id (≤ 20 per call).
async fn artist_hits(s: &NodeState, ids: &[String]) -> Result<Vec<SourceHit>, ApiError> {
    let mut q: Vec<(&str, String)> = vec![("countryCode", country(s)), ("include", "profileArt".into())];
    for id in ids.iter().take(20) {
        q.push(("filter[id]", id.clone()));
    }
    let v = get_json(s, "/artists", &q).await?;
    let inc = included(&v);
    Ok(v["data"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|ar| {
                    let id = ar["id"].as_str()?;
                    Some(SourceHit {
                        id: format!("artist:{id}"),
                        title: ar["attributes"]["name"].as_str().unwrap_or("").to_string(),
                        creator: None,
                        year: None,
                        downloads: ar["attributes"]["numberOfFollowers"].as_u64(),
                        image: profile_art_url(&inc, ar),
                        kind: "artist".into(),
                        duration_ms: None,
                        library_artist_id: None,
                        isrc: None,
                        upc: None,
                        library_track_id: None,
                        library_album_id: None,
                        uri: None,
                        url: tidal_url("artist", id),
                    })
                })
                .collect()
        })
        .unwrap_or_default())
}

/// The artist's profile picture closest to 640 px wide.
fn profile_art_url(inc: &Included<'_>, res: &Value) -> Option<String> {
    let (id, _) = rel_ids(res, "profileArt").into_iter().next()?;
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

/// `GET /v1/sources/tidal/collection?refresh=`: the user's favourite tracks
/// and albums, cached for a quarter hour (it is many calls to TIDAL).
pub(crate) async fn collection(
    State(s): State<Arc<NodeState>>,
    Query(q): Query<CollectionQuery>,
) -> ApiResult<TidalCollection> {
    signed_in_user(&s)?;
    if q.refresh.unwrap_or(0) == 0 {
        let cached = s
            .library
            .lock()
            .expect("library poisoned")
            .get_setting(COLLECTION_KEY)
            .ok()
            .flatten()
            .and_then(|j| serde_json::from_str::<(i64, TidalCollection)>(&j).ok())
            .filter(|(at, _)| now_unix() - at < COLLECTION_TTL_SECS);
        if let Some((_, c)) = cached {
            return Ok(Json(c));
        }
    }
    // The collection resources take `me` for the signed-in user.
    let track_ids = relationship_ids(
        &s,
        "/userCollectionTracks/me/relationships/items",
        &[("sort", "-addedAt".into())],
        usize::MAX,
    )
    .await?;
    let album_ids = relationship_ids(
        &s,
        "/userCollectionAlbums/me/relationships/items",
        &[("sort", "-addedAt".into())],
        usize::MAX,
    )
    .await?;
    let (mut tracks, _) = track_hits(&s, &track_ids).await?;
    let mut albums = album_hits(&s, &album_ids).await?;
    let lib = s.library.lock().expect("library poisoned");
    for h in tracks.iter_mut().chain(albums.iter_mut()) {
        crate::streaming::mark_library(&lib, h);
    }
    let c = TidalCollection { tracks, albums };
    let _ = lib.set_setting(
        COLLECTION_KEY,
        &serde_json::to_string(&(now_unix(), &c)).expect("serializes"),
    );
    Ok(Json(c))
}

/// `POST /v1/sources/tidal/collection/import`: the collection as node
/// bookmarks (ISRC where TIDAL has one, else the TIDAL link; ADR 0010).
pub(crate) async fn import_collection(State(s): State<Arc<NodeState>>) -> ApiResult<ImportReport> {
    signed_in_user(&s)?;
    let track_ids = relationship_ids(
        &s,
        "/userCollectionTracks/me/relationships/items",
        &[],
        usize::MAX,
    )
    .await?;
    let album_ids = relationship_ids(
        &s,
        "/userCollectionAlbums/me/relationships/items",
        &[],
        usize::MAX,
    )
    .await?;
    let (_, docs) = track_hits(&s, &track_ids).await?;
    let mut report = ImportReport::default();
    let lib = s.library.lock().expect("library poisoned");
    for t in &docs {
        match canonical_track(t) {
            Some(uri) => {
                crate::bookmarks::add(&lib, uri)?;
                report.imported += 1;
            }
            None => report.skipped += 1,
        }
    }
    for id in &album_ids {
        match pm_model::MusicUri::parse(&tidal_url("album", id)) {
            Ok(uri) => {
                crate::bookmarks::add(&lib, uri)?;
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
    axum::extract::Query(opt): axum::extract::Query<ImportOptions>,
) -> ApiResult<ImportReport> {
    let keep = opt.keep_source.unwrap_or(false);
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
    let mut sources = Vec::new();
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
                Some(u) => {
                    uris.push(u.to_url());
                    sources.push((keep && key.0 == "tracks").then(|| pm_pubky::ItemSource {
                        service: SOURCE_ID.into(),
                        uri: Some(tidal_url("track", &key.1)),
                    }));
                }
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
            sources,
            ..Default::default()
        }),
    )
    .await?;
    Ok(Json(ImportReport {
        imported,
        skipped,
        playlist: Some(detail),
        ..Default::default()
    }))
}

// ---- versions: where a library album or track streams from on TIDAL ----

pub(crate) fn cache_get(s: &NodeState, key: &str) -> Option<Vec<Version>> {
    cache_get_ttl(s, key, 7 * 24 * 3600)
}

pub(crate) fn cache_get_ttl(s: &NodeState, key: &str, ttl_secs: i64) -> Option<Vec<Version>> {
    let lib = s.library.lock().expect("library poisoned");
    let j = lib.get_setting(key).ok().flatten()?;
    let (at, v): (i64, Vec<Version>) = serde_json::from_str(&j).ok()?;
    (now_unix() - at < ttl_secs).then_some(v)
}

pub(crate) fn cache_put(s: &NodeState, key: &str, v: &[Version]) {
    let lib = s.library.lock().expect("library poisoned");
    let _ = lib.set_setting(key, &serde_json::to_string(&(now_unix(), v)).expect("serializes"));
}

/// TIDAL albums with the library album's UPC.
pub(crate) async fn album_versions(s: &NodeState, album: &pm_model::Album) -> Result<Vec<Version>, ApiError> {
    if !creds(s).configured() {
        return Ok(Vec::new());
    }
    let Some(upc) = album.upc.as_ref() else {
        return Ok(Vec::new());
    };
    let key = format!("tidal.version.album.{}", album.id.unwrap_or(0));
    if let Some(v) = cache_get(s, &key) {
        return Ok(v);
    }
    let v = get_json(
        s,
        "/albums",
        &[
            ("countryCode", country(s)),
            ("filter[barcodeId]", upc.to_string()),
            ("include", "artists".into()),
        ],
    )
    .await?;
    let out: Vec<Version> = v["data"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|al| {
                    let id = al["id"].as_str()?;
                    Some(Version {
                        page: None,
                        source: SOURCE_ID.into(),
                        id: format!("album:{id}"),
                        label: format!("TIDAL · {}", format_label(al).trim_end_matches(" (TIDAL)")),
                        url: tidal_url("album", id),
                        uri: None,
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    cache_put(s, &key, &out);
    Ok(out)
}

/// TIDAL tracks with the library track's ISRC.
pub(crate) async fn track_versions(s: &NodeState, track: &pm_model::Track) -> Result<Vec<Version>, ApiError> {
    if !creds(s).configured() {
        return Ok(Vec::new());
    }
    let Some(isrc) = track.isrc.as_ref() else {
        return Ok(Vec::new());
    };
    isrc_versions(s, &isrc.to_string()).await
}

/// TIDAL tracks with this ISRC (cached per ISRC).
pub(crate) async fn isrc_versions(s: &NodeState, isrc: &str) -> Result<Vec<Version>, ApiError> {
    if !creds(s).configured() {
        return Ok(Vec::new());
    }
    let key = format!("tidal.version.isrc.{isrc}");
    if let Some(v) = cache_get(s, &key) {
        return Ok(v);
    }
    let v = get_json(
        s,
        "/tracks",
        &[("countryCode", country(s)), ("filter[isrc]", isrc.to_string())],
    )
    .await?;
    let out: Vec<Version> = v["data"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|t| {
                    let id = t["id"].as_str()?;
                    Some(Version {
                        page: None,
                        source: SOURCE_ID.into(),
                        id: format!("track:{id}"),
                        label: format!("TIDAL · {}", format_label(t).trim_end_matches(" (TIDAL)")),
                        url: tidal_url("track", id),
                        uri: Some(track_uri(id)),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    cache_put(s, &key, &out);
    Ok(out)
}

/// Whole tracks fetched for browsers, kept a short while so seeking and
/// replaying do not hit TIDAL again. Previews are tiny; full tracks are tens
/// of megabytes, so only a few are kept.
/// (track id, fetched at, bytes, media type).
type CachedStream = (String, std::time::Instant, Arc<Vec<u8>>, String);

pub(crate) fn stream_cache() -> &'static Mutex<Vec<CachedStream>> {
    static C: std::sync::OnceLock<Mutex<Vec<CachedStream>>> = std::sync::OnceLock::new();
    C.get_or_init(|| Mutex::new(Vec::new()))
}

/// Drops the tracks fetched for one person (`Who::tag`): they were removed.
pub(crate) fn forget_streams_of(tag: &str) {
    let prefix = format!("{tag}/");
    stream_cache()
        .lock()
        .expect("stream cache")
        .retain(|(k, _, _, _)| !k.starts_with(&prefix));
}
const STREAM_CACHE_ENTRIES: usize = 6;
const STREAM_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(20 * 60);
const STREAM_MAX_BYTES: usize = 300 * 1024 * 1024;

/// `GET /v1/sources/tidal/stream/{id}`: the track as one `audio/mp4` body
/// with a known length and byte ranges, so browsers (the web UI's own zone)
/// can play, pause and seek it like a file. Same rules as node playback
/// (sign-in, previews, DRM refused).
pub(crate) async fn stream(
    State(s): State<Arc<NodeState>>,
    axum::Extension(caller): axum::Extension<crate::accounts::Caller>,
    axum::extract::Path(id): axum::extract::Path<String>,
    headers: axum::http::HeaderMap,
) -> Result<Response, ApiError> {
    if id.is_empty() || !id.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return Err(ApiError::bad_request("bad TIDAL track id"));
    }
    // With the caller's own account; what one person fetched is never served to another
    let who = Who::of(&caller);
    if who == Who::Nobody {
        return Err(ApiError::new(StatusCode::FORBIDDEN, not_connected(&who)));
    }
    let key = format!("{}/{id}", who.tag());
    let cached = {
        let mut c = stream_cache().lock().expect("stream cache");
        c.retain(|(_, at, _, _)| at.elapsed() < STREAM_CACHE_TTL);
        c.iter()
            .find(|(k, _, _, _)| *k == key)
            .map(|(_, _, b, mime)| (b.clone(), mime.clone()))
    };
    // Full tracks (unofficial) first when switched on; any trouble falls back to the official
    // stream (a preview), as on the node
    let full = if cached.is_none()
        && crate::tidal_unofficial::load(&s.library.lock().expect("library poisoned"), &who).enabled
    {
        let s2 = s.clone();
        let id2 = id.clone();
        let who2 = who.clone();
        tokio::task::spawn_blocking(move || -> Option<(Vec<u8>, String)> {
            use std::io::Read;
            let ep = endpoints(&s2);
            let r = crate::tidal_unofficial::open_blocking(&s2.library, &ep, &id2, &who2);
            crate::tidal_unofficial::record(&s2.library, &who2, &r);
            let mut f = match r {
                Ok(Some(f)) => f,
                Ok(None) => return None,
                Err(e) => {
                    tracing::warn!(track = %id2, "TIDAL full track not available: {e:?}");
                    return None;
                }
            };
            let mut out = Vec::new();
            f.reader
                .by_ref()
                .take(STREAM_MAX_BYTES as u64)
                .read_to_end(&mut out)
                .ok()?;
            Some((out, f.mime.to_string()))
        })
        .await
        .ok()
        .flatten()
    } else {
        None
    };
    let (bytes, mime) = match (cached, full) {
        (Some(c), _) => c,
        (None, Some((b, mime))) => {
            let bytes = Arc::new(b);
            let mut c = stream_cache().lock().expect("stream cache");
            c.retain(|(k, _, _, _)| *k != key);
            if c.len() >= STREAM_CACHE_ENTRIES {
                c.remove(0);
            }
            c.push((
                key.clone(),
                std::time::Instant::now(),
                bytes.clone(),
                mime.clone(),
            ));
            tracing::info!(track = %id, bytes = bytes.len(), "TIDAL full track fetched for a browser (unofficial)");
            (bytes, mime)
        }
        (None, None) => {
            let token = user_token(&s, &who)
                .await?
                .ok_or_else(|| ApiError::new(StatusCode::PRECONDITION_FAILED, not_connected(&who)))?;
            let api = api_base(&s);
            let v: Value = s
                .http_client()
                .get(format!("{api}/trackManifests/{id}"))
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
                .await
                .map_err(upstream)?
                .json()
                .await
                .map_err(upstream)?;
            let m = parse_manifest(&v).map_err(upstream)?;
            if m.drm {
                return Err(upstream("this track is DRM-protected"));
            }
            let mime = if m.formats.iter().any(|f| f.starts_with("FLAC")) {
                "audio/mp4; codecs=\"flac\""
            } else {
                "audio/mp4; codecs=\"mp4a.40.2\""
            }
            .to_string();
            let uri = m.uri.clone();
            let bytes = tokio::task::spawn_blocking(move || -> Result<Vec<u8>, String> {
                use std::io::Read;
                let client = crate::sources::http_client();
                let mut reader = crate::hls::HlsReader::open(client, &uri)?;
                let mut out = Vec::new();
                let mut buf = vec![0u8; 256 * 1024];
                loop {
                    let n = reader.read(&mut buf).map_err(|e| e.to_string())?;
                    if n == 0 {
                        break;
                    }
                    out.extend_from_slice(&buf[..n]);
                    if out.len() > STREAM_MAX_BYTES {
                        return Err("track larger than the proxy allows".into());
                    }
                }
                Ok(out)
            })
            .await
            .map_err(|e| upstream(e.to_string()))?
            .map_err(upstream)?;
            let bytes = Arc::new(bytes);
            let mut c = stream_cache().lock().expect("stream cache");
            c.retain(|(k, _, _, _)| *k != key);
            if c.len() >= STREAM_CACHE_ENTRIES {
                c.remove(0);
            }
            c.push((
                key.clone(),
                std::time::Instant::now(),
                bytes.clone(),
                mime.clone(),
            ));
            tracing::info!(track = %id, bytes = bytes.len(), presentation = %m.presentation, "TIDAL stream fetched for a browser");
            (bytes, mime)
        }
    };
    let mut resp = crate::ranged_response(&headers, bytes.as_ref().clone(), &mime);
    resp.headers_mut().insert(
        "Cache-Control",
        axum::http::HeaderValue::from_static("private, max-age=600"),
    );
    Ok(resp)
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

/// Why a track can't play for this person: they have no TIDAL sign-in of their own.
pub(crate) fn not_connected(who: &Who) -> String {
    match who {
        Who::Owner => "TIDAL: sign in on the Sources page first".into(),
        Who::Listener(_) => "Connect TIDAL (Settings › You › Services) to play this".into(),
        Who::Nobody => "TIDAL plays with the account of whoever queued it, and this has none".into(),
    }
}

/// A valid access token of `who`'s for the audio thread: the stored one, or a blocking refresh
/// when it is about to expire.
fn user_token_blocking(
    library: &std::sync::Mutex<pm_library::Library>,
    ep: &TidalEndpoints,
    client: &reqwest::blocking::Client,
    who: &Who,
) -> Result<String, String> {
    let u = {
        let lib = library.lock().map_err(|_| "library poisoned")?;
        load_user_from(&lib, who).ok_or_else(|| not_connected(who))?
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
    // Only over the session this refresh started from: one signed out (or a person removed)
    // meanwhile stays gone
    let key = who.setting(USER_KEY).ok_or_else(|| not_connected(who))?;
    let was = u.access_token.clone();
    let still = |cur: &str| serde_json::from_str::<UserSession>(cur).is_ok_and(|c| c.access_token == was);
    let raw = serde_json::to_string(&fresh).expect("session serializes");
    if !crate::vault_cache::store_refreshed(&lib, &key, still, &raw) {
        // Use what is there now, if anything
        return lib
            .get_setting(&key)
            .ok()
            .flatten()
            .and_then(|v| serde_json::from_str::<UserSession>(&v).ok())
            .map(|c| c.access_token)
            .ok_or_else(|| not_connected(who));
    }
    Ok(fresh.access_token)
}

/// Open `tidal:track:<id>` for playback: fetch the stream manifest with the
/// user's token, then read the HLS segments as one FLAC/AAC stream.
pub(crate) fn open_track_blocking(
    library: &std::sync::Mutex<pm_library::Library>,
    endpoints: &SharedEndpoints,
    id: &str,
    who: &Who,
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
    // Full tracks (unofficial) first when the owner switched it on; any trouble falls back to the
    // official path (a preview) and is noted
    let full = crate::tidal_unofficial::open_blocking(library, &ep, id, who);
    crate::tidal_unofficial::record(library, who, &full);
    match full {
        Ok(Some(f)) => {
            tracing::info!(track = id, "TIDAL full track (unofficial)");
            if let Ok(lib) = library.lock() {
                let _ = lib.delete_setting(PLAYBACK_LIMIT_KEY);
            }
            return crate::sources::frame_source_from_reader(f.reader, Some(f.ext), &uri);
        }
        Ok(None) => {}
        Err(e) => tracing::warn!(
            track = id,
            "TIDAL full track not available, playing the official stream: {e:?}"
        ),
    }
    if !ep.creds.configured() {
        return Err(err("TIDAL is not configured on this node".into()));
    }
    let client = crate::sources::http_client();
    let token = user_token_blocking(library, &ep, client, who).map_err(err)?;
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
    let limit = if m.presentation == "FULL" {
        None
    } else {
        tracing::warn!(
            track = id,
            reason = ?m.preview_reason,
            "TIDAL serves a preview, not the full track"
        );
        Some(m.preview_reason.clone().unwrap_or_else(|| m.presentation.clone()))
    };
    if let Ok(lib) = library.lock() {
        let _ = match &limit {
            Some(r) => lib.set_setting(PLAYBACK_LIMIT_KEY, r),
            None => lib.delete_setting(PLAYBACK_LIMIT_KEY).map(|_| ()),
        };
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
