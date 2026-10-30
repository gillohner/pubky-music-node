//! SoundCloud, unofficial mode (STEWARD-DUAL-MODE.md): SoundCloud's web client API (`api-v2`), with
//! a **client ID the owner enters** (the node ships none and reads none off soundcloud.com) and,
//! for the owner's likes and playlists, their own sign-in token. Off by default, behind a warning.
//! Search (tracks and sets, or a pasted soundcloud.com link), track and set pages, likes and
//! playlists with imports, and streams: progressive MP3 first, else HLS (MP3 or AAC) read as one
//! stream. Tracks SoundCloud encrypts (Go+) are skipped, never decrypted; a track SoundCloud only
//! snips plays as its 30-second preview and says so. A refused client ID switches the mode off and
//! tells the tab why, with Try again.

use crate::state::NodeState;
use crate::streaming::StreamingService;
use crate::types::*;
use crate::{ApiError, ApiResult};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;

pub const SOURCE_ID: &str = "soundcloud";
pub const TRACK_URI_PREFIX: &str = "soundcloud:track:";
pub const STATE_KEY: &str = "soundcloud.unofficial";
/// The owner's client ID and sign-in token (a secret in backups).
pub const CLIENT_KEY: &str = "soundcloud.client";
const API_BASE: &str = "https://api-v2.soundcloud.com";
/// What the page says before the mode is switched on.
pub const WARNING: &str = "This uses SoundCloud's web API, which SoundCloud has not approved for other apps. It can stop working at any time, and SoundCloud could act on the account whose token you enter. Tracks SoundCloud encrypts (Go+) are skipped.";
/// The HLS pages a browser relay reads at once.
const RELAY_CHUNK: usize = 64 * 1024;

pub fn track_uri(id: &str) -> String {
    format!("{TRACK_URI_PREFIX}{id}")
}

fn base_override() -> &'static std::sync::RwLock<Option<String>> {
    static B: std::sync::OnceLock<std::sync::RwLock<Option<String>>> = std::sync::OnceLock::new();
    B.get_or_init(|| std::sync::RwLock::new(None))
}

/// Tests: SoundCloud's API stood in.
pub fn set_api_base(url: &str) {
    *base_override().write().expect("soundcloud base") = Some(url.trim_end_matches('/').to_string());
}

fn api_base() -> String {
    base_override()
        .read()
        .expect("soundcloud base")
        .clone()
        .unwrap_or_else(|| API_BASE.to_string())
}

fn now() -> i64 {
    pm_library::db::now_unix()
}

fn upstream(e: impl std::fmt::Display) -> ApiError {
    ApiError::new(StatusCode::BAD_GATEWAY, format!("SoundCloud: {e}"))
}

fn numeric(id: &str) -> Result<&str, ApiError> {
    let id = id.strip_prefix("track:").unwrap_or(id);
    if id.is_empty() || id.len() > 20 || !id.bytes().all(|b| b.is_ascii_digit()) {
        return Err(ApiError::bad_request("bad SoundCloud track id"));
    }
    Ok(id)
}

// ---- state -------------------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Health {
    pub ok: bool,
    pub detail: String,
    pub at: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ModeState {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub health: Option<Health>,
    /// The signed-in user's name, from `me`, once a token is set.
    #[serde(default)]
    pub user: Option<String>,
}

#[derive(Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Client {
    pub id: String,
    /// The owner's own sign-in token (`Authorization: OAuth …`), for likes and playlists; optional.
    #[serde(default)]
    pub token: String,
    /// `settings` or `config`.
    #[serde(default)]
    pub from: String,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("id", &self.id)
            .field("token", &if self.token.is_empty() { "" } else { "<set>" })
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

fn config_client() -> &'static std::sync::OnceLock<Client> {
    static C: std::sync::OnceLock<Client> = std::sync::OnceLock::new();
    &C
}

/// The client from node.toml (`[soundcloud] client_id` / `token`), set once at start-up;
/// Settings override it.
pub fn set_config_client(id: &str, token: &str) {
    if !id.trim().is_empty() {
        let _ = config_client().set(Client {
            id: id.trim().into(),
            token: token.trim().into(),
            from: "config".into(),
        });
    }
}

/// The client the owner set: in Settings, else node.toml, else `PM_SOUNDCLOUD_CLIENT_ID` /
/// `PM_SOUNDCLOUD_TOKEN`.
pub fn client_of(lib: &pm_library::Library) -> Option<Client> {
    let kept: Option<Client> = lib
        .get_setting(CLIENT_KEY)
        .ok()
        .flatten()
        .and_then(|v| serde_json::from_str(&v).ok());
    if let Some(c) = kept.filter(|c| !c.id.is_empty()) {
        return Some(c);
    }
    if let Some(c) = config_client().get() {
        return Some(c.clone());
    }
    let id = std::env::var("PM_SOUNDCLOUD_CLIENT_ID").unwrap_or_default();
    let token = std::env::var("PM_SOUNDCLOUD_TOKEN").unwrap_or_default();
    (!id.trim().is_empty()).then(|| Client {
        id: id.trim().into(),
        token: token.trim().into(),
        from: "config".into(),
    })
}

/// On and with a client ID: SoundCloud searches and plays.
pub fn streams(s: &NodeState) -> bool {
    let lib = s.library.lock().expect("library poisoned");
    load(&lib).enabled && client_of(&lib).is_some()
}

fn ready(s: &NodeState) -> Result<Client, ApiError> {
    let lib = s.library.lock().expect("library poisoned");
    let st = load(&lib);
    let c = client_of(&lib).ok_or_else(|| {
        ApiError::new(
            StatusCode::PRECONDITION_FAILED,
            "Enter a SoundCloud client ID first (Sources › SoundCloud)",
        )
    })?;
    if !st.enabled {
        return Err(ApiError::new(
            StatusCode::PRECONDITION_FAILED,
            "SoundCloud is off: switch it on under Sources › SoundCloud",
        ));
    }
    Ok(c)
}

