//! Qobuz (owner, 2026-10-22: "add qobuz same way as tidal"; STEWARD-DUAL-MODE.md). Qobuz has no
//! open API, so the source is unofficial-only: it uses the private API Qobuz's own players use,
//! opt-in behind a warning, and can stop working at any time.
//!
//! - **App:** a Qobuz app ID and secret the owner enters in Sources › Qobuz (or `[qobuz] app_id` /
//!   `app_secret`, `PM_QOBUZ_APP_ID` / `_SECRET`). The node has no built-in app and never fetches
//!   one; without it the source stays off.
//! - **Sign-in:** the owner's Qobuz email and password, sent once to `user/login`; the node keeps
//!   only the session token Qobuz answers with (a sign-in: a backup carries it only under the
//!   passphrase). Accounts without a streaming plan are refused there.
//! - **Streams:** `track/getFileUrl` (signed with the app secret) answers a plain FLAC URL, up to
//!   24-bit/192 kHz, read straight into the decoder. A sample (30 s) answer is refused, not played.
//! - **Health:** a failure of the mode itself (refused app, ended session) switches it off and the
//!   page says why, with Retry. Qobuz links still open on qobuz.com and match through Pinakes.

use crate::state::NodeState;
use crate::streaming::StreamingService;
use crate::types::*;
use crate::{ApiError, ApiResult};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use md5::{Digest, Md5};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;

pub const SOURCE_ID: &str = "qobuz";
pub const TRACK_URI_PREFIX: &str = "qobuz:track:";
pub const STATE_KEY: &str = "qobuz.unofficial";
pub const APP_KEY: &str = "qobuz.app";
const API_BASE: &str = "https://www.qobuz.com/api.json/0.2";
/// What the page says before the mode is switched on.
pub const WARNING: &str = "This uses Qobuz's private API, which Qobuz has not approved for other apps. It can stop working at any time, and Qobuz could act on the account. It needs a paid Qobuz plan.";

pub fn track_uri(id: &str) -> String {
    format!("{TRACK_URI_PREFIX}{id}")
}

/// `https://open.qobuz.com/{kind}/{id}`: the service's own page.
pub fn page_url(kind: &str, id: &str) -> String {
    format!("https://open.qobuz.com/{kind}/{id}")
}

fn base_override() -> &'static std::sync::RwLock<Option<String>> {
    static B: std::sync::OnceLock<std::sync::RwLock<Option<String>>> = std::sync::OnceLock::new();
    B.get_or_init(|| std::sync::RwLock::new(None))
}

/// Point the node at another Qobuz API (tests: a mock).
pub fn set_api_base(url: &str) {
    *base_override().write().expect("qobuz base") = Some(url.trim_end_matches('/').to_string());
}

fn api_base() -> String {
    if let Some(b) = base_override().read().expect("qobuz base").clone() {
        return b;
    }
    std::env::var("PM_QOBUZ_API_BASE")
        .ok()
        .map(|v| v.trim().trim_end_matches('/').to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| API_BASE.into())
}

fn now() -> i64 {
    pm_library::db::now_unix()
}

fn upstream(e: impl std::fmt::Display) -> ApiError {
    ApiError::new(StatusCode::BAD_GATEWAY, format!("Qobuz: {e}"))
}

fn numeric(id: &str) -> Result<&str, ApiError> {
    if id.is_empty() || id.len() > 20 || !id.bytes().all(|b| b.is_ascii_digit()) {
        return Err(ApiError::bad_request("bad Qobuz id"));
    }
    Ok(id)
}

/// Album ids are letters and digits (`0060253780968`, `xu5ypgnw3ugzb`).
fn album_id(id: &str) -> Result<&str, ApiError> {
    if id.is_empty() || id.len() > 40 || !id.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return Err(ApiError::bad_request("bad Qobuz album id"));
    }
    Ok(id)
}

// ---- state ---------------------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Session {
    pub token: String,
    #[serde(default)]
    pub user_id: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    /// The plan's name as Qobuz labels it (`Studio`, `Sublime`, …).
    #[serde(default)]
    pub plan: Option<String>,
    #[serde(default)]
    pub country: Option<String>,
    /// The plan streams hi-res (else 16-bit/44.1 kHz at most).
    #[serde(default)]
    pub hires: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Health {
    pub ok: bool,
    pub detail: String,
    pub at: i64,
}

