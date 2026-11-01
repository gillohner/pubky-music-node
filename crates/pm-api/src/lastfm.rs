//! Last.fm (docs/compare-music-assistant.md, "port these" #12): scrobbling and similar artists,
//! through Last.fm's official API with an **API key and shared secret the owner enters** (Settings ›
//! Accounts › Last.fm, or `[lastfm] api_key` / `api_secret`, `PM_LASTFM_API_KEY` / `_SECRET`). The
//! node ships no key; the secret never leaves the node, which signs every request itself.
//!
//! Sign-in is Last.fm's web flow: the browser authorises the app on last.fm and comes back to
//! `/lastfm/callback` (no API token there, like `/tidal/callback`); the node turns that token into
//! a session key (Last.fm gives them no expiry) and keeps it as a secret. A play counts the way the
//! ListenBrainz one does: half the track or four minutes, or its end; "now playing" goes out when
//! it starts. Scrobbles Last.fm can't take wait on the node and go with the next ones.

use crate::state::NodeState;
use crate::{ApiError, ApiResult};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Redirect, Response};
use axum::Json;
use md5::{Digest, Md5};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// The owner's app: `{api_key, api_secret, from}` (a secret in backups).
pub const SOURCE_ID: &str = "lastfm";
pub const APP_KEY: &str = "lastfm.app";
/// The signed-in user's session key and name, and whether plays go out (a secret in backups).
pub const SESSION_KEY: &str = "lastfm.session";
const PENDING_KEY: &str = "lastfm.pending";
const PENDING_MAX: usize = 1000;
pub const API_BASE: &str = "https://ws.audioscrobbler.com/2.0/";
const AUTH_URL: &str = "https://www.last.fm/api/auth/";
/// Last.fm takes up to 50 scrobbles per call.
const BATCH: usize = 50;
/// Last.fm counts tracks of 30 s and longer.
const MIN_LENGTH_MS: u64 = 30_000;

// ---- settings ----------------------------------------------------------------------------------

#[derive(Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct App {
    pub api_key: String,
    pub api_secret: String,
    /// `settings` or `config`.
    #[serde(default)]
    pub from: String,
}

impl std::fmt::Debug for App {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("App")
            .field("api_key", &self.api_key)
            .field(
                "api_secret",
                &if self.api_secret.is_empty() { "" } else { "<set>" },
            )
            .field("from", &self.from)
            .finish()
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Session {
    pub key: String,
    pub user: String,
    /// Send what plays here.
    #[serde(default = "yes")]
    pub submit: bool,
    /// The app this sign-in was imported from (`rescrobbled`, `pasted`); none for one made here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    /// Why the session stopped working, when it did (`error 9`: Last.fm asks to sign in again).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub problem: Option<String>,
}

fn yes() -> bool {
    true
}

fn config_app() -> &'static std::sync::OnceLock<App> {
    static C: std::sync::OnceLock<App> = std::sync::OnceLock::new();
    &C
}

/// The app from node.toml (`[lastfm] api_key` / `api_secret`), set once at start-up; Settings
/// override it.
pub fn set_config_app(key: &str, secret: &str) {
    if !key.trim().is_empty() && !secret.trim().is_empty() {
        let _ = config_app().set(App {
            api_key: key.trim().into(),
            api_secret: secret.trim().into(),
            from: "config".into(),
        });
    }
}

fn base_override() -> &'static std::sync::RwLock<Option<String>> {
    static B: std::sync::OnceLock<std::sync::RwLock<Option<String>>> = std::sync::OnceLock::new();
    B.get_or_init(|| std::sync::RwLock::new(None))
}

/// Tests: Last.fm stood in.
pub fn set_api_base(url: &str) {
    *base_override().write().expect("lastfm base") = Some(url.to_string());
}

#[cfg(test)]
pub(crate) static API_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn api_base() -> String {
    base_override()
        .read()
        .expect("lastfm base")
        .clone()
        .unwrap_or_else(|| API_BASE.to_string())
}

fn setting<T: serde::de::DeserializeOwned>(lib: &pm_library::Library, key: &str) -> Option<T> {
    lib.get_setting(key)
        .ok()
        .flatten()
        .and_then(|v| serde_json::from_str(&v).ok())
}