fn signed_in(s: &NodeState) -> Result<Client, ApiError> {
    let c = ready(s)?;
    if c.token.is_empty() {
        return Err(ApiError::new(
            StatusCode::PRECONDITION_FAILED,
            "Enter your SoundCloud sign-in token first (Sources › SoundCloud)",
        ));
    }
    Ok(c)
}

/// Whether a failure is the mode's (switch it off) or only this track's.
#[derive(Debug)]
pub enum StreamError {
    Mode(String),
    Track(String),
}

/// What an answer's status means.
fn judge(status: u16, what: &str) -> Result<(), StreamError> {
    match status {
        200..=299 => Ok(()),
        401 | 403 => Err(StreamError::Mode(format!(
            "SoundCloud refused the client ID ({status}) for {what}"
        ))),
        404 => Err(StreamError::Track(format!("SoundCloud has no {what}"))),
        429 => Err(StreamError::Track(
            "SoundCloud is rate-limiting this node; try again later".into(),
        )),
        _ => Err(StreamError::Track(format!(
            "SoundCloud answered {status} for {what}"
        ))),
    }
}

fn mark_down(s: &NodeState, why: &str) {
    tracing::warn!("SoundCloud (unofficial) switched off: {why}");
    update(s, |st| {
        st.enabled = false;
        st.health = Some(Health {
            ok: false,
            detail: why.to_string(),
            at: now(),
        });
    });
    crate::search::forget_answers();
}

// ---- API ---------------------------------------------------------------------------------------

/// `GET <api>/<path>?client_id=…` (or a full `next_href`), signed in when the owner gave a token.
async fn get_json(
    s: &NodeState,
    c: &Client,
    path: &str,
    query: &[(&str, String)],
) -> Result<Value, ApiError> {
    let url = if path.starts_with("http://") || path.starts_with("https://") {
        path.to_string()
    } else {
        format!("{}/{}", api_base(), path.trim_start_matches('/'))
    };
    let mut req = s
        .http_client()
        .get(&url)
        .query(&[("client_id", c.id.as_str())])
        .query(query);
    if !c.token.is_empty() {
        req = req.header(reqwest::header::AUTHORIZATION, format!("OAuth {}", c.token));
    }
    let resp = req.send().await.map_err(|e| upstream(e.without_url()))?;
    let status = resp.status().as_u16();
    let what = path.split('?').next().unwrap_or(path).to_string();
    match judge(status, &what) {
        Ok(()) => resp.json().await.map_err(upstream),
        Err(StreamError::Mode(m)) => {
            // With a token along, a refusal may be the token's, not the client's: a client-only
            // call tells them apart, so a stale token never switches the mode off.
            if !c.token.is_empty() && client_alone_works(s, c).await {
                return Err(ApiError::new(
                    StatusCode::UNAUTHORIZED,
                    "SoundCloud refused the sign-in token: it may have expired; enter a fresh one",
                ));
            }
            mark_down(s, &m);
            Err(ApiError::new(StatusCode::BAD_GATEWAY, m))
        }
        Err(StreamError::Track(m)) if status == 404 => Err(ApiError::not_found(m)),
        Err(StreamError::Track(m)) => Err(ApiError::new(StatusCode::BAD_GATEWAY, m)),
    }
}

/// A cheap call with the client ID alone: does SoundCloud still take it?
async fn client_alone_works(s: &NodeState, c: &Client) -> bool {
    s.http_client()
        .get(format!("{}/search/tracks", api_base()))
        .query(&[("client_id", c.id.as_str()), ("q", "a"), ("limit", "1")])
        .send()
        .await
        .is_ok_and(|r| r.status().is_success())
}

// ---- documents ---------------------------------------------------------------------------------

fn image_of(t: &Value) -> Option<String> {
    t["artwork_url"]
        .as_str()
        .or(t["user"]["avatar_url"].as_str())
        .filter(|u| !u.is_empty())
        .map(|u| u.replace("-large.", "-t500x500."))
}

fn year_of(t: &Value) -> Option<i32> {
    ["release_date", "display_date", "created_at"]
        .iter()
        .find_map(|k| t[*k].as_str())
        .and_then(|d| d.get(0..4)?.parse().ok())
}

fn artist_of(t: &Value) -> Option<String> {
    t["publisher_metadata"]["artist"]
        .as_str()
        .or(t["user"]["username"].as_str())
        .filter(|a| !a.is_empty())
        .map(str::to_string)
}

fn id_of(v: &Value) -> Option<String> {
    v["id"]
        .as_u64()
        .map(|n| n.to_string())
        .or_else(|| v["id"].as_str().map(str::to_string))
}

/// How a track's audio comes: progressive MP3, or HLS in MP3 or AAC. Encrypted transcodings are
/// never chosen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    Progressive,
    HlsMp3,
    HlsAac,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Pick {
    pub url: String,
    pub kind: Kind,
    pub preset: String,
    /// SoundCloud serves only a 30-second snippet of this track (`policy: SNIP`).
    pub preview: bool,
}