/// What the node keeps for this source.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ModeState {
    #[serde(default)]
    pub enabled: bool,
    /// `max` (up to 24-bit/192 kHz) or `cd` (16-bit/44.1 kHz).
    #[serde(default)]
    pub quality: String,
    #[serde(default)]
    pub session: Option<Session>,
    #[serde(default)]
    pub health: Option<Health>,
}

#[derive(Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct App {
    pub id: String,
    pub secret: String,
    /// `settings` or `config`.
    #[serde(default)]
    pub from: String,
}

impl std::fmt::Debug for App {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("App")
            .field("id", &self.id)
            .field("secret", &if self.secret.is_empty() { "" } else { "<set>" })
            .field("from", &self.from)
            .finish()
    }
}

pub fn load(lib: &pm_library::Library) -> ModeState {
    lib.get_setting(STATE_KEY)
        .ok()
        .flatten()
        .and_then(|v| serde_json::from_str(&v).ok())
        .unwrap_or_default()
}

fn save(lib: &pm_library::Library, st: &ModeState) {
    let _ = lib.set_setting(STATE_KEY, &serde_json::to_string(st).expect("state serializes"));
}

fn update(s: &NodeState, f: impl FnOnce(&mut ModeState)) {
    let lib = s.library.lock().expect("library poisoned");
    let mut st = load(&lib);
    f(&mut st);
    save(&lib, &st);
}

/// [`update`] for an interactive sign-in: the state and the vault's "new sign-in" mark are
/// written under one library guard, so no vault pass sees one without the other.
pub(crate) fn update_signed_in(s: &NodeState, service: &str, f: impl FnOnce(&mut ModeState)) {
    let lib = s.library.lock().expect("library poisoned");
    let mut st = load(&lib);
    f(&mut st);
    save(&lib, &st);
    crate::vault_cache::new_sign_in(&lib, service);
}

fn config_app() -> &'static std::sync::OnceLock<App> {
    static C: std::sync::OnceLock<App> = std::sync::OnceLock::new();
    &C
}

/// The app from node.toml (`[qobuz] app_id` / `app_secret`), set once at start-up; Settings
/// override it.
pub fn set_config_app(id: &str, secret: &str) {
    if !id.trim().is_empty() {
        let _ = config_app().set(App {
            id: id.trim().into(),
            secret: secret.trim().into(),
            from: "config".into(),
        });
    }
}

/// The app id the node's Qobuz sign-in is made with (`vault_cache`).
pub(crate) fn app_id(s: &NodeState) -> Option<String> {
    app_of(&s.library.lock().expect("library poisoned")).map(|a| a.id)
}

/// The app the owner set: in Settings, else node.toml, else `PM_QOBUZ_APP_ID` / `_SECRET`.
pub fn app_of(lib: &pm_library::Library) -> Option<App> {
    let kept: Option<App> = lib
        .get_setting(APP_KEY)
        .ok()
        .flatten()
        .and_then(|v| serde_json::from_str(&v).ok());
    if let Some(a) = kept.filter(|a| !a.id.is_empty()) {
        return Some(a);
    }
    if let Some(a) = config_app().get() {
        return Some(a.clone());
    }
    let id = std::env::var("PM_QOBUZ_APP_ID").unwrap_or_default();
    let secret = std::env::var("PM_QOBUZ_APP_SECRET").unwrap_or_default();
    (!id.trim().is_empty()).then(|| App {
        id: id.trim().into(),
        secret: secret.trim().into(),
        from: "config".into(),
    })
}

/// On, signed in and with an app: Qobuz searches and plays.
pub fn streams(s: &NodeState) -> bool {
    let lib = s.library.lock().expect("library poisoned");
    let st = load(&lib);
    st.enabled && st.session.is_some() && app_of(&lib).is_some()
}

/// The app and the session token, when the source can answer.
fn ready(s: &NodeState) -> Result<(App, Session), ApiError> {
    let lib = s.library.lock().expect("library poisoned");
    let st = load(&lib);
    let app = app_of(&lib).ok_or_else(|| {
        ApiError::new(
            StatusCode::PRECONDITION_FAILED,
            "Enter a Qobuz app ID and secret first (Sources › Qobuz)",
        )
    })?;
    if !st.enabled {
        return Err(ApiError::new(
            StatusCode::PRECONDITION_FAILED,
            "Qobuz is off: switch it on under Sources › Qobuz",
        ));
    }
    let sess = st.session.ok_or_else(|| {
        ApiError::new(
            StatusCode::PRECONDITION_FAILED,
            "Sign in to Qobuz first (Sources › Qobuz)",
        )
    })?;
    Ok((app, sess))
}