/// The API key the node's Last.fm session is made with (`vault_cache`).
pub(crate) fn app_id(s: &NodeState) -> Option<String> {
    app_of(&s.library.lock().expect("library poisoned")).map(|a| a.api_key)
}

/// The app the owner set: in Settings, else node.toml, else `PM_LASTFM_API_KEY` / `_SECRET`.
pub fn app_of(lib: &pm_library::Library) -> Option<App> {
    if let Some(a) = setting::<App>(lib, APP_KEY).filter(|a| !a.api_key.is_empty()) {
        return Some(a);
    }
    if let Some(a) = config_app().get() {
        return Some(a.clone());
    }
    let key = std::env::var("PM_LASTFM_API_KEY").unwrap_or_default();
    let secret = std::env::var("PM_LASTFM_API_SECRET").unwrap_or_default();
    (!key.trim().is_empty() && !secret.trim().is_empty()).then(|| App {
        api_key: key.trim().into(),
        api_secret: secret.trim().into(),
        from: "config".into(),
    })
}

fn session_of(lib: &pm_library::Library) -> Option<Session> {
    setting::<Session>(lib, SESSION_KEY).filter(|s| !s.key.is_empty())
}

fn save_session(lib: &pm_library::Library, sess: Option<&Session>) {
    match sess {
        Some(s) => {
            let _ = lib.set_setting(
                SESSION_KEY,
                &serde_json::to_string(s).expect("session serializes"),
            );
        }
        None => {
            let _ = lib.delete_setting(SESSION_KEY);
        }
    }
}

fn pending(lib: &pm_library::Library) -> Vec<Value> {
    setting::<Vec<Value>>(lib, PENDING_KEY).unwrap_or_default()
}

fn set_pending(lib: &pm_library::Library, v: &[Value]) {
    if v.is_empty() {
        let _ = lib.delete_setting(PENDING_KEY);
    } else {
        let _ = lib.set_setting(
            PENDING_KEY,
            &serde_json::to_string(v).expect("pending serializes"),
        );
    }
}

// ---- the API, signed -----------------------------------------------------------------------------

/// Last.fm's signature: every parameter but `format` and `callback`, sorted by name, name and
/// value run together, the shared secret appended, MD5 as hex.
pub fn sign(params: &[(String, String)], secret: &str) -> String {
    let mut sorted: Vec<&(String, String)> = params
        .iter()
        .filter(|(k, _)| k != "format" && k != "callback")
        .collect();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));
    let mut s = String::new();
    for (k, v) in sorted {
        s.push_str(k);
        s.push_str(v);
    }
    s.push_str(secret);
    Md5::digest(s.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// A call's error, kept apart: the session is gone (sign in again), the app is refused (key or
/// secret), or something passing.
#[derive(Debug)]
pub enum CallError {
    Session(String),
    App(String),
    Other(String),
}

impl std::fmt::Display for CallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CallError::Session(m) | CallError::App(m) | CallError::Other(m) => f.write_str(m),
        }
    }
}

/// Last.fm's `{"error": n, "message": …}`.
pub fn judge(v: &Value) -> Result<(), CallError> {
    let Some(code) = v["error"].as_u64() else {
        return Ok(());
    };
    let msg = v["message"].as_str().unwrap_or("").to_string();
    Err(match code {
        4 | 9 | 14 => CallError::Session(format!("Last.fm asks you to sign in again: {msg}")),
        10 | 13 | 26 => CallError::App(format!("Last.fm refused the API key or secret: {msg}")),
        _ => CallError::Other(format!("Last.fm answered {code}: {msg}")),
    })
}