/// The transcoding to play, best first; `None` when every one is encrypted or broken.
pub fn pick(t: &Value) -> Option<Pick> {
    let snip = t["policy"].as_str() == Some("SNIP");
    let mut best: Option<Pick> = None;
    for tr in t["media"]["transcodings"].as_array()?.iter() {
        let proto = tr["format"]["protocol"].as_str().unwrap_or("");
        let mime = tr["format"]["mime_type"].as_str().unwrap_or("");
        let preset = tr["preset"].as_str().unwrap_or("");
        let url = tr["url"].as_str().unwrap_or("");
        if url.is_empty()
            || proto.contains("encrypted")
            || proto.starts_with("ctr-")
            || proto.starts_with("cbc-")
            || preset.starts_with("abr")
        {
            continue;
        }
        let kind = match (proto, mime) {
            ("progressive", m) if m.contains("mpeg") => Kind::Progressive,
            ("hls", m) if m.contains("mpeg") => Kind::HlsMp3,
            ("hls", m) if m.contains("mp4") => Kind::HlsAac,
            _ => continue,
        };
        if best.as_ref().is_none_or(|b| kind < b.kind) {
            best = Some(Pick {
                url: url.to_string(),
                kind,
                preset: preset.to_string(),
                preview: snip || tr["snipped"].as_bool() == Some(true),
            });
        }
    }
    best
}

fn format_of(t: &Value) -> String {
    match pick(t) {
        None => "Encrypted (SoundCloud Go+): not playable here".into(),
        Some(p) => {
            let base = match p.kind {
                Kind::Progressive => "MP3 (SoundCloud)",
                Kind::HlsMp3 => "MP3 (SoundCloud, HLS)",
                Kind::HlsAac => "AAC (SoundCloud, HLS)",
            };
            if p.preview {
                format!("{base} · 30-second preview")
            } else {
                base.into()
            }
        }
    }
}

fn track_hit(t: &Value) -> Option<SourceHit> {
    let id = id_of(t)?;
    let title = t["title"].as_str()?.to_string();
    Some(SourceHit {
        title,
        creator: artist_of(t),
        year: year_of(t),
        downloads: t["playback_count"].as_u64(),
        image: image_of(t),
        kind: "track".into(),
        duration_ms: t["full_duration"].as_u64().or(t["duration"].as_u64()),
        library_artist_id: None,
        isrc: None,
        upc: None,
        library_track_id: None,
        library_album_id: None,
        uri: Some(track_uri(&id)),
        url: t["permalink_url"]
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| format!("https://soundcloud.com/tracks/{id}")),
        id,
    })
}

fn set_hit(p: &Value) -> Option<SourceHit> {
    let id = id_of(p)?;
    let title = p["title"].as_str()?.to_string();
    Some(SourceHit {
        title,
        creator: p["user"]["username"].as_str().map(str::to_string),
        year: year_of(p),
        downloads: None,
        image: image_of(p),
        kind: "item".into(),
        duration_ms: p["duration"].as_u64(),
        library_artist_id: None,
        isrc: None,
        upc: None,
        library_track_id: None,
        library_album_id: None,
        uri: None,
        url: p["permalink_url"].as_str().unwrap_or("").to_string(),
        id: format!("playlist:{id}"),
    })
}

fn source_track(t: &Value, no: u32) -> Option<SourceTrack> {
    let id = id_of(t)?;
    Some(SourceTrack {
        title: t["title"].as_str()?.to_string(),
        track_no: Some(no),
        duration_ms: t["full_duration"].as_u64().or(t["duration"].as_u64()),
        format: format_of(t),
        url: track_uri(&id),
        artist: artist_of(t),
    })
}

fn is_soundcloud_url(term: &str) -> bool {
    let t = term.trim();
    (t.starts_with("https://") || t.starts_with("http://"))
        && t.split('/')
            .nth(2)
            .is_some_and(|h| h == "soundcloud.com" || h.ends_with(".soundcloud.com"))
}

/// A set's tracks in order: the full documents it carries, the stubs (id only) fetched in batches.
async fn set_tracks(s: &NodeState, c: &Client, p: &Value) -> Result<Vec<Value>, ApiError> {
    let listed = p["tracks"].as_array().cloned().unwrap_or_default();
    let stubs: Vec<String> = listed
        .iter()
        .filter(|t| t["title"].is_null())
        .filter_map(id_of)
        .collect();
    let mut fetched: std::collections::HashMap<String, Value> = std::collections::HashMap::new();
    for chunk in stubs.chunks(50) {
        let v = get_json(s, c, "tracks", &[("ids", chunk.join(","))]).await?;
        for t in v.as_array().cloned().unwrap_or_default() {
            if let Some(id) = id_of(&t) {
                fetched.insert(id, t);
            }
        }
    }
    Ok(listed
        .into_iter()
        .filter_map(|t| {
            if t["title"].is_null() {
                fetched.remove(&id_of(&t)?)
            } else {
                Some(t)
            }
        })
        .collect())
}

fn set_item(p: &Value, tracks: &[Value]) -> Option<SourceItem> {
    let id = id_of(p)?;
    Some(SourceItem {
        source: SOURCE_ID.into(),
        id: format!("playlist:{id}"),
        title: p["title"].as_str()?.to_string(),
        creator: p["user"]["username"].as_str().map(str::to_string),
        year: year_of(p),
        description: p["description"]
            .as_str()
            .filter(|d| !d.trim().is_empty())
            .map(str::to_string),
        licence: p["license"]
            .as_str()
            .filter(|l| !l.trim().is_empty() && *l != "all-rights-reserved")
            .map(str::to_string),
        image: image_of(p),
        url: p["permalink_url"].as_str().unwrap_or("").to_string(),
        tracks: tracks
            .iter()
            .enumerate()
            .filter_map(|(i, t)| source_track(t, i as u32 + 1))
            .collect(),
        library_album_id: None,
    })
}

fn track_item(t: &Value) -> Option<SourceItem> {
    let h = track_hit(t)?;
    Some(SourceItem {
        source: SOURCE_ID.into(),
        id: h.id.clone(),
        title: h.title.clone(),
        creator: h.creator.clone(),
        year: h.year,
        description: t["description"]
            .as_str()
            .filter(|d| !d.trim().is_empty())
            .map(str::to_string),
        licence: t["license"]
            .as_str()
            .filter(|l| !l.trim().is_empty() && *l != "all-rights-reserved")
            .map(str::to_string),
        image: h.image.clone(),
        url: h.url.clone(),
        tracks: source_track(t, 1).into_iter().collect(),
        library_album_id: None,
    })
}