// ---- API -----------------------------------------------------------------------------------------

async fn get_json(s: &NodeState, path: &str, query: &[(&str, String)]) -> Result<Value, ApiError> {
    let (app, sess) = ready(s)?;
    let resp = s
        .http_client()
        .get(format!("{}/{path}", api_base()))
        .header("X-App-Id", &app.id)
        .header("X-User-Auth-Token", &sess.token)
        .query(query)
        .send()
        .await
        .map_err(|e| upstream(e.without_url()))?;
    let status = resp.status();
    let v: Value = resp.json().await.unwrap_or(Value::Null);
    match status.as_u16() {
        200..=299 => Ok(v),
        401 => {
            let why = format!("Qobuz ended the session: {}", message(&v));
            mark_down(s, &why);
            Err(ApiError::new(StatusCode::BAD_GATEWAY, why))
        }
        404 => Err(ApiError::not_found("not on Qobuz")),
        _ => Err(upstream(format!("{path} answered {status}: {}", message(&v)))),
    }
}

fn message(v: &Value) -> String {
    v["message"]
        .as_str()
        .or(v["error"].as_str())
        .unwrap_or("")
        .to_string()
}

/// A failure of the mode itself: switch it off and say why.
fn mark_down(s: &NodeState, why: &str) {
    tracing::warn!("Qobuz (unofficial) switched off: {why}");
    update(s, |st| {
        st.enabled = false;
        st.health = Some(Health {
            ok: false,
            detail: why.into(),
            at: now(),
        });
    });
}

fn image_of(v: &Value) -> Option<String> {
    v["image"]["large"]
        .as_str()
        .or(v["image"]["small"].as_str())
        .or(v["image"]["thumbnail"].as_str())
        .map(str::to_string)
}

fn year_of(v: &Value) -> Option<i32> {
    v["release_date_original"]
        .as_str()
        .and_then(|d| d.get(..4))
        .and_then(|y| y.parse().ok())
        .or_else(|| {
            v["released_at"].as_i64().map(|t| {
                // Unix seconds to a year, near enough for a label
                (1970 + t / 31_556_952) as i32
            })
        })
}

fn artist_of(v: &Value) -> Option<String> {
    v["performer"]["name"]
        .as_str()
        .or(v["artist"]["name"].as_str())
        .or(v["album"]["artist"]["name"].as_str())
        .map(str::to_string)
}

fn format_of(v: &Value) -> String {
    let bits = v["maximum_bit_depth"].as_u64().unwrap_or(16);
    let rate = v["maximum_sampling_rate"].as_f64().unwrap_or(44.1);
    if bits > 16 {
        format!("FLAC {bits}-bit/{rate} kHz (Qobuz)")
    } else {
        "FLAC (Qobuz)".into()
    }
}

fn track_hit(t: &Value) -> Option<SourceHit> {
    let id = t["id"].as_u64()?.to_string();
    Some(SourceHit {
        id: format!("track:{id}"),
        title: t["title"].as_str()?.to_string(),
        creator: artist_of(t),
        year: year_of(&t["album"]),
        downloads: None,
        image: image_of(&t["album"]),
        kind: "track".into(),
        duration_ms: t["duration"].as_u64().map(|d| d * 1000),
        library_artist_id: None,
        isrc: t["isrc"].as_str().map(str::to_string),
        upc: None,
        library_track_id: None,
        library_album_id: None,
        uri: Some(track_uri(&id)),
        url: page_url("track", &id),
    })
}

fn album_hit(a: &Value) -> Option<SourceHit> {
    let id = a["id"]
        .as_str()
        .map(str::to_string)
        .or_else(|| a["id"].as_u64().map(|n| n.to_string()))?;
    Some(SourceHit {
        id: format!("album:{id}"),
        title: a["title"].as_str()?.to_string(),
        creator: artist_of(a),
        year: year_of(a),
        downloads: None,
        image: image_of(a),
        kind: "item".into(),
        duration_ms: None,
        library_artist_id: None,
        isrc: None,
        upc: a["upc"].as_str().map(str::to_string),
        library_track_id: None,
        library_album_id: None,
        uri: None,
        url: page_url("album", &id),
    })
}