/// `POST` a signed method (`sk` when a session is along).
async fn call(
    s: &NodeState,
    app: &App,
    method: &str,
    sk: Option<&str>,
    mut params: Vec<(String, String)>,
) -> Result<Value, CallError> {
    params.push(("method".into(), method.into()));
    params.push(("api_key".into(), app.api_key.clone()));
    if let Some(k) = sk {
        params.push(("sk".into(), k.into()));
    }
    let sig = sign(&params, &app.api_secret);
    params.push(("api_sig".into(), sig));
    params.push(("format".into(), "json".into()));
    let body = {
        // Built before any await: the serializer isn't Send
        let mut ser = url::form_urlencoded::Serializer::new(String::new());
        for (k, v) in &params {
            ser.append_pair(k, v);
        }
        ser.finish()
    };
    let resp = s
        .http_client()
        .post(api_base())
        .header(reqwest::header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await
        .map_err(|e| CallError::Other(format!("Last.fm: {}", e.without_url())))?;
    let v: Value = resp
        .json()
        .await
        .map_err(|e| CallError::Other(format!("Last.fm: {e}")))?;
    judge(&v)?;
    Ok(v)
}

/// `GET` a public method (no signature): similar artists and the like.
async fn get(s: &NodeState, app: &App, method: &str, params: &[(&str, String)]) -> Result<Value, CallError> {
    let resp = s
        .http_client()
        .get(api_base())
        .query(&[
            ("method", method),
            ("api_key", app.api_key.as_str()),
            ("format", "json"),
        ])
        .query(params)
        .send()
        .await
        .map_err(|e| CallError::Other(format!("Last.fm: {}", e.without_url())))?;
    let v: Value = resp
        .json()
        .await
        .map_err(|e| CallError::Other(format!("Last.fm: {e}")))?;
    judge(&v)?;
    Ok(v)
}

/// A session Last.fm no longer takes is marked, so the card says to sign in again.
fn note_problem(s: &NodeState, e: &CallError) {
    if let CallError::Session(m) = e {
        let lib = s.library.lock().expect("library poisoned");
        if let Some(mut sess) = session_of(&lib) {
            sess.problem = Some(m.clone());
            save_session(&lib, Some(&sess));
        }
    }
}

// ---- scrobbles ---------------------------------------------------------------------------------

/// What a scrobble says about a queue item, as Last.fm's parameters (without the index).
fn scrobble_of(s: &NodeState, item: &pm_zones::QueueItem) -> Option<HashMap<String, String>> {
    let mut p: HashMap<String, String> = HashMap::new();
    let mut artist = item.artist.clone().filter(|a| !a.trim().is_empty());
    let mut title = item.title.clone().filter(|t| !t.trim().is_empty());
    let mut album = item.album.clone().filter(|a| !a.trim().is_empty());
    let mut duration = item.duration_ms;
    if let Some(id) = item
        .uri
        .strip_prefix(crate::sources::TRACK_SCHEME)
        .and_then(|i| i.parse::<i64>().ok())
    {
        let t = s
            .library
            .lock()
            .expect("library poisoned")
            .get_track(pm_model::TrackId(id))
            .ok()??;
        title = Some(t.title.clone());
        let names: Vec<&str> = t.artists.iter().map(|a| a.name.as_str()).collect();
        if !names.is_empty() {
            artist = Some(names.join(", "));
        }
        if let Some(a) = &t.album {
            album = Some(a.title.clone());
            if let Some(aa) = a.album_artist.as_deref().filter(|x| !x.is_empty()) {
                if Some(aa) != artist.as_deref() {
                    p.insert("albumArtist".into(), aa.to_string());
                }
            }
        }
        if let Some(n) = t.track_no {
            p.insert("trackNumber".into(), n.to_string());
        }
        if let Some(m) = &t.recording_mbid {
            p.insert("mbid".into(), m.to_string());
        }
        duration = t.duration_ms.or(duration);
    }
    if duration.is_some_and(|d| d < MIN_LENGTH_MS) {
        return None;
    }
    p.insert("artist".into(), artist?);
    p.insert("track".into(), title?);
    if let Some(a) = album {
        p.insert("album".into(), a);
    }
    if let Some(d) = duration {
        p.insert("duration".into(), (d / 1000).to_string());
    }
    p.insert("chosenByUser".into(), "1".into());
    Some(p)
}

type PlayKey = (String, String, u64);

/// Plays under way: the scrobble to send once the play counts, and whether it did.
fn playing() -> &'static Mutex<HashMap<PlayKey, (Value, bool)>> {
    static P: std::sync::OnceLock<Mutex<HashMap<PlayKey, (Value, bool)>>> = std::sync::OnceLock::new();
    P.get_or_init(Default::default)
}

fn ready(s: &NodeState) -> Option<(App, Session)> {
    let lib = s.library.lock().expect("library poisoned");
    let app = app_of(&lib)?;
    let sess = session_of(&lib).filter(|x| x.submit && x.problem.is_none())?;
    Some((app, sess))
}