pub struct SoundCloud;

#[async_trait::async_trait]
impl StreamingService for SoundCloud {
    fn id(&self) -> &'static str {
        SOURCE_ID
    }

    fn unified(&self, s: &NodeState) -> bool {
        streams(s)
    }

    fn features(&self) -> &'static [&'static str] {
        &["search", "play", "library", "playlists"]
    }

    fn stage(&self) -> &'static str {
        "unofficial"
    }

    fn info(&self, s: &NodeState) -> SourceInfo {
        let lib = s.library.lock().expect("library poisoned");
        let st = load(&lib);
        let client = client_of(&lib);
        let status = if client.is_none() {
            "not configured: enter a SoundCloud client ID under Sources › SoundCloud".to_string()
        } else if let Some(h) = st.health.as_ref().filter(|h| !h.ok && !st.enabled) {
            format!("error: {}", h.detail)
        } else if !st.enabled {
            "off: switch SoundCloud (unofficial) on under Sources › SoundCloud".to_string()
        } else if client.is_some_and(|c| !c.token.is_empty()) {
            "signed in".to_string()
        } else {
            "ready".to_string()
        };
        SourceInfo {
            id: SOURCE_ID.into(),
            name: "SoundCloud".into(),
            description: "Unofficial: tracks and sets through SoundCloud's web API with a client ID you enter; your likes and playlists with your own sign-in token".into(),
            status,
        }
    }

    async fn search(&self, s: &NodeState, term: &str, limit: u32) -> Result<Vec<SourceHit>, ApiError> {
        let c = ready(s)?;
        let limit = limit.clamp(1, 50);
        if is_soundcloud_url(term) {
            let v = get_json(s, &c, "resolve", &[("url", term.trim().to_string())]).await?;
            return Ok(match v["kind"].as_str() {
                Some("track") => track_hit(&v).into_iter().collect(),
                Some("playlist") => set_hit(&v).into_iter().collect(),
                _ => Vec::new(),
            });
        }
        let tq = [("q", term.to_string()), ("limit", limit.to_string())];
        let pq = [("q", term.to_string()), ("limit", "5".to_string())];
        let (tracks, sets) = tokio::join!(
            get_json(s, &c, "search/tracks", &tq),
            get_json(s, &c, "search/playlists", &pq)
        );
        let tracks = tracks?;
        let mut out: Vec<SourceHit> = sets
            .ok()
            .and_then(|v| v["collection"].as_array().cloned())
            .unwrap_or_default()
            .iter()
            .filter_map(set_hit)
            .take(3)
            .collect();
        out.extend(
            tracks["collection"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(track_hit)
                .take(limit as usize),
        );
        Ok(out)
    }

    async fn item(&self, s: &NodeState, id: &str) -> Result<SourceItem, ApiError> {
        let c = ready(s)?;
        if let Some(pid) = id.strip_prefix("playlist:") {
            if pid.is_empty() || !pid.bytes().all(|b| b.is_ascii_digit()) {
                return Err(ApiError::bad_request("bad SoundCloud set id"));
            }
            let p = get_json(s, &c, &format!("playlists/{pid}"), &[]).await?;
            let tracks = set_tracks(s, &c, &p).await?;
            return set_item(&p, &tracks)
                .ok_or_else(|| ApiError::not_found(format!("no SoundCloud set {pid}")));
        }
        let tid = numeric(id)?;
        let t = get_json(s, &c, &format!("tracks/{tid}"), &[]).await?;
        track_item(&t).ok_or_else(|| ApiError::not_found(format!("no SoundCloud track {tid}")))
    }
}

// ---- the owner's likes and playlists -----------------------------------------------------------

async fn me(s: &NodeState, c: &Client) -> Result<Value, ApiError> {
    let v = get_json(s, c, "me", &[]).await?;
    if id_of(&v).is_none() {
        return Err(ApiError::new(
            StatusCode::BAD_GATEWAY,
            "SoundCloud didn't recognise the sign-in token",
        ));
    }
    Ok(v)
}

#[derive(Debug, Default, Serialize)]
pub struct Likes {
    pub tracks: Vec<SourceHit>,
    pub playlists: Vec<SourceHit>,
}

async fn likes_of(s: &NodeState, c: &Client) -> Result<(Likes, Vec<Value>), ApiError> {
    let uid = id_of(&me(s, c).await?).expect("checked");
    let mut page = get_json(
        s,
        c,
        &format!("users/{uid}/likes"),
        &[("limit", "200".into()), ("linked_partitioning", "1".into())],
    )
    .await?;
    let mut out = Likes::default();
    let mut docs = Vec::new();
    for _ in 0..10 {
        for e in page["collection"].as_array().cloned().unwrap_or_default() {
            if !e["track"].is_null() {
                if let Some(h) = track_hit(&e["track"]) {
                    out.tracks.push(h);
                    docs.push(e["track"].clone());
                }
            } else if !e["playlist"].is_null() {
                if let Some(h) = set_hit(&e["playlist"]) {
                    out.playlists.push(h);
                    docs.push(e["playlist"].clone());
                }
            }
        }
        let Some(next) = page["next_href"].as_str().map(str::to_string) else {
            break;
        };
        page = get_json(s, c, &next, &[]).await?;
    }
    Ok((out, docs))
}

/// `GET /v1/sources/soundcloud/likes`: the signed-in user's liked tracks and sets.
pub(crate) async fn likes(State(s): State<Arc<NodeState>>) -> ApiResult<Likes> {
    let c = signed_in(&s)?;
    Ok(Json(likes_of(&s, &c).await?.0))
}