fn artist_hit(a: &Value) -> Option<SourceHit> {
    let id = a["id"].as_u64()?.to_string();
    Some(SourceHit {
        id: format!("artist:{id}"),
        title: a["name"].as_str()?.to_string(),
        creator: None,
        year: None,
        downloads: None,
        image: image_of(a).or_else(|| a["picture"].as_str().map(str::to_string)),
        kind: "artist".into(),
        duration_ms: None,
        library_artist_id: None,
        isrc: None,
        upc: None,
        library_track_id: None,
        library_album_id: None,
        uri: None,
        url: page_url("artist", &id),
    })
}

fn source_track(t: &Value, album: &Value) -> Option<SourceTrack> {
    let id = t["id"].as_u64()?.to_string();
    Some(SourceTrack {
        title: t["title"].as_str()?.to_string(),
        track_no: t["track_number"].as_u64().map(|n| n as u32),
        duration_ms: t["duration"].as_u64().map(|d| d * 1000),
        format: format_of(if t["maximum_bit_depth"].is_null() {
            album
        } else {
            t
        }),
        url: track_uri(&id),
        artist: artist_of(t),
    })
}

pub struct Qobuz;

#[async_trait::async_trait]
impl StreamingService for Qobuz {
    fn id(&self) -> &'static str {
        SOURCE_ID
    }

    fn unified(&self, s: &NodeState) -> bool {
        streams(s)
    }

    fn features(&self) -> &'static [&'static str] {
        &["search", "play", "artists"]
    }

    fn stage(&self) -> &'static str {
        "unofficial"
    }

    fn info(&self, s: &NodeState) -> SourceInfo {
        let lib = s.library.lock().expect("library poisoned");
        let st = load(&lib);
        let status = if app_of(&lib).is_none() {
            "not configured: enter a Qobuz app ID and secret under Sources › Qobuz".to_string()
        } else if let Some(h) = st.health.as_ref().filter(|h| !h.ok && !st.enabled) {
            format!("error: {}", h.detail)
        } else if !st.enabled {
            "off: switch Qobuz (unofficial) on under Sources › Qobuz".to_string()
        } else if st.session.is_none() {
            "needs sign-in".to_string()
        } else {
            "signed in".to_string()
        };
        SourceInfo {
            id: SOURCE_ID.into(),
            name: "Qobuz".into(),
            description: "Unofficial: search, albums and full tracks in FLAC up to 24-bit/192 kHz with your Qobuz plan, through Qobuz's private API".into(),
            status,
        }
    }

    async fn search(&self, s: &NodeState, term: &str, limit: u32) -> Result<Vec<SourceHit>, ApiError> {
        let limit = limit.clamp(1, 50);
        let v = get_json(
            s,
            "catalog/search",
            &[("query", term.to_string()), ("limit", limit.to_string())],
        )
        .await?;
        let needle = term.trim().to_lowercase();
        let mut out: Vec<SourceHit> = v["artists"]["items"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(artist_hit)
            .filter(|a| a.title.to_lowercase() == needle)
            .take(1)
            .collect();
        out.extend(
            v["albums"]["items"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(album_hit),
        );
        out.extend(
            v["tracks"]["items"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(track_hit),
        );
        out.truncate(limit as usize * 2);
        Ok(out)
    }

    async fn item(&self, s: &NodeState, id: &str) -> Result<SourceItem, ApiError> {
        if let Some(aid) = id.strip_prefix("album:") {
            let a = get_json(s, "album/get", &[("album_id", album_id(aid)?.to_string())]).await?;
            let tracks = a["tracks"]["items"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|t| t["streamable"].as_bool() != Some(false))
                .filter_map(|t| source_track(t, &a))
                .collect();
            return Ok(SourceItem {
                source: SOURCE_ID.into(),
                id: id.into(),
                title: a["title"].as_str().unwrap_or_default().to_string(),
                creator: artist_of(&a),
                year: year_of(&a),
                description: a["label"]["name"].as_str().map(|l| format!("Label: {l}")),
                licence: None,
                image: image_of(&a),
                url: page_url("album", aid),
                tracks,
                library_album_id: None,
            });
        }
        if let Some(tid) = id.strip_prefix("track:") {
            let t = get_json(s, "track/get", &[("track_id", numeric(tid)?.to_string())]).await?;
            let album = t["album"].clone();
            return Ok(SourceItem {
                source: SOURCE_ID.into(),
                id: id.into(),
                title: t["title"].as_str().unwrap_or_default().to_string(),
                creator: artist_of(&t),
                year: year_of(&album),
                description: album["title"].as_str().map(|a| format!("From {a}")),
                licence: None,
                image: image_of(&album),
                url: page_url("track", tid),
                tracks: source_track(&t, &album).into_iter().collect(),
                library_album_id: None,
            });
        }
        Err(ApiError::bad_request("a Qobuz item is album:<id> or track:<id>"))
    }

    async fn artist(&self, s: &NodeState, id: &str) -> Result<SourceArtist, ApiError> {
        let aid = numeric(id.strip_prefix("artist:").unwrap_or(id))?.to_string();
        let a = get_json(
            s,
            "artist/get",
            &[
                ("artist_id", aid.clone()),
                ("extra", "albums".into()),
                ("limit", "50".into()),
            ],
        )
        .await?;
        Ok(SourceArtist {
            source: SOURCE_ID.into(),
            id: format!("artist:{aid}"),
            name: a["name"].as_str().unwrap_or_default().to_string(),
            image: image_of(&a),
            url: page_url("artist", &aid),
            albums: a["albums"]["items"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(album_hit)
                .collect(),
            library_artist_id: None,
        })
    }
}

// ---- status, settings, sign-in ---------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QobuzStatus {
    pub enabled: bool,
    pub signed_in: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    pub hires: bool,
    pub quality: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub health: Option<Health>,
    /// Where the app comes from: `settings` or `config`; absent = none yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_from: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_id: Option<String>,
    pub warning: String,
}

fn view(s: &NodeState) -> QobuzStatus {
    let lib = s.library.lock().expect("library poisoned");
    let st = load(&lib);
    let app = app_of(&lib);
    QobuzStatus {
        enabled: st.enabled,
        signed_in: st.session.is_some(),
        user: st
            .session
            .as_ref()
            .and_then(|x| x.name.clone().or(x.user_id.clone())),
        plan: st.session.as_ref().and_then(|x| x.plan.clone()),
        hires: st.session.as_ref().is_some_and(|x| x.hires),
        quality: if st.quality.is_empty() {
            "max".into()
        } else {
            st.quality.clone()
        },
        health: st.health.clone(),
        app_from: app.as_ref().map(|a| a.from.clone()),
        app_id: app.map(|a| a.id),
        warning: WARNING.into(),
    }
}

/// `GET /v1/sources/qobuz/unofficial`.
pub(crate) async fn get_status(State(s): State<Arc<NodeState>>) -> ApiResult<QobuzStatus> {
    Ok(Json(view(&s)))
}

#[derive(Debug, Deserialize)]
pub struct QobuzUpdate {
    pub enabled: Option<bool>,
    pub quality: Option<String>,
    /// Replace the app (both; an empty id forgets it and goes back to node.toml's).
    pub app_id: Option<String>,
    pub app_secret: Option<String>,
}

/// `PUT /v1/sources/qobuz/unofficial {enabled?, quality?, app_id?, app_secret?}`.
pub(crate) async fn put(
    State(s): State<Arc<NodeState>>,
    Json(u): Json<QobuzUpdate>,
) -> ApiResult<QobuzStatus> {
    if let Some(q) = &u.quality {
        if q != "max" && q != "cd" {
            return Err(ApiError::bad_request("quality is max or cd"));
        }
    }
    if let Some(id) = &u.app_id {
        let id = id.trim();
        let secret = u.app_secret.as_deref().unwrap_or_default().trim();
        let lib = s.library.lock().expect("library poisoned");
        if id.is_empty() {
            lib.delete_setting(APP_KEY)?;
        } else {
            if !id.bytes().all(|b| b.is_ascii_digit()) || id.len() > 20 {
                return Err(ApiError::bad_request("A Qobuz app ID is digits only"));
            }
            if secret.is_empty() || secret.len() > 128 || !secret.bytes().all(|b| b.is_ascii_alphanumeric()) {
                return Err(ApiError::bad_request(
                    "The Qobuz app secret is letters and digits",
                ));
            }
            let a = App {
                id: id.into(),
                secret: secret.into(),
                from: "settings".into(),
            };
            lib.set_setting(APP_KEY, &serde_json::to_string(&a).expect("app serializes"))?;
        }
    }
    update(&s, |st| {
        if let Some(e) = u.enabled {
            st.enabled = e;
            if e {
                // Switching it on again is the Retry
                st.health = None;
            }
        }
        if let Some(q) = u.quality {
            st.quality = q;
        }
    });
    crate::search::forget_answers();
    Ok(Json(view(&s)))
}

#[derive(Deserialize)]
pub struct Login {
    pub email: String,
    pub password: String,
}

/// `POST /v1/sources/qobuz/unofficial/login {email, password}`: signs in once; the node keeps
/// only the session token. The password is sent to Qobuz as its MD5 (Qobuz's login takes that)
/// and never stored or logged.
pub(crate) async fn login(State(s): State<Arc<NodeState>>, Json(l): Json<Login>) -> ApiResult<QobuzStatus> {
    let app = app_of(&s.library.lock().expect("library poisoned")).ok_or_else(|| {
        ApiError::new(
            StatusCode::PRECONDITION_FAILED,
            "Enter a Qobuz app ID and secret first",
        )
    })?;
    if l.email.trim().is_empty() || l.password.is_empty() {
        return Err(ApiError::bad_request("Enter your Qobuz email and password"));
    }
    let hash = hex(&Md5::digest(l.password.as_bytes()));
    // Built before any await: the serializer is not Send
    let body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("email", l.email.trim())
        .append_pair("password", &hash)
        .append_pair("app_id", &app.id)
        .finish();
    let resp = s
        .http_client()
        .post(format!("{}/user/login", api_base()))
        .header("X-App-Id", &app.id)
        .header(reqwest::header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await
        .map_err(|e| upstream(e.without_url()))?;
    let status = resp.status();
    let v: Value = resp.json().await.unwrap_or(Value::Null);
    if status.as_u16() == 400 && message(&v).to_lowercase().contains("app") {
        return Err(upstream(format!("Qobuz refused the app ID: {}", message(&v))));
    }
    if !status.is_success() {
        return Err(ApiError::new(
            StatusCode::UNAUTHORIZED,
            format!("Qobuz refused the sign-in ({status}): {}", message(&v)),
        ));
    }
    let token = v["user_auth_token"]
        .as_str()
        .filter(|t| !t.is_empty())
        .ok_or_else(|| upstream("the sign-in answered without a session"))?;
    let user = &v["user"];
    let params = &user["credential"]["parameters"];
    if params.is_null() || params.as_object().is_some_and(|o| o.is_empty()) {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "This Qobuz account has no streaming plan: full tracks need a paid Qobuz plan",
        ));
    }
    let sess = Session {
        token: token.to_string(),
        user_id: user["id"].as_u64().map(|n| n.to_string()),
        name: user["display_name"]
            .as_str()
            .or(user["login"].as_str())
            .map(str::to_string),
        plan: user["credential"]["label"]
            .as_str()
            .or(user["credential"]["description"].as_str())
            .map(str::to_string),
        country: user["country_code"].as_str().map(str::to_string),
        hires: params["hires_streaming"].as_bool().unwrap_or(false),
    };
    update_signed_in(&s, "qobuz", |st| {
        st.session = Some(sess);
        st.health = None;
    });
    crate::search::forget_answers();
    tracing::info!("Qobuz sign-in stored");
    Ok(Json(view(&s)))
}

/// `DELETE /v1/sources/qobuz/unofficial/login`: forget the session (the mode stays as set).
pub(crate) async fn logout(State(s): State<Arc<NodeState>>) -> ApiResult<QobuzStatus> {
    update(&s, |st| st.session = None);
    crate::search::forget_answers();
    Ok(Json(view(&s)))
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

// ---- playback -------------------------------------------------------------------------------------

/// Whether a failure is the mode's (switch it off) or only this track's.
#[derive(Debug)]
pub enum StreamError {
    Mode(String),
    Track(String),
}

/// The signed `track/getFileUrl` request: Qobuz checks
/// `md5("trackgetFileUrlformat_id{f}intentstreamtrack_id{id}{ts}{secret}")`.
fn file_url_query(app: &App, id: &str, format_id: u32, ts: i64) -> Vec<(&'static str, String)> {
    let sig = hex(&Md5::digest(
        format!(
            "trackgetFileUrlformat_id{format_id}intentstreamtrack_id{id}{ts}{}",
            app.secret
        )
        .as_bytes(),
    ));
    vec![
        ("request_ts", ts.to_string()),
        ("request_sig", sig),
        ("track_id", id.to_string()),
        ("format_id", format_id.to_string()),
        ("intent", "stream".into()),
    ]
}

/// FLAC 24-bit up to 192 kHz (27) or 16-bit/44.1 kHz (6).
fn format_id(st: &ModeState) -> u32 {
    if st.quality == "cd" || st.session.as_ref().is_some_and(|x| !x.hires) {
        6
    } else {
        27
    }
}

/// What `track/getFileUrl` answered, judged: the stream URL, or why not.
fn judge_file_url(status: u16, v: &Value) -> Result<String, StreamError> {
    match status {
        200..=299 => {}
        400 if message(v).to_lowercase().contains("signature") => {
            return Err(StreamError::Mode(format!(
                "Qobuz refused the app secret: {}",
                message(v)
            )))
        }
        401 => {
            return Err(StreamError::Mode(format!(
                "Qobuz ended the session: {}",
                message(v)
            )))
        }
        403 => {
            return Err(StreamError::Track(format!(
                "Qobuz won't stream this track: {}",
                message(v)
            )))
        }
        404 => return Err(StreamError::Track("Qobuz has no stream for this track".into())),
        _ => {
            return Err(StreamError::Track(format!(
                "Qobuz answered {status}: {}",
                message(v)
            )))
        }
    }
    if v["sample"].as_bool() == Some(true) {
        return Err(StreamError::Track(
            "Qobuz sent a 30-second sample: the plan may not include this track".into(),
        ));
    }
    v["url"]
        .as_str()
        .filter(|u| u.starts_with("https://") || u.starts_with("http://"))
        .map(str::to_string)
        .ok_or_else(|| StreamError::Mode("Qobuz answered without a stream URL".into()))
}

/// The stream URL of a track (the audio thread).
pub fn file_url_blocking(
    library: &std::sync::Mutex<pm_library::Library>,
    id: &str,
) -> Result<String, StreamError> {
    let (app, st) = {
        let lib = library.lock().map_err(|_| StreamError::Track("library".into()))?;
        (app_of(&lib), load(&lib))
    };
    let app = app.ok_or_else(|| StreamError::Track("no Qobuz app on this node".into()))?;
    let sess = st
        .session
        .clone()
        .filter(|_| st.enabled)
        .ok_or_else(|| StreamError::Track("Qobuz is off or not signed in (Sources › Qobuz)".into()))?;
    let resp = crate::sources::http_client()
        .get(format!("{}/track/getFileUrl", api_base()))
        .header("X-App-Id", &app.id)
        .header("X-User-Auth-Token", &sess.token)
        .query(&file_url_query(&app, id, format_id(&st), now()))
        .send()
        .map_err(|e| StreamError::Track(format!("Qobuz: {}", e.without_url())))?;
    let status = resp.status().as_u16();
    let v: Value = resp.json().unwrap_or(Value::Null);
    judge_file_url(status, &v)
}

async fn file_url(s: &NodeState, id: &str) -> Result<String, StreamError> {
    let (app, sess) = ready(s).map_err(|e| StreamError::Track(e.message))?;
    let st = load(&s.library.lock().expect("library poisoned"));
    let resp = s
        .http_client()
        .get(format!("{}/track/getFileUrl", api_base()))
        .header("X-App-Id", &app.id)
        .header("X-User-Auth-Token", &sess.token)
        .query(&file_url_query(&app, id, format_id(&st), now()))
        .send()
        .await
        .map_err(|e| StreamError::Track(format!("Qobuz: {}", e.without_url())))?;
    let status = resp.status().as_u16();
    let v: Value = resp.json().await.unwrap_or(Value::Null);
    judge_file_url(status, &v)
}

/// Record how the source did: a failure of the mode itself switches it off.
pub fn record(library: &std::sync::Mutex<pm_library::Library>, r: &Result<String, StreamError>) {
    let Ok(lib) = library.lock() else { return };
    let mut st = load(&lib);
    match r {
        Ok(_) => {
            st.health = Some(Health {
                ok: true,
                detail: "full tracks are playing".into(),
                at: now(),
            });
        }
        Err(StreamError::Mode(e)) => {
            tracing::warn!("Qobuz (unofficial) switched off: {e}");
            st.enabled = false;
            st.health = Some(Health {
                ok: false,
                detail: e.clone(),
                at: now(),
            });
        }
        Err(StreamError::Track(_)) => return,
    }
    save(&lib, &st);
}

/// A `qobuz:track:<id>` for a zone.
pub(crate) fn open_track_blocking(
    library: &std::sync::Mutex<pm_library::Library>,
    id: &str,
) -> Result<Box<dyn pm_zones::FrameSource>, pm_zones::SourceError> {
    let fail = |reason: String| pm_zones::SourceError::Open {
        uri: track_uri(id),
        reason,
    };
    if id.is_empty() || !id.bytes().all(|b| b.is_ascii_digit()) {
        return Err(fail("bad Qobuz track id".into()));
    }
    let r = file_url_blocking(library, id);
    record(library, &r);
    let url = r.map_err(|e| match e {
        StreamError::Mode(m) | StreamError::Track(m) => fail(m),
    })?;
    crate::sources::open_http_stream(&url).map_err(|_| fail("the Qobuz stream did not open".into()))
}

/// `GET /v1/sources/qobuz/stream/{id}`: the track for browsers, relayed by the node so the
/// session and the app never reach the page.
pub(crate) async fn stream(
    State(s): State<Arc<NodeState>>,
    Path(id): Path<String>,
) -> Result<axum::response::Response, ApiError> {
    let id = numeric(&id)?.to_string();
    let r = file_url(&s, &id).await;
    record(&s.library, &r);
    let url = r.map_err(|e| match e {
        StreamError::Mode(m) => ApiError::new(StatusCode::BAD_GATEWAY, m),
        StreamError::Track(m) => ApiError::not_found(m),
    })?;
    let resp = s
        .http_client()
        .get(&url)
        .send()
        .await
        .map_err(|e| upstream(e.without_url()))?;
    if !resp.status().is_success() {
        return Err(upstream(format!("stream answered {}", resp.status())));
    }
    let len = resp.content_length();
    let mut out = axum::response::Response::builder()
        .header(axum::http::header::CONTENT_TYPE, "audio/flac")
        .header(axum::http::header::CACHE_CONTROL, "private, max-age=600");
    if let Some(n) = len {
        out = out.header(axum::http::header::CONTENT_LENGTH, n);
    }
    out.body(axum::body::Body::from_stream(resp.bytes_stream()))
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_file_url_request_is_signed_with_the_app_secret() {
        let app = App {
            id: "100000000".into(),
            secret: "abc123".into(),
            from: "settings".into(),
        };
        let q = file_url_query(&app, "5966783", 27, 1_700_000_000);
        let sig = &q.iter().find(|(k, _)| *k == "request_sig").unwrap().1;
        let want = hex(&Md5::digest(
            b"trackgetFileUrlformat_id27intentstreamtrack_id59667831700000000abc123",
        ));
        assert_eq!(sig, &want);
        assert!(format!("{app:?}").contains("<set>") && !format!("{app:?}").contains("abc123"));
    }

    #[test]
    fn a_sample_is_refused_and_a_bad_signature_turns_the_mode_off() {
        let ok = judge_file_url(
            200,
            &serde_json::json!({"url": "https://x/a.flac", "format_id": 27}),
        );
        assert_eq!(ok.unwrap(), "https://x/a.flac");
        assert!(matches!(
            judge_file_url(
                200,
                &serde_json::json!({"url": "https://x/a.mp3", "sample": true})
            ),
            Err(StreamError::Track(_))
        ));
        assert!(matches!(
            judge_file_url(
                400,
                &serde_json::json!({"message": "Invalid Request Signature parameter (request_sig)"})
            ),
            Err(StreamError::Mode(_))
        ));
        assert!(matches!(
            judge_file_url(404, &Value::Null),
            Err(StreamError::Track(_))
        ));
    }
}