/// A zone started `item`: tell Last.fm what plays now, and count the play once it has run half
/// the track or four minutes (its end counts too).
pub(crate) fn track_started(s: &Arc<NodeState>, zone: Arc<pm_zones::Zone>, item: pm_zones::QueueItem) {
    let Some((app, sess)) = ready(s) else { return };
    let Some(params) = scrobble_of(s, &item) else {
        return;
    };
    let started = pm_library::db::now_unix();
    let mut scrobble = json!(params);
    scrobble["timestamp"] = json!(started.to_string());
    let key: PlayKey = (s.node_id.clone(), zone.id().0.clone(), item.id);
    playing()
        .lock()
        .expect("lastfm playing")
        .insert(key.clone(), (scrobble, false));
    let s = s.clone();
    tokio::spawn(async move {
        let now_playing: Vec<(String, String)> = params
            .iter()
            .filter(|(k, _)| *k != "chosenByUser")
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        if let Err(e) = call(&s, &app, "track.updateNowPlaying", Some(&sess.key), now_playing).await {
            tracing::debug!("lastfm now playing: {e}");
            note_problem(&s, &e);
        }
        let need_ms = item.duration_ms.map_or(240_000, |d| (d / 2).min(240_000));
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            let st = zone.state();
            match st
                .now_playing
                .filter(|np| np.item.id == item.id && np.item.uri == item.uri)
            {
                Some(np) if np.position_ms >= need_ms => {
                    count_once(&s, &key).await;
                    break;
                }
                Some(_) => {}
                None => break,
            }
        }
        playing().lock().expect("lastfm playing").remove(&key);
    });
}

/// A zone played `item` to its end: that counts.
pub(crate) fn track_ended(s: &Arc<NodeState>, zone: &pm_zones::ZoneId, item: &pm_zones::QueueItem) {
    let key: PlayKey = (s.node_id.clone(), zone.0.clone(), item.id);
    let s = s.clone();
    tokio::spawn(async move { count_once(&s, &key).await });
}

async fn count_once(s: &Arc<NodeState>, key: &PlayKey) {
    let scrobble = {
        let mut p = playing().lock().expect("lastfm playing");
        match p.get_mut(key) {
            Some((v, counted)) if !*counted => {
                *counted = true;
                v.clone()
            }
            _ => return,
        }
    };
    send_scrobbles(s, vec![scrobble]).await;
}

/// Send scrobbles with any that waited; keep them all when Last.fm can't take them.
async fn send_scrobbles(s: &NodeState, new: Vec<Value>) {
    let Some((app, sess)) = ready(s) else {
        // Not connected (any more): keep them for when the owner signs in again
        let lib = s.library.lock().expect("library poisoned");
        let mut all = pending(&lib);
        all.extend(new);
        let skip = all.len().saturating_sub(PENDING_MAX);
        set_pending(&lib, &all[skip..]);
        return;
    };
    let mut all = pending(&s.library.lock().expect("library poisoned"));
    all.extend(new);
    let mut left: Vec<Value> = Vec::new();
    for chunk in all.chunks(BATCH) {
        let mut params: Vec<(String, String)> = Vec::new();
        for (i, sc) in chunk.iter().enumerate() {
            if let Some(m) = sc.as_object() {
                for (k, v) in m {
                    if let Some(v) = v.as_str() {
                        params.push((format!("{k}[{i}]"), v.to_string()));
                    }
                }
            }
        }
        match call(s, &app, "track.scrobble", Some(&sess.key), params).await {
            Ok(v) => {
                let ignored = v["scrobbles"]["@attr"]["ignored"].as_u64().unwrap_or(0);
                if ignored > 0 {
                    tracing::debug!(ignored, "lastfm ignored some scrobbles");
                }
            }
            Err(e) => {
                tracing::debug!("lastfm: {e}; keeping {} scrobbles for later", chunk.len());
                note_problem(s, &e);
                left.extend_from_slice(chunk);
            }
        }
    }
    let skip = left.len().saturating_sub(PENDING_MAX);
    set_pending(&s.library.lock().expect("library poisoned"), &left[skip..]);
}

// ---- similar artists ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct SimilarArtist {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mbid: Option<String>,
    pub url: String,
    /// Last.fm's similarity, 0 to 1.
    pub score: f64,
    /// The same artist in this library, when it is there.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub library_artist_id: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct Similar {
    pub artists: Vec<SimilarArtist>,
    /// Why there is nothing: `not configured`, `no artist`, or Last.fm's words.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// Last.fm's `artist.getSimilar` answer as rows, matched to the library by MusicBrainz id, else