#[derive(Debug, Default, Serialize)]
pub struct LikesImport {
    pub imported: usize,
    pub skipped: usize,
}

/// `POST /v1/sources/soundcloud/likes/import`: bookmark them here too, by their SoundCloud link.
pub(crate) async fn import_likes(State(s): State<Arc<NodeState>>) -> ApiResult<LikesImport> {
    let c = signed_in(&s)?;
    let (_, docs) = likes_of(&s, &c).await?;
    let mut r = LikesImport::default();
    {
        let lib = s.library.lock().expect("library poisoned");
        for d in &docs {
            match d["permalink_url"]
                .as_str()
                .and_then(|u| pm_model::MusicUri::parse(u).ok())
            {
                Some(uri) => {
                    crate::bookmarks::add(&lib, uri)?;
                    r.imported += 1;
                }
                None => r.skipped += 1,
            }
        }
    }
    s.notify_library();
    Ok(Json(r))
}

#[derive(Debug, Clone, Serialize)]
pub struct SetSummary {
    pub id: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub items: u64,
    pub url: String,
}

/// `GET /v1/sources/soundcloud/playlists`: the signed-in user's own sets.
pub(crate) async fn playlists(State(s): State<Arc<NodeState>>) -> ApiResult<Vec<SetSummary>> {
    let c = signed_in(&s)?;
    let uid = id_of(&me(&s, &c).await?).expect("checked");
    let mut page = get_json(
        &s,
        &c,
        &format!("users/{uid}/playlists"),
        &[("limit", "200".into()), ("linked_partitioning", "1".into())],
    )
    .await?;
    let mut out = Vec::new();
    for _ in 0..5 {
        for p in page["collection"].as_array().cloned().unwrap_or_default() {
            let Some(id) = id_of(&p) else { continue };
            out.push(SetSummary {
                name: p["title"].as_str().unwrap_or("").to_string(),
                description: p["description"]
                    .as_str()
                    .filter(|d| !d.trim().is_empty())
                    .map(str::to_string),
                items: p["track_count"]
                    .as_u64()
                    .or_else(|| p["tracks"].as_array().map(|a| a.len() as u64))
                    .unwrap_or(0),
                url: p["permalink_url"].as_str().unwrap_or("").to_string(),
                id,
            });
        }
        let Some(next) = page["next_href"].as_str().map(str::to_string) else {
            break;
        };
        page = get_json(&s, &c, &next, &[]).await?;
    }
    Ok(Json(out))
}

/// `POST /v1/sources/soundcloud/playlists/{id}/import[?keep_source=true]`: a set as a private
/// Pubky playlist of its tracks' SoundCloud links, pinned to SoundCloud when asked.
pub(crate) async fn import_playlist(
    State(s): State<Arc<NodeState>>,
    Path(id): Path<String>,
    Query(opt): Query<ImportOptions>,
) -> ApiResult<crate::tidal::ImportReport> {
    let c = ready(&s)?;
    if id.is_empty() || !id.bytes().all(|b| b.is_ascii_digit()) {
        return Err(ApiError::bad_request("bad SoundCloud set id"));
    }
    let keep = opt.keep_source.unwrap_or(false);
    let p = get_json(&s, &c, &format!("playlists/{id}"), &[]).await?;
    let name = p["title"].as_str().unwrap_or("SoundCloud set").to_string();
    let description = p["description"]
        .as_str()
        .filter(|d| !d.trim().is_empty())
        .map(str::to_string);
    let tracks = set_tracks(&s, &c, &p).await?;
    let mut uris = Vec::new();
    let mut sources = Vec::new();
    let mut skipped = 0usize;
    for t in &tracks {
        let link = t["permalink_url"].as_str().unwrap_or("");
        match pm_model::MusicUri::parse(link) {
            Ok(u) => {
                uris.push(u.to_url());
                sources.push(keep.then(|| pm_pubky::ItemSource {
                    service: SOURCE_ID.into(),
                    uri: Some(link.to_string()),
                }));
            }
            Err(_) => skipped += 1,
        }
    }
    if uris.is_empty() {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "none of the set's tracks could be identified",
        ));
    }
    let imported = uris.len();
    let Json(detail) = crate::playlists::create(
        State(s.clone()),
        Json(CreatePlaylist {
            name,
            description,
            image: image_of(&p),
            uris,
            track_ids: Vec::new(),
            sources,
            ..Default::default()
        }),
    )
    .await?;
    Ok(Json(crate::tidal::ImportReport {
        imported,
        skipped,
        playlist: Some(detail),
    }))
}

// ---- the tab -----------------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct SoundCloudStatus {
    pub enabled: bool,
    /// Where the client comes from: `settings` or `config`; absent = none yet.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_from: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    /// A sign-in token is set (it never comes back).
    pub signed_in: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub health: Option<Health>,
    pub warning: String,
}

fn view(s: &NodeState) -> SoundCloudStatus {
    let lib = s.library.lock().expect("library poisoned");
    let st = load(&lib);
    let c = client_of(&lib);
    SoundCloudStatus {
        enabled: st.enabled,
        client_from: c.as_ref().map(|c| c.from.clone()),
        signed_in: c.as_ref().is_some_and(|c| !c.token.is_empty()),
        client_id: c.map(|c| c.id),
        user: st.user,
        health: st.health,
        warning: WARNING.into(),
    }
}

/// `GET /v1/sources/soundcloud/unofficial`.
pub(crate) async fn get_status(State(s): State<Arc<NodeState>>) -> ApiResult<SoundCloudStatus> {
    Ok(Json(view(&s)))
}

#[derive(Debug, Deserialize)]
pub struct SoundCloudUpdate {
    pub enabled: Option<bool>,
    /// Replace the client (an empty id forgets it and goes back to node.toml's).
    pub client_id: Option<String>,
    /// The owner's sign-in token; empty forgets it. Checked with `me` before it is kept.
    pub token: Option<String>,
}