/// by name.
pub fn similar_from(v: &Value, lib: &pm_library::Library) -> Vec<SimilarArtist> {
    let by_name: HashMap<String, i64> = lib
        .list_artists()
        .unwrap_or_default()
        .into_iter()
        .map(|a| (a.name.to_lowercase(), a.id))
        .collect();
    v["similarartists"]["artist"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|a| {
            let name = a["name"].as_str()?.to_string();
            let mbid = a["mbid"].as_str().filter(|m| !m.is_empty()).map(str::to_string);
            let in_library = mbid
                .as_deref()
                .and_then(|m| m.parse::<pm_model::Mbid>().ok())
                .and_then(|m| lib.artist_by_mbid(m).ok().flatten())
                .map(|a| a.id)
                .or_else(|| by_name.get(&name.to_lowercase()).copied());
            Some(SimilarArtist {
                url: a["url"].as_str().unwrap_or("").to_string(),
                score: a["match"]
                    .as_str()
                    .and_then(|m| m.parse().ok())
                    .or_else(|| a["match"].as_f64())
                    .unwrap_or(0.0),
                library_artist_id: in_library,
                name,
                mbid,
            })
        })
        .collect()
}

/// `GET /v1/library/artists/{id}/similar`: artists like this one, from Last.fm (by MusicBrainz id
/// when the artist has one, else by name), with the ones in this library marked.
pub(crate) async fn similar(State(s): State<Arc<NodeState>>, Path(id): Path<i64>) -> ApiResult<Similar> {
    let (app, artist) = {
        let lib = s.library.lock().expect("library poisoned");
        (app_of(&lib), lib.get_artist(id)?)
    };
    let Some(app) = app else {
        return Ok(Json(Similar {
            note: Some("not configured".into()),
            ..Default::default()
        }));
    };
    let Some(artist) = artist else {
        return Err(ApiError::not_found(format!("no artist {id}")));
    };
    let mut params = vec![("limit", "12".to_string()), ("autocorrect", "1".to_string())];
    match &artist.mbid {
        Some(m) => params.push(("mbid", m.to_string())),
        None => params.push(("artist", artist.name.clone())),
    }
    match get(&s, &app, "artist.getSimilar", &params).await {
        Ok(v) => {
            let lib = s.library.lock().expect("library poisoned");
            Ok(Json(Similar {
                artists: similar_from(&v, &lib),
                note: None,
            }))
        }
        Err(e) => Ok(Json(Similar {
            note: Some(e.to_string()),
            ..Default::default()
        })),
    }
}

// ---- the card ----------------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct LastfmView {
    pub configured: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub app_from: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    pub connected: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    pub submit: bool,
    pub pending: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub problem: Option<String>,
}

pub(crate) fn view(s: &NodeState) -> LastfmView {
    let lib = s.library.lock().expect("library poisoned");
    let app = app_of(&lib);
    let sess = session_of(&lib);
    LastfmView {
        configured: app.is_some(),
        app_from: app.as_ref().map(|a| a.from.clone()),
        api_key: app.map(|a| a.api_key),
        connected: sess.is_some(),
        user: sess.as_ref().map(|x| x.user.clone()),
        submit: sess.as_ref().is_some_and(|x| x.submit),
        pending: pending(&lib).len(),
        problem: sess.and_then(|x| x.problem),
    }
}

/// `GET /v1/lastfm`.
pub(crate) async fn status(State(s): State<Arc<NodeState>>) -> ApiResult<LastfmView> {
    Ok(Json(view(&s)))
}

#[derive(Debug, Deserialize)]
pub struct LastfmUpdate {
    /// The owner's app, both at once; an empty key forgets it.
    pub api_key: Option<String>,
    pub api_secret: Option<String>,
    pub submit: Option<bool>,
}