/// `PUT /v1/sources/soundcloud/unofficial {enabled?, client_id?, token?}`.
pub(crate) async fn put(
    State(s): State<Arc<NodeState>>,
    Json(u): Json<SoundCloudUpdate>,
) -> ApiResult<SoundCloudStatus> {
    let mut check_token = false;
    {
        let lib = s.library.lock().expect("library poisoned");
        let kept: Option<Client> = lib
            .get_setting(CLIENT_KEY)
            .ok()
            .flatten()
            .and_then(|v| serde_json::from_str(&v).ok());
        let mut next = kept.clone();
        if let Some(id) = u.client_id.as_deref() {
            let id = id.trim();
            if id.is_empty() {
                next = None;
            } else {
                if id.len() < 16
                    || id.len() > 64
                    || !id
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
                {
                    return Err(ApiError::bad_request(
                        "A SoundCloud client ID is about 32 letters and digits",
                    ));
                }
                let token = next.as_ref().map(|c| c.token.clone()).unwrap_or_default();
                next = Some(Client {
                    id: id.into(),
                    token,
                    from: "settings".into(),
                });
            }
        }
        if let Some(t) = u.token.as_deref() {
            let t = t.trim();
            if t.len() > 512 || !t.bytes().all(|b| b.is_ascii_graphic()) {
                return Err(ApiError::bad_request(
                    "That doesn't look like a SoundCloud sign-in token",
                ));
            }
            match next.as_mut() {
                Some(c) => c.token = t.into(),
                None => {
                    // No client of the owner's in Settings: the token joins the config client's id
                    let Some(cfg) = client_of(&lib) else {
                        return Err(ApiError::bad_request("Enter a SoundCloud client ID first"));
                    };
                    next = Some(Client {
                        id: cfg.id,
                        token: t.into(),
                        from: "settings".into(),
                    });
                }
            }
            check_token = !t.is_empty();
        }
        if next != kept {
            match &next {
                Some(c) => {
                    lib.set_setting(CLIENT_KEY, &serde_json::to_string(c).expect("client serializes"))?
                }
                None => {
                    lib.delete_setting(CLIENT_KEY)?;
                }
            }
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
        if u.token.as_deref().is_some_and(|t| t.trim().is_empty()) {
            st.user = None;
        }
    });
    crate::search::forget_answers();
    if check_token {
        // Does SoundCloud know this token? (The mode must be on for the call.)
        let c = {
            let lib = s.library.lock().expect("library poisoned");
            client_of(&lib)
        };
        if let Some(c) = c {
            match me(&s, &c).await {
                Ok(v) => {
                    let name = v["username"]
                        .as_str()
                        .or(v["permalink"].as_str())
                        .map(str::to_string);
                    update(&s, |st| st.user = name);
                }
                Err(e) => {
                    // Keep the id, drop the token the owner just pasted
                    let lib = s.library.lock().expect("library poisoned");
                    if let Some(mut k) = lib
                        .get_setting(CLIENT_KEY)
                        .ok()
                        .flatten()
                        .and_then(|v| serde_json::from_str::<Client>(&v).ok())
                    {
                        k.token.clear();
                        lib.set_setting(CLIENT_KEY, &serde_json::to_string(&k).expect("client serializes"))?;
                    }
                    drop(lib);
                    return Err(ApiError::bad_request(format!(
                        "SoundCloud didn't take the sign-in token: {}",
                        e.message
                    )));
                }
            }
        }
    }
    Ok(Json(view(&s)))
}

// ---- streams -----------------------------------------------------------------------------------

/// The media URL of a track: its document, the transcoding to play, then the URL behind it.
fn stream_url_blocking(
    library: &std::sync::Mutex<pm_library::Library>,
    id: &str,
) -> Result<(String, Pick), StreamError> {
    let (c, st) = {
        let lib = library.lock().map_err(|_| StreamError::Track("library".into()))?;
        (client_of(&lib), load(&lib))
    };
    let c = c.ok_or_else(|| StreamError::Track("no SoundCloud client ID on this node".into()))?;
    if !st.enabled {
        return Err(StreamError::Track(
            "SoundCloud is off (Sources › SoundCloud)".into(),
        ));
    }
    let http = crate::sources::http_client();
    let with_auth = |r: reqwest::blocking::RequestBuilder| {
        if c.token.is_empty() {
            r
        } else {
            r.header(reqwest::header::AUTHORIZATION, format!("OAuth {}", c.token))
        }
    };
    let resp = with_auth(
        http.get(format!("{}/tracks/{id}", api_base()))
            .query(&[("client_id", c.id.as_str())]),
    )
    .send()
    .map_err(|e| StreamError::Track(format!("SoundCloud: {}", e.without_url())))?;
    judge(resp.status().as_u16(), &format!("track {id}"))?;
    let t: Value = resp
        .json()
        .map_err(|e| StreamError::Track(format!("SoundCloud: {e}")))?;
    resolve_media(&t, |url, q| {
        let resp = with_auth(http.get(url).query(&[("client_id", c.id.as_str())]).query(q))
            .send()
            .map_err(|e| StreamError::Track(format!("SoundCloud: {}", e.without_url())))?;
        let status = resp.status().as_u16();
        Ok((status, resp.json().unwrap_or(Value::Null)))
    })
}