/// `PUT /v1/lastfm {api_key?, api_secret?, submit?}`.
pub(crate) async fn update(
    State(s): State<Arc<NodeState>>,
    Json(u): Json<LastfmUpdate>,
) -> ApiResult<LastfmView> {
    let lib = s.library.lock().expect("library poisoned");
    if let Some(k) = u.api_key.as_deref() {
        let k = k.trim();
        let secret = u.api_secret.as_deref().unwrap_or_default().trim();
        if k.is_empty() {
            lib.delete_setting(APP_KEY)?;
            // The session belonged to that app
            save_session(&lib, None);
        } else {
            if k.len() != 32 || !k.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err(ApiError::bad_request(
                    "A Last.fm API key is 32 hexadecimal characters",
                ));
            }
            if secret.len() != 32 || !secret.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err(ApiError::bad_request(
                    "The shared secret is 32 hexadecimal characters",
                ));
            }
            let a = App {
                api_key: k.into(),
                api_secret: secret.into(),
                from: "settings".into(),
            };
            lib.set_setting(APP_KEY, &serde_json::to_string(&a).expect("app serializes"))?;
        }
    }
    if let Some(sub) = u.submit {
        if let Some(mut sess) = session_of(&lib) {
            sess.submit = sub;
            save_session(&lib, Some(&sess));
        }
    }
    drop(lib);
    Ok(Json(view(&s)))
}

/// `DELETE /v1/lastfm`: sign out (the app stays).
pub(crate) async fn disconnect(State(s): State<Arc<NodeState>>) -> ApiResult<LastfmView> {
    let lib = s.library.lock().expect("library poisoned");
    save_session(&lib, None);
    set_pending(&lib, &[]);
    drop(lib);
    Ok(Json(view(&s)))
}

#[derive(Debug, Serialize)]
pub struct LoginUrl {
    pub url: String,
}

/// `GET /v1/lastfm/login`: Last.fm's authorisation page, coming back to this node's
/// `/lastfm/callback`.
pub(crate) async fn login(
    State(s): State<Arc<NodeState>>,
    headers: axum::http::HeaderMap,
) -> ApiResult<LoginUrl> {
    let app = app_of(&s.library.lock().expect("library poisoned")).ok_or_else(|| {
        ApiError::new(
            StatusCode::PRECONDITION_FAILED,
            "Enter your Last.fm API key and shared secret first (Settings › Accounts)",
        )
    })?;
    let cb = format!("{}/lastfm/callback", crate::tidal::external_base(&s, &headers));
    let url = reqwest::Url::parse_with_params(
        AUTH_URL,
        &[("api_key", app.api_key.as_str()), ("cb", cb.as_str())],
    )
    .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(LoginUrl { url: url.to_string() }))
}

#[derive(Debug, Deserialize)]
pub struct CallbackQuery {
    pub token: Option<String>,
}

/// `GET /lastfm/callback?token=…` (no API token): the browser comes back from last.fm; the node
/// turns the token into a session and keeps it.
pub(crate) async fn callback(State(s): State<Arc<NodeState>>, Query(q): Query<CallbackQuery>) -> Response {
    match finish_login(&s, q.token.as_deref().unwrap_or("")).await {
        Ok(user) => Redirect::to(&format!("/?lastfm=signed_in&user={}", urlencode(&user))).into_response(),
        Err(e) => Redirect::to(&format!("/?lastfm=error&detail={}", urlencode(&e))).into_response(),
    }
}

fn urlencode(v: &str) -> String {
    let mut out = String::new();
    for b in v.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

async fn finish_login(s: &NodeState, token: &str) -> Result<String, String> {
    if token.is_empty() || token.len() > 128 || !token.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return Err("Last.fm sent no token back".into());
    }
    let app = app_of(&s.library.lock().expect("library poisoned")).ok_or("no Last.fm app on this node")?;
    let v = call(
        s,
        &app,
        "auth.getSession",
        None,
        vec![("token".into(), token.into())],
    )
    .await
    .map_err(|e| e.to_string())?;
    let key = v["session"]["key"]
        .as_str()
        .ok_or("Last.fm answered without a session")?;
    let user = v["session"]["name"].as_str().unwrap_or("").to_string();
    let lib = s.library.lock().expect("library poisoned");
    save_session(
        &lib,
        Some(&Session {
            key: key.into(),
            user: user.clone(),
            submit: true,
            problem: None,
            from: None,
        }),
    );
    crate::vault_cache::new_sign_in(&lib, "lastfm");
    Ok(user)
}

// ---- one Sign in, the ways side by side (STEWARD-CREDENTIALS-REUSE.md step 3) ----

pub(crate) const WAY_OWN: &str = "own";
pub(crate) const WAY_IMPORT: &str = "import";
pub(crate) const WAY_PASTE: &str = "paste";

/// Last.fm's ways, for `crate::signin` (Settings › Accounts).
pub(crate) struct LastfmSignIn;

#[async_trait::async_trait]
impl crate::signin::SignInSource for LastfmSignIn {
    /// The node's one sign-in, the owner's (the scrobbles and the API key are the node's).
    fn per_person(&self) -> bool {
        false
    }

    fn describe(
        &self,
        s: &NodeState,
        _who: &crate::accounts::Who,
        headers: Option<&axum::http::HeaderMap>,
    ) -> crate::signin::SignIn {
        use crate::signin::{InUse, SignIn, Way, NOT_SIGNED_IN};
        let (app, sess, chosen) = {
            let lib = s.library.lock().expect("library poisoned");
            (
                app_of(&lib),
                session_of(&lib),
                crate::signin::chosen(&lib, &crate::accounts::Who::Owner, SOURCE_ID),
            )
        };
        let scanned = if headers.is_some() {
            crate::imports::scan_lastfm()
        } else {
            Vec::new()
        };
        let note = if scanned.is_empty() {
            "Nothing in the known places on this machine (rescrobbled). From another machine, paste its key, secret and session key below.".to_string()
        } else {
            format!(
                "Found: {}. rescrobbled's files are only read; its API key, secret and session are kept here like the node's own.",
                scanned.iter().map(|(f, _)| if f.usable { f.app.clone() } else { format!("{} (not usable)", f.app) }).collect::<Vec<_>>().join(" · ")
            )
        };
        let app_note = "Uses the imported API key and secret for this node, replacing its current Last.fm app. Other users signed in with the old app may need to sign in again.";
        let ways = vec![
            Way {
                id: WAY_OWN.into(),
                label: "Your own Last.fm API account".into(),
                note: format!(
                    "An API key and shared secret from last.fm/api/account/create{}, then Last.fm's permission page in the browser.",
                    match app.as_ref().map(|a| a.from.as_str()) {
                        Some("config") => " (set in the node config)",
                        Some(_) => " (set)",
                        None => "",
                    }
                ),
                action: "browser".into(),
                recommended: true,
                available: true,
                reason: None,
            },
            Way {
                id: WAY_IMPORT.into(),
                label: "A sign-in from another app on this machine".into(),
                note: format!("{note} {app_note}"),
                action: "import".into(),
                recommended: false,
                available: true,
                reason: None,
            },
            Way {
                id: WAY_PASTE.into(),
                label: "Paste a key, secret and session key".into(),
                note: format!("Three lines: the API key, its shared secret, and a session key made with that key (rescrobbled keeps one in ~/.config/rescrobbled/session). Nothing is shown again once saved. {app_note}"),
                action: "paste".into(),
                recommended: false,
                available: true,
                reason: None,
            },
        ];
        let in_use = sess.as_ref().map(|x| {
            let (way, label) = match x.from.as_deref() {
                Some("pasted") => (WAY_PASTE, "Pasted sign-in".to_string()),
                Some(app) => (WAY_IMPORT, format!("{app}'s sign-in")),
                None => (WAY_OWN, "Your own Last.fm app".to_string()),
            };
            InUse {
                way: way.into(),
                label,
                user: (!x.user.is_empty()).then(|| x.user.clone()),
                expires_at: None,
                gives: Some(
                    if x.submit {
                        "scrobbles and similar artists"
                    } else {
                        "similar artists; scrobbling off"
                    }
                    .into(),
                ),
            }
        });
        let line = match &in_use {
            None => NOT_SIGNED_IN.to_string(),
            Some(u) => format!(
                "{}{}",
                u.label,
                u.user.as_ref().map(|n| format!(" · {n}")).unwrap_or_default()
            ),
        };
        SignIn {
            source: SOURCE_ID.into(),
            ways,
            chosen,
            in_use,
            line,
            imports: scanned.into_iter().map(|(f, _)| f).collect(),
            places: if headers.is_some() {
                crate::imports::lastfm_places_text()
            } else {
                Vec::new()
            },
            details: if headers.is_some() {
                serde_json::json!({ "lastfm": view(s) })
            } else {
                serde_json::Value::Null
            },
        }
    }