/// From a track document to its media URL, through `fetch(transcoding url, query)`.
fn resolve_media(
    t: &Value,
    fetch: impl FnOnce(&str, &[(&str, String)]) -> Result<(u16, Value), StreamError>,
) -> Result<(String, Pick), StreamError> {
    let p = pick(t).ok_or_else(|| {
        StreamError::Track("SoundCloud encrypts this track (Go+); the node doesn't play those".into())
    })?;
    let mut q: Vec<(&str, String)> = Vec::new();
    if let Some(a) = t["track_authorization"].as_str() {
        q.push(("track_authorization", a.to_string()));
    }
    let (status, v) = fetch(&p.url, &q)?;
    judge(status, "the stream")?;
    let url = v["url"]
        .as_str()
        .filter(|u| u.starts_with("https://") || u.starts_with("http://"))
        .ok_or_else(|| StreamError::Mode("SoundCloud answered without a stream URL".into()))?;
    Ok((url.to_string(), p))
}

async fn stream_url(s: &NodeState, id: &str) -> Result<(String, Pick), StreamError> {
    let c = ready(s).map_err(|e| StreamError::Track(e.message))?;
    let t = get_json(s, &c, &format!("tracks/{id}"), &[])
        .await
        .map_err(|e| StreamError::Track(e.message))?;
    let p = pick(&t).ok_or_else(|| {
        StreamError::Track("SoundCloud encrypts this track (Go+); the node doesn't play those".into())
    })?;
    let mut q: Vec<(&str, String)> = vec![("client_id", c.id.clone())];
    if let Some(a) = t["track_authorization"].as_str() {
        q.push(("track_authorization", a.to_string()));
    }
    let mut req = s.http_client().get(&p.url).query(&q);
    if !c.token.is_empty() {
        req = req.header(reqwest::header::AUTHORIZATION, format!("OAuth {}", c.token));
    }
    let resp = req
        .send()
        .await
        .map_err(|e| StreamError::Track(format!("SoundCloud: {}", e.without_url())))?;
    judge(resp.status().as_u16(), "the stream")?;
    let v: Value = resp.json().await.unwrap_or(Value::Null);
    let url = v["url"]
        .as_str()
        .filter(|u| u.starts_with("https://") || u.starts_with("http://"))
        .ok_or_else(|| StreamError::Mode("SoundCloud answered without a stream URL".into()))?;
    Ok((url.to_string(), p))
}