    fn sign_out_of(&self, s: &NodeState, _who: &crate::accounts::Who, _way: &str) -> Result<(), ApiError> {
        let lib = s.library.lock().expect("library poisoned");
        save_session(&lib, None);
        set_pending(&lib, &[]);
        Ok(())
    }

    async fn import(
        &self,
        s: &Arc<NodeState>,
        _who: &crate::accounts::Who,
        body: crate::signin::ImportBody,
    ) -> Result<&'static str, ApiError> {
        let (imp, from, way) = match (body.text.as_deref().map(str::trim), body.id.as_deref()) {
            (Some(text), _) if !text.is_empty() => (
                crate::imports::parse_lastfm(text)
                    .map_err(|e| ApiError::bad_request(format!("Not usable: {e}")))?,
                "pasted".to_string(),
                WAY_PASTE,
            ),
            (_, Some(id)) => {
                let (f, imp) = crate::imports::scan_lastfm()
                    .into_iter()
                    .find(|(f, _)| f.id == id)
                    .ok_or_else(|| ApiError::not_found(format!("nothing found for {id} on this machine")))?;
                let Some(imp) = imp else {
                    return Err(ApiError::bad_request(format!(
                        "{}'s sign-in is not usable: {}",
                        f.app,
                        f.reason.unwrap_or_default()
                    )));
                };
                (imp, f.id, WAY_IMPORT)
            }
            _ => {
                return Err(ApiError::bad_request(
                    "Say which sign-in (id) or paste one (text)",
                ))
            }
        };
        let app = App {
            api_key: imp.api_key.clone(),
            api_secret: imp.api_secret.clone(),
            from: "import".into(),
        };
        // Check the candidate before changing the working app or session. Service failures are
        // not evidence that a sign-in works, and upstream errors may echo supplied secrets.
        let user = match call(s, &app, "user.getInfo", Some(&imp.session_key), Vec::new()).await {
            Ok(v) => v["user"]["name"]
                .as_str()
                .filter(|n| !n.is_empty())
                .ok_or_else(|| {
                    ApiError::bad_request(
                        "Not usable: Last.fm did not return an account. Try again or try another way",
                    )
                })?
                .to_string(),
            Err(CallError::Other(_)) => {
                return Err(ApiError::bad_request(
                    "Could not check the sign-in with Last.fm. Try again when it is reachable",
                ))
            }
            Err(_) => {
                return Err(ApiError::bad_request(
                    "Not usable: Last.fm did not accept it. Try another way",
                ))
            }
        };
        let lib = s.library.lock().expect("library poisoned");
        lib.set_setting(APP_KEY, &serde_json::to_string(&app).expect("app serializes"))?;
        save_session(
            &lib,
            Some(&Session {
                key: imp.session_key.clone(),
                user,
                submit: true,
                problem: None,
                from: Some(from),
            }),
        );
        crate::vault_cache::new_sign_in(&lib, "lastfm");
        set_pending(&lib, &[]);
        Ok(way)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signatures_follow_lastfms_rule() {
        // The example on last.fm/api/webauth: sorted names and values run together, then the secret
        let params = vec![
            ("token".to_string(), "xxxxxxx".to_string()),
            ("api_key".to_string(), "xxxxxxxx".to_string()),
            ("method".to_string(), "auth.getSession".to_string()),
            ("format".to_string(), "json".to_string()),
        ];
        let want: String = Md5::digest(b"api_keyxxxxxxxxmethodauth.getSessiontokenxxxxxxxmysecret")
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert_eq!(sign(&params, "mysecret"), want);
        assert!(matches!(
            judge(&json!({"error": 9, "message": "Invalid session key"})),
            Err(CallError::Session(_))
        ));
        assert!(matches!(
            judge(&json!({"error": 10, "message": "Invalid API key"})),
            Err(CallError::App(_))
        ));
        assert!(judge(&json!({"scrobbles": {}})).is_ok());
        let a = App {
            api_key: "k".into(),
            api_secret: "s3cret".into(),
            from: "settings".into(),
        };
        assert!(format!("{a:?}").contains("<set>") && !format!("{a:?}").contains("s3cret"));
        assert_eq!(urlencode("a b/ü"), "a%20b%2F%C3%BC");
    }
}