/// Record how the source did: a failure of the mode itself switches it off.
pub fn record<T>(library: &std::sync::Mutex<pm_library::Library>, r: &Result<T, StreamError>) {
    let Ok(lib) = library.lock() else { return };
    let mut st = load(&lib);
    match r {
        Ok(_) => {
            st.health = Some(Health {
                ok: true,
                detail: "tracks are playing".into(),
                at: now(),
            });
        }
        Err(StreamError::Mode(e)) => {
            tracing::warn!("SoundCloud (unofficial) switched off: {e}");
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

/// A `soundcloud:track:<id>` for a zone: progressive MP3 as any HTTP source, HLS read back to back.
pub(crate) fn open_track_blocking(
    library: &std::sync::Mutex<pm_library::Library>,
    id: &str,
) -> Result<Box<dyn pm_zones::FrameSource>, pm_zones::SourceError> {
    let fail = |reason: String| pm_zones::SourceError::Open {
        uri: track_uri(id),
        reason,
    };
    if id.is_empty() || !id.bytes().all(|b| b.is_ascii_digit()) {
        return Err(fail("bad SoundCloud track id".into()));
    }
    let r = stream_url_blocking(library, id);
    record(library, &r);
    let (url, p) = r.map_err(|e| match e {
        StreamError::Mode(m) | StreamError::Track(m) => fail(m),
    })?;
    match p.kind {
        Kind::Progressive => crate::sources::open_http_stream(&url)
            .map_err(|_| fail("the SoundCloud stream did not open".into())),
        Kind::HlsMp3 | Kind::HlsAac => {
            let reader = crate::hls::HlsReader::open(crate::sources::http_client(), &url)
                .map_err(|e| fail(format!("SoundCloud HLS: {e}")))?;
            let ext = if p.kind == Kind::HlsMp3 { "mp3" } else { "m4a" };
            crate::sources::frame_source_from_reader(Box::new(reader), Some(ext), &track_uri(id))
        }
    }
}

/// `GET /v1/sources/soundcloud/stream/{id}`: the track for browsers, relayed by the node so the
/// client ID and token never reach the page. HLS is stitched into one stream on the way.
pub(crate) async fn stream(
    State(s): State<Arc<NodeState>>,
    Path(id): Path<String>,
) -> Result<axum::response::Response, ApiError> {
    let id = numeric(&id)?.to_string();
    let r = stream_url(&s, &id).await;
    record(&s.library, &r);
    let (url, p) = r.map_err(|e| match e {
        StreamError::Mode(m) => ApiError::new(StatusCode::BAD_GATEWAY, m),
        StreamError::Track(m) => ApiError::not_found(m),
    })?;
    let mime = match p.kind {
        Kind::Progressive | Kind::HlsMp3 => "audio/mpeg",
        Kind::HlsAac => "audio/mp4",
    };
    let out = axum::response::Response::builder()
        .header(axum::http::header::CONTENT_TYPE, mime)
        .header(axum::http::header::CACHE_CONTROL, "private, max-age=600");
    let body = match p.kind {
        Kind::Progressive => {
            let resp = s
                .http_client()
                .get(&url)
                .send()
                .await
                .map_err(|e| upstream(e.without_url()))?;
            if !resp.status().is_success() {
                return Err(upstream(format!("stream answered {}", resp.status())));
            }
            axum::body::Body::from_stream(resp.bytes_stream())
        }
        Kind::HlsMp3 | Kind::HlsAac => {
            let mut reader = tokio::task::spawn_blocking(move || {
                crate::hls::HlsReader::open(crate::sources::http_client(), &url)
            })
            .await
            .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
            .map_err(|e| upstream(format!("HLS: {e}")))?;
            let (tx, rx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(8);
            tokio::task::spawn_blocking(move || {
                use std::io::Read;
                let mut buf = vec![0u8; RELAY_CHUNK];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            if tx
                                .blocking_send(Ok(bytes::Bytes::copy_from_slice(&buf[..n])))
                                .is_err()
                            {
                                break;
                            }
                        }
                        Err(e) => {
                            let _ = tx.blocking_send(Err(e));
                            break;
                        }
                    }
                }
            });
            axum::body::Body::from_stream(tokio_stream::wrappers::ReceiverStream::new(rx))
        }
    };
    out.body(body)
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(transcodings: Value, policy: &str) -> Value {
        serde_json::json!({
            "id": 123456, "title": "Night Drive", "kind": "track", "policy": policy,
            "permalink_url": "https://soundcloud.com/someone/night-drive", "duration": 200000, "full_duration": 200000,
            "artwork_url": "https://i1.sndcdn.com/artworks-x-large.jpg", "created_at": "2024-03-02T10:00:00Z",
            "user": {"username": "Someone", "avatar_url": "https://i1.sndcdn.com/avatars-y-large.jpg"},
            "publisher_metadata": {"artist": "Someone Else"}, "playback_count": 42,
            "track_authorization": "eyJ…", "media": {"transcodings": transcodings}
        })
    }

    #[test]
    fn progressive_mp3_wins_and_encrypted_tracks_are_skipped() {
        let t = track(
            serde_json::json!([
                {"url": "https://api-v2.soundcloud.com/media/1/hls", "preset": "mp3_1_0", "format": {"protocol": "hls", "mime_type": "audio/mpeg"}},
                {"url": "https://api-v2.soundcloud.com/media/1/progressive", "preset": "mp3_1_0", "format": {"protocol": "progressive", "mime_type": "audio/mpeg"}},
                {"url": "https://api-v2.soundcloud.com/media/1/aac", "preset": "aac_160k", "format": {"protocol": "hls", "mime_type": "audio/mp4; codecs=\"mp4a.40.2\""}, "quality": "hq"},
                {"url": "https://api-v2.soundcloud.com/media/1/enc", "preset": "aac_256k", "format": {"protocol": "encrypted-hls", "mime_type": "audio/mp4"}},
                {"url": "https://api-v2.soundcloud.com/media/1/abr", "preset": "abr_sq", "format": {"protocol": "hls", "mime_type": "audio/mpeg"}}
            ]),
            "ALLOW",
        );
        let p = pick(&t).unwrap();
        assert_eq!(
            (p.kind, p.url.as_str(), p.preview),
            (
                Kind::Progressive,
                "https://api-v2.soundcloud.com/media/1/progressive",
                false
            )
        );
        assert_eq!(format_of(&t), "MP3 (SoundCloud)");
        let h = track_hit(&t).unwrap();
        assert_eq!(
            (h.id.as_str(), h.creator.as_deref(), h.year),
            ("123456", Some("Someone Else"), Some(2024))
        );
        assert_eq!(h.uri.as_deref(), Some("soundcloud:track:123456"));
        assert_eq!(
            h.image.as_deref(),
            Some("https://i1.sndcdn.com/artworks-x-t500x500.jpg")
        );
        // Only HLS: AAC when there is no MP3; only encrypted: nothing
        let t = track(
            serde_json::json!([
                {"url": "https://x/aac", "preset": "aac_160k", "format": {"protocol": "hls", "mime_type": "audio/mp4"}},
                {"url": "https://x/ctr", "preset": "aac_256k", "format": {"protocol": "ctr-encrypted-hls", "mime_type": "audio/mp4"}}
            ]),
            "SNIP",
        );
        let p = pick(&t).unwrap();
        assert_eq!((p.kind, p.preview), (Kind::HlsAac, true));
        assert_eq!(format_of(&t), "AAC (SoundCloud, HLS) · 30-second preview");
        let t = track(
            serde_json::json!([{"url": "https://x/ctr", "preset": "aac_256k", "format": {"protocol": "ctr-encrypted-hls", "mime_type": "audio/mp4"}}]),
            "ALLOW",
        );
        assert!(pick(&t).is_none());
        assert!(format_of(&t).contains("not playable"));
        assert!(
            is_soundcloud_url("https://soundcloud.com/someone/night-drive")
                && !is_soundcloud_url("night drive")
        );
    }

    #[test]
    fn the_media_url_comes_through_the_transcoding_with_the_authorization() {
        let t = track(
            serde_json::json!([{"url": "https://api-v2.soundcloud.com/media/1/progressive", "preset": "mp3_1_0", "format": {"protocol": "progressive", "mime_type": "audio/mpeg"}}]),
            "ALLOW",
        );
        let (url, p) = resolve_media(&t, |u, q| {
            assert_eq!(u, "https://api-v2.soundcloud.com/media/1/progressive");
            assert_eq!(q, &[("track_authorization", "eyJ…".to_string())]);
            Ok((
                200,
                serde_json::json!({"url": "https://cf-media.sndcdn.com/a.mp3?Policy=x"}),
            ))
        })
        .unwrap();
        assert_eq!(
            (url.as_str(), p.kind),
            ("https://cf-media.sndcdn.com/a.mp3?Policy=x", Kind::Progressive)
        );
        // A refused client turns the mode off; a missing track is only that track's
        assert!(matches!(
            resolve_media(&t, |_, _| Ok((401, Value::Null))),
            Err(StreamError::Mode(_))
        ));
        assert!(matches!(judge(404, "track 1"), Err(StreamError::Track(_))));
        assert!(matches!(judge(403, "search"), Err(StreamError::Mode(_))));
        let c = Client {
            id: "abc".into(),
            token: "secret".into(),
            from: "settings".into(),
        };
        assert!(format!("{c:?}").contains("<set>") && !format!("{c:?}").contains("secret"));
    }
}
